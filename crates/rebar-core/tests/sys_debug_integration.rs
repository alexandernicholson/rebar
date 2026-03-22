use std::sync::Arc;
use std::time::Duration;

use rebar_core::gen_server::{spawn_gen_server, CallError, GenServer, GenServerContext};
use rebar_core::process::ProcessId;
use rebar_core::runtime::Runtime;
use rebar_core::sys::ProcessRunState;

// ---------------------------------------------------------------------------
// Test GenServer: a simple counter
// ---------------------------------------------------------------------------

struct Counter;

#[async_trait::async_trait]
impl GenServer for Counter {
    type State = u64;
    type Call = CounterCall;
    type Cast = CounterCast;
    type Reply = u64;

    async fn init(&self, _ctx: &GenServerContext) -> Result<Self::State, String> {
        Ok(0)
    }

    async fn handle_call(
        &self,
        msg: Self::Call,
        _from: ProcessId,
        state: &mut Self::State,
        _ctx: &GenServerContext,
    ) -> Self::Reply {
        match msg {
            CounterCall::Get => *state,
            CounterCall::IncrementAndGet => {
                *state += 1;
                *state
            }
        }
    }

    async fn handle_cast(
        &self,
        msg: Self::Cast,
        state: &mut Self::State,
        _ctx: &GenServerContext,
    ) {
        match msg {
            CounterCast::Increment => *state += 1,
            CounterCast::Set(v) => *state = v,
        }
    }
}

#[derive(Debug)]
enum CounterCall {
    Get,
    IncrementAndGet,
}

#[derive(Debug)]
enum CounterCast {
    Increment,
    Set(u64),
}

/// Shorthand timeout used across tests.
const T: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// 1. sys_get_state: returns current state
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sys_get_state_returns_initial_state() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;

    let state = server.sys_get_state(T).await.unwrap();
    assert_eq!(state, 0);
}

#[tokio::test]
async fn sys_get_state_reflects_mutations() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;

    // Increment a few times
    server.cast(CounterCast::Increment).unwrap();
    server.cast(CounterCast::Increment).unwrap();
    server.cast(CounterCast::Increment).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let state = server.sys_get_state(T).await.unwrap();
    assert_eq!(state, 3);
}

#[tokio::test]
async fn sys_get_state_after_call_mutation() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;

    let reply = server.call(CounterCall::IncrementAndGet, T).await.unwrap();
    assert_eq!(reply, 1);

    let state = server.sys_get_state(T).await.unwrap();
    assert_eq!(state, 1);
}

// ---------------------------------------------------------------------------
// 2. sys_suspend: stops message processing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sys_suspend_stops_call_processing() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;

    server.sys_suspend(T).await.unwrap();

    // A call should now time out because the server is suspended
    let result = server.call(CounterCall::Get, Duration::from_millis(100)).await;
    assert!(matches!(result, Err(CallError::Timeout)));
}

#[tokio::test]
async fn sys_suspend_stops_cast_processing() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;

    // Set the state to a known value first
    server.cast(CounterCast::Set(10)).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    server.sys_suspend(T).await.unwrap();

    // Cast some increments while suspended
    server.cast(CounterCast::Increment).unwrap();
    server.cast(CounterCast::Increment).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // State should still be 10 (casts not processed while suspended)
    let state = server.sys_get_state(T).await.unwrap();
    assert_eq!(state, 10);
}

// ---------------------------------------------------------------------------
// 3. sys_resume: restarts processing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sys_resume_restarts_call_processing() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;

    server.sys_suspend(T).await.unwrap();
    server.sys_resume(T).await.unwrap();

    // Calls should work again
    let reply = server.call(CounterCall::Get, T).await.unwrap();
    assert_eq!(reply, 0);
}

#[tokio::test]
async fn sys_resume_processes_queued_messages() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;

    server.sys_suspend(T).await.unwrap();

    // Queue some casts while suspended
    server.cast(CounterCast::Increment).unwrap();
    server.cast(CounterCast::Increment).unwrap();
    server.cast(CounterCast::Increment).unwrap();

    // Resume and wait for queued messages to be processed
    server.sys_resume(T).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let state = server.sys_get_state(T).await.unwrap();
    assert_eq!(state, 3);
}

// ---------------------------------------------------------------------------
// 4. sys commands work while suspended
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sys_get_state_works_while_suspended() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;

    // Set up some state, then suspend
    server.cast(CounterCast::Set(42)).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    server.sys_suspend(T).await.unwrap();

    // get_state should still work
    let state = server.sys_get_state(T).await.unwrap();
    assert_eq!(state, 42);
}

#[tokio::test]
async fn sys_get_status_works_while_suspended() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;

    server.sys_suspend(T).await.unwrap();

    let status = server.sys_get_status(T).await.unwrap();
    assert_eq!(status.status, ProcessRunState::Suspended);
    assert_eq!(status.pid, server.pid());
}

