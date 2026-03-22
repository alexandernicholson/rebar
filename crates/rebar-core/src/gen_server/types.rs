use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use crate::process::{ExitReason, Message, ProcessId, SendError};
use crate::router::MessageRouter;

/// Errors returned by `GenServerRef::call`.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    #[error("call timeout: server did not respond in time")]
    Timeout,
    #[error("server dead: channel closed")]
    ServerDead,
}

/// Context available to `GenServer` callbacks.
/// Provides access to the server's PID, message routing, and continue signaling.
pub struct GenServerContext {
    pid: ProcessId,
    router: Option<Arc<dyn MessageRouter>>,
    continue_tx: Option<mpsc::UnboundedSender<rmpv::Value>>,
}

impl GenServerContext {
    /// Create a context with a router and continue channel.
    pub(crate) fn with_continue(
        pid: ProcessId,
        router: Arc<dyn MessageRouter>,
        continue_tx: mpsc::UnboundedSender<rmpv::Value>,
    ) -> Self {
        Self {
            pid,
            router: Some(router),
            continue_tx: Some(continue_tx),
        }
    }

    /// Return this server's PID.
    #[must_use]
    pub const fn self_pid(&self) -> ProcessId {
        self.pid
    }

    /// Send a raw message to another process.
    ///
    /// # Errors
    ///
    /// Returns `SendError::ProcessDead` if no router is configured or the
    /// destination process is not reachable.
    pub fn send(&self, dest: ProcessId, payload: rmpv::Value) -> Result<(), SendError> {
        self.router
            .as_ref()
            .map_or(Err(SendError::ProcessDead(dest)), |router| {
                router.route(self.pid, dest, payload)
            })
    }

    /// Enqueue a continue message to be processed before the next mailbox message.
    ///
    /// This is the Rust equivalent of returning `{:noreply, state, {:continue, term}}`
    /// in Elixir's `GenServer`. The continue message will be passed to `handle_continue`.
    pub fn continue_with(&self, payload: rmpv::Value) {
        if let Some(tx) = &self.continue_tx {
            let _ = tx.send(payload);
        }
    }

    /// Send a message to `dest` after `delay`. Convenience wrapper around [`crate::timer::send_after`].
    #[must_use]
    pub fn send_after(
        &self,
        dest: ProcessId,
        payload: rmpv::Value,
        delay: std::time::Duration,
    ) -> Option<crate::timer::TimerRef> {
        let router = self.router.as_ref()?.clone();
        Some(crate::timer::send_after(router, self.pid, dest, payload, delay))
    }

    /// Send a message to self after `delay`.
    #[must_use]
    pub fn send_after_self(
        &self,
        payload: rmpv::Value,
        delay: std::time::Duration,
    ) -> Option<crate::timer::TimerRef> {
        self.send_after(self.pid, payload, delay)
    }

    /// Send a message to `dest` at regular intervals.
    #[must_use]
    pub fn send_interval(
        &self,
        dest: ProcessId,
        payload: rmpv::Value,
        interval: std::time::Duration,
    ) -> Option<crate::timer::TimerRef> {
        let router = self.router.as_ref()?.clone();
        Some(crate::timer::send_interval(
            router, self.pid, dest, payload, interval,
        ))
    }
}

#[cfg(test)]
impl GenServerContext {
    /// Create a new context (used in tests).
    pub(crate) fn new(pid: ProcessId) -> Self {
        Self {
            pid,
            router: None,
            continue_tx: None,
        }
    }
}

/// The `GenServer` trait. Implement this for your server's logic.
///
/// Associated types define the message protocol:
/// - `State`: server state, mutated by callbacks
/// - `Call`: synchronous request message (gets a Reply)
/// - `Cast`: asynchronous fire-and-forget message
/// - `Reply`: response to a Call
#[async_trait::async_trait]
pub trait GenServer: Send + Sync + 'static {
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
    #[allow(clippy::unused_async)]
    async fn handle_info(
        &self,
        _msg: Message,
        _state: &mut Self::State,
        _ctx: &GenServerContext,
    ) {
    }

    /// Handle a continue message. Called when `ctx.continue_with(payload)` was
    /// invoked during `init`, `handle_call`, `handle_cast`, `handle_info`, or
    /// a previous `handle_continue`.
    ///
    /// Continue messages are processed before the next message from the mailbox,
    /// allowing deferred initialization or multi-step processing without blocking
    /// the caller.
    ///
    /// Equivalent to Elixir's `handle_continue/2`.
    /// Default: ignores the message.
    #[allow(clippy::unused_async)]
    async fn handle_continue(
        &self,
        _msg: rmpv::Value,
        _state: &mut Self::State,
        _ctx: &GenServerContext,
    ) {
    }

    /// Called when the server is shutting down.
    #[allow(clippy::unused_async)]
    async fn terminate(&self, _reason: ExitReason, _state: &mut Self::State) {}
}

/// Internal envelope for call messages, carrying the reply channel.
pub(crate) struct CallEnvelope<S: GenServer> {
    pub msg: S::Call,
    pub from: ProcessId,
    pub reply_tx: oneshot::Sender<S::Reply>,
}
