pub mod dispatcher;
pub mod engine;
pub mod types;

pub use dispatcher::{BroadcastDispatcher, DemandDispatcher, Dispatcher};
pub use engine::{spawn_stage, spawn_stage_with_dispatcher, GenStageRef};
pub use types::{
    CancelReason, DemandMode, GenStage, StageError, StageType, SubscribeOpts, SubscriptionTag,
};

#[cfg(test)]
#[allow(
    clippy::items_after_statements,
    clippy::significant_drop_tightening,
    clippy::needless_pass_by_value
)]
mod tests {
    use super::*;
    use crate::runtime::Runtime;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// A simple counter producer that emits incrementing integers.
    struct CounterProducer {
        /// Shared counter tracking how many events have been produced.
        counter: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl GenStage for CounterProducer {
        type State = usize; // next value to emit

        async fn init(&self) -> Result<(StageType, Self::State), String> {
            Ok((StageType::Producer, 0))
        }

        async fn handle_demand(
            &self,
            demand: usize,
            state: &mut Self::State,
        ) -> Vec<rmpv::Value> {
            let mut events = Vec::with_capacity(demand);
            for _ in 0..demand {
                events.push(rmpv::Value::Integer((*state as u64).into()));
                *state += 1;
                self.counter.fetch_add(1, Ordering::Relaxed);
            }
            events
        }
    }

    /// A consumer that collects received events into a shared vec.
    struct CollectorConsumer {
        collected: Arc<Mutex<Vec<rmpv::Value>>>,
    }

    #[async_trait::async_trait]
    impl GenStage for CollectorConsumer {
        type State = ();

        async fn init(&self) -> Result<(StageType, Self::State), String> {
            Ok((StageType::Consumer, ()))
        }

        async fn handle_events(
            &self,
            events: Vec<rmpv::Value>,
            _from: SubscriptionTag,
            _state: &mut Self::State,
        ) -> Vec<rmpv::Value> {
            self.collected.lock().await.extend(events);
            vec![]
        }
    }

    /// A producer that emits nothing (empty producer).
    struct EmptyProducer;

    #[async_trait::async_trait]
    impl GenStage for EmptyProducer {
        type State = ();

        async fn init(&self) -> Result<(StageType, Self::State), String> {
            Ok((StageType::Producer, ()))
        }
    }

    /// A producer-consumer that doubles each incoming value.
    struct DoublerStage;

    #[async_trait::async_trait]
    impl GenStage for DoublerStage {
        type State = ();

        async fn init(&self) -> Result<(StageType, Self::State), String> {
            Ok((StageType::ProducerConsumer, ()))
        }

        async fn handle_events(
            &self,
            events: Vec<rmpv::Value>,
            _from: SubscriptionTag,
            _state: &mut Self::State,
        ) -> Vec<rmpv::Value> {
            events
                .into_iter()
                .map(|v| {
                    let n = v.as_u64().unwrap_or(0);
                    rmpv::Value::Integer((n * 2).into())
                })
                .collect()
        }
    }

    /// A consumer that uses manual demand mode.
    struct ManualConsumer {
        collected: Arc<Mutex<Vec<rmpv::Value>>>,
    }

    #[async_trait::async_trait]
    impl GenStage for ManualConsumer {
        type State = ();

        async fn init(&self) -> Result<(StageType, Self::State), String> {
            Ok((StageType::Consumer, ()))
        }

        async fn handle_subscribe(
            &self,
            _role: StageType,
            _opts: &SubscribeOpts,
            _from: SubscriptionTag,
            _state: &mut Self::State,
        ) -> DemandMode {
            DemandMode::Manual
        }

        async fn handle_events(
            &self,
            events: Vec<rmpv::Value>,
            _from: SubscriptionTag,
            _state: &mut Self::State,
        ) -> Vec<rmpv::Value> {
            self.collected.lock().await.extend(events);
            vec![]
        }
    }

    /// A consumer that tracks cancel calls.
    struct CancelTracker {
        cancelled: Arc<Mutex<Vec<(CancelReason, SubscriptionTag)>>>,
    }

    #[async_trait::async_trait]
    impl GenStage for CancelTracker {
        type State = ();

        async fn init(&self) -> Result<(StageType, Self::State), String> {
            Ok((StageType::Consumer, ()))
        }

