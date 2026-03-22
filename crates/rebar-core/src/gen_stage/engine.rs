use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::process::{ExitReason, ProcessId};
use crate::runtime::Runtime;

use super::dispatcher::{ConsumerDemand, DemandDispatcher, DispatchResult, Dispatcher};
use super::types::{
    CancelReason, DemandMode, GenStage, StageError, StageType, SubscribeOpts, SubscriptionTag,
};

// ---------------------------------------------------------------------------
// Internal message types
// ---------------------------------------------------------------------------

/// Commands sent to a stage's event loop.
enum StageCommand {
    /// A consumer subscribes to this producer.
    Subscribe {
        consumer_tag: SubscriptionTag,
        consumer_cmd_tx: mpsc::Sender<Self>,
        opts: SubscribeOpts,
        reply: oneshot::Sender<Result<(), StageError>>,
    },
    /// Cancel a subscription.
    Cancel {
        tag: SubscriptionTag,
        reason: CancelReason,
    },
    /// Demand from a consumer to a producer.
    AskDemand {
        tag: SubscriptionTag,
        demand: usize,
    },
    /// Events delivered from a producer to a consumer.
    Events {
        tag: SubscriptionTag,
        events: Vec<rmpv::Value>,
    },
    /// Synchronous call.
    Call {
        msg: rmpv::Value,
        reply: oneshot::Sender<rmpv::Value>,
    },
    /// Asynchronous cast.
    Cast {
        msg: rmpv::Value,
    },
    /// Notify the consumer that the producer confirmed the subscription.
    SubscriptionConfirmed {
        tag: SubscriptionTag,
        producer_cmd_tx: mpsc::Sender<Self>,
        opts: SubscribeOpts,
    },
}

// ---------------------------------------------------------------------------
// Subscription tracking
// ---------------------------------------------------------------------------

/// Tracks a subscription from the consumer's perspective (upstream producer).
struct UpstreamSubscription {
    producer_cmd_tx: mpsc::Sender<StageCommand>,
    mode: DemandMode,
    min_demand: usize,
    /// How many events have been received but not yet re-demanded.
    pending_events: usize,
}

/// Tracks a subscription from the producer's perspective (downstream consumer).
struct DownstreamSubscription {
    consumer_cmd_tx: mpsc::Sender<StageCommand>,
}

// ---------------------------------------------------------------------------
// GenStageRef
// ---------------------------------------------------------------------------

/// A handle to a running [`GenStage`] process.
///
/// Provides methods to subscribe, cancel subscriptions, request demand,
/// and send calls/casts to the stage.
#[derive(Clone)]
pub struct GenStageRef {
    pid: ProcessId,
    cmd_tx: mpsc::Sender<StageCommand>,
}

impl GenStageRef {
    /// The PID of the running stage process.
    #[must_use]
    pub const fn pid(&self) -> ProcessId {
        self.pid
    }

    /// Subscribe this consumer to a producer.
    ///
    /// The consumer will begin receiving events from the producer according
    /// to the demand protocol. The consumer's `handle_subscribe` callback
    /// determines whether demand is automatic or manual.
    ///
    /// # Errors
    ///
    /// Returns [`StageError::Dead`] if the consumer or producer stage has
    /// terminated, or [`StageError::SubscriptionFailed`] if the subscription
    /// was rejected.
    pub async fn subscribe(
        &self,
        producer: &Self,
        opts: SubscribeOpts,
    ) -> Result<SubscriptionTag, StageError> {
        let tag = SubscriptionTag::next();
        let (reply_tx, reply_rx) = oneshot::channel();

        // Tell the producer about the new subscription
        producer
            .cmd_tx
            .send(StageCommand::Subscribe {
                consumer_tag: tag,
                consumer_cmd_tx: self.cmd_tx.clone(),
                opts,
                reply: reply_tx,
            })
            .await
            .map_err(|_| StageError::Dead)?;

        reply_rx.await.map_err(|_| StageError::Dead)?.map(|()| tag)
    }

