use std::sync::Arc;
use std::time::Duration;

use rebar_core::gen_server::{spawn_gen_server, GenServer, GenServerContext};
use rebar_core::process::{Message, ProcessId};
use rebar_core::runtime::Runtime;
use rebar_core::timer;

// --- handle_continue tests ---

struct ContinueServer;

#[async_trait::async_trait]
impl GenServer for ContinueServer {
    type State = Vec<String>;
    type Call = String;
    type Cast = ();
    type Reply = Vec<String>;

    async fn init(&self, ctx: &GenServerContext) -> Result<Self::State, String> {
        // Schedule a continue from init
        ctx.continue_with(rmpv::Value::String("from_init".into()));
        Ok(vec!["init".to_string()])
    }

    async fn handle_call(
        &self,
        msg: Self::Call,
        _from: ProcessId,
        state: &mut Self::State,
        ctx: &GenServerContext,
    ) -> Self::Reply {
        if msg == "get" {
            state.clone()
        } else if msg == "trigger_continue" {
            ctx.continue_with(rmpv::Value::String("from_call".into()));
            state.clone()
        } else {
            vec![]
        }
    }

    async fn handle_cast(
        &self,
        _msg: Self::Cast,
        _state: &mut Self::State,
        _ctx: &GenServerContext,
    ) {
    }

    async fn handle_continue(
        &self,
        msg: rmpv::Value,
        state: &mut Self::State,
        _ctx: &GenServerContext,
    ) {
        if let Some(s) = msg.as_str() {
            state.push(format!("continue:{s}"));
        }
    }
}

#[tokio::test]
async fn handle_continue_called_after_init() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(rt, ContinueServer).await;

    // The call acts as a synchronization barrier; the continue from init
    // is processed before the call since GenServer uses biased select
    let state = server
        .call("get".to_string(), Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(state, vec!["init", "continue:from_init"]);
}

#[tokio::test]
async fn handle_continue_called_from_handle_call() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(rt, ContinueServer).await;

    // Trigger another continue from handle_call
    // The call acts as a synchronization barrier for the init continue
    server
        .call("trigger_continue".to_string(), Duration::from_secs(1))
        .await
        .unwrap();

    // The next call acts as a synchronization barrier for the continue
    let state = server
        .call("get".to_string(), Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(
        state,
        vec!["init", "continue:from_init", "continue:from_call"]
    );
}

#[tokio::test]
async fn handle_continue_default_noop_does_not_break_existing() {
    // A GenServer without handle_continue should still work
    struct SimpleServer;

    #[async_trait::async_trait]
    impl GenServer for SimpleServer {
        type State = u64;
        type Call = ();
        type Cast = ();
        type Reply = u64;

        async fn init(&self, _ctx: &GenServerContext) -> Result<Self::State, String> {
            Ok(42)
        }

        async fn handle_call(
            &self,
            _msg: Self::Call,
            _from: ProcessId,
            state: &mut Self::State,
            _ctx: &GenServerContext,
        ) -> Self::Reply {
            *state
        }

        async fn handle_cast(
            &self,
            _msg: Self::Cast,
            _state: &mut Self::State,
            _ctx: &GenServerContext,
        ) {
        }
    }

    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(rt, SimpleServer).await;
    let reply = server.call((), Duration::from_secs(1)).await.unwrap();
    assert_eq!(reply, 42);
}

// --- GenServerContext timer tests ---

struct TimerServer;

#[async_trait::async_trait]
impl GenServer for TimerServer {
    type State = Vec<String>;
    type Call = String;
    type Cast = ();
    type Reply = Vec<String>;

    async fn init(&self, ctx: &GenServerContext) -> Result<Self::State, String> {
        // Schedule a message to self after 20ms
        ctx.send_after_self(
            rmpv::Value::String("delayed_init".into()),
            Duration::from_millis(20),
        );
        Ok(vec![])
    }

    async fn handle_call(
        &self,
        msg: Self::Call,
        _from: ProcessId,
        state: &mut Self::State,
        _ctx: &GenServerContext,
    ) -> Self::Reply {
        if msg == "get" {
            state.clone()
        } else {
            vec![]
        }
    }

    async fn handle_cast(
        &self,
        _msg: Self::Cast,
        _state: &mut Self::State,
        _ctx: &GenServerContext,
    ) {
    }

