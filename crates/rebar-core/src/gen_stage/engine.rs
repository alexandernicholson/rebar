use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::process::mailbox::Mailbox;
use crate::process::table::ProcessHandle;
use crate::process::{ExitReason, ProcessId};
use crate::runtime::Runtime;

use super::dispatcher::{ConsumerDemand, DemandDispatcher, DispatchResult, Dispatcher};
use super::types::{
    CancelReason, DemandMode, GenStage, StageError, StageType, SubscribeOpts, SubscriptionTag,
};

/// Maximum number of events buffered by a stage before the overflow policy
/// kicks in. Bounds memory when a downstream consumer is slow or absent.
const MAX_EVENT_BUFFER: usize = 100_000;

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
///
/// The sender to the producer is held *weakly*: a consumer must not keep its
/// producer alive (and vice versa via [`DownstreamSubscription`]). Otherwise a
/// pair of peered stages forms a strong-reference cycle that never terminates
/// even after all external handles are dropped — and producer/consumer death
/// could never be observed by the surviving peer. A failed upgrade means the
/// producer has terminated.
struct UpstreamSubscription {
    producer_cmd_tx: mpsc::WeakSender<StageCommand>,
    mode: DemandMode,
    min_demand: usize,
    /// Negotiated upper bound on in-flight demand for this subscription.
    max_demand: usize,
    /// How many events have been received but not yet re-demanded.
    pending_events: usize,
    /// Demand we have asked for but not yet been satisfied (the in-flight
    /// window). Tracked separately so re-asks never push the outstanding
    /// window above `max_demand`.
    outstanding_demand: usize,
}

/// Tracks a subscription from the producer's perspective (downstream consumer).
///
/// Held weakly for the same reason as [`UpstreamSubscription`]: a producer
/// must not keep a dropped consumer alive, and a failed upgrade (or closed
/// channel) signals consumer death so the producer can run the implicit
/// cancel path.
struct DownstreamSubscription {
    consumer_cmd_tx: mpsc::WeakSender<StageCommand>,
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
        match self.cmd_tx.try_send(StageCommand::Cast { msg }) {
            // `Full` means the command channel is full but the stage is alive.
            // A cast is fire-and-forget (matching OTP `cast` semantics): we
            // must not misreport a live, backpressured stage as `Dead`. The
            // cast is dropped rather than blocking the caller. Only a closed
            // channel (stage terminated) is reported as `Dead`.
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(StageError::Dead),
        }
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

    // Insert a routable, monitorable handle at spawn. Without this the stage's
    // PID is allocated but never present in the table, so `table.send`,
    // `monitor`, `register` and pg all treat the live stage as dead. The stage
    // has no `handle_info`, so the mailbox is only used to make the PID
    // addressable; we drain it in the loop to avoid unbounded growth.
    let (mailbox_tx, mailbox_rx) = Mailbox::unbounded();
    runtime
        .table()
        .insert(pid, ProcessHandle::new(mailbox_tx));

    let table = Arc::clone(runtime.table());