    /// Cancel a subscription.
    ///
    /// # Errors
    ///
    /// Returns [`StageError::Dead`] if the stage has terminated.
    pub async fn cancel(&self, tag: SubscriptionTag) -> Result<(), StageError> {
        self.cmd_tx
            .send(StageCommand::Cancel {
                tag,
                reason: CancelReason::Cancel,
            })
            .await
            .map_err(|_| StageError::Dead)
    }

    /// Manually request `demand` events from the producer for the given subscription.
    ///
    /// Only meaningful when the subscription is in [`DemandMode::Manual`].
    ///
    /// # Errors
    ///
    /// Returns [`StageError::Dead`] if the stage has terminated.
    pub async fn ask(
        &self,
        tag: SubscriptionTag,
        demand: usize,
    ) -> Result<(), StageError> {
        self.cmd_tx
            .send(StageCommand::AskDemand { tag, demand })
            .await
            .map_err(|_| StageError::Dead)
    }

    /// Send a synchronous call to the stage.
    ///
    /// Blocks (asynchronously) until the stage replies or the `timeout` expires.
    ///
    /// # Errors
    ///
    /// Returns [`StageError::Dead`] if the stage has terminated, or
    /// [`StageError::Timeout`] if no reply is received within `timeout`.
    pub async fn call(
        &self,
        msg: rmpv::Value,
        timeout: Duration,
    ) -> Result<rmpv::Value, StageError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(StageCommand::Call { msg, reply: reply_tx })
            .await
            .map_err(|_| StageError::Dead)?;

        match tokio::time::timeout(timeout, reply_rx).await {
            Ok(Ok(val)) => Ok(val),
            Ok(Err(_)) => Err(StageError::Dead),
            Err(_) => Err(StageError::Timeout),
        }
    }

    /// Send an asynchronous cast (fire-and-forget) to the stage.
    ///
    /// # Errors
    ///
    /// Returns [`StageError::Dead`] if the stage has terminated.
    pub fn cast(&self, msg: rmpv::Value) -> Result<(), StageError> {
        self.cmd_tx
            .try_send(StageCommand::Cast { msg })
            .map_err(|_| StageError::Dead)
    }
}

// ---------------------------------------------------------------------------
// spawn_stage
// ---------------------------------------------------------------------------

/// Spawn a [`GenStage`] process on the given runtime.
///
/// Returns a [`GenStageRef`] handle for interacting with the stage.
///
/// The stage's [`init`](GenStage::init) callback is invoked before this
/// function returns (within the spawned task). If `init` fails the stage
/// terminates immediately.
pub async fn spawn_stage<S: GenStage>(
    runtime: Arc<Runtime>,
    stage_impl: S,
) -> GenStageRef {
    spawn_stage_with_dispatcher(runtime, stage_impl, DemandDispatcher::new()).await
}

/// Spawn a [`GenStage`] process with a custom [`Dispatcher`].
///
/// This is the same as [`spawn_stage`] but allows choosing a dispatcher
/// strategy (e.g. [`BroadcastDispatcher`](super::BroadcastDispatcher)).
pub async fn spawn_stage_with_dispatcher<S: GenStage, D: Dispatcher>(
    runtime: Arc<Runtime>,
    stage_impl: S,
    dispatcher: D,
) -> GenStageRef {
    let pid = runtime.table().allocate_pid();
    let (cmd_tx, cmd_rx) = mpsc::channel::<StageCommand>(256);

    let table = Arc::clone(runtime.table());

    // Use a WeakSender inside the loop so that when all external GenStageRef
    // handles are dropped, the channel closes and the loop terminates.
    let weak_tx = cmd_tx.downgrade();
    tokio::spawn(async move {
        let inner = tokio::spawn(async move {
            run_stage_loop(stage_impl, dispatcher, cmd_rx, weak_tx).await;
        });
        let _ = inner.await;
        table.remove(&pid);
    });

    GenStageRef { pid, cmd_tx }
}

