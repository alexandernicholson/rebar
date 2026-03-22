use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::gen_server::types::{CallEnvelope, CallError, GenServer, GenServerContext};
use crate::process::mailbox::Mailbox;
use crate::process::table::ProcessHandle;
use crate::process::{ExitReason, ProcessId, SendError};
use crate::runtime::Runtime;
use crate::sys::{DebugOpts, ProcessRunState, ProcessStatus};

// ---------------------------------------------------------------------------
// Sys command types (internal to the engine)
// ---------------------------------------------------------------------------

/// A boxed closure that reads the state and sends back data via a oneshot.
///
/// This allows `sys_get_state` to clone the state inside the engine loop
/// without requiring `S::State` to be `Any`.
type StateGetter<S> = Box<dyn FnOnce(&<S as GenServer>::State) + Send>;

/// Commands sent over the sys channel to the engine loop.
pub(crate) enum SysCommand<S: GenServer> {
    /// Clone the current state and send it back.
    GetState(StateGetter<S>),
    /// Suspend the process (stop processing calls/casts/info).
    Suspend(oneshot::Sender<()>),
    /// Resume a suspended process.
    Resume(oneshot::Sender<()>),
    /// Return the process status.
    GetStatus(oneshot::Sender<ProcessStatus>),
}

// ---------------------------------------------------------------------------
// GenServerRef
// ---------------------------------------------------------------------------

/// Handle to a running `GenServer`, used by clients to send calls and casts.
pub struct GenServerRef<S: GenServer> {
    pid: ProcessId,
    call_tx: mpsc::Sender<CallEnvelope<S>>,
    cast_tx: mpsc::UnboundedSender<S::Cast>,
    sys_tx: mpsc::Sender<SysCommand<S>>,
}

// Manual Clone because derive requires S: Clone which we don't need
impl<S: GenServer> Clone for GenServerRef<S> {
    fn clone(&self) -> Self {
        Self {
            pid: self.pid,
            call_tx: self.call_tx.clone(),
            cast_tx: self.cast_tx.clone(),
            sys_tx: self.sys_tx.clone(),
        }
    }
}

impl<S: GenServer> GenServerRef<S> {
    /// The PID of the `GenServer` process.
    #[must_use]
    pub const fn pid(&self) -> ProcessId {
        self.pid
    }

    /// Send a synchronous call and wait for the reply.
    ///
    /// # Errors
    ///
    /// Returns `CallError::ServerDead` if the server has shut down, or
    /// `CallError::Timeout` if no reply arrives within the given duration.
    pub async fn call(&self, msg: S::Call, timeout: Duration) -> Result<S::Reply, CallError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let envelope = CallEnvelope {
            msg,
            from: ProcessId::new(0, 0), // caller PID not tracked for now
            reply_tx,
        };
        self.call_tx
            .send(envelope)
            .await
            .map_err(|_| CallError::ServerDead)?;

