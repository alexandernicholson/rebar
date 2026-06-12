use std::sync::Arc;
use std::time::Duration;

use rebar_core::gen_server::{spawn_gen_server, CallError, GenServer, GenServerContext};
use rebar_core::process::monitor::DownMessage;
use rebar_core::process::{Message, ProcessId};
use rebar_core::runtime::Runtime;

/// A counter `GenServer` that tracks a count.
struct Counter;

#[async_trait::async_trait]
impl GenServer for Counter {
    type State = u64;
    type Call = CounterCall;
    type Cast = CounterCast;
    type Reply = CounterReply;

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
            CounterCall::Get => CounterReply::Count(*state),
            CounterCall::IncrementAndGet => {
                *state += 1;
                CounterReply::Count(*state)
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
            CounterCast::Reset => *state = 0,
        }
    }

    async fn handle_info(
        &self,
        msg: Message,
        state: &mut Self::State,
        _ctx: &GenServerContext,
    ) {
        // If we get a raw message with an integer payload, add it to state
        if let Some(val) = msg.payload().as_u64() {
            *state += val;
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
    Reset,
}

#[derive(Debug, PartialEq)]
enum CounterReply {
    Count(u64),
}

#[tokio::test]
async fn counter_get_initial() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(rt, Counter).await;
    let reply = server.call(CounterCall::Get, Duration::from_secs(1)).await.unwrap();
    assert_eq!(reply, CounterReply::Count(0));
}

#[tokio::test]
async fn counter_increment_and_get() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(rt, Counter).await;
    let reply = server
        .call(CounterCall::IncrementAndGet, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(reply, CounterReply::Count(1));
}

#[tokio::test]
async fn counter_cast_increment_then_get() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(rt, Counter).await;
    server.cast(CounterCast::Increment).unwrap();
    server.cast(CounterCast::Increment).unwrap();
    server.cast(CounterCast::Increment).unwrap();
    // Yield to let the GenServer process casts before the call
    // (biased select prioritizes calls over casts)
    tokio::task::yield_now().await;
    let reply = server.call(CounterCall::Get, Duration::from_secs(1)).await.unwrap();
    assert_eq!(reply, CounterReply::Count(3));
}

#[tokio::test]
async fn counter_cast_reset() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(rt, Counter).await;
    server.cast(CounterCast::Increment).unwrap();
    server.cast(CounterCast::Increment).unwrap();
    server.cast(CounterCast::Reset).unwrap();
    // Yield to let the GenServer process casts before the call
    tokio::task::yield_now().await;
    let reply = server.call(CounterCall::Get, Duration::from_secs(1)).await.unwrap();
    assert_eq!(reply, CounterReply::Count(0));
}

#[tokio::test]
async fn counter_handle_info_via_send() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;
    // Send a raw message to the GenServer's PID via the runtime
    rt.send(server.pid(), rmpv::Value::Integer(5u64.into()))
        .await
        .unwrap();
    // Yield to let the GenServer process the info message before the call
    // (biased select prioritizes calls over info messages)
    tokio::task::yield_now().await;
    let reply = server.call(CounterCall::Get, Duration::from_secs(1)).await.unwrap();
    assert_eq!(reply, CounterReply::Count(5));
}

#[tokio::test]
async fn counter_concurrent_calls() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(rt, Counter).await;

    let mut handles = Vec::new();
    for _ in 0..10 {
        let s = server.clone();
        handles.push(tokio::spawn(async move {
            s.call(CounterCall::IncrementAndGet, Duration::from_secs(1))
                .await
                .unwrap()
        }));
    }

    for h in handles {
        let _ = h.await.unwrap();
    }

    // All calls should have been processed sequentially by the GenServer
    // Final count should be 10
    let final_count = server
        .call(CounterCall::Get, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(final_count, CounterReply::Count(10));
}

#[tokio::test]
async fn gen_server_ref_clone_works() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(rt, Counter).await;
    let server2 = server.clone();
    assert_eq!(server.pid(), server2.pid());

    server.cast(CounterCast::Increment).unwrap();
    // Yield to let the GenServer process the cast before the call
    tokio::task::yield_now().await;
    let reply = server2
        .call(CounterCall::Get, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(reply, CounterReply::Count(1));
}

