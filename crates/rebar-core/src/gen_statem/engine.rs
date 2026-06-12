use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::gen_server::CallError;
use crate::process::mailbox::Mailbox;
use crate::process::table::ProcessHandle;
use crate::process::{ExitReason, ProcessId, SendError};
use crate::runtime::Runtime;

use super::types::{Action, EventType, GenStatem, TimeoutKind, TransitionResult};

/// Internal envelope for call messages, carrying the reply channel.
///
/// The `msg` field carries the typed call payload from [`GenStatemRef::call`].
/// The engine encodes it into the `rmpv::Value` event passed to
/// [`GenStatem::handle_event`] via [`GenStatem::encode_call`], then delivers the
/// typed `reply_tx` separately through [`EventType::Call`].
pub(crate) struct StatemCallEnvelope<S: GenStatem> {
    pub msg: S::Call,
    pub reply_tx: oneshot::Sender<S::Reply>,
}

/// Internal envelope for cast messages.
///
/// The `msg` field carries the typed cast payload from [`GenStatemRef::cast`],
/// encoded into the event payload via [`GenStatem::encode_cast`].
pub(crate) struct StatemCastEnvelope<S: GenStatem> {
    pub msg: S::Cast,
}

/// A handle to a running state machine, used by clients to send calls and casts.
pub struct GenStatemRef<S: GenStatem> {
    pid: ProcessId,
    call_tx: mpsc::Sender<StatemCallEnvelope<S>>,
    cast_tx: mpsc::UnboundedSender<StatemCastEnvelope<S>>,
}

// Manual Clone because derive requires S: Clone which we don't need.
impl<S: GenStatem> Clone for GenStatemRef<S> {
    fn clone(&self) -> Self {
        Self {
            pid: self.pid,
            call_tx: self.call_tx.clone(),
            cast_tx: self.cast_tx.clone(),
        }
    }
}

impl<S: GenStatem> GenStatemRef<S> {
    /// The PID of the state machine process.
    #[must_use]
    pub const fn pid(&self) -> ProcessId {
        self.pid
    }

    /// Send a synchronous call and wait for the reply.
    ///
    /// # Errors
    ///
    /// Returns [`CallError::ServerDead`] if the state machine has shut down, or
    /// [`CallError::Timeout`] if no reply arrives within the given duration.
    pub async fn call(&self, msg: S::Call, timeout: Duration) -> Result<S::Reply, CallError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let envelope = StatemCallEnvelope { msg, reply_tx };
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
    /// Returns [`SendError::ProcessDead`] if the state machine has shut down.
    pub fn cast(&self, msg: S::Cast) -> Result<(), SendError> {
        self.cast_tx
            .send(StatemCastEnvelope { msg })
            .map_err(|_| SendError::ProcessDead(self.pid))
    }
}

/// An event queued for processing by the engine.
///
/// Events may be postponed and replayed on state change.
struct QueuedEvent<Reply> {
    event_type: EventType<Reply>,
    payload: rmpv::Value,
    /// Whether this event can be postponed (only external events).
    postponable: bool,
}

/// Active timeout tracked by the engine.
struct ActiveTimeout {
    payload: rmpv::Value,
    sleep: Pin<Box<tokio::time::Sleep>>,
}

