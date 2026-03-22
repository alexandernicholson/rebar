use super::types::SubscriptionTag;

/// Tracks a single downstream consumer's demand within a dispatcher.
#[derive(Debug)]
pub struct ConsumerDemand {
    /// The subscription tag identifying this consumer.
    pub tag: SubscriptionTag,
    /// How many events this consumer is still willing to receive.
    pub pending_demand: usize,
}

impl ConsumerDemand {
    /// Create a new `ConsumerDemand` with the given tag and zero demand.
    #[must_use]
    pub const fn new(tag: SubscriptionTag) -> Self {
        Self {
            tag,
            pending_demand: 0,
        }
    }
}

/// Strategy for distributing events to downstream consumers.
///
/// Implementors decide how batches of events are split across multiple
/// consumers, taking their individual pending demand into account.
pub trait Dispatcher: Send + Sync + 'static {
    /// Dispatch `events` to the given `subscribers`.
    ///
    /// Returns any events that could not be dispatched (e.g. because no
    /// subscriber has remaining demand). These will be buffered by the engine.
    fn dispatch(
        &mut self,
        events: Vec<rmpv::Value>,
        subscribers: &mut [ConsumerDemand],
    ) -> DispatchResult;

    /// Notify the dispatcher that a subscriber has been added.
    fn subscribe(&mut self, _tag: SubscriptionTag) {}

    /// Notify the dispatcher that a subscriber has been removed.
    fn cancel(&mut self, _tag: SubscriptionTag) {}
}

/// The result of a dispatch operation.
///
/// Contains per-subscriber batches and any leftover events that could not
/// be delivered because no subscriber had remaining demand.
#[derive(Debug, Default)]
pub struct DispatchResult {
    /// Events to send to each subscriber, keyed by `SubscriptionTag`.
    pub deliveries: Vec<(SubscriptionTag, Vec<rmpv::Value>)>,
    /// Events that could not be dispatched and should be buffered.
    pub leftover: Vec<rmpv::Value>,
}

// ---------------------------------------------------------------------------
// DemandDispatcher
// ---------------------------------------------------------------------------

/// Distributes events to consumers sequentially based on their pending demand.
///
/// Each consumer receives events up to its pending demand, in subscription
/// order. Any events remaining after all consumers are satisfied are returned
/// as leftover for buffering.
pub struct DemandDispatcher;

impl DemandDispatcher {
    /// Create a new `DemandDispatcher`.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for DemandDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Dispatcher for DemandDispatcher {
    fn dispatch(
        &mut self,
        events: Vec<rmpv::Value>,
        subscribers: &mut [ConsumerDemand],
    ) -> DispatchResult {
        let mut remaining = events;
        let mut deliveries = Vec::new();

        for sub in subscribers.iter_mut() {
            if remaining.is_empty() {
                break;
            }
            if sub.pending_demand == 0 {
                continue;
            }

            let take = remaining.len().min(sub.pending_demand);
            let batch: Vec<rmpv::Value> = remaining.drain(..take).collect();
            sub.pending_demand -= batch.len();
            deliveries.push((sub.tag, batch));
        }

        DispatchResult {
            deliveries,
            leftover: remaining,
        }
    }
}

// ---------------------------------------------------------------------------
// BroadcastDispatcher
// ---------------------------------------------------------------------------

/// Sends every event to every subscribed consumer, ignoring individual demand.
///
/// Each subscriber receives a clone of the entire event batch. Pending demand
/// is decremented by the number of events delivered.
pub struct BroadcastDispatcher;