// ---------------------------------------------------------------------------
// Stage loop state
// ---------------------------------------------------------------------------

/// Mutable state for the stage event loop, extracted to keep the main
/// loop function under the line-count lint threshold.
struct LoopState<D> {
    dispatcher: D,
    upstreams: HashMap<SubscriptionTag, UpstreamSubscription>,
    downstreams: HashMap<SubscriptionTag, DownstreamSubscription>,
    consumer_demands: Vec<ConsumerDemand>,
    event_buffer: Vec<rmpv::Value>,
    stage_type: StageType,
}

impl<D: Dispatcher> LoopState<D> {
    fn new(dispatcher: D, stage_type: StageType) -> Self {
        Self {
            dispatcher,
            upstreams: HashMap::new(),
            downstreams: HashMap::new(),
            consumer_demands: Vec::new(),
            event_buffer: Vec::new(),
            stage_type,
        }
    }

    /// Dispatch events through the dispatcher and buffer leftovers.
    async fn dispatch_and_deliver(&mut self, events: Vec<rmpv::Value>) {
        if events.is_empty() {
            return;
        }
        let DispatchResult {
            deliveries,
            leftover,
        } = self
            .dispatcher
            .dispatch(events, &mut self.consumer_demands);
        self.event_buffer.extend(leftover);
        deliver_events(&self.downstreams, deliveries).await;
    }