    async fn handle_info(
        &self,
        msg: Message,
        state: &mut Self::State,
        _ctx: &GenServerContext,
    ) {
        if let Some(s) = msg.payload().as_str() {
            state.push(s.to_string());
        }
    }
}

#[tokio::test]
async fn gen_server_context_send_after_self() {
    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(rt, TimerServer).await;

    // Wait for the timer to fire
    tokio::time::sleep(Duration::from_millis(100)).await;

    let state = server
        .call("get".to_string(), Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(state, vec!["delayed_init"]);
}

// --- ProcessContext timer tests ---

#[tokio::test]
async fn process_context_send_after_to_self() {
    let rt = Runtime::new(1);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();

    rt.spawn(move |mut ctx| async move {
        ctx.send_after(
            rmpv::Value::String("timer_msg".into()),
            Duration::from_millis(20),
        );
        let msg = ctx.recv().await.unwrap();
        done_tx
            .send(msg.payload().as_str().unwrap().to_string())
            .unwrap();
    })
    .await;

    let result = tokio::time::timeout(Duration::from_secs(1), done_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result, "timer_msg");
}

#[tokio::test]
async fn process_context_send_after_to_other() {
    let rt = Runtime::new(1);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();

    let receiver = rt
        .spawn(move |mut ctx| async move {
            let msg = ctx.recv().await.unwrap();
            done_tx
                .send(msg.payload().as_str().unwrap().to_string())
                .unwrap();
        })
        .await;

    rt.spawn(move |ctx| async move {
        ctx.send_after_to(
            receiver,
            rmpv::Value::String("to_other".into()),
            Duration::from_millis(20),
        );
        // Keep alive while timer fires
        std::future::pending::<()>().await;
    })
    .await;

    let result = tokio::time::timeout(Duration::from_secs(1), done_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result, "to_other");
}

#[tokio::test]
async fn process_context_send_interval() {
    let rt = Runtime::new(1);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();

    rt.spawn(move |mut ctx| async move {
        let _timer = ctx.send_interval(
            rmpv::Value::String("tick".into()),
            Duration::from_millis(20),
        );

        let mut count = 0u64;
        for _ in 0..3 {
            let msg = ctx.recv().await.unwrap();
            if msg.payload().as_str() == Some("tick") {
                count += 1;
            }
        }
        done_tx.send(count).unwrap();
    })
    .await;

    let result = tokio::time::timeout(Duration::from_secs(2), done_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result, 3);
}

// --- Standalone timer function tests (via Runtime) ---

#[tokio::test]
async fn timer_with_runtime_processes() {
    let rt = Runtime::new(1);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();

    let receiver = rt
        .spawn(move |mut ctx| async move {
            let msg = ctx.recv().await.unwrap();
            done_tx
                .send(msg.payload().as_str().unwrap().to_string())
                .unwrap();
        })
        .await;

    // Use standalone send_after with the runtime's router
    let table = rt.table().clone();
    let router: Arc<dyn rebar_core::router::MessageRouter> =
        Arc::new(rebar_core::router::LocalRouter::new(table));

    timer::send_after(
        router,
        ProcessId::new(1, 0),
        receiver,
        rmpv::Value::String("standalone".into()),
        Duration::from_millis(20),
    );

    let result = tokio::time::timeout(Duration::from_secs(1), done_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result, "standalone");
}

// --- Existing interface preservation test ---

#[tokio::test]
async fn existing_gen_server_unchanged() {
    // Verify the original CounterServer pattern still works
    struct Counter;

    #[async_trait::async_trait]
    impl GenServer for Counter {
        type State = u64;
        type Call = String;
        type Cast = String;
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
            if msg == "get" {
                *state
            } else {
                0
            }
        }

        async fn handle_cast(
            &self,
            msg: Self::Cast,
            state: &mut Self::State,
            _ctx: &GenServerContext,
        ) {
            if msg == "inc" {
                *state += 1;
            }
        }
    }

    let rt = Arc::new(Runtime::new(1));
    let server = spawn_gen_server(Arc::clone(&rt), Counter).await;
    server.cast("inc".to_string()).unwrap();
    server.cast("inc".to_string()).unwrap();
    // Yield to let the GenServer process casts before the call
    tokio::task::yield_now().await;
    let reply = server
        .call("get".to_string(), Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(reply, 2);
}
