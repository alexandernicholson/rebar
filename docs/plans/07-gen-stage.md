# Plan: GenStage / Flow

## Summary

Implement Elixir's `GenStage` — back-pressure-aware producer→producer_consumer→consumer pipelines with demand-driven data flow. GenStage is how the BEAM ecosystem handles stream processing (Broadway builds on GenStage). This enables building data pipelines that automatically flow-control without overwhelming downstream stages.

## Rebar Integration Points

- **GenServer patterns**: GenStage is built on similar callback patterns
- **Runtime**: Stages are processes spawned via Runtime
- **ProcessId**: Stages have PIDs, subscriptions reference PIDs
- **MessageRouter**: Demand/event messages sent via standard routing

## Proposed API

```rust
/// The role of a stage in a pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageType {
    Producer,
    ProducerConsumer,
    Consumer,
}

/// How a dispatcher distributes events to consumers.
pub trait Dispatcher: Send + Sync + 'static {
    /// Dispatch events to subscribed consumers based on their demand.
    fn dispatch(
        &mut self,
        events: Vec<rmpv::Value>,
        subscribers: &mut [Subscription],
    ) -> Vec<rmpv::Value>; // returns buffered (undispatched) events
}

/// Built-in dispatcher: round-robin based on demand.
pub struct DemandDispatcher;

/// Built-in dispatcher: broadcast all events to all consumers.
pub struct BroadcastDispatcher;

/// Built-in dispatcher: partition events by key.
pub struct PartitionDispatcher<F>
where
    F: Fn(&rmpv::Value) -> usize + Send + Sync;

/// Subscription options.
pub struct SubscribeOpts {
    pub max_demand: usize,    // default: 1000
    pub min_demand: usize,    // default: 750 (= max_demand * 3/4)
}

/// Subscription cancel behavior.
#[derive(Debug, Clone, Copy)]
pub enum CancelMode {
    Permanent,  // consumer exits on cancel (default)
    Transient,  // consumer exits only on abnormal cancel
    Temporary,  // consumer never exits on cancel
}

/// Demand handling mode.
#[derive(Debug, Clone, Copy)]
pub enum DemandMode {
    /// Consumer automatically sends demand after handle_events.
    Automatic,
    /// Developer controls demand via ask().
    Manual,
}

/// A subscription between a consumer and a producer.
pub struct Subscription {
    pub producer_pid: ProcessId,
    pub consumer_pid: ProcessId,
    pub tag: u64,
    pub demand: usize,
    pub max_demand: usize,
    pub min_demand: usize,
    pub mode: DemandMode,
}

/// The GenStage trait for producers and consumers.
#[async_trait]
pub trait GenStage: Send + Sync + 'static {
    type State: Send + 'static;

    /// Return the stage type and initial state.
    async fn init(&self) -> Result<(StageType, Self::State), String>;

    /// Handle demand from consumers (producers only).
    /// Must return events to emit.
    async fn handle_demand(
        &self,
        demand: usize,
        state: &mut Self::State,
    ) -> Vec<rmpv::Value> {
        let _ = (demand, state);
        vec![]
    }

    /// Handle incoming events from producers (consumers and producer_consumers).
    /// Returns events to emit downstream (empty for pure consumers).
    async fn handle_events(
        &self,
        events: Vec<rmpv::Value>,
        from: SubscriptionTag,
        state: &mut Self::State,
    ) -> Vec<rmpv::Value> {
        let _ = (events, from, state);
        vec![]
    }

    /// Called when a consumer subscribes. Return demand mode.
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
    async fn handle_cancel(
        &self,
        reason: CancelReason,
        from: SubscriptionTag,
        state: &mut Self::State,
    ) {
        let _ = (reason, from, state);
    }

    /// Handle synchronous calls.
    async fn handle_call(
        &self,
        msg: rmpv::Value,
        state: &mut Self::State,
    ) -> (rmpv::Value, Vec<rmpv::Value>) {
        let _ = msg;
        (rmpv::Value::Nil, vec![])
    }

    /// Handle async casts.
    async fn handle_cast(
        &self,
        msg: rmpv::Value,
        state: &mut Self::State,
    ) -> Vec<rmpv::Value> {
        let _ = (msg, state);
        vec![]
    }

    /// Handle raw info messages.
    async fn handle_info(
        &self,
        msg: rmpv::Value,
        state: &mut Self::State,
    ) -> Vec<rmpv::Value> {
        let _ = (msg, state);
        vec![]
    }

    /// Cleanup on shutdown.
    async fn terminate(&self, _reason: ExitReason, _state: &mut Self::State) {}
}

#[derive(Debug, Clone, Copy)]
pub struct SubscriptionTag(u64);

#[derive(Debug)]
pub enum CancelReason {
    Cancel,      // consumer cancelled
    Down,        // producer/consumer died
}

/// A reference to a running stage.
#[derive(Clone)]
pub struct GenStageRef {
    pid: ProcessId,
    cmd_tx: mpsc::Sender<StageCommand>,
}

impl GenStageRef {
    pub fn pid(&self) -> ProcessId;

    /// Subscribe this consumer to a producer.
    pub async fn subscribe(
        &self,
        producer: &GenStageRef,
        opts: SubscribeOpts,
    ) -> Result<SubscriptionTag, StageError>;

    /// Cancel a subscription.
    pub async fn cancel(&self, tag: SubscriptionTag) -> Result<(), StageError>;

    /// Manually request demand (for manual mode).
    pub async fn ask(&self, tag: SubscriptionTag, demand: usize) -> Result<(), StageError>;

    /// Synchronous call.
    pub async fn call(&self, msg: rmpv::Value, timeout: Duration) -> Result<rmpv::Value, StageError>;

    /// Async cast.
    pub fn cast(&self, msg: rmpv::Value) -> Result<(), StageError>;
}

/// Spawn a stage process.
pub async fn spawn_stage<S: GenStage>(
    runtime: Arc<Runtime>,
    stage: S,
) -> GenStageRef;

/// Stage errors.
#[derive(Debug, thiserror::Error)]
pub enum StageError {
    #[error("stage dead")]
    Dead,
    #[error("call timeout")]
    Timeout,
    #[error("subscription failed: {0}")]
    SubscriptionFailed(String),
}
```