#[tokio::test]
async fn gen_server_init_failure() {
    struct FailInit;

    #[async_trait::async_trait]
    impl GenServer for FailInit {
        type State = ();
        type Call = ();
        type Cast = ();
        type Reply = ();

        async fn init(&self, _ctx: &GenServerContext) -> Result<Self::State, String> {
            Err("init failed".into())
        }

        async fn handle_call(
            &self, _msg: (), _from: ProcessId, _state: &mut (), _ctx: &GenServerContext,
        ) -> () {
        }

        async fn handle_cast(
            &self, _msg: (), _state: &mut (), _ctx: &GenServerContext,
        ) {
        }
    }

    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(rt, FailInit).await;
    // Server should be dead due to init failure, call should fail (timeout or dead)
    let result = server.call((), Duration::from_millis(100)).await;
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Regression: a dying gen_server must fire monitor DOWNs (fix #1).
//
// Before the fix the engine exited via `ProcessTable::remove`, which is a bare
// map delete: a watcher monitoring the server's PID waited forever for a DOWN
// that never arrived. The exit now routes through `cleanup_process`, the
// canonical death path that fires DOWNs and unregisters names.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn dying_gen_server_fires_monitor_down() {
    let rt = Arc::new(Runtime::new(1));

    // Hold the server in an Option so we can drop all its refs to kill it.
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;
    let target = server.pid();

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (down_tx, down_rx) = tokio::sync::oneshot::channel();
    rt.spawn(move |mut ctx| async move {
        let mref = ctx.monitor(target);
        ready_tx.send(()).unwrap();
        let msg = ctx.recv().await.unwrap();
        let down = DownMessage::from_value(msg.payload()).expect("payload is a DOWN message");
        down_tx.send((mref, down)).unwrap();
    })
    .await;

    // Ensure the monitor is registered before the server dies.
    ready_rx.await.unwrap();

    // Dropping the last ref closes all client channels; the engine breaks out of
    // its loop and the death runs through cleanup_process.
    drop(server);

    let (mref, down) = tokio::time::timeout(Duration::from_secs(2), down_rx)
        .await
        .expect("DOWN must be delivered when the gen_server dies")
        .unwrap();
    assert_eq!(down.monitor_ref, mref);
    assert_eq!(down.pid, target);
}

// ---------------------------------------------------------------------------
// Regression: call() must time out on a backed-up send, not just the reply
// (fix #3). A suspended server stops servicing its bounded call channel, so a
// caller that fills the channel must get CallError::Timeout from the
// `call_tx.send().await` rather than hanging forever.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn call_to_suspended_server_times_out_on_full_channel() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;

    // Suspend the server: it now only services sys commands, never calls.
    server
        .sys_suspend(Duration::from_secs(1))
        .await
        .expect("suspend acknowledged");

    // Saturate the bounded call channel (capacity 64) plus a generous margin so
    // a subsequent send has no buffer slot and would block indefinitely.
    let mut fillers = Vec::new();
    for _ in 0..256 {
        let s = server.clone();
        fillers.push(tokio::spawn(async move {
            let _ = s.call(CounterCall::Get, Duration::from_secs(30)).await;
        }));
    }

    // Give the filler tasks a chance to occupy the channel buffer.
    tokio::task::yield_now().await;

    // This call cannot even enqueue within the budget -> Timeout, not a hang.
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        server.call(CounterCall::Get, Duration::from_millis(50)),
    )
    .await
    .expect("call() must return within its own timeout, not hang");

    assert!(
        matches!(result, Err(CallError::Timeout)),
        "expected Timeout from a full/suspended server, got {result:?}"
    );

    for f in fillers {
        f.abort();
    }
}

// ---------------------------------------------------------------------------
// Regression: a self-reenqueuing handle_continue must not starve calls (fix
// #5). The engine processes at most one continue per loop iteration and the
// continue arm is the lowest-priority select branch, so calls still progress
// even under a continue that perpetually re-enqueues itself.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn greedy_continue_does_not_starve_calls() {
    struct ContinueLooper;

    #[async_trait::async_trait]
    impl GenServer for ContinueLooper {
        type State = u64;
        type Call = ();
        type Cast = ();
        type Reply = u64;

        async fn init(&self, ctx: &GenServerContext) -> Result<Self::State, String> {
            // Kick off a perpetual continue chain.
            ctx.continue_with(rmpv::Value::Nil);
            Ok(0)
        }

        async fn handle_call(
            &self,
            _msg: (),
            _from: ProcessId,
            state: &mut u64,
            _ctx: &GenServerContext,
        ) -> u64 {
            *state
        }

        async fn handle_cast(&self, _msg: (), _state: &mut u64, _ctx: &GenServerContext) {}

        async fn handle_continue(
            &self,
            _msg: rmpv::Value,
            state: &mut u64,
            ctx: &GenServerContext,
        ) {
            *state += 1;
            // Re-enqueue: a greedy drain loop would spin here forever and never
            // service the pending call.
            ctx.continue_with(rmpv::Value::Nil);
        }
    }

    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(rt, ContinueLooper).await;

    // Despite the perpetual continue, the call must be serviced promptly.
    let reply = tokio::time::timeout(
        Duration::from_secs(2),
        server.call((), Duration::from_secs(1)),
    )
    .await
    .expect("call must not be starved by a self-reenqueuing continue")
    .expect("call succeeds");

    // State has advanced (continues ran) but the call still got through.
    assert!(reply >= 1, "expected at least one continue to have run");
}

// ---------------------------------------------------------------------------
// Regression: a panicking gen_server callback must still fire monitor DOWNs
// (fix #2). The inner task's JoinError is detected and the death is routed
// through cleanup_process rather than being swallowed.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn panicking_gen_server_fires_monitor_down() {
    struct Panicker;

    #[async_trait::async_trait]
    impl GenServer for Panicker {
        type State = ();
        type Call = ();
        type Cast = ();
        type Reply = ();

        async fn init(&self, _ctx: &GenServerContext) -> Result<Self::State, String> {
            Ok(())
        }

        async fn handle_call(
            &self,
            _msg: (),
            _from: ProcessId,
            _state: &mut (),
            _ctx: &GenServerContext,
        ) {
            panic!("intentional panic to exercise cleanup on JoinError");
        }

        async fn handle_cast(&self, _msg: (), _state: &mut (), _ctx: &GenServerContext) {}
    }

    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Panicker).await;
    let target = server.pid();

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (down_tx, down_rx) = tokio::sync::oneshot::channel();
    rt.spawn(move |mut ctx| async move {
        let mref = ctx.monitor(target);
        ready_tx.send(()).unwrap();
        let msg = ctx.recv().await.unwrap();
        let down = DownMessage::from_value(msg.payload()).expect("payload is a DOWN message");
        down_tx.send((mref, down)).unwrap();
    })
    .await;

    ready_rx.await.unwrap();

    // Trigger the panic inside the gen_server's task. The call itself will fail.
    let _ = server.call((), Duration::from_millis(200)).await;

    let (mref, down) = tokio::time::timeout(Duration::from_secs(2), down_rx)
        .await
        .expect("DOWN must be delivered even when the gen_server panics")
        .unwrap();
    assert_eq!(down.monitor_ref, mref);
    assert_eq!(down.pid, target);
}