/// Spawn a `GenStatem` as a process in the runtime.
///
/// Initializes the state machine and starts the event loop. Returns a typed
/// [`GenStatemRef`] for interacting with the state machine.
///
/// # Panics
///
/// Panics are caught by the inner tokio task; a panic in `init` or
/// `handle_event` will cause the state machine to stop without crashing
/// the runtime.
#[allow(clippy::too_many_lines, clippy::unused_async)]
pub async fn spawn_gen_statem<S: GenStatem>(
    runtime: Arc<Runtime>,
    statem: S,
) -> GenStatemRef<S> {
    let pid = runtime.table().allocate_pid();

    let (call_tx, mut call_rx) = mpsc::channel::<StatemCallEnvelope<S>>(64);
    let (cast_tx, mut cast_rx) = mpsc::unbounded_channel::<StatemCastEnvelope<S>>();

    // Mailbox-backed handle so the statem is routable / monitorable / nameable
    // through the process table. The mailbox receiver drives `EventType::Info`.
    let (mailbox_tx, mut mailbox_rx) = Mailbox::unbounded();
    runtime
        .table()
        .insert(pid, ProcessHandle::new(mailbox_tx));

    let table = Arc::clone(runtime.table());

    tokio::spawn(async move {
        let inner = tokio::spawn(async move {
            // Initialize
            let Ok((mut current_state, mut data)) = statem.init().await else {
                return;
            };

            let (_, state_enter) = statem.callback_mode();

            // Timeout state
            let mut state_timeout: Option<ActiveTimeout> = None;
            let mut event_timeout: Option<ActiveTimeout> = None;
            let mut generic_timeouts: HashMap<String, ActiveTimeout> = HashMap::new();

            // Postponed events queue (FIFO)
            let mut postponed: Vec<QueuedEvent<S::Reply>> = Vec::new();

            // Internal events queue (from NextEvent actions and enter events)
            let mut internal_queue: Vec<QueuedEvent<S::Reply>> = Vec::new();

            // Fire initial state enter if enabled
            if state_enter {
                internal_queue.push(QueuedEvent {
                    event_type: EventType::Enter {
                        old_state_name: format!("{current_state:?}"),
                    },
                    payload: rmpv::Value::Nil,
                    postponable: false,
                });
            }

            loop {
                // Process internal events first (enter events, NextEvent, replayed postponed)
                if !internal_queue.is_empty() {
                    let event = internal_queue.remove(0);
                    if dispatch_event(
                        &statem,
                        event,
                        &mut current_state,
                        &mut data,
                        EventCtx {
                            state_timeout: &mut state_timeout,
                            event_timeout: &mut event_timeout,
                            generic_timeouts: &mut generic_timeouts,
                            postponed: &mut postponed,
                            internal_queue: &mut internal_queue,
                        },
                        state_enter,
                    )
                    .await
                    {
                        break;
                    }
                    continue;
                }

                // Build the next external/timeout event. We construct a full
                // QueuedEvent (carrying its EventType and payload) so that a
                // postponed event is replayed with its original type and content,
                // and so a postponed Call still owns its reply_tx and stays
                // answerable.
                //
                // Fairness: timeouts are checked first in the biased select only
                // when they are actually armed and ready. tokio's biased select
                // still polls each ready branch, but because external channels are
                // unbounded and timeouts pend until their deadline, a ready timeout
                // races fairly with pending external work rather than being
                // starved by the unconditional `event_timeout = None` reset (which
                // only runs once an external event is actually dequeued).
                let next_event: QueuedEvent<S::Reply> = tokio::select! {
                    biased;

                    () = wait_for_timeout(&mut state_timeout) => {
                        let payload = state_timeout.take()
                            .map_or(rmpv::Value::Nil, |t| t.payload);
                        QueuedEvent {
                            event_type: EventType::StateTimeout,
                            payload,
                            postponable: false,
                        }
                    }

                    () = wait_for_timeout(&mut event_timeout) => {
                        let payload = event_timeout.take()
                            .map_or(rmpv::Value::Nil, |t| t.payload);
                        QueuedEvent {
                            event_type: EventType::EventTimeout,
                            payload,
                            postponable: false,
                        }
                    }

                    name_payload = next_generic_timeout(&mut generic_timeouts) => {
                        let (name, payload) = name_payload;
                        QueuedEvent {
                            event_type: EventType::Timeout(name),
                            payload,
                            postponable: false,
                        }
                    }

                    call = call_rx.recv() => {
                        let Some(envelope) = call else {
                            statem
                                .terminate(ExitReason::Normal, &current_state, &mut data)
                                .await;
                            break;
                        };
                        // Cancel event timeout on any external event.
                        event_timeout = None;
                        let payload = statem.encode_call(&envelope.msg);
                        QueuedEvent {
                            event_type: EventType::Call(envelope.reply_tx),
                            payload,
                            postponable: true,
                        }
                    }

                    cast = cast_rx.recv() => {
                        let Some(envelope) = cast else {
                            statem
                                .terminate(ExitReason::Normal, &current_state, &mut data)
                                .await;
                            break;
                        };
                        // Cancel event timeout on any external event.
                        event_timeout = None;
                        let payload = statem.encode_cast(&envelope.msg);
                        QueuedEvent {
                            event_type: EventType::Cast,
                            payload,
                            postponable: true,
                        }
                    }

                    msg = mailbox_rx.recv() => {
                        let Some(msg) = msg else {
                            statem
                                .terminate(ExitReason::Normal, &current_state, &mut data)
                                .await;
                            break;
                        };
                        // Cancel event timeout on any external event.
                        event_timeout = None;
                        QueuedEvent {
                            event_type: EventType::Info,
                            payload: msg.payload().clone(),
                            postponable: true,
                        }
                    }
                };

                if dispatch_event(
                    &statem,
                    next_event,
                    &mut current_state,
                    &mut data,
                    EventCtx {
                        state_timeout: &mut state_timeout,
                        event_timeout: &mut event_timeout,
                        generic_timeouts: &mut generic_timeouts,
                        postponed: &mut postponed,
                        internal_queue: &mut internal_queue,
                    },
                    state_enter,
                )
                .await
                {
                    break;
                }
            }
        });

        // Panic isolation: whether the inner task completes normally or panics,
        // run the canonical death path. `cleanup_process` is what fires monitor
        // DOWN messages, unregisters names, and invokes exit hooks — bypassing
        // it on an inner-task panic would silently leak the process's identity
        // and hang any watcher. The `Err` branch is reached only on an abnormal
        // exit (panic or cancellation); we still run the same cleanup so the
        // crash is observable to monitors rather than swallowed.
        let exit = inner.await;
        if let Err(join_err) = &exit {
            debug_assert!(
                join_err.is_panic() || join_err.is_cancelled(),
                "unexpected JoinError variant"
            );
        }
        table.cleanup_process(pid);
    });

    GenStatemRef {
        pid,
        call_tx,
        cast_tx,
    }
}

