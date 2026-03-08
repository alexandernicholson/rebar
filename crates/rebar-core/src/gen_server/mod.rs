pub mod types;

pub use types::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{ExitReason, Message, ProcessId, SendError};
    use crate::router::MessageRouter;

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
        assert!(format!("{}", err).contains("timeout"));
        let err = CallError::ServerDead;
        assert!(format!("{}", err).contains("dead"));
    }
}