## File Structure

```
crates/rebar-core/src/gen_stage/
├── mod.rs              # pub mod + re-exports
├── types.rs            # GenStage trait, StageType, SubscribeOpts, etc.
├── engine.rs           # spawn_stage(), event loop, demand tracking
├── dispatcher/
│   ├── mod.rs          # Dispatcher trait
│   ├── demand.rs       # DemandDispatcher
│   ├── broadcast.rs    # BroadcastDispatcher
│   └── partition.rs    # PartitionDispatcher
└── buffer.rs           # Event buffering logic
```

## TDD Test Plan

### Unit Tests (lifecycle)
1. `producer_starts` — init returns Producer type
2. `consumer_starts` — init returns Consumer type
3. `producer_consumer_starts` — init returns ProducerConsumer type
4. `stage_stops_cleanly` — terminate called

### Unit Tests (demand flow)
5. `consumer_sends_demand_on_subscribe` — automatic demand
6. `producer_receives_demand` — handle_demand called
7. `producer_emits_events` — events delivered to consumer
8. `demand_respected` — producer doesn't over-emit
9. `min_demand_triggers_reask` — demand replenished at threshold
10. `max_demand_caps_request` — never asks for more than max

### Unit Tests (dispatchers)
11. `demand_dispatcher_round_robin` — events distributed evenly
12. `broadcast_dispatcher_all_receive` — every consumer gets every event
13. `partition_dispatcher_by_key` — events routed to correct partition
14. `dispatcher_respects_demand` — no dispatch without demand

### Unit Tests (buffering)
15. `producer_buffers_when_no_demand` — events queued
16. `buffer_drained_when_demand_arrives` — queued events sent
17. `buffer_overflow_drops_oldest` — configurable strategy

### Unit Tests (subscriptions)
18. `subscribe_creates_subscription` — handle_subscribe called
19. `cancel_removes_subscription` — handle_cancel called
20. `producer_death_cancels_subscriptions` — downstream notified
21. `consumer_death_removes_demand` — upstream adjusts
22. `manual_demand_mode` — no automatic demand, ask() works

### Unit Tests (pipeline)
23. `producer_to_consumer_flow` — end-to-end data flow
24. `producer_to_producer_consumer_to_consumer` — three-stage pipeline
25. `backpressure_slows_producer` — producer blocks when consumers slow

### Integration Tests
26. `gen_stage_does_not_break_gen_server` — existing tests pass
27. `stage_under_supervisor` — supervised pipeline
28. `realistic_etl_pipeline` — producer reads data, transformer processes, consumer writes

## Existing Interface Guarantees

- `GenServer` trait — UNCHANGED
- `Runtime` — UNCHANGED
- `Supervisor` — UNCHANGED
- New `pub mod gen_stage;` added to `lib.rs` — purely additive

---

## Elixir Reference Documentation

### GenStage Module (hex package gen_stage v1.3.2)

#### Overview

GenStage implements data processing pipelines with built-in back-pressure. Stages exchange data via a demand protocol where consumers request data from producers, preventing buffer overflow.

#### Stage Types

- **Producer**: Generates events in response to demand. Implements `handle_demand/2`.
- **Consumer**: Receives events from producers. Implements `handle_events/3`.
- **Producer-Consumer**: Both receives and emits. Implements `handle_events/3` and emits output.

#### Back-Pressure Mechanism

1. Consumers subscribe to producers
2. Consumers request specific quantities (demand)
3. Producers emit only up to the requested amount
4. Demand tracked per subscription separately

#### Demand Configuration

- **`:max_demand`**: Upper limit on events accepted at once
- **`:min_demand`**: Threshold triggering new demand requests

Example: With `max_demand: 1000` and `min_demand: 750`, consumer requests 1000 initially, then re-requests after processing reduces pending to 750.

#### Core Callbacks

**`init/1`**: Returns `{:producer, state}`, `{:consumer, state}`, or `{:producer_consumer, state}` with options.

**`handle_demand/2`** (producers): Called with demand count. Returns `{:noreply, events, state}`.

**`handle_events/3`** (consumers/producer_consumers): Processes event batches. Returns `{:noreply, events, state}`.

**`handle_subscribe/4`**: On subscription. Returns `{:automatic, state}` or `{:manual, state}`.

**`handle_cancel/3`**: On subscription cancel. Cleanup opportunity.

#### Dispatchers

- **DemandDispatcher** (default): Sequential dispatch based on per-consumer demand
- **BroadcastDispatcher**: Every event to all consumers
- **PartitionDispatcher**: Route by partition key

#### Buffering

Producers auto-buffer when no demand. Configurable via `:buffer_size` (default 10,000) and `:buffer_keep` (`:first` or `:last`).

#### Manual Demand

When `handle_subscribe` returns `{:manual, state}`, use `GenStage.ask(subscription, demand)` for explicit demand control. Enables rate limiting, async processing patterns.

#### Anti-Patterns

Use processes/stages to model runtime properties (concurrency, data-transfer), not for code organization. Scale via multiple consumer instances rather than pipeline depth. For simple parallel processing of in-memory data, use `Task.async_stream/2` instead.