        match tokio::time::timeout(timeout, reply_rx).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => Err(CallError::ServerDead),
            Err(_) => Err(CallError::Timeout),
        }
    }

    /// Send an asynchronous cast (fire-and-forget).
    ///
    /// # Errors
    ///
    /// Returns `SendError::ProcessDead` if the server has shut down.
    pub fn cast(&self, msg: S::Cast) -> Result<(), SendError> {
        self.cast_tx
            .send(msg)
            .map_err(|_| SendError::ProcessDead(self.pid))
    }

    // -----------------------------------------------------------------------
    // Sys debug methods
    // -----------------------------------------------------------------------

    /// Get the current state of the server (for debugging).
    ///
    /// Equivalent to Erlang's `:sys.get_state/2`.
    ///
    /// # Errors
    ///
    /// Returns `CallError::Timeout` if the server does not respond within the
    /// given duration, or `CallError::ServerDead` if the server has shut down.
    pub async fn sys_get_state(&self, timeout: Duration) -> Result<S::State, CallError>
    where
        S::State: Clone,
    {
        let (tx, rx) = oneshot::channel();
        let getter: StateGetter<S> = Box::new(move |state: &S::State| {
            let _ = tx.send(state.clone());
        });
        self.sys_tx
            .send(SysCommand::GetState(getter))
            .await
            .map_err(|_| CallError::ServerDead)?;

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(state)) => Ok(state),
            Ok(Err(_)) => Err(CallError::ServerDead),
            Err(_) => Err(CallError::Timeout),
        }
    }

    /// Suspend the server so it stops processing calls, casts, and info
    /// messages. Sys commands continue to be processed while suspended.
    ///
    /// Equivalent to Erlang's `:sys.suspend/2`. Calling this on an already
    /// suspended server is idempotent.
    ///
    /// # Errors
    ///
    /// Returns `CallError::Timeout` if the server does not respond within the
    /// given duration, or `CallError::ServerDead` if the server has shut down.
    pub async fn sys_suspend(&self, timeout: Duration) -> Result<(), CallError> {
        let (tx, rx) = oneshot::channel();
        self.sys_tx
            .send(SysCommand::Suspend(tx))
            .await
            .map_err(|_| CallError::ServerDead)?;

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(CallError::ServerDead),
            Err(_) => Err(CallError::Timeout),
        }
    }

    /// Resume a suspended server so it processes messages again.
    ///
    /// Equivalent to Erlang's `:sys.resume/2`. Calling this on a running
    /// (non-suspended) server is idempotent.
    ///
    /// # Errors
    ///
    /// Returns `CallError::Timeout` if the server does not respond within the
    /// given duration, or `CallError::ServerDead` if the server has shut down.
    pub async fn sys_resume(&self, timeout: Duration) -> Result<(), CallError> {
        let (tx, rx) = oneshot::channel();
        self.sys_tx
            .send(SysCommand::Resume(tx))
            .await
            .map_err(|_| CallError::ServerDead)?;

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(CallError::ServerDead),
            Err(_) => Err(CallError::Timeout),
        }
    }

    /// Get the server's status including PID, run state, and debug options.
    ///
    /// Equivalent to Erlang's `:sys.get_status/2`.
    ///
    /// # Errors
    ///
    /// Returns `CallError::Timeout` if the server does not respond within the
    /// given duration, or `CallError::ServerDead` if the server has shut down.
    pub async fn sys_get_status(
        &self,
        timeout: Duration,
    ) -> Result<ProcessStatus, CallError> {
        let (tx, rx) = oneshot::channel();
        self.sys_tx
            .send(SysCommand::GetStatus(tx))
            .await
            .map_err(|_| CallError::ServerDead)?;

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(status)) => Ok(status),
            Ok(Err(_)) => Err(CallError::ServerDead),
            Err(_) => Err(CallError::Timeout),
        }
    }
}

// ---------------------------------------------------------------------------
// Engine-internal debug state
// ---------------------------------------------------------------------------

/// Internal debug state maintained by the engine loop.
struct InternalDebugState {
    suspended: bool,
    debug_opts: DebugOpts,
}

impl InternalDebugState {
    const fn new() -> Self {
        Self {
            suspended: false,
            debug_opts: DebugOpts::new(),
        }
    }

    const fn run_state(&self) -> ProcessRunState {
        if self.suspended {
            ProcessRunState::Suspended
        } else {
            ProcessRunState::Running
        }
    }
}

// ---------------------------------------------------------------------------
// Handle a single sys command
// ---------------------------------------------------------------------------

/// Process a single `SysCommand` against the engine's state and debug info.
///
/// Returns `true` if the engine should break out of a suspended loop (i.e.
/// a `Resume` command was received).
fn handle_sys_command<S: GenServer>(
    cmd: SysCommand<S>,
    state: &S::State,
    debug: &mut InternalDebugState,
    pid: ProcessId,
) -> bool {
    match cmd {
        SysCommand::GetState(getter) => {
            getter(state);
            false
        }
        SysCommand::Suspend(reply) => {
            debug.suspended = true;
            let _ = reply.send(());
            false
        }
        SysCommand::Resume(reply) => {
            debug.suspended = false;
            let _ = reply.send(());
            // Signal to break out of the suspended loop
            true
        }
        SysCommand::GetStatus(reply) => {
            let status = ProcessStatus {
                pid,
                status: debug.run_state(),
                debug: debug.debug_opts.clone(),
                state_description: format!("{pid} — state type: {}", std::any::type_name::<S::State>()),
            };
            let _ = reply.send(status);
            false
        }
    }
}

// ---------------------------------------------------------------------------
// spawn_gen_server
// ---------------------------------------------------------------------------