    /// Dispatch events only if this stage type can emit downstream.
    async fn maybe_dispatch_emitted(&mut self, emitted: Vec<rmpv::Value>) {
        if !emitted.is_empty()
            && (self.stage_type == StageType::Producer
                || self.stage_type == StageType::ProducerConsumer)
        {
            self.dispatch_and_deliver(emitted).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

/// The main event loop for a stage process.
#[allow(clippy::too_many_lines)]
async fn run_stage_loop<S: GenStage, D: Dispatcher>(
    stage_impl: S,
    dispatcher: D,
    mut cmd_rx: mpsc::Receiver<StageCommand>,
    self_weak_tx: mpsc::WeakSender<StageCommand>,
) {
    // Initialize
    let Ok((stage_type, mut user_state)) = stage_impl.init().await else {
        return;
    };

    let mut ls = LoopState::new(dispatcher, stage_type);

    loop {
        let Some(cmd) = cmd_rx.recv().await else {
            // Channel closed — all refs dropped
            stage_impl
                .terminate(ExitReason::Normal, &mut user_state)
                .await;
            break;
        };

        match cmd {
            StageCommand::Subscribe {
                consumer_tag,
                consumer_cmd_tx,
                opts,
                reply,
            } => {
                handle_subscribe(
                    &stage_impl,
                    &mut user_state,
                    &mut ls,
                    &self_weak_tx,
                    SubscribeArgs {
                        consumer_tag,
                        consumer_cmd_tx,
                        opts,
                        reply,
                    },
                )
                .await;
            }

            StageCommand::SubscriptionConfirmed {
                tag,
                producer_cmd_tx,
                opts,
            } => {
                handle_subscription_confirmed(
                    &stage_impl,
                    &mut user_state,
                    &mut ls,
                    tag,
                    producer_cmd_tx,
                    opts,
                )
                .await;
            }

            StageCommand::Cancel { tag, reason } => {
                handle_cancel(&stage_impl, &mut user_state, &mut ls, tag, reason).await;
            }

            StageCommand::AskDemand { tag, demand } => {
                handle_ask_demand(&stage_impl, &mut user_state, &mut ls, tag, demand).await;
            }

            StageCommand::Events { tag, events } => {
                handle_events(&stage_impl, &mut user_state, &mut ls, tag, events).await;
            }

            StageCommand::Call { msg, reply } => {
                let (response, emitted) =
                    stage_impl.handle_call(msg, &mut user_state).await;
                let _ = reply.send(response);
                ls.maybe_dispatch_emitted(emitted).await;
            }

            StageCommand::Cast { msg } => {
                let emitted = stage_impl.handle_cast(msg, &mut user_state).await;
                ls.maybe_dispatch_emitted(emitted).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Command handlers
// ---------------------------------------------------------------------------

/// Arguments for a subscribe command, bundled to avoid exceeding the
/// argument-count lint.
struct SubscribeArgs {
    consumer_tag: SubscriptionTag,
    consumer_cmd_tx: mpsc::Sender<StageCommand>,
    opts: SubscribeOpts,
    reply: oneshot::Sender<Result<(), StageError>>,
}

/// Handle a `Subscribe` command — a downstream consumer subscribes to us (producer side).
async fn handle_subscribe<S: GenStage, D: Dispatcher>(
    stage_impl: &S,
    user_state: &mut S::State,
    ls: &mut LoopState<D>,
    self_weak_tx: &mpsc::WeakSender<StageCommand>,
    args: SubscribeArgs,
) {
    let SubscribeArgs {
        consumer_tag,
        consumer_cmd_tx,
        opts,
        reply,
    } = args;

    // Call handle_subscribe on the producer side (for notification/acceptance)
    let _ = stage_impl
        .handle_subscribe(ls.stage_type, &opts, consumer_tag, user_state)
        .await;

    ls.dispatcher.subscribe(consumer_tag);
    ls.consumer_demands
        .push(ConsumerDemand::new(consumer_tag));

    ls.downstreams.insert(
        consumer_tag,
        DownstreamSubscription {
            consumer_cmd_tx: consumer_cmd_tx.clone(),
        },
    );

    // Upgrade weak sender to get our own cmd_tx for the consumer to send demand to
    if let Some(strong_tx) = self_weak_tx.upgrade() {
        let _ = consumer_cmd_tx
            .send(StageCommand::SubscriptionConfirmed {
                tag: consumer_tag,
                producer_cmd_tx: strong_tx,
                opts,
            })
            .await;
    }

    let _ = reply.send(Ok(()));
}

/// Handle a `SubscriptionConfirmed` command — a producer confirmed our subscription (consumer side).
///
/// The consumer's `handle_subscribe` callback is invoked here to determine
/// the demand mode (automatic or manual).
async fn handle_subscription_confirmed<S: GenStage, D: Dispatcher>(
    stage_impl: &S,
    user_state: &mut S::State,
    ls: &mut LoopState<D>,
    tag: SubscriptionTag,
    producer_cmd_tx: mpsc::Sender<StageCommand>,
    opts: SubscribeOpts,
) {
    let max_demand = opts.max_demand;
    let min_demand = opts.min_demand;

    // The consumer decides the demand mode
    let mode = stage_impl
        .handle_subscribe(ls.stage_type, &opts, tag, user_state)
        .await;

    ls.upstreams.insert(
        tag,
        UpstreamSubscription {
            producer_cmd_tx: producer_cmd_tx.clone(),
            mode,
            min_demand,
            pending_events: 0,
        },
    );

    // In automatic mode, immediately send initial demand
    if mode == DemandMode::Automatic {
        let _ = producer_cmd_tx
            .send(StageCommand::AskDemand {
                tag,
                demand: max_demand,
            })
            .await;
    }
}

/// Handle a `Cancel` command.
async fn handle_cancel<S: GenStage, D: Dispatcher>(
    stage_impl: &S,
    user_state: &mut S::State,
    ls: &mut LoopState<D>,
    tag: SubscriptionTag,
    reason: CancelReason,
) {
    // Check if it's a downstream subscription (we are producer)
    if let Some(down) = ls.downstreams.remove(&tag) {
        ls.dispatcher.cancel(tag);
        ls.consumer_demands.retain(|cd| cd.tag != tag);
        stage_impl
            .handle_cancel(reason, tag, user_state)
            .await;
        let _ = down
            .consumer_cmd_tx
            .send(StageCommand::Cancel { tag, reason })
            .await;
    }
    // Check if it's an upstream subscription (we are consumer)
    else if let Some(up) = ls.upstreams.remove(&tag) {
        stage_impl
            .handle_cancel(reason, tag, user_state)
            .await;
        let _ = up
            .producer_cmd_tx
            .send(StageCommand::Cancel { tag, reason })
            .await;
    }
}

/// Handle an `AskDemand` command.
///
/// If this stage is a producer/producer-consumer and the tag matches a
/// downstream consumer, the demand is handled locally.  If the tag matches
/// an upstream producer (i.e. we are a consumer calling `ask()`), the demand
/// is forwarded to the producer.
async fn handle_ask_demand<S: GenStage, D: Dispatcher>(
    stage_impl: &S,
    user_state: &mut S::State,
    ls: &mut LoopState<D>,
    tag: SubscriptionTag,
    demand: usize,
) {
    // If the tag belongs to an upstream subscription, forward demand to the producer
    if let Some(up) = ls.upstreams.get(&tag) {
        let _ = up
            .producer_cmd_tx
            .send(StageCommand::AskDemand { tag, demand })
            .await;
        return;
    }

    // Otherwise, we are the producer receiving demand from a downstream consumer
    if let Some(cd) = ls.consumer_demands.iter_mut().find(|cd| cd.tag == tag) {
        cd.pending_demand += demand;
    }

    // Try to drain the buffer first
    if !ls.event_buffer.is_empty() {
        let buffered = std::mem::take(&mut ls.event_buffer);
        let DispatchResult {
            deliveries,
            leftover,
        } = ls
            .dispatcher
            .dispatch(buffered, &mut ls.consumer_demands);
        ls.event_buffer = leftover;
        deliver_events(&ls.downstreams, deliveries).await;
    }

    // If there's still demand after draining the buffer, ask the stage for events
    let total_demand: usize = ls
        .consumer_demands
        .iter()
        .map(|cd| cd.pending_demand)
        .sum();
    if total_demand > 0
        && (ls.stage_type == StageType::Producer
            || ls.stage_type == StageType::ProducerConsumer)
    {
        let events = stage_impl
            .handle_demand(total_demand, user_state)
            .await;
        if !events.is_empty() {
            ls.dispatch_and_deliver(events).await;
        }
    }
}

/// Handle an `Events` command — events arriving from an upstream producer.
async fn handle_events<S: GenStage, D: Dispatcher>(
    stage_impl: &S,
    user_state: &mut S::State,
    ls: &mut LoopState<D>,
    tag: SubscriptionTag,
    events: Vec<rmpv::Value>,
) {
    let event_count = events.len();

    let emitted = stage_impl
        .handle_events(events, tag, user_state)
        .await;

    // In automatic mode, track events and re-demand when threshold hit
    if let Some(up) = ls.upstreams.get_mut(&tag)
        && up.mode == DemandMode::Automatic
    {
        up.pending_events += event_count;
        if up.pending_events >= up.min_demand {
            let ask = up.pending_events;
            up.pending_events = 0;
            let _ = up
                .producer_cmd_tx
                .send(StageCommand::AskDemand {
                    tag,
                    demand: ask,
                })
                .await;
        }
    }

    // If this is a producer_consumer, dispatch emitted events downstream
    if ls.stage_type == StageType::ProducerConsumer && !emitted.is_empty() {
        ls.dispatch_and_deliver(emitted).await;
    }
}

/// Deliver dispatched events to the appropriate downstream consumers.
async fn deliver_events(
    downstreams: &HashMap<SubscriptionTag, DownstreamSubscription>,
    deliveries: Vec<(SubscriptionTag, Vec<rmpv::Value>)>,
) {
    for (tag, events) in deliveries {
        if let Some(down) = downstreams.get(&tag) {
            let _ = down
                .consumer_cmd_tx
                .send(StageCommand::Events { tag, events })
                .await;
        }
    }
}