/// Wait for a timeout to fire. If no timeout is set, pends forever.
async fn wait_for_timeout(timeout: &mut Option<ActiveTimeout>) {
    match timeout {
        Some(t) => t.sleep.as_mut().await,
        None => std::future::pending::<()>().await,
    }
}

/// Wait for the next generic timeout to fire. Returns the name and payload.
///
/// If there are no active generic timeouts, this future never resolves.
async fn next_generic_timeout(
    timeouts: &mut HashMap<String, ActiveTimeout>,
) -> (String, rmpv::Value) {
    if timeouts.is_empty() {
        return std::future::pending::<(String, rmpv::Value)>().await;
    }

    // Find the timeout with the earliest deadline.
    let (earliest_name, earliest_deadline) = timeouts
        .iter()
        .map(|(name, t)| (name.clone(), t.sleep.deadline()))
        .min_by_key(|(_, d)| *d)
        .expect("timeouts is non-empty");

    tokio::time::sleep_until(earliest_deadline).await;

    let timeout = timeouts
        .remove(&earliest_name)
        .expect("timeout was just found");
    (earliest_name, timeout.payload)
}

/// The event loop's mutable scratch state, bundled by reference so it can be
/// threaded through dispatch without an unwieldy argument list. The fields are
/// borrowed from separate loop locals (which the `select!` needs as disjoint
/// borrows) only at the dispatch call site.
struct EventCtx<'a, S: GenStatem> {
    state_timeout: &'a mut Option<ActiveTimeout>,
    event_timeout: &'a mut Option<ActiveTimeout>,
    generic_timeouts: &'a mut HashMap<String, ActiveTimeout>,
    postponed: &'a mut Vec<QueuedEvent<S::Reply>>,
    internal_queue: &'a mut Vec<QueuedEvent<S::Reply>>,
}

/// Dispatch a single `QueuedEvent` to `handle_event` and apply the result.
///
/// Owns the event so that, if the transition postpones it, the *original*
/// `EventType` (including a Call's `reply_tx`) and payload can be re-queued for
/// replay on the next state change.
///
/// Returns `true` if the state machine should stop.
async fn dispatch_event<S: GenStatem>(
    statem: &S,
    event: QueuedEvent<S::Reply>,
    current_state: &mut S::State,
    data: &mut S::Data,
    ctx: EventCtx<'_, S>,
    state_enter: bool,
) -> bool {
    let EventCtx {
        state_timeout,
        event_timeout,
        generic_timeouts,
        postponed,
        internal_queue,
    } = ctx;
    let QueuedEvent {
        event_type,
        payload,
        postponable,
    } = event;

    // Split the event_type into a re-buildable descriptor plus the (optional)
    // reply channel. handle_event consumes a freshly-rebuilt EventType; if the
    // transition postpones, we reconstruct the QueuedEvent from the descriptor
    // and the reply_tx so the postponed type and any pending call survive.
    let (descriptor, reply_tx) = split_event_type(event_type);
    let dispatch_type = rebuild_event_type::<S::Reply>(&descriptor, reply_tx);

    let result = statem
        .handle_event(dispatch_type, payload.clone(), current_state, data)
        .await;

    handle_result(
        statem,
        result,
        current_state,
        data,
        state_timeout,
        event_timeout,
        generic_timeouts,
        postponed,
        internal_queue,
        state_enter,
        postponable,
        &descriptor,
        &payload,
    )
    .await
}