impl BroadcastDispatcher {
    /// Create a new `BroadcastDispatcher`.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for BroadcastDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Dispatcher for BroadcastDispatcher {
    fn dispatch(
        &mut self,
        events: Vec<rmpv::Value>,
        subscribers: &mut [ConsumerDemand],
    ) -> DispatchResult {
        if events.is_empty() || subscribers.is_empty() {
            return DispatchResult {
                deliveries: Vec::new(),
                leftover: events,
            };
        }

        // Check if any subscriber has demand
        let any_demand = subscribers.iter().any(|s| s.pending_demand > 0);
        if !any_demand {
            return DispatchResult {
                deliveries: Vec::new(),
                leftover: events,
            };
        }

        let mut deliveries = Vec::with_capacity(subscribers.len());

        for sub in subscribers.iter_mut() {
            if sub.pending_demand > 0 {
                let take = events.len().min(sub.pending_demand);
                let batch = events[..take].to_vec();
                sub.pending_demand = sub.pending_demand.saturating_sub(batch.len());
                deliveries.push((sub.tag, batch));
            }
        }

        DispatchResult {
            deliveries,
            leftover: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_events(n: usize) -> Vec<rmpv::Value> {
        (0..n)
            .map(|i| rmpv::Value::Integer(rmpv::Integer::from(i as u64)))
            .collect()
    }

    // -----------------------------------------------------------------------
    // DemandDispatcher tests
    // -----------------------------------------------------------------------

    #[test]
    fn demand_dispatcher_single_consumer() {
        let mut dispatcher = DemandDispatcher::new();
        let tag = SubscriptionTag::next();
        let mut subs = vec![ConsumerDemand {
            tag,
            pending_demand: 5,
        }];

        let result = dispatcher.dispatch(make_events(3), &mut subs);
        assert_eq!(result.deliveries.len(), 1);
        assert_eq!(result.deliveries[0].0, tag);
        assert_eq!(result.deliveries[0].1.len(), 3);
        assert!(result.leftover.is_empty());
        assert_eq!(subs[0].pending_demand, 2);
    }

    #[test]
    fn demand_dispatcher_distributes_by_demand() {
        let mut dispatcher = DemandDispatcher::new();
        let tag1 = SubscriptionTag::next();
        let tag2 = SubscriptionTag::next();
        let mut subs = vec![
            ConsumerDemand {
                tag: tag1,
                pending_demand: 3,
            },
            ConsumerDemand {
                tag: tag2,
                pending_demand: 3,
            },
        ];

        let result = dispatcher.dispatch(make_events(5), &mut subs);
        // First consumer gets 3 (its full demand), second gets 2
        assert_eq!(result.deliveries.len(), 2);
        assert_eq!(result.deliveries[0].1.len(), 3);
        assert_eq!(result.deliveries[1].1.len(), 2);
        assert!(result.leftover.is_empty());
    }

    #[test]
    fn demand_dispatcher_respects_zero_demand() {
        let mut dispatcher = DemandDispatcher::new();
        let tag = SubscriptionTag::next();
        let mut subs = vec![ConsumerDemand {
            tag,
            pending_demand: 0,
        }];

        let result = dispatcher.dispatch(make_events(5), &mut subs);
        assert!(result.deliveries.is_empty());
        assert_eq!(result.leftover.len(), 5);
    }

    #[test]
    fn demand_dispatcher_buffers_excess() {
        let mut dispatcher = DemandDispatcher::new();
        let tag = SubscriptionTag::next();
        let mut subs = vec![ConsumerDemand {
            tag,
            pending_demand: 2,
        }];

        let result = dispatcher.dispatch(make_events(5), &mut subs);
        assert_eq!(result.deliveries[0].1.len(), 2);
        assert_eq!(result.leftover.len(), 3);
    }

    #[test]
    fn demand_dispatcher_no_subscribers() {
        let mut dispatcher = DemandDispatcher::new();
        let mut subs: Vec<ConsumerDemand> = vec![];
        let result = dispatcher.dispatch(make_events(5), &mut subs);
        assert!(result.deliveries.is_empty());
        assert_eq!(result.leftover.len(), 5);
    }

    #[test]
    fn demand_dispatcher_empty_events() {
        let mut dispatcher = DemandDispatcher::new();
        let tag = SubscriptionTag::next();
        let mut subs = vec![ConsumerDemand {
            tag,
            pending_demand: 5,
        }];

        let result = dispatcher.dispatch(vec![], &mut subs);
        assert!(result.deliveries.is_empty());
        assert!(result.leftover.is_empty());
    }

    // -----------------------------------------------------------------------
    // BroadcastDispatcher tests
    // -----------------------------------------------------------------------

    #[test]
    fn broadcast_dispatcher_all_receive() {
        let mut dispatcher = BroadcastDispatcher::new();
        let tag1 = SubscriptionTag::next();
        let tag2 = SubscriptionTag::next();
        let mut subs = vec![
            ConsumerDemand {
                tag: tag1,
                pending_demand: 10,
            },
            ConsumerDemand {
                tag: tag2,
                pending_demand: 10,
            },
        ];

        let result = dispatcher.dispatch(make_events(3), &mut subs);
        assert_eq!(result.deliveries.len(), 2);
        assert_eq!(result.deliveries[0].1.len(), 3);
        assert_eq!(result.deliveries[1].1.len(), 3);
        assert!(result.leftover.is_empty());
    }

    #[test]
    fn broadcast_dispatcher_no_demand_buffers() {
        let mut dispatcher = BroadcastDispatcher::new();
        let tag = SubscriptionTag::next();
        let mut subs = vec![ConsumerDemand {
            tag,
            pending_demand: 0,
        }];

        let result = dispatcher.dispatch(make_events(3), &mut subs);
        assert!(result.deliveries.is_empty());
        assert_eq!(result.leftover.len(), 3);
    }

    #[test]
    fn broadcast_dispatcher_empty_events() {
        let mut dispatcher = BroadcastDispatcher::new();
        let tag = SubscriptionTag::next();
        let mut subs = vec![ConsumerDemand {
            tag,
            pending_demand: 5,
        }];

        let result = dispatcher.dispatch(vec![], &mut subs);
        assert!(result.deliveries.is_empty());
        assert!(result.leftover.is_empty());
    }

    #[test]
    fn broadcast_dispatcher_limits_by_demand() {
        let mut dispatcher = BroadcastDispatcher::new();
        let tag = SubscriptionTag::next();
        let mut subs = vec![ConsumerDemand {
            tag,
            pending_demand: 2,
        }];

        let result = dispatcher.dispatch(make_events(5), &mut subs);
        assert_eq!(result.deliveries.len(), 1);
        // Gets min(5, 2) = 2 events
        assert_eq!(result.deliveries[0].1.len(), 2);
    }
}
