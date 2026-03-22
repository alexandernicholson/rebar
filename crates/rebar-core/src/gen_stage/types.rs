use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::process::ExitReason;

/// The role of a stage in a pipeline.
///
/// Determines which callbacks are invoked on the stage:
/// - `Producer` — handles demand, emits events downstream.
/// - `Consumer` — receives events from upstream producers.
/// - `ProducerConsumer` — receives events and emits transformed events downstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageType {
    /// A stage that only produces events in response to demand.
    Producer,
    /// A stage that receives events from upstream and emits transformed events downstream.
    ProducerConsumer,
    /// A terminal stage that only consumes events.
    Consumer,
}

impl fmt::Display for StageType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Producer => write!(f, "producer"),
            Self::ProducerConsumer => write!(f, "producer_consumer"),
            Self::Consumer => write!(f, "consumer"),
        }
    }
}

/// How a consumer requests demand from its upstream producer.
///
/// In `Automatic` mode the stage re-asks the producer after processing
/// events. In `Manual` mode the application must call
/// [`GenStageRef::ask`](super::GenStageRef::ask) explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemandMode {
    /// After `handle_events`, the consumer automatically sends new demand
    /// to the producer when the processed count reaches `min_demand`.
    Automatic,
    /// The developer controls demand via [`GenStageRef::ask`](super::GenStageRef::ask).
    Manual,
}

/// Reason a subscription was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    /// The consumer explicitly cancelled the subscription.
    Cancel,
    /// The remote stage (producer or consumer) terminated.
    Down,
}

impl fmt::Display for CancelReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancel => write!(f, "cancel"),
            Self::Down => write!(f, "down"),
        }
    }
}

/// A unique tag identifying a subscription between a consumer and producer.
///
/// Tags are monotonically increasing and globally unique within a runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriptionTag(u64);

impl SubscriptionTag {
    /// Generate a new unique `SubscriptionTag`.
    pub(crate) fn next() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    /// Return the raw `u64` value of this tag.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for SubscriptionTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SubscriptionTag({})", self.0)
    }
}

/// Options controlling a subscription between a consumer and a producer.
///
/// # Defaults
///
/// | Field | Default |
/// |---|---|
/// | `max_demand` | 1000 |
/// | `min_demand` | 500 |
#[derive(Debug, Clone)]
pub struct SubscribeOpts {
    /// Upper limit on events accepted at once.
    pub max_demand: usize,
    /// Threshold at which the consumer re-requests demand.
    /// When pending demand drops to this level, a new batch is requested.
    pub min_demand: usize,
}

impl Default for SubscribeOpts {
    fn default() -> Self {
        Self {
            max_demand: 1000,
            min_demand: 500,
        }
    }
}

impl SubscribeOpts {
    /// Create a new `SubscribeOpts` with the given `max_demand` and `min_demand`.
    #[must_use]
    pub const fn new(max_demand: usize, min_demand: usize) -> Self {
        Self {
            max_demand,
            min_demand,
        }
    }
}

/// Errors returned by [`GenStageRef`](super::GenStageRef) operations.
#[derive(Debug, thiserror::Error)]
pub enum StageError {
    /// The stage process has terminated and is no longer reachable.
    #[error("stage dead")]
    Dead,
    /// A synchronous call to the stage timed out.
    #[error("call timeout")]
    Timeout,
    /// A subscription request failed with the given reason.
    #[error("subscription failed: {0}")]
    SubscriptionFailed(String),
}

/// The `GenStage` trait for back-pressure-aware producer/consumer stages.
///
/// Implement this trait to define a stage in a data processing pipeline.
/// The stage type (producer, consumer, or producer-consumer) is determined
/// by the return value of [`init`](GenStage::init).
///
/// # Lifecycle
///
/// 1. [`init`](GenStage::init) is called once to determine the stage type and initial state.
/// 2. Producers receive [`handle_demand`](GenStage::handle_demand) calls from downstream consumers.
/// 3. Consumers receive [`handle_events`](GenStage::handle_events) calls with batches from upstream.
/// 4. [`terminate`](GenStage::terminate) is called on shutdown.
#[async_trait::async_trait]
pub trait GenStage: Send + Sync + 'static {
    /// The mutable state maintained by this stage.
    type State: Send + 'static;

    /// Return the stage type and initial state.
    ///
    /// # Errors
    ///
    /// Return `Err(reason)` to abort stage startup.
    async fn init(&self) -> Result<(StageType, Self::State), String>;

    /// Handle demand from downstream consumers (producers only).
    ///
    /// Called when a consumer requests `demand` events. Must return a `Vec`
    /// of events to emit downstream. Returning fewer events than requested is
    /// fine; the remaining demand is tracked.
    #[allow(clippy::unused_async)]
    async fn handle_demand(
        &self,
        demand: usize,
        state: &mut Self::State,
    ) -> Vec<rmpv::Value> {
        let _ = (demand, state);
        vec![]
    }

    /// Handle incoming events from upstream producers.
    ///
    /// For consumers, the returned `Vec` is ignored. For producer-consumers,
    /// the returned events are dispatched downstream.
    #[allow(clippy::unused_async)]
    async fn handle_events(
        &self,
        events: Vec<rmpv::Value>,
        from: SubscriptionTag,
        state: &mut Self::State,
    ) -> Vec<rmpv::Value> {
        let _ = (events, from, state);
        vec![]
    }

    /// Called when a consumer subscribes to this producer.
    ///
    /// Return [`DemandMode::Automatic`] (default) or [`DemandMode::Manual`]
    /// to control how the subscription handles demand.
    #[allow(clippy::unused_async)]
    async fn handle_subscribe(
        &self,
        role: StageType,
        opts: &SubscribeOpts,
        from: SubscriptionTag,
        state: &mut Self::State,
    ) -> DemandMode {
        let _ = (role, opts, from, state);
        DemandMode::Automatic
    }

    /// Called when a subscription is cancelled.
    #[allow(clippy::unused_async)]
    async fn handle_cancel(
        &self,
        reason: CancelReason,
        from: SubscriptionTag,
        state: &mut Self::State,
    ) {
        let _ = (reason, from, state);
    }

    /// Handle a synchronous call.
    ///
    /// Returns a reply value and optionally emitted events.
    #[allow(clippy::unused_async)]
    async fn handle_call(
        &self,
        msg: rmpv::Value,
        state: &mut Self::State,
    ) -> (rmpv::Value, Vec<rmpv::Value>) {
        let _ = (msg, state);
        (rmpv::Value::Nil, vec![])
    }

    /// Handle an asynchronous cast (fire-and-forget).
    ///
    /// Returns events to emit downstream (empty for pure consumers).
    #[allow(clippy::unused_async)]
    async fn handle_cast(
        &self,
        msg: rmpv::Value,
        state: &mut Self::State,
    ) -> Vec<rmpv::Value> {
        let _ = (msg, state);
        vec![]
    }

    /// Cleanup on shutdown.
    #[allow(clippy::unused_async)]
    async fn terminate(&self, _reason: ExitReason, _state: &mut Self::State) {}
}