/// A clonable description of an `EventType` that omits the non-clonable
/// `reply_tx`. Used to reconstruct a postponed event's original type.
#[derive(Clone)]
enum EventDescriptor {
    /// A Call. The reply channel is carried separately.
    Call,
    Cast,
    Info,
    StateTimeout,
    Timeout(String),
    EventTimeout,
    Internal,
    Enter { old_state_name: String },
}

/// Split an `EventType` into a clonable descriptor and the reply channel it
/// may have carried (only present for `Call`).
fn split_event_type<Reply>(
    event_type: EventType<Reply>,
) -> (EventDescriptor, Option<oneshot::Sender<Reply>>) {
    match event_type {
        EventType::Call(tx) => (EventDescriptor::Call, Some(tx)),
        EventType::Cast => (EventDescriptor::Cast, None),
        EventType::Info => (EventDescriptor::Info, None),
        EventType::StateTimeout => (EventDescriptor::StateTimeout, None),
        EventType::Timeout(name) => (EventDescriptor::Timeout(name), None),
        EventType::EventTimeout => (EventDescriptor::EventTimeout, None),
        EventType::Internal => (EventDescriptor::Internal, None),
        EventType::Enter { old_state_name } => {
            (EventDescriptor::Enter { old_state_name }, None)
        }
    }
}

/// Rebuild an `EventType` from a descriptor and an optional reply channel.
fn rebuild_event_type<Reply>(
    descriptor: &EventDescriptor,
    reply_tx: Option<oneshot::Sender<Reply>>,
) -> EventType<Reply> {
    match descriptor {
        EventDescriptor::Call => {
            EventType::Call(reply_tx.expect("Call descriptor without reply_tx"))
        }
        EventDescriptor::Cast => EventType::Cast,
        EventDescriptor::Info => EventType::Info,
        EventDescriptor::StateTimeout => EventType::StateTimeout,
        EventDescriptor::Timeout(name) => EventType::Timeout(name.clone()),
        EventDescriptor::EventTimeout => EventType::EventTimeout,
        EventDescriptor::Internal => EventType::Internal,
        EventDescriptor::Enter { old_state_name } => EventType::Enter {
            old_state_name: old_state_name.clone(),
        },
    }
}

