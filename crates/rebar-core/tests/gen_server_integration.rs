use std::sync::Arc;
use std::time::Duration;

use rebar_core::gen_server::{spawn_gen_server, CallError, GenServer, GenServerContext, GenServerRef};
use rebar_core::process::{ExitReason, Message, ProcessId, SendError};
use rebar_core::runtime::Runtime;

/// A counter GenServer that tracks a count.
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
    tokio::time::sleep(Duration::from_millis(20)).await;
    let reply = server.call(CounterCall::Get, Duration::from_secs(1)).await.unwrap();
    assert_eq!(reply, CounterReply::Count(3));
}

#[tokio::test]
async fn counter_cast_reset() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(rt, Counter).await;
    server.cast(CounterCast::Increment).unwrap();
    server.cast(CounterCast::Increment).unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    server.cast(CounterCast::Reset).unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
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
    tokio::time::sleep(Duration::from_millis(50)).await;
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

    let mut results = Vec::new();
    for h in handles {
        results.push(h.await.unwrap());
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
    tokio::time::sleep(Duration::from_millis(10)).await;
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
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Server should be dead, call should fail
    let result = server.call((), Duration::from_millis(100)).await;
    assert!(result.is_err());
}