    // Use a WeakSender inside the loop so that when all external GenStageRef
    // handles are dropped, the channel closes and the loop terminates.
    let weak_tx = cmd_tx.downgrade();
    tokio::spawn(async move {
        let inner = tokio::spawn(async move {
            run_stage_loop(stage_impl, dispatcher, cmd_rx, weak_tx, mailbox_rx).await;
        });
        let _ = inner.await;
        // Route death through the canonical cleanup path so monitors get
        // DOWN, registered names are released, and exit hooks run — `remove`
        // alone would leak all of that.
        table.cleanup_process(pid);
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

    /// Push leftover events into the bounded buffer, applying the
    /// drop-oldest overflow policy so a slow/absent downstream cannot grow
    /// memory without bound (the buffer used to be unbounded → OOM).
    fn buffer_leftover(&mut self, leftover: Vec<rmpv::Value>) {
        if leftover.is_empty() {
            return;
        }
        self.event_buffer.extend(leftover);
        if self.event_buffer.len() > MAX_EVENT_BUFFER {
            let overflow = self.event_buffer.len() - MAX_EVENT_BUFFER;
            // Drop-oldest: discard the head so the freshest events survive.
            self.event_buffer.drain(..overflow);
            #[cfg(feature = "tracing")]
            tracing::warn!(
                dropped = overflow,
                buffer = MAX_EVENT_BUFFER,
                "gen_stage event buffer overflow; dropping oldest events"
            );
        }
    }

    /// Total demand currently advertised by downstream consumers.
    fn total_downstream_demand(&self) -> usize {
        self.consumer_demands
            .iter()
            .map(|cd| cd.pending_demand)
            .sum()
    }

    /// Dispatch events through the dispatcher and buffer leftovers.
    ///
    /// Returns the set of downstream subscription tags whose delivery channel
    /// was found closed (an implicit cancel); the caller cleans them up.
    fn dispatch_and_deliver(&mut self, events: Vec<rmpv::Value>) -> Vec<SubscriptionTag> {
        if events.is_empty() {
            return Vec::new();
        }
        let DispatchResult {
            deliveries,
            leftover,
        } = self
            .dispatcher
            .dispatch(events, &mut self.consumer_demands);
        self.buffer_leftover(leftover);
        let (undelivered, dead) = deliver_events(&self.downstreams, deliveries);
        // Undelivered events (channel Full) are re-buffered so we keep
        // servicing the command loop instead of blocking — this avoids
        // cross-stage deadlock on bounded channels.
        self.buffer_leftover(undelivered);
        dead
    }

    /// Dispatch events only if this stage type can emit downstream.
    fn maybe_dispatch_emitted(&mut self, emitted: Vec<rmpv::Value>) -> Vec<SubscriptionTag> {
        if !emitted.is_empty()
            && (self.stage_type == StageType::Producer
                || self.stage_type == StageType::ProducerConsumer)
        {
            return self.dispatch_and_deliver(emitted);
        }
        Vec::new()
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
    mut mailbox_rx: crate::process::mailbox::MailboxRx,
) {
    // Initialize
    let Ok((stage_type, mut user_state)) = stage_impl.init().await else {
        return;
    };

    let mut ls = LoopState::new(dispatcher, stage_type);

    loop {
        // Drain the routing mailbox so it never grows unbounded. The stage
        // exposes no `handle_info`, so inbound mailbox messages are discarded;
        // this just keeps the mailbox empty while the PID stays addressable.
        while mailbox_rx.try_recv().is_some() {}

        let Some(cmd) = cmd_rx.recv().await else {
            // Channel closed — all refs dropped. Notify every connected peer
            // (upstreams and downstreams) with Cancel{Down} so they run their
            // handle_cancel/teardown instead of dangling on a dead stage.
            propagate_down(&ls).await;
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
                let dead = ls.maybe_dispatch_emitted(emitted);
                cleanup_dead_downstreams(&stage_impl, &mut user_state, &mut ls, dead).await;
            }

            StageCommand::Cast { msg } => {
                let emitted = stage_impl.handle_cast(msg, &mut user_state).await;
                let dead = ls.maybe_dispatch_emitted(emitted);
                cleanup_dead_downstreams(&stage_impl, &mut user_state, &mut ls, dead).await;
            }
        }
    }
}

/// Notify all connected peers that this stage is going down.
///
/// Sends `Cancel{reason: Down}` to every upstream producer and downstream
/// consumer so peers run their `handle_cancel`/teardown instead of being left
/// pointing at a dead stage.
async fn propagate_down<D: Dispatcher>(ls: &LoopState<D>) {
    for (tag, up) in &ls.upstreams {
        send_weak(
            &up.producer_cmd_tx,
            StageCommand::Cancel {
                tag: *tag,
                reason: CancelReason::Down,
            },
        )
        .await;
    }
    for (tag, down) in &ls.downstreams {
        send_weak(
            &down.consumer_cmd_tx,
            StageCommand::Cancel {
                tag: *tag,
                reason: CancelReason::Down,
            },
        )
        .await;
    }
}

/// Treat downstream consumers whose delivery channel was found closed as an
/// implicit cancel: run `handle_cancel`, drop the subscription and its demand
/// accounting so we stop allocating it demand and dropping events into a dead
/// channel.
async fn cleanup_dead_downstreams<S: GenStage, D: Dispatcher>(
    stage_impl: &S,
    user_state: &mut S::State,
    ls: &mut LoopState<D>,
    dead: Vec<SubscriptionTag>,
) {
    for tag in dead {
        if ls.downstreams.remove(&tag).is_some() {
            ls.dispatcher.cancel(tag);
            ls.consumer_demands.retain(|cd| cd.tag != tag);
            stage_impl
                .handle_cancel(CancelReason::Down, tag, user_state)
                .await;
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
            consumer_cmd_tx: consumer_cmd_tx.downgrade(),
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
    let max_demand = opts.max_demand.max(1);
    // Clamp `min_demand` to `max_demand`. A `min_demand > max_demand`
    // subscription would, in automatic mode, never reach its re-ask threshold
    // after the first batch (pending_events caps at max_demand < min_demand) →
    // the pipeline stalls. Clamping keeps automatic mode making progress.
    let min_demand = opts.min_demand.min(max_demand);

    // The consumer decides the demand mode
    let mode = stage_impl
        .handle_subscribe(ls.stage_type, &opts, tag, user_state)
        .await;

    ls.upstreams.insert(
        tag,
        UpstreamSubscription {
            producer_cmd_tx: producer_cmd_tx.downgrade(),
            mode,
            min_demand,
            max_demand,
            pending_events: 0,
            outstanding_demand: 0,
        },
    );

    // In automatic mode, immediately send initial demand (the full window).
    if mode == DemandMode::Automatic {
        if let Some(up) = ls.upstreams.get_mut(&tag) {
            up.outstanding_demand = max_demand;
        }
        let _ = producer_cmd_tx
            .send(StageCommand::AskDemand {
                tag,
                demand: max_demand,
            })
            .await;
    }
}

/// Send a command over a weakly-held peer sender, upgrading first.
///
/// Returns `false` if the peer has terminated (upgrade failed or channel
/// closed), so the caller can treat it as an implicit cancel.
async fn send_weak(weak: &mpsc::WeakSender<StageCommand>, cmd: StageCommand) -> bool {
    match weak.upgrade() {
        Some(tx) => tx.send(cmd).await.is_ok(),
        None => false,
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
        send_weak(&down.consumer_cmd_tx, StageCommand::Cancel { tag, reason }).await;
    }
    // Check if it's an upstream subscription (we are consumer)
    else if let Some(up) = ls.upstreams.remove(&tag) {
        stage_impl
            .handle_cancel(reason, tag, user_state)
            .await;
        send_weak(&up.producer_cmd_tx, StageCommand::Cancel { tag, reason }).await;
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
        let weak = up.producer_cmd_tx.clone();
        if !send_weak(&weak, StageCommand::AskDemand { tag, demand }).await {
            // Producer is gone: synthesize an implicit Cancel{Down} so this
            // consumer runs its teardown instead of dangling.
            if ls.upstreams.remove(&tag).is_some() {
                stage_impl
                    .handle_cancel(CancelReason::Down, tag, user_state)
                    .await;
            }
        }
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
        let (undelivered, dead) = deliver_events(&ls.downstreams, deliveries);
        ls.buffer_leftover(undelivered);
        cleanup_dead_downstreams(stage_impl, user_state, ls, dead).await;
    }

    // After draining the buffer, ask the user's producer callback only for the
    // demand *increment* from this command (not the cumulative outstanding
    // demand). Passing the running total made convention-following producers
    // re-produce events they already emitted on each new ask.
    //
    // For pure producers this is bounded by the negotiated window via the
    // consumer's re-ask logic; we additionally do not pull more than the
    // current outstanding downstream demand can absorb.
    if ls.stage_type == StageType::Producer {
        let want = demand.min(ls.total_downstream_demand());
        if want > 0 {
            let events = stage_impl.handle_demand(want, user_state).await;
            let dead = ls.dispatch_and_deliver(events);
            cleanup_dead_downstreams(stage_impl, user_state, ls, dead).await;
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

    // If this is a producer_consumer, dispatch emitted events downstream first
    // so `total_downstream_demand` below reflects the post-dispatch state.
    if ls.stage_type == StageType::ProducerConsumer && !emitted.is_empty() {
        let dead = ls.dispatch_and_deliver(emitted);
        cleanup_dead_downstreams(stage_impl, user_state, ls, dead).await;
    }

    // For a producer_consumer, only re-ask upstream for what we can actually
    // dispatch downstream, so a slow/absent downstream cannot make us pull an
    // unbounded amount from upstream into the (now bounded) event buffer.
    let downstream_room = if ls.stage_type == StageType::ProducerConsumer {
        ls.total_downstream_demand()
    } else {
        usize::MAX
    };

    // In automatic mode, track events and re-demand when the threshold is hit,
    // capping the in-flight window at `max_demand`. We compute the demand to
    // re-ask under the mutable borrow, then release the borrow before the
    // (await-ing) send so we can clean up the upstream if the producer is dead.
    let mut reask: Option<(mpsc::WeakSender<StageCommand>, usize)> = None;
    if let Some(up) = ls.upstreams.get_mut(&tag)
        && up.mode == DemandMode::Automatic
    {
        up.pending_events += event_count;
        // These events satisfy outstanding demand.
        up.outstanding_demand = up.outstanding_demand.saturating_sub(event_count);

        if up.pending_events >= up.min_demand {
            up.pending_events = 0;
            // Re-ask up to the negotiated window: never let outstanding demand
            // ratchet beyond `max_demand`, and never pull more than the
            // downstream can currently absorb.
            let headroom = up.max_demand.saturating_sub(up.outstanding_demand);
            let ask = headroom.min(downstream_room);
            if ask > 0 {
                up.outstanding_demand += ask;
                reask = Some((up.producer_cmd_tx.clone(), ask));
            }
        }
    }

    if let Some((weak, ask)) = reask
        && !send_weak(&weak, StageCommand::AskDemand { tag, demand: ask }).await
        && ls.upstreams.remove(&tag).is_some()
    {
        // Producer died: synthesize Cancel{Down} for this consumer.
        stage_impl
            .handle_cancel(CancelReason::Down, tag, user_state)
            .await;
    }
}

/// Deliver dispatched events to the appropriate downstream consumers.
///
/// Uses non-blocking `try_send`: events that cannot be delivered because the
/// consumer's channel is full are returned (so the caller re-buffers them and
/// keeps servicing its command loop — blocking here can deadlock a
/// producer/consumer cycle). Tags whose channel is closed are returned
/// separately so the caller can treat them as an implicit cancel.
fn deliver_events(
    downstreams: &HashMap<SubscriptionTag, DownstreamSubscription>,
    deliveries: Vec<(SubscriptionTag, Vec<rmpv::Value>)>,
) -> (Vec<rmpv::Value>, Vec<SubscriptionTag>) {
    let mut undelivered = Vec::new();
    let mut dead = Vec::new();
    for (tag, events) in deliveries {
        let Some(down) = downstreams.get(&tag) else {
            continue;
        };
        let Some(tx) = down.consumer_cmd_tx.upgrade() else {
            // Consumer has terminated entirely.
            undelivered.extend(events);
            dead.push(tag);
            continue;
        };
        match tx.try_send(StageCommand::Events { tag, events }) {
            Ok(()) => {}
            // Channel full: keep servicing our loop instead of blocking; the
            // undelivered events are re-buffered by the caller. The events are
            // recovered from the rejected command (always an `Events`).
            Err(mpsc::error::TrySendError::Full(cmd)) => {
                if let StageCommand::Events { events, .. } = cmd {
                    undelivered.extend(events);
                }
            }
            // Channel closed: an implicit cancel — re-buffer and mark dead.
            Err(mpsc::error::TrySendError::Closed(cmd)) => {
                if let StageCommand::Events { events, .. } = cmd {
                    undelivered.extend(events);
                }
                dead.push(tag);
            }
        }
    }
    (undelivered, dead)
}