/// Handle the result of `handle_event`, applying transitions and actions.
///
/// Returns `true` if the state machine should stop.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn handle_result<S: GenStatem>(
    statem: &S,
    result: TransitionResult<S::State, S::Data, S::Reply>,
    current_state: &mut S::State,
    data: &mut S::Data,
    state_timeout: &mut Option<ActiveTimeout>,
    event_timeout: &mut Option<ActiveTimeout>,
    generic_timeouts: &mut HashMap<String, ActiveTimeout>,
    postponed: &mut Vec<QueuedEvent<S::Reply>>,
    internal_queue: &mut Vec<QueuedEvent<S::Reply>>,
    state_enter: bool,
    postponable: bool,
    descriptor: &EventDescriptor,
    event_payload: &rmpv::Value,
) -> bool {
    // Push the in-flight event onto the postponed queue, preserving its original
    // EventType and payload (fixes the previous Cast-as-stand-in loss). A Call
    // cannot be postponed because its reply_tx was consumed by handle_event; in
    // that case we send an explicit ServerDead-style error reply rather than
    // silently dropping the caller's oneshot.
    let postpone_event =
        |postponed: &mut Vec<QueuedEvent<S::Reply>>| match descriptor {
            EventDescriptor::Call => {
                // reply_tx already consumed; dropping it surfaces ServerDead to
                // the awaiting caller instead of hanging it.
            }
            _ => postponed.push(QueuedEvent {
                event_type: rebuild_event_type::<S::Reply>(descriptor, None),
                payload: event_payload.clone(),
                postponable: true,
            }),
        };

    match result {
        TransitionResult::NextState {
            state: new_state,
            data: new_data,
            actions,
        } => {
            let state_changed = *current_state != new_state;
            let old_state_name = format!("{current_state:?}");
            *current_state = new_state;
            *data = new_data;

            // Clear the OLD state timeout BEFORE processing actions, so that a
            // StateTimeout armed by this transition survives. (Previously the
            // post-process unconditional `*state_timeout = None` wiped a timeout
            // armed during the same transition.)
            if state_changed {
                *state_timeout = None;
            }

            let should_postpone = process_actions(
                actions,
                state_timeout,
                event_timeout,
                generic_timeouts,
                internal_queue,
            );

            if should_postpone && postponable {
                postpone_event(postponed);
            }

            if state_changed {
                // Replay postponed events: prepend to internal queue in FIFO order
                let replayed = std::mem::take(postponed);
                let existing = std::mem::take(internal_queue);
                *internal_queue = replayed;
                internal_queue.extend(existing);

                // Fire state enter callback (at the front, before replayed events)
                if state_enter {
                    internal_queue.insert(
                        0,
                        QueuedEvent {
                            event_type: EventType::Enter { old_state_name },
                            payload: rmpv::Value::Nil,
                            postponable: false,
                        },
                    );
                }
            }

            false
        }
        TransitionResult::KeepState {
            data: new_data,
            actions,
        } => {
            *data = new_data;

            let should_postpone = process_actions(
                actions,
                state_timeout,
                event_timeout,
                generic_timeouts,
                internal_queue,
            );

            if should_postpone && postponable {
                postpone_event(postponed);
            }

            false
        }
        TransitionResult::KeepStateAndData { actions } => {
            let should_postpone = process_actions(
                actions,
                state_timeout,
                event_timeout,
                generic_timeouts,
                internal_queue,
            );

            if should_postpone && postponable {
                postpone_event(postponed);
            }

            false
        }
        TransitionResult::Stop {
            reason,
            data: new_data,
        } => {
            *data = new_data;
            statem.terminate(reason, current_state, data).await;
            true
        }
        TransitionResult::StopAndReply {
            reason,
            data: new_data,
            replies,
        } => {
            *data = new_data;
            for (tx, reply) in replies {
                let _ = tx.send(reply);
            }
            statem.terminate(reason, current_state, data).await;
            true
        }
    }
}

/// Process actions from a transition result.
///
/// Returns `true` if an `Action::Postpone` was present. Note that the postpone
/// decision is applied by the caller (which owns the in-flight event).
fn process_actions<State, Reply>(
    actions: Vec<Action<State, Reply>>,
    state_timeout: &mut Option<ActiveTimeout>,
    event_timeout: &mut Option<ActiveTimeout>,
    generic_timeouts: &mut HashMap<String, ActiveTimeout>,
    internal_queue: &mut Vec<QueuedEvent<Reply>>,
) -> bool {
    let mut should_postpone = false;

    for action in actions {
        match action {
            Action::Reply(tx, reply) => {
                let _ = tx.send(reply);
            }
            Action::StateTimeout(duration, payload) => {
                *state_timeout = Some(ActiveTimeout {
                    payload,
                    sleep: Box::pin(tokio::time::sleep(duration)),
                });
            }
            Action::EventTimeout(duration, payload) => {
                *event_timeout = Some(ActiveTimeout {
                    payload,
                    sleep: Box::pin(tokio::time::sleep(duration)),
                });
            }
            Action::GenericTimeout(name, duration, payload) => {
                generic_timeouts.insert(
                    name,
                    ActiveTimeout {
                        payload,
                        sleep: Box::pin(tokio::time::sleep(duration)),
                    },
                );
            }
            Action::CancelTimeout(kind) => match kind {
                TimeoutKind::State => {
                    *state_timeout = None;
                }
                TimeoutKind::Event => {
                    *event_timeout = None;
                }
                TimeoutKind::Generic(name) => {
                    generic_timeouts.remove(&name);
                }
            },
            Action::Postpone => {
                should_postpone = true;
            }
            Action::NextEvent(payload) => {
                internal_queue.push(QueuedEvent {
                    event_type: EventType::Internal,
                    payload,
                    postponable: false,
                });
            }
            Action::Hibernate | Action::_Phantom(_) => {
                // No-op: tokio doesn't support hibernation
            }
        }
    }

    should_postpone
}