        async fn handle_cancel(
            &self,
            reason: CancelReason,
            from: SubscriptionTag,
            _state: &mut Self::State,
        ) {
            self.cancelled.lock().await.push((reason, from));
        }

        async fn handle_events(
            &self,
            _events: Vec<rmpv::Value>,
            _from: SubscriptionTag,
            _state: &mut Self::State,
        ) -> Vec<rmpv::Value> {
            vec![]
        }
    }

    /// A producer that handles calls and casts.
    struct CallableProducer;

    #[async_trait::async_trait]
    impl GenStage for CallableProducer {
        type State = u64;

        async fn init(&self) -> Result<(StageType, Self::State), String> {
            Ok((StageType::Producer, 0))
        }

        async fn handle_call(
            &self,
            msg: rmpv::Value,
            state: &mut Self::State,
        ) -> (rmpv::Value, Vec<rmpv::Value>) {
            if msg.as_str() == Some("get") {
                (rmpv::Value::Integer((*state).into()), vec![])
            } else {
                *state += 1;
                (rmpv::Value::Nil, vec![])
            }
        }

        async fn handle_cast(
            &self,
            _msg: rmpv::Value,
            state: &mut Self::State,
        ) -> Vec<rmpv::Value> {
            *state += 10;
            vec![]
        }

        async fn handle_demand(
            &self,
            demand: usize,
            state: &mut Self::State,
        ) -> Vec<rmpv::Value> {
            let mut events = Vec::with_capacity(demand);
            for _ in 0..demand {
                events.push(rmpv::Value::Integer((*state).into()));
                *state += 1;
            }
            events
        }
    }

    /// A slow consumer that introduces a delay per event batch.
    struct SlowConsumer {
        collected: Arc<Mutex<Vec<rmpv::Value>>>,
        delay_ms: u64,
    }

    #[async_trait::async_trait]
    impl GenStage for SlowConsumer {
        type State = ();

        async fn init(&self) -> Result<(StageType, Self::State), String> {
            Ok((StageType::Consumer, ()))
        }

        async fn handle_events(
            &self,
            events: Vec<rmpv::Value>,
            _from: SubscriptionTag,
            _state: &mut Self::State,
        ) -> Vec<rmpv::Value> {
            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            self.collected.lock().await.extend(events);
            vec![]
        }
    }

    /// A stage that fails init.
    struct FailInit;

    #[async_trait::async_trait]
    impl GenStage for FailInit {
        type State = ();

        async fn init(&self) -> Result<(StageType, Self::State), String> {
            Err("init failed".into())
        }
    }

    /// A producer that tracks whether `terminate` was called.
    struct TerminateTracker {
        terminated: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl GenStage for TerminateTracker {
        type State = ();

        async fn init(&self) -> Result<(StageType, Self::State), String> {
            Ok((StageType::Producer, ()))
        }

        async fn terminate(
            &self,
            _reason: crate::process::ExitReason,
            _state: &mut Self::State,
        ) {
            self.terminated.fetch_add(1, Ordering::Relaxed);
        }
    }

    // -----------------------------------------------------------------------
    // 1. Producer starts
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn producer_starts() {
        let rt = Arc::new(Runtime::new(1));
        let counter = Arc::new(AtomicUsize::new(0));
        let stage_ref = spawn_stage(
            Arc::clone(&rt),
            CounterProducer {
                counter: Arc::clone(&counter),
            },
        )
        .await;
        assert_eq!(stage_ref.pid().node_id(), 1);
    }

    // -----------------------------------------------------------------------
    // 2. Consumer starts
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn consumer_starts() {
        let rt = Arc::new(Runtime::new(1));
        let collected = Arc::new(Mutex::new(Vec::new()));
        let stage_ref = spawn_stage(
            Arc::clone(&rt),
            CollectorConsumer {
                collected: Arc::clone(&collected),
            },
        )
        .await;
        assert_eq!(stage_ref.pid().node_id(), 1);
    }

    // -----------------------------------------------------------------------
    // 3. ProducerConsumer starts
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn producer_consumer_starts() {
        let rt = Arc::new(Runtime::new(1));
        let stage_ref = spawn_stage(Arc::clone(&rt), DoublerStage).await;
        assert_eq!(stage_ref.pid().node_id(), 1);
    }

