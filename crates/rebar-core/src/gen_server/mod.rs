pub mod engine;
pub mod types;

pub use engine::*;
pub use types::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ProcessId;
    use crate::runtime::Runtime;
    use std::sync::Arc;
    use std::time::Duration;

    // Test: GenServer trait is object-safe enough to implement
    struct CounterServer;

    #[async_trait::async_trait]
    impl GenServer for CounterServer {
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

    #[test]
    fn gen_server_context_has_pid() {
        let ctx = GenServerContext::new(ProcessId::new(1, 5));
        assert_eq!(ctx.self_pid(), ProcessId::new(1, 5));
    }

    #[test]
    fn call_error_display() {
        let err = CallError::Timeout;
        assert!(format!("{err}").contains("timeout"));
        let err = CallError::ServerDead;
        assert!(format!("{err}").contains("dead"));
    }

    #[tokio::test]
    async fn spawn_gen_server_returns_ref_with_pid() {
        let rt = Arc::new(Runtime::new(1));
        let server_ref = spawn_gen_server(Arc::clone(&rt), CounterServer).await;
        assert_eq!(server_ref.pid().node_id(), 1);
    }

    #[tokio::test]
    async fn gen_server_call_returns_reply() {
        let rt = Arc::new(Runtime::new(1));
        let server_ref = spawn_gen_server(Arc::clone(&rt), CounterServer).await;
        let reply = server_ref
            .call("get".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(reply, 0);
    }

    #[tokio::test]
    async fn gen_server_cast_then_call() {
        let rt = Arc::new(Runtime::new(1));
        let server_ref = spawn_gen_server(Arc::clone(&rt), CounterServer).await;
        server_ref.cast("inc".to_string()).unwrap();
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        let reply = server_ref
            .call("get".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(reply, 1);
    }

    #[tokio::test]
    async fn gen_server_multiple_casts() {
        let rt = Arc::new(Runtime::new(1));
        let server_ref = spawn_gen_server(Arc::clone(&rt), CounterServer).await;
        for _ in 0..10 {
            server_ref.cast("inc".to_string()).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let reply = server_ref
            .call("get".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(reply, 10);
    }

    #[tokio::test]
    async fn gen_server_call_timeout() {
        struct SlowServer;

        #[async_trait::async_trait]
        impl GenServer for SlowServer {
            type State = ();
            type Call = ();
            type Cast = ();
            type Reply = ();

            async fn init(&self, _ctx: &GenServerContext) -> Result<Self::State, String> {
                Ok(())
            }

            async fn handle_call(
                &self,
                _msg: Self::Call,
                _from: ProcessId,
                _state: &mut Self::State,
                _ctx: &GenServerContext,
            ) -> Self::Reply {
                tokio::time::sleep(Duration::from_secs(10)).await;
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
        let server_ref = spawn_gen_server(Arc::clone(&rt), SlowServer).await;
        let result = server_ref.call((), Duration::from_millis(50)).await;
        assert!(matches!(result, Err(CallError::Timeout)));
    }

    #[tokio::test]
    async fn gen_server_ref_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GenServerRef<CounterServer>>();
    }
}
