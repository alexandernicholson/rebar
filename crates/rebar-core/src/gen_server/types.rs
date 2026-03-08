use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::process::{ExitReason, Message, ProcessId, SendError};
use crate::router::MessageRouter;

/// Errors returned by GenServerRef::call.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    #[error("call timeout: server did not respond in time")]
    Timeout,
    #[error("server dead: channel closed")]
    ServerDead,
}

/// Context available to GenServer callbacks.
/// Provides access to the server's PID and message routing.
pub struct GenServerContext {
    pid: ProcessId,
    router: Option<Arc<dyn MessageRouter>>,
}

impl GenServerContext {
    /// Create a new context (used internally by the engine).
    pub(crate) fn new(pid: ProcessId) -> Self {
        Self { pid, router: None }
    }

    /// Create a context with a router for sending messages.
    pub(crate) fn with_router(pid: ProcessId, router: Arc<dyn MessageRouter>) -> Self {
        Self {
            pid,
            router: Some(router),
        }
    }

    /// Return this server's PID.
    pub fn self_pid(&self) -> ProcessId {
        self.pid
    }

    /// Send a raw message to another process.
    pub fn send(&self, dest: ProcessId, payload: rmpv::Value) -> Result<(), SendError> {
        match &self.router {
            Some(router) => router.route(self.pid, dest, payload),
            None => Err(SendError::ProcessDead(dest)),
        }
    }
}

/// The GenServer trait. Implement this for your server's logic.
///
/// Associated types define the message protocol:
/// - `State`: server state, mutated by callbacks
/// - `Call`: synchronous request message (gets a Reply)
/// - `Cast`: asynchronous fire-and-forget message
/// - `Reply`: response to a Call
#[async_trait::async_trait]
pub trait GenServer: Send + 'static {
    type State: Send + 'static;
    type Call: Send + 'static;
    type Cast: Send + 'static;
    type Reply: Send + 'static;

    /// Initialize server state. Called once at startup.
    async fn init(&self, ctx: &GenServerContext) -> Result<Self::State, String>;

    /// Handle a synchronous call. Must return a reply.
    async fn handle_call(
        &self,
        msg: Self::Call,
        from: ProcessId,
        state: &mut Self::State,
        ctx: &GenServerContext,
    ) -> Self::Reply;

    /// Handle an asynchronous cast (fire-and-forget).
    async fn handle_cast(
        &self,
        msg: Self::Cast,
        state: &mut Self::State,
        ctx: &GenServerContext,
    );

    /// Handle a raw info message from the standard mailbox.
    /// Default: ignores the message.
    async fn handle_info(
        &self,
        _msg: Message,
        _state: &mut Self::State,
        _ctx: &GenServerContext,
    ) {
    }

    /// Called when the server is shutting down.
    async fn terminate(&self, _reason: ExitReason, _state: &mut Self::State) {}
}

/// Internal envelope for call messages, carrying the reply channel.
pub(crate) struct CallEnvelope<S: GenServer> {
    pub msg: S::Call,
    pub from: ProcessId,
    pub reply_tx: oneshot::Sender<S::Reply>,
}