// ---------------------------------------------------------------------------
// 5. double suspend / resume idempotent
// ---------------------------------------------------------------------------

#[tokio::test]
async fn double_suspend_is_idempotent() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;

    server.sys_suspend(T).await.unwrap();
    server.sys_suspend(T).await.unwrap();

    // Should still be able to get state and resume
    let state = server.sys_get_state(T).await.unwrap();
    assert_eq!(state, 0);
    server.sys_resume(T).await.unwrap();

    let reply = server.call(CounterCall::Get, T).await.unwrap();
    assert_eq!(reply, 0);
}

#[tokio::test]
async fn resume_without_suspend_is_idempotent() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;

    // Resume when not suspended should be fine
    server.sys_resume(T).await.unwrap();

    let reply = server.call(CounterCall::Get, T).await.unwrap();
    assert_eq!(reply, 0);
}

// ---------------------------------------------------------------------------
// 6. sys_get_status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sys_get_status_running() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;

    let status = server.sys_get_status(T).await.unwrap();
    assert_eq!(status.pid, server.pid());
    assert_eq!(status.status, ProcessRunState::Running);
    assert!(!status.debug.trace);
    assert!(status.debug.log.is_none());
    assert!(!status.debug.statistics);
}

#[tokio::test]
async fn sys_get_status_suspended() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;

    server.sys_suspend(T).await.unwrap();

    let status = server.sys_get_status(T).await.unwrap();
    assert_eq!(status.status, ProcessRunState::Suspended);
}

#[tokio::test]
async fn sys_get_status_after_resume() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;

    server.sys_suspend(T).await.unwrap();
    server.sys_resume(T).await.unwrap();

    let status = server.sys_get_status(T).await.unwrap();
    assert_eq!(status.status, ProcessRunState::Running);
}

// ---------------------------------------------------------------------------
// 7. Existing GenServer behavior unchanged
// ---------------------------------------------------------------------------

#[tokio::test]
async fn existing_call_cast_behavior_unchanged() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;

    // Calls work
    let reply = server.call(CounterCall::Get, T).await.unwrap();
    assert_eq!(reply, 0);

    // Casts work
    server.cast(CounterCast::Increment).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let reply = server.call(CounterCall::Get, T).await.unwrap();
    assert_eq!(reply, 1);
}

#[tokio::test]
async fn existing_gen_server_ref_clone_still_works() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;
    let clone = server.clone();

    assert_eq!(server.pid(), clone.pid());
    server.cast(CounterCast::Increment).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let reply = clone.call(CounterCall::Get, T).await.unwrap();
    assert_eq!(reply, 1);
}

// ---------------------------------------------------------------------------
// 8. Concurrent sys operations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_sys_get_state_calls() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;

    server.cast(CounterCast::Set(99)).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut handles = Vec::new();
    for _ in 0..10 {
        let s = server.clone();
        handles.push(tokio::spawn(async move {
            s.sys_get_state(T).await.unwrap()
        }));
    }

    for h in handles {
        let state = h.await.unwrap();
        assert_eq!(state, 99);
    }
}

// ---------------------------------------------------------------------------
// 9. Suspend then resume cycle preserves state
// ---------------------------------------------------------------------------

#[tokio::test]
async fn suspend_resume_cycle_preserves_state() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;

    // Build up some state
    for _ in 0..5 {
        server.cast(CounterCast::Increment).unwrap();
    }
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Suspend
    server.sys_suspend(T).await.unwrap();
    let state_while_suspended = server.sys_get_state(T).await.unwrap();
    assert_eq!(state_while_suspended, 5);

    // Resume
    server.sys_resume(T).await.unwrap();
    let state_after_resume = server.sys_get_state(T).await.unwrap();
    assert_eq!(state_after_resume, 5);

    // Continue using the server normally
    let reply = server.call(CounterCall::IncrementAndGet, T).await.unwrap();
    assert_eq!(reply, 6);
}

// ---------------------------------------------------------------------------
// 10. Multiple suspend/resume cycles
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multiple_suspend_resume_cycles() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;

    for i in 0..5 {
        server.sys_suspend(T).await.unwrap();
        let state = server.sys_get_state(T).await.unwrap();
        assert_eq!(state, i);
        server.sys_resume(T).await.unwrap();

        let reply = server.call(CounterCall::IncrementAndGet, T).await.unwrap();
        assert_eq!(reply, i + 1);
    }
}

// ---------------------------------------------------------------------------
// 11. Status description contains type info
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sys_get_status_has_state_description() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;

    let status = server.sys_get_status(T).await.unwrap();
    // The description should mention the state type
    assert!(!status.state_description.is_empty());
}

// ---------------------------------------------------------------------------
// 12. GenServerRef is still Send + Sync with sys channel
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gen_server_ref_with_sys_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<rebar_core::gen_server::GenServerRef<Counter>>();
}
