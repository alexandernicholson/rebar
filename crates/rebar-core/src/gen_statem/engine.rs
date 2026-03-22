use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::gen_server::CallError;
use crate::process::{ExitReason, ProcessId, SendError};
use crate::runtime::Runtime;

use super::types::{Action, EventType, GenStatem, TimeoutKind, TransitionResult};

/// Internal envelope for call messages, carrying the reply channel.
///
/// The `msg` field carries the typed call payload from [`GenStatemRef::call`].
/// The engine drops it after delivering the event because [`GenStatem::handle_event`]
/// receives an `rmpv::Value` event payload instead. The typed `Call` associated type
/// primarily serves as a compile-time marker for the `GenStatemRef` API.
pub(crate) struct StatemCallEnvelope<S: GenStatem> {
    pub msg: S::Call,
    pub reply_tx: oneshot::Sender<S::Reply>,
}

/// Internal envelope for cast messages.
///
/// The `msg` field carries the typed cast payload from [`GenStatemRef::cast`].
/// See [`StatemCallEnvelope`] for why this is dropped by the engine.
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
                    let postponable = event.postponable;
                    let event_payload = event.payload.clone();
                    let result = statem
                        .handle_event(
                            event.event_type,
                            event.payload,
                            &current_state,
                            &mut data,
                        )
                        .await;

                    if handle_result(
                        &statem,
                        result,
                        &mut current_state,
                        &mut data,
                        &mut state_timeout,
                        &mut event_timeout,
                        &mut generic_timeouts,
                        &mut postponed,
                        &mut internal_queue,
                        state_enter,
                        postponable,
                        &event_payload,
                    )
                    .await
                    {
                        break;
                    }
                    continue;
                }

                // Select between external events and timeouts
                tokio::select! {
                    biased;

                    call = call_rx.recv() => {
                        if let Some(envelope) = call {
                            // Cancel event timeout on any external event
                            event_timeout = None;

                            // Explicitly drop the typed message; handle_event
                            // receives rmpv::Value instead.
                            drop(envelope.msg);
                            let payload = rmpv::Value::Nil;
                            let payload_clone = payload.clone();
                            let result = statem
                                .handle_event(
                                    EventType::Call(envelope.reply_tx),
                                    payload,
                                    &current_state,
                                    &mut data,
                                )
                                .await;

                            if handle_result(
                                &statem,
                                result,
                                &mut current_state,
                                &mut data,
                                &mut state_timeout,
                                &mut event_timeout,
                                &mut generic_timeouts,
                                &mut postponed,
                                &mut internal_queue,
                                state_enter,
                                false, // calls cannot be postponed (reply_tx consumed)
                                &payload_clone,
                            )
                            .await
                            {
                                break;
                            }
                        } else {
                            statem
                                .terminate(ExitReason::Normal, &current_state, &mut data)
                                .await;
                            break;
                        }
                    }

                    cast = cast_rx.recv() => {
                        if let Some(envelope) = cast {
                            // Cancel event timeout on any external event
                            event_timeout = None;

                            // Explicitly drop the typed message; handle_event
                            // receives rmpv::Value instead.
                            drop(envelope.msg);

                            let payload = rmpv::Value::Nil;
                            let payload_clone = payload.clone();
                            let result = statem
                                .handle_event(
                                    EventType::Cast,
                                    payload,
                                    &current_state,
                                    &mut data,
                                )
                                .await;

                            if handle_result(
                                &statem,
                                result,
                                &mut current_state,
                                &mut data,
                                &mut state_timeout,
                                &mut event_timeout,
                                &mut generic_timeouts,
                                &mut postponed,
                                &mut internal_queue,
                                state_enter,
                                true,
                                &payload_clone,
                            )
                            .await
                            {
                                break;
                            }
                        } else {
                            statem
                                .terminate(ExitReason::Normal, &current_state, &mut data)
                                .await;
                            break;
                        }
                    }

                    () = wait_for_timeout(&mut state_timeout) => {
                        let payload = state_timeout.take()
                            .map_or(rmpv::Value::Nil, |t| t.payload);
                        let payload_clone = payload.clone();

                        let result = statem
                            .handle_event(
                                EventType::StateTimeout,
                                payload,
                                &current_state,
                                &mut data,
                            )
                            .await;

                        if handle_result(
                            &statem,
                            result,
                            &mut current_state,
                            &mut data,
                            &mut state_timeout,
                            &mut event_timeout,
                            &mut generic_timeouts,
                            &mut postponed,
                            &mut internal_queue,
                            state_enter,
                            false,
                            &payload_clone,
                        )
                        .await
                        {
                            break;
                        }
                    }

                    () = wait_for_timeout(&mut event_timeout) => {
                        let payload = event_timeout.take()
                            .map_or(rmpv::Value::Nil, |t| t.payload);
                        let payload_clone = payload.clone();

                        let result = statem
                            .handle_event(
                                EventType::EventTimeout,
                                payload,
                                &current_state,
                                &mut data,
                            )
                            .await;

                        if handle_result(
                            &statem,
                            result,
                            &mut current_state,
                            &mut data,
                            &mut state_timeout,
                            &mut event_timeout,
                            &mut generic_timeouts,
                            &mut postponed,
                            &mut internal_queue,
                            state_enter,
                            false,
                            &payload_clone,
                        )
                        .await
                        {
                            break;
                        }
                    }

                    name_payload = next_generic_timeout(&mut generic_timeouts) => {
                        let (name, payload) = name_payload;
                        let payload_clone = payload.clone();

                        let result = statem
                            .handle_event(
                                EventType::Timeout(name),
                                payload,
                                &current_state,
                                &mut data,
                            )
                            .await;

                        if handle_result(
                            &statem,
                            result,
                            &mut current_state,
                            &mut data,
                            &mut state_timeout,
                            &mut event_timeout,
                            &mut generic_timeouts,
                            &mut postponed,
                            &mut internal_queue,
                            state_enter,
                            false,
                            &payload_clone,
                        )
                        .await
                        {
                            break;
                        }
                    }
                }
            }
        });

        // Panic isolation
        let _ = inner.await;
        table.remove(&pid);
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
    event_payload: &rmpv::Value,
) -> bool {
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

            let should_postpone = process_actions(
                actions,
                state_timeout,
                event_timeout,
                generic_timeouts,
                internal_queue,
            );

            if should_postpone && postponable {
                // Re-queue the event as postponed (with Cast event type as a stand-in)
                postponed.push(QueuedEvent {
                    event_type: EventType::Cast,
                    payload: event_payload.clone(),
                    postponable: true,
                });
            }

            if state_changed {
                // Cancel state timeout on state change
                *state_timeout = None;

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
                postponed.push(QueuedEvent {
                    event_type: EventType::Cast,
                    payload: event_payload.clone(),
                    postponable: true,
                });
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
                postponed.push(QueuedEvent {
                    event_type: EventType::Cast,
                    payload: event_payload.clone(),
                    postponable: true,
                });
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
/// Returns `true` if the `Postpone` action was present.
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