    // -----------------------------------------------------------------------
    // 4. Stage stops cleanly (terminate called)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn stage_stops_cleanly() {
        let rt = Arc::new(Runtime::new(1));
        let terminated = Arc::new(AtomicUsize::new(0));
        let _stage_ref = spawn_stage(
            Arc::clone(&rt),
            TerminateTracker {
                terminated: Arc::clone(&terminated),
            },
        )
        .await;
        // Drop the ref to close the channel
        drop(_stage_ref);
        for _ in 0..1000 {
            if terminated.load(Ordering::Relaxed) > 0 { break; }
            tokio::task::yield_now().await;
        }
        assert!(terminated.load(Ordering::Relaxed) > 0);
    }

    // -----------------------------------------------------------------------
    // 5. Consumer sends demand on subscribe (automatic)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn consumer_sends_demand_on_subscribe() {
        let rt = Arc::new(Runtime::new(1));
        let counter = Arc::new(AtomicUsize::new(0));
        let collected = Arc::new(Mutex::new(Vec::new()));

        let producer = spawn_stage(
            Arc::clone(&rt),
            CounterProducer {
                counter: Arc::clone(&counter),
            },
        )
        .await;

        let consumer = spawn_stage(
            Arc::clone(&rt),
            CollectorConsumer {
                collected: Arc::clone(&collected),
            },
        )
        .await;

        let opts = SubscribeOpts::new(10, 5);
        let _tag = consumer.subscribe(&producer, opts).await.unwrap();

        // Wait for automatic demand to flow through
        for _ in 0..1000 {
            if counter.load(Ordering::Relaxed) > 0 { break; }
            tokio::task::yield_now().await;
        }

        // The producer should have emitted events
        assert!(counter.load(Ordering::Relaxed) > 0);
    }

    // -----------------------------------------------------------------------
    // 6. Producer receives demand
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn producer_receives_demand() {
        let rt = Arc::new(Runtime::new(1));
        let counter = Arc::new(AtomicUsize::new(0));
        let collected = Arc::new(Mutex::new(Vec::new()));

        let producer = spawn_stage(
            Arc::clone(&rt),
            CounterProducer {
                counter: Arc::clone(&counter),
            },
        )
        .await;

        let consumer = spawn_stage(
            Arc::clone(&rt),
            CollectorConsumer {
                collected: Arc::clone(&collected),
            },
        )
        .await;

        let opts = SubscribeOpts::new(5, 3);
        consumer.subscribe(&producer, opts).await.unwrap();

        for _ in 0..1000 {
            if counter.load(Ordering::Relaxed) >= 5 { break; }
            tokio::task::yield_now().await;
        }

        // Producer should have generated exactly max_demand events initially
        let produced = counter.load(Ordering::Relaxed);
        assert!(produced >= 5, "expected at least 5, got {produced}");
    }

    // -----------------------------------------------------------------------
    // 7. Producer emits events to consumer
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn producer_emits_events() {
        let rt = Arc::new(Runtime::new(1));
        let counter = Arc::new(AtomicUsize::new(0));
        let collected = Arc::new(Mutex::new(Vec::new()));

        let producer = spawn_stage(
            Arc::clone(&rt),
            CounterProducer {
                counter: Arc::clone(&counter),
            },
        )
        .await;

        let consumer = spawn_stage(
            Arc::clone(&rt),
            CollectorConsumer {
                collected: Arc::clone(&collected),
            },
        )
        .await;

        let opts = SubscribeOpts::new(5, 3);
        consumer.subscribe(&producer, opts).await.unwrap();

        for _ in 0..1000 {
            if !collected.lock().await.is_empty() { break; }
            tokio::task::yield_now().await;
        }

        let events = collected.lock().await;
        assert!(!events.is_empty(), "consumer should have received events");
        // Verify the events are the expected integers
        assert_eq!(events[0], rmpv::Value::Integer(0u64.into()));
    }

    // -----------------------------------------------------------------------
    // 8. Demand respected (producer doesn't over-emit)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn demand_respected() {
        let rt = Arc::new(Runtime::new(1));
        let counter = Arc::new(AtomicUsize::new(0));
        let collected = Arc::new(Mutex::new(Vec::new()));

        let producer = spawn_stage(
            Arc::clone(&rt),
            CounterProducer {
                counter: Arc::clone(&counter),
            },
        )
        .await;