/// Spawn a `GenServer` as a process in the runtime.
///
/// Returns a typed `GenServerRef` for interacting with the server.
#[allow(clippy::unused_async)]
pub async fn spawn_gen_server<S: GenServer>(
    runtime: Arc<Runtime>,
    server: S,
) -> GenServerRef<S> {
    let pid = runtime.table().allocate_pid();

    // Typed channels for call/cast
    let (call_tx, mut call_rx) = mpsc::channel::<CallEnvelope<S>>(64);
    let (cast_tx, mut cast_rx) = mpsc::unbounded_channel::<S::Cast>();

    // Sys debug channel — highest priority in the event loop
    let (sys_tx, mut sys_rx) = mpsc::channel::<SysCommand<S>>(16);

    // Standard mailbox for handle_info
    let (mailbox_tx, mut mailbox_rx) = Mailbox::unbounded();
    runtime
        .table()
        .insert(pid, ProcessHandle::new(mailbox_tx));

    let table = Arc::clone(runtime.table());

    // Create a LocalRouter for the GenServer context to send messages
    let local_router: Arc<dyn crate::router::MessageRouter> =
        Arc::new(crate::router::LocalRouter::new(Arc::clone(runtime.table())));

    // Continue channel for handle_continue support
    let (continue_tx, mut continue_rx) = mpsc::unbounded_channel::<rmpv::Value>();
    let ctx = GenServerContext::with_continue(pid, local_router, continue_tx);

    // Spawn the event loop
    tokio::spawn(async move {
        let inner = tokio::spawn(async move {
            // Initialize
            let mut state = match server.init(&ctx).await {
                Ok(s) => s,
                Err(_e) => {
                    return;
                }
            };

            let mut debug = InternalDebugState::new();

            // Event loop: select between sys, call, cast, info, and continue
            loop {
                // Process any pending continue messages before external messages.
                // This ensures handle_continue runs before the next mailbox message,
                // matching Elixir's semantics.
                while let Ok(payload) = continue_rx.try_recv() {
                    server
                        .handle_continue(payload, &mut state, &ctx)
                        .await;
                }

                // Drain any pending sys commands first (non-blocking).
                while let Ok(cmd) = sys_rx.try_recv() {
                    handle_sys_command::<S>(cmd, &state, &mut debug, pid);
                }

                // If suspended, enter a loop that only processes sys commands.
                if debug.suspended {
                    loop {
                        if let Some(cmd) = sys_rx.recv().await {
                            let resumed =
                                handle_sys_command::<S>(cmd, &state, &mut debug, pid);
                            if resumed {
                                break;
                            }
                        } else {
                            // sys channel closed — server shutting down
                            server.terminate(ExitReason::Normal, &mut state).await;
                            return;
                        }
                    }
                    // After resuming, restart the main loop
                    continue;
                }

                tokio::select! {
                    biased;

                    // Highest priority: sys debug commands
                    cmd = sys_rx.recv() => {
                        if let Some(cmd) = cmd {
                            handle_sys_command::<S>(cmd, &state, &mut debug, pid);
                            // If we just got suspended, the top of the loop
                            // will enter the suspended branch.
                        } else {
                            // sys channel closed — all refs dropped
                            server.terminate(ExitReason::Normal, &mut state).await;
                            break;
                        }
                    }

                    // Prioritize calls (they have timeouts)
                    call = call_rx.recv() => {
                        if let Some(envelope) = call {
                            let reply = server
                                .handle_call(envelope.msg, envelope.from, &mut state, &ctx)
                                .await;
                            let _ = envelope.reply_tx.send(reply);
                        } else {
                            // All call senders dropped
                            server.terminate(ExitReason::Normal, &mut state).await;
                            break;
                        }
                    }

                    cast = cast_rx.recv() => {
                        if let Some(msg) = cast {
                            server.handle_cast(msg, &mut state, &ctx).await;
                        } else {
                            server.terminate(ExitReason::Normal, &mut state).await;
                            break;
                        }
                    }

                    info = mailbox_rx.recv() => {
                        if let Some(msg) = info {
                            server.handle_info(msg, &mut state, &ctx).await;
                        } else {
                            server.terminate(ExitReason::Normal, &mut state).await;
                            break;
                        }
                    }
                }
            }
        });

        // Panic isolation: whether inner completes or panics, clean up
        let _ = inner.await;
        table.remove(&pid);
    });

    GenServerRef {
        pid,
        call_tx,
        cast_tx,
        sys_tx,
    }
}