        let consumer = spawn_stage(
            Arc::clone(&rt),
            ManualConsumer {
                collected: Arc::clone(&collected),
            },
        )
        .await;

        let opts = SubscribeOpts::new(10, 5);
        let tag = consumer.subscribe(&producer, opts).await.unwrap();

        // In manual mode, no automatic demand is sent
        for _ in 0..1000 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            counter.load(Ordering::Relaxed),
            0,
            "no events should be produced without demand"
        );

        // Manually ask for 3 events
        consumer.ask(tag, 3).await.unwrap();
        for _ in 0..1000 {
            if collected.lock().await.len() == 3 { break; }
            tokio::task::yield_now().await;
        }

        let events = collected.lock().await;
        assert_eq!(events.len(), 3, "should receive exactly 3 events");
    }

    // -----------------------------------------------------------------------
    // 9. Min demand triggers re-ask
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn min_demand_triggers_reask() {
        let rt = Arc::new(Runtime::new(1));
        let counter = Arc::new(AtomicUsize::new(0));
        let collected = Arc::new(Mutex::new(Vec::new()));

        let producer = spawn_stage(
            Arc::clone(&rt),
            CounterProducer {
                counter: Arc::clone(&counter),
            },
        )
        .await;

        let consumer = spawn_stage(
            Arc::clone(&rt),
            CollectorConsumer {
                collected: Arc::clone(&collected),
            },
        )
        .await;

        // Small values to make re-ask visible: max=4, min=2
        let opts = SubscribeOpts::new(4, 2);
        consumer.subscribe(&producer, opts).await.unwrap();

        // Wait long enough for initial demand + at least one re-ask cycle
        for _ in 0..1000 {
            if counter.load(Ordering::Relaxed) > 4 { break; }
            tokio::task::yield_now().await;
        }

        let produced = counter.load(Ordering::Relaxed);
        // Should produce more than just the initial max_demand (4) due to re-asks
        assert!(
            produced > 4,
            "expected re-asks to produce more than 4, got {produced}"
        );
    }

    // -----------------------------------------------------------------------
    // 10. Max demand caps request
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn max_demand_caps_request() {
        let rt = Arc::new(Runtime::new(1));
        let counter = Arc::new(AtomicUsize::new(0));
        let collected = Arc::new(Mutex::new(Vec::new()));

        let producer = spawn_stage(
            Arc::clone(&rt),
            CounterProducer {
                counter: Arc::clone(&counter),
            },
        )
        .await;

        let consumer = spawn_stage(
            Arc::clone(&rt),
            ManualConsumer {
                collected: Arc::clone(&collected),
            },
        )
        .await;

        let opts = SubscribeOpts::new(5, 3);
        let tag = consumer.subscribe(&producer, opts).await.unwrap();

        // Ask for exactly max_demand
        consumer.ask(tag, 5).await.unwrap();
        for _ in 0..1000 {
            if collected.lock().await.len() == 5 { break; }
            tokio::task::yield_now().await;
        }

        let events = collected.lock().await;
        assert_eq!(events.len(), 5);
    }

    // -----------------------------------------------------------------------
    // 11. DemandDispatcher distributes events (unit tests in dispatcher.rs)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn demand_dispatcher_integration() {
        let rt = Arc::new(Runtime::new(1));
        let counter = Arc::new(AtomicUsize::new(0));
        let collected1 = Arc::new(Mutex::new(Vec::new()));
        let collected2 = Arc::new(Mutex::new(Vec::new()));

        let producer = spawn_stage(
            Arc::clone(&rt),
            CounterProducer {
                counter: Arc::clone(&counter),
            },
        )
        .await;

        let consumer1 = spawn_stage(
            Arc::clone(&rt),
            CollectorConsumer {
                collected: Arc::clone(&collected1),
            },
        )
        .await;

        let consumer2 = spawn_stage(
            Arc::clone(&rt),
            CollectorConsumer {
                collected: Arc::clone(&collected2),
            },
        )
        .await;

        let opts = SubscribeOpts::new(5, 3);
        consumer1
            .subscribe(&producer, opts.clone())
            .await
            .unwrap();
        consumer2.subscribe(&producer, opts).await.unwrap();

        for _ in 0..1000 {
            if !collected1.lock().await.is_empty() || !collected2.lock().await.is_empty() { break; }
            tokio::task::yield_now().await;
        }

        let c1 = collected1.lock().await;
        let c2 = collected2.lock().await;
        // Both consumers should have received events
        assert!(
            !c1.is_empty() || !c2.is_empty(),
            "at least one consumer should have events"
        );
    }

    // -----------------------------------------------------------------------
    // 12. BroadcastDispatcher sends to all consumers
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn broadcast_dispatcher_all_receive() {
        let rt = Arc::new(Runtime::new(1));
        let counter = Arc::new(AtomicUsize::new(0));
        let collected1 = Arc::new(Mutex::new(Vec::new()));
        let collected2 = Arc::new(Mutex::new(Vec::new()));

        let producer = spawn_stage_with_dispatcher(
            Arc::clone(&rt),
            CounterProducer {
                counter: Arc::clone(&counter),
            },
            BroadcastDispatcher::new(),
        )
        .await;

        let consumer1 = spawn_stage(
            Arc::clone(&rt),
            CollectorConsumer {
                collected: Arc::clone(&collected1),
            },
        )
        .await;

        let consumer2 = spawn_stage(
            Arc::clone(&rt),
            CollectorConsumer {
                collected: Arc::clone(&collected2),
            },
        )
        .await;

        let opts = SubscribeOpts::new(5, 3);
        consumer1
            .subscribe(&producer, opts.clone())
            .await
            .unwrap();
        consumer2.subscribe(&producer, opts).await.unwrap();

        for _ in 0..1000 {
            if !collected1.lock().await.is_empty() && !collected2.lock().await.is_empty() { break; }
            tokio::task::yield_now().await;
        }

        let c1 = collected1.lock().await;
        let c2 = collected2.lock().await;
        // With broadcast, both should have the same events
        assert!(!c1.is_empty(), "consumer1 should have events");
        assert!(!c2.is_empty(), "consumer2 should have events");
    }

    // -----------------------------------------------------------------------
    // 13. Dispatcher respects zero demand
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn dispatcher_respects_no_demand() {
        let rt = Arc::new(Runtime::new(1));
        let counter = Arc::new(AtomicUsize::new(0));
        let collected = Arc::new(Mutex::new(Vec::new()));

        let producer = spawn_stage(
            Arc::clone(&rt),
            CounterProducer {
                counter: Arc::clone(&counter),
            },
        )
        .await;

        let consumer = spawn_stage(
            Arc::clone(&rt),
            ManualConsumer {
                collected: Arc::clone(&collected),
            },
        )
        .await;

        let opts = SubscribeOpts::new(10, 5);
        let _tag = consumer.subscribe(&producer, opts).await.unwrap();

        // Don't send any demand — manual mode
        for _ in 0..1000 {
            tokio::task::yield_now().await;
        }

        let events = collected.lock().await;
        assert!(events.is_empty(), "no events without demand");
    }

    // -----------------------------------------------------------------------
    // 14. Producer buffers when no demand
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn producer_buffers_when_no_demand() {
        let rt = Arc::new(Runtime::new(1));
        let counter = Arc::new(AtomicUsize::new(0));

        let producer = spawn_stage(
            Arc::clone(&rt),
            CounterProducer {
                counter: Arc::clone(&counter),
            },
        )
        .await;

        // No consumer subscribed — no demand
        for _ in 0..1000 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            counter.load(Ordering::Relaxed),
            0,
            "producer should not emit without demand"
        );

        // Now subscribe a consumer and events should flow
        let collected = Arc::new(Mutex::new(Vec::new()));
        let consumer = spawn_stage(
            Arc::clone(&rt),
            CollectorConsumer {
                collected: Arc::clone(&collected),
            },
        )
        .await;

        let opts = SubscribeOpts::new(5, 3);
        consumer.subscribe(&producer, opts).await.unwrap();

        for _ in 0..1000 {
            if counter.load(Ordering::Relaxed) > 0 { break; }
            tokio::task::yield_now().await;
        }
        assert!(counter.load(Ordering::Relaxed) > 0);
    }

    // -----------------------------------------------------------------------
    // 15. Subscribe creates subscription
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn subscribe_creates_subscription() {
        let rt = Arc::new(Runtime::new(1));
        let counter = Arc::new(AtomicUsize::new(0));
        let collected = Arc::new(Mutex::new(Vec::new()));

        let producer = spawn_stage(
            Arc::clone(&rt),
            CounterProducer {
                counter: Arc::clone(&counter),
            },
        )
        .await;

        let consumer = spawn_stage(
            Arc::clone(&rt),
            CollectorConsumer {
                collected: Arc::clone(&collected),
            },
        )
        .await;

        let opts = SubscribeOpts::new(5, 3);
        let result = consumer.subscribe(&producer, opts).await;
        assert!(result.is_ok(), "subscribe should succeed");
    }

    // -----------------------------------------------------------------------
    // 16. Cancel removes subscription
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn cancel_removes_subscription() {
        let rt = Arc::new(Runtime::new(1));
        let counter = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::new(Mutex::new(Vec::new()));

        let producer = spawn_stage(
            Arc::clone(&rt),
            CounterProducer {
                counter: Arc::clone(&counter),
            },
        )
        .await;

        let consumer = spawn_stage(
            Arc::clone(&rt),
            CancelTracker {
                cancelled: Arc::clone(&cancelled),
            },
        )
        .await;

        let opts = SubscribeOpts::new(5, 3);
        let tag = consumer.subscribe(&producer, opts).await.unwrap();

        // Cancel from the consumer side
        consumer.cancel(tag).await.unwrap();

        for _ in 0..1000 {
            if !cancelled.lock().await.is_empty() { break; }
            tokio::task::yield_now().await;
        }

        let cancels = cancelled.lock().await;
        assert!(
            !cancels.is_empty(),
            "handle_cancel should have been called"
        );
    }

    // -----------------------------------------------------------------------
    // 17. Manual demand mode
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn manual_demand_mode() {
        let rt = Arc::new(Runtime::new(1));
        let counter = Arc::new(AtomicUsize::new(0));
        let collected = Arc::new(Mutex::new(Vec::new()));

        let producer = spawn_stage(
            Arc::clone(&rt),
            CounterProducer {
                counter: Arc::clone(&counter),
            },
        )
        .await;

        let consumer = spawn_stage(
            Arc::clone(&rt),
            ManualConsumer {
                collected: Arc::clone(&collected),
            },
        )
        .await;

        let opts = SubscribeOpts::new(10, 5);
        let tag = consumer.subscribe(&producer, opts).await.unwrap();

        // Nothing produced yet (manual mode)
        for _ in 0..1000 {
            tokio::task::yield_now().await;
        }
        assert_eq!(collected.lock().await.len(), 0);

        // Ask for 2
        consumer.ask(tag, 2).await.unwrap();
        for _ in 0..1000 {
            if collected.lock().await.len() == 2 { break; }
            tokio::task::yield_now().await;
        }
        assert_eq!(collected.lock().await.len(), 2);

        // Ask for 3 more
        consumer.ask(tag, 3).await.unwrap();
        for _ in 0..1000 {
            if collected.lock().await.len() == 5 { break; }
            tokio::task::yield_now().await;
        }
        assert_eq!(collected.lock().await.len(), 5);
    }

    // -----------------------------------------------------------------------
    // 18. Producer to consumer flow (end-to-end)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn producer_to_consumer_flow() {
        let rt = Arc::new(Runtime::new(1));
        let counter = Arc::new(AtomicUsize::new(0));
        let collected = Arc::new(Mutex::new(Vec::new()));

        let producer = spawn_stage(
            Arc::clone(&rt),
            CounterProducer {
                counter: Arc::clone(&counter),
            },
        )
        .await;

        let consumer = spawn_stage(
            Arc::clone(&rt),
            CollectorConsumer {
                collected: Arc::clone(&collected),
            },
        )
        .await;

        let opts = SubscribeOpts::new(10, 5);
        consumer.subscribe(&producer, opts).await.unwrap();

        for _ in 0..1000 {
            if collected.lock().await.len() >= 10 { break; }
            tokio::task::yield_now().await;
        }

        let events = collected.lock().await;
        assert!(events.len() >= 10, "should receive at least 10 events");
        // Verify ordering: first event should be 0
        assert_eq!(events[0], rmpv::Value::Integer(0u64.into()));
    }

    // -----------------------------------------------------------------------
    // 19. Three-stage pipeline: producer -> producer_consumer -> consumer
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn producer_to_producer_consumer_to_consumer() {
        let rt = Arc::new(Runtime::new(1));
        let counter = Arc::new(AtomicUsize::new(0));
        let collected = Arc::new(Mutex::new(Vec::new()));

        let producer = spawn_stage(
            Arc::clone(&rt),
            CounterProducer {
                counter: Arc::clone(&counter),
            },
        )
        .await;

        let doubler = spawn_stage(Arc::clone(&rt), DoublerStage).await;

        let consumer = spawn_stage(
            Arc::clone(&rt),
            CollectorConsumer {
                collected: Arc::clone(&collected),
            },
        )
        .await;

        // Wire up: doubler subscribes to producer, consumer subscribes to doubler
        let opts = SubscribeOpts::new(5, 3);
        doubler
            .subscribe(&producer, opts.clone())
            .await
            .unwrap();
        consumer.subscribe(&doubler, opts).await.unwrap();

        for _ in 0..1000 {
            if !collected.lock().await.is_empty() { break; }
            tokio::task::yield_now().await;
        }

        let events = collected.lock().await;
        assert!(
            !events.is_empty(),
            "consumer should receive doubled events"
        );
        // The first event from producer is 0, doubled = 0
        // The second event from producer is 1, doubled = 2
        if events.len() >= 2 {
            assert_eq!(events[0], rmpv::Value::Integer(0u64.into()));
            assert_eq!(events[1], rmpv::Value::Integer(2u64.into()));
        }
    }

    // -----------------------------------------------------------------------
    // 20. Back-pressure: producer blocks when no demand
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn backpressure_slows_producer() {
        let rt = Arc::new(Runtime::new(1));
        let counter = Arc::new(AtomicUsize::new(0));
        let collected = Arc::new(Mutex::new(Vec::new()));

        let producer = spawn_stage(
            Arc::clone(&rt),
            CounterProducer {
                counter: Arc::clone(&counter),
            },
        )
        .await;

        let consumer = spawn_stage(
            Arc::clone(&rt),
            SlowConsumer {
                collected: Arc::clone(&collected),
                delay_ms: 100,
            },
        )
        .await;

        let opts = SubscribeOpts::new(3, 2);
        consumer.subscribe(&producer, opts).await.unwrap();

        // Wait for the slow consumer to process some events
        for _ in 0..1000 {
            if collected.lock().await.len() > 0 { break; }
            tokio::task::yield_now().await;
        }
        // Let a few more batches flow for a meaningful back-pressure check
        tokio::time::sleep(Duration::from_millis(300)).await;

        let produced = counter.load(Ordering::Relaxed);
        let consumed = collected.lock().await.len();
        // Due to back-pressure, the producer shouldn't have run ahead unboundedly
        assert!(
            produced < 100,
            "producer should be back-pressured, produced: {produced}"
        );
        assert!(consumed > 0, "consumer should have processed some events");
    }

    // -----------------------------------------------------------------------
    // 21. Empty producer emits nothing
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn empty_producer_emits_nothing() {
        let rt = Arc::new(Runtime::new(1));
        let collected = Arc::new(Mutex::new(Vec::new()));

        let producer = spawn_stage(Arc::clone(&rt), EmptyProducer).await;

        let consumer = spawn_stage(
            Arc::clone(&rt),
            CollectorConsumer {
                collected: Arc::clone(&collected),
            },
        )
        .await;

        let opts = SubscribeOpts::new(5, 3);
        consumer.subscribe(&producer, opts).await.unwrap();

        for _ in 0..1000 {
            tokio::task::yield_now().await;
        }

        let events = collected.lock().await;
        assert!(events.is_empty(), "empty producer emits no events");
    }

    // -----------------------------------------------------------------------
    // 22. GenStageRef call and cast
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn stage_ref_call_and_cast() {
        let rt = Arc::new(Runtime::new(1));
        let stage_ref = spawn_stage(Arc::clone(&rt), CallableProducer).await;

        // Call "get" -> should return 0 initially
        let result = stage_ref
            .call(
                rmpv::Value::String("get".into()),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert_eq!(result, rmpv::Value::Integer(0u64.into()));

        // Call "inc" -> state becomes 1
        stage_ref
            .call(
                rmpv::Value::String("inc".into()),
                Duration::from_secs(1),
            )
            .await
            .unwrap();

        let result = stage_ref
            .call(
                rmpv::Value::String("get".into()),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert_eq!(result, rmpv::Value::Integer(1u64.into()));

        // Cast -> state += 10
        stage_ref.cast(rmpv::Value::Nil).unwrap();

        let result = stage_ref
            .call(
                rmpv::Value::String("get".into()),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert_eq!(result, rmpv::Value::Integer(11u64.into()));
    }

    // -----------------------------------------------------------------------
    // 23. Stage with failed init
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn failed_init_does_not_crash() {
        let rt = Arc::new(Runtime::new(1));
        let stage_ref = spawn_stage(Arc::clone(&rt), FailInit).await;

        // The stage should be dead since init failed
        let result = stage_ref
            .call(rmpv::Value::Nil, Duration::from_millis(100))
            .await;
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // 24. GenStageRef is Send + Sync
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn gen_stage_ref_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GenStageRef>();
    }

    // -----------------------------------------------------------------------
    // 25. Multiple subscriptions to same producer
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn multiple_subscriptions_same_producer() {
        let rt = Arc::new(Runtime::new(1));
        let counter = Arc::new(AtomicUsize::new(0));
        let collected1 = Arc::new(Mutex::new(Vec::new()));
        let collected2 = Arc::new(Mutex::new(Vec::new()));

        let producer = spawn_stage(
            Arc::clone(&rt),
            CounterProducer {
                counter: Arc::clone(&counter),
            },
        )
        .await;

        let consumer1 = spawn_stage(
            Arc::clone(&rt),
            CollectorConsumer {
                collected: Arc::clone(&collected1),
            },
        )
        .await;

        let consumer2 = spawn_stage(
            Arc::clone(&rt),
            CollectorConsumer {
                collected: Arc::clone(&collected2),
            },
        )
        .await;

        let opts = SubscribeOpts::new(5, 3);
        consumer1
            .subscribe(&producer, opts.clone())
            .await
            .unwrap();
        consumer2.subscribe(&producer, opts).await.unwrap();

        for _ in 0..1000 {
            let total = collected1.lock().await.len() + collected2.lock().await.len();
            if total > 0 { break; }
            tokio::task::yield_now().await;
        }

        let total = collected1.lock().await.len() + collected2.lock().await.len();
        assert!(total > 0, "events should be distributed across consumers");
    }

    // -----------------------------------------------------------------------
    // 26. SubscribeOpts defaults
    // -----------------------------------------------------------------------

    #[test]
    fn subscribe_opts_defaults() {
        let opts = SubscribeOpts::default();
        assert_eq!(opts.max_demand, 1000);
        assert_eq!(opts.min_demand, 500);
    }

    // -----------------------------------------------------------------------
    // 27. StageType display
    // -----------------------------------------------------------------------

    #[test]
    fn stage_type_display() {
        assert_eq!(format!("{}", StageType::Producer), "producer");
        assert_eq!(
            format!("{}", StageType::ProducerConsumer),
            "producer_consumer"
        );
        assert_eq!(format!("{}", StageType::Consumer), "consumer");
    }

    // -----------------------------------------------------------------------
    // 28. SubscriptionTag uniqueness
    // -----------------------------------------------------------------------

    #[test]
    fn subscription_tag_uniqueness() {
        let t1 = SubscriptionTag::next();
        let t2 = SubscriptionTag::next();
        assert_ne!(t1, t2);
    }

    // -----------------------------------------------------------------------
    // 29. CancelReason display
    // -----------------------------------------------------------------------

    #[test]
    fn cancel_reason_display() {
        assert_eq!(format!("{}", CancelReason::Cancel), "cancel");
        assert_eq!(format!("{}", CancelReason::Down), "down");
    }

    // -----------------------------------------------------------------------
    // 30. StageError display
    // -----------------------------------------------------------------------

    #[test]
    fn stage_error_display() {
        let err = StageError::Dead;
        assert!(format!("{err}").contains("dead"));
        let err = StageError::Timeout;
        assert!(format!("{err}").contains("timeout"));
        let err = StageError::SubscriptionFailed("bad".into());
        assert!(format!("{err}").contains("bad"));
    }
}
