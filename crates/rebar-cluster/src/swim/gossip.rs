use std::collections::HashMap;
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GossipUpdate {
    Alive {
        node_id: u64,
        addr: SocketAddr,
        incarnation: u64,
        cert_hash: Option<[u8; 32]>,
    },
    Suspect {
        node_id: u64,
        addr: SocketAddr,
        incarnation: u64,
    },
    Dead {
        node_id: u64,
        addr: SocketAddr,
        incarnation: u64,
    },
    Leave {
        node_id: u64,
        addr: SocketAddr,
        incarnation: u64,
    },
}

impl GossipUpdate {
    /// The node this update is about.
    #[must_use]
    pub const fn node_id(&self) -> u64 {
        match self {
            Self::Alive { node_id, .. }
            | Self::Suspect { node_id, .. }
            | Self::Dead { node_id, .. }
            | Self::Leave { node_id, .. } => *node_id,
        }
    }

    /// The incarnation this update carries.
    #[must_use]
    pub const fn incarnation(&self) -> u64 {
        match self {
            Self::Alive { incarnation, .. }
            | Self::Suspect { incarnation, .. }
            | Self::Dead { incarnation, .. }
            | Self::Leave { incarnation, .. } => *incarnation,
        }
    }

    /// Precedence rank for coalescing same-node updates at equal incarnation.
    /// Dead/Leave override Suspect override Alive (a death rumour should win a
    /// tie so it is not lost), matching SWIM state-merge precedence.
    const fn precedence(&self) -> u8 {
        match self {
            Self::Alive { .. } => 0,
            Self::Suspect { .. } => 1,
            Self::Dead { .. } | Self::Leave { .. } => 2,
        }
    }

    /// Whether `self` should replace `other` when coalescing updates about the
    /// same node. Newest (highest incarnation) wins; ties broken by precedence.
    #[must_use]
    fn supersedes(&self, other: &Self) -> bool {
        match self.incarnation().cmp(&other.incarnation()) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => self.precedence() >= other.precedence(),
        }
    }
}

/// A pending update plus its remaining retransmit budget.
struct PendingUpdate {
    update: GossipUpdate,
    /// Remaining times this update will be disseminated before it is dropped.
    retransmits_left: u32,
}

/// Default cap on the number of distinct nodes' updates buffered at once.
/// Prevents unbounded growth under churn.
const DEFAULT_MAX_ENTRIES: usize = 1024;

/// A bounded, coalescing gossip queue.
///
/// At most one pending update is kept per `node_id` (newest/highest-precedence
/// wins). Each update carries a retransmit budget so it disseminates roughly
/// `log(n)` times and is then dropped, giving epidemic spread without
/// unbounded growth.
pub struct GossipQueue {
    /// One pending update per `node_id`.
    pending: HashMap<u64, PendingUpdate>,
    /// Cap on distinct entries; oldest-by-budget evicted when exceeded.
    max_entries: usize,
    /// Retransmit budget assigned to each newly-added update.
    retransmit_budget: u32,
}

impl Default for GossipQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl GossipQueue {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            max_entries: DEFAULT_MAX_ENTRIES,
            retransmit_budget: 4,
        }
    }

    /// Create a queue sized for a cluster of `cluster_size` members. The
    /// retransmit budget scales as `~log2(n) + 1` so each update spreads
    /// epidemically then drops.
    #[must_use]
    pub fn with_cluster_size(cluster_size: usize, max_entries: usize) -> Self {
        let budget = (usize::BITS - cluster_size.max(1).leading_zeros()).max(1);
        Self {
            pending: HashMap::new(),
            max_entries: max_entries.max(1),
            retransmit_budget: budget,
        }
    }

    /// Number of distinct pending updates.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Add (or coalesce) an update. At most one update per node is retained;
    /// the newest/highest-precedence one wins, and its retransmit budget is
    /// reset so fresh information spreads fully.
    pub fn add(&mut self, update: GossipUpdate) {
        let node_id = update.node_id();
        let budget = self.retransmit_budget;
        match self.pending.get_mut(&node_id) {
            Some(existing) if !update.supersedes(&existing.update) => {
                // Keep the existing (fresher) update untouched.
            }
            Some(existing) => {
                existing.update = update;
                existing.retransmits_left = budget;
            }
            None => {
                if self.pending.len() >= self.max_entries {
                    self.evict_one();
                }
                self.pending.insert(
                    node_id,
                    PendingUpdate {
                        update,
                        retransmits_left: budget,
                    },
                );
            }
        }
    }

    /// Evict the entry with the smallest remaining retransmit budget (closest
    /// to being dropped anyway), keeping the queue bounded.
    fn evict_one(&mut self) {
        if let Some(&victim) = self
            .pending
            .iter()
            .min_by_key(|(_, p)| p.retransmits_left)
            .map(|(id, _)| id)
        {
            self.pending.remove(&victim);
        }
    }

    /// Drain up to `max` updates for piggy-backing on this tick. Each returned
    /// update has its retransmit budget decremented; entries whose budget is
    /// exhausted are dropped. Highest remaining budget is sent first so fresh
    /// updates get priority.
    pub fn drain(&mut self, max: usize) -> Vec<GossipUpdate> {
        let mut ids: Vec<u64> = self.pending.keys().copied().collect();
        ids.sort_by_key(|id| std::cmp::Reverse(self.pending[id].retransmits_left));
        ids.truncate(max);

        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(entry) = self.pending.get_mut(&id) {
                out.push(entry.update.clone());
                entry.retransmits_left = entry.retransmits_left.saturating_sub(1);
                if entry.retransmits_left == 0 {
                    self.pending.remove(&id);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_addr(port: u16) -> SocketAddr {
        format!("127.0.0.1:{port}").parse().unwrap()
    }

    // 1. add_update_to_queue
    #[test]
    fn add_update_to_queue() {
        let mut q = GossipQueue::new();
        q.add(GossipUpdate::Alive {
            node_id: 1,
            addr: test_addr(4000),
            incarnation: 0,
            cert_hash: None,
        });
        let items = q.drain(10);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0],
            GossipUpdate::Alive {
                node_id: 1,
                addr: test_addr(4000),
                incarnation: 0,
                cert_hash: None,
            }
        );
    }

    // 2. drain_returns_bounded_count
    #[test]
    fn drain_returns_bounded_count() {
        let mut q = GossipQueue::new();
        for i in 0..5 {
            q.add(GossipUpdate::Alive {
                node_id: i,
                addr: test_addr(4000 + u16::try_from(i).unwrap()),
                incarnation: 0,
                cert_hash: None,
            });
        }
        // drain(3) returns at most 3 updates per tick.
        let items = q.drain(3);
        assert_eq!(items.len(), 3);
        // All 5 nodes still have retransmit budget left, so they remain
        // pending (epidemic re-dissemination).
        assert_eq!(q.len(), 5);
    }

    // 3. drain_more_than_available_returns_all
    #[test]
    fn drain_more_than_available_returns_all() {
        let mut q = GossipQueue::new();
        q.add(GossipUpdate::Alive {
            node_id: 1,
            addr: test_addr(4000),
            incarnation: 0,
            cert_hash: None,
        });
        q.add(GossipUpdate::Dead {
            node_id: 2,
            addr: test_addr(4001),
            incarnation: 0,
        });
        let items = q.drain(100);
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn coalesces_to_one_update_per_node() {
        let mut q = GossipQueue::new();
        q.add(GossipUpdate::Alive {
            node_id: 1,
            addr: test_addr(4000),
            incarnation: 1,
            cert_hash: None,
        });
        // A fresher Suspect about the same node coalesces, replacing the Alive.
        q.add(GossipUpdate::Suspect {
            node_id: 1,
            addr: test_addr(4000),
            incarnation: 2,
        });
        assert_eq!(q.len(), 1);
        let items = q.drain(10);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].incarnation(), 2);
        assert!(matches!(items[0], GossipUpdate::Suspect { .. }));
    }

    #[test]
    fn stale_update_does_not_replace_fresher() {
        let mut q = GossipQueue::new();
        q.add(GossipUpdate::Alive {
            node_id: 1,
            addr: test_addr(4000),
            incarnation: 5,
            cert_hash: None,
        });
        // A stale Suspect at a lower incarnation must not displace the Alive.
        q.add(GossipUpdate::Suspect {
            node_id: 1,
            addr: test_addr(4000),
            incarnation: 2,
        });
        let items = q.drain(10);
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0], GossipUpdate::Alive { .. }));
        assert_eq!(items[0].incarnation(), 5);
    }

    #[test]
    fn retransmit_budget_drops_after_exhaustion() {
        // cluster size 2 -> budget = floor(log2(2)) + 1 = 2 retransmits.
        let mut q = GossipQueue::with_cluster_size(2, 1024);
        q.add(GossipUpdate::Alive {
            node_id: 1,
            addr: test_addr(4000),
            incarnation: 0,
            cert_hash: None,
        });
        assert_eq!(q.drain(10).len(), 1);
        assert_eq!(q.drain(10).len(), 1);
        // Budget exhausted: dropped.
        assert_eq!(q.drain(10).len(), 0);
        assert!(q.is_empty());
    }

    #[test]
    fn queue_is_bounded_by_max_entries() {
        let mut q = GossipQueue::with_cluster_size(64, 4);
        for i in 0..100u64 {
            q.add(GossipUpdate::Alive {
                node_id: i,
                addr: test_addr(4000),
                incarnation: 0,
                cert_hash: None,
            });
        }
        assert!(q.len() <= 4, "queue must stay bounded, got {}", q.len());
    }

    // 4. drain_empty_queue
    #[test]
    fn drain_empty_queue() {
        let mut q = GossipQueue::new();
        let items = q.drain(10);
        assert!(items.is_empty());
    }

    // 5. gossip_alive_serialization_roundtrip
    #[test]
    fn gossip_alive_serialization_roundtrip() {
        let update = GossipUpdate::Alive {
            node_id: 42,
            addr: test_addr(5000),
            incarnation: 7,
            cert_hash: None,
        };
        let bytes = rmp_serde::to_vec(&update).unwrap();
        let decoded: GossipUpdate = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(update, decoded);
    }

    // 6. gossip_suspect_serialization_roundtrip
    #[test]
    fn gossip_suspect_serialization_roundtrip() {
        let update = GossipUpdate::Suspect {
            node_id: 10,
            addr: test_addr(6000),
            incarnation: 3,
        };
        let bytes = rmp_serde::to_vec(&update).unwrap();
        let decoded: GossipUpdate = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(update, decoded);
    }

    // 7. gossip_dead_serialization_roundtrip
    #[test]
    fn gossip_dead_serialization_roundtrip() {
        let update = GossipUpdate::Dead {
            node_id: 99,
            addr: test_addr(7000),
            incarnation: 4,
        };
        let bytes = rmp_serde::to_vec(&update).unwrap();
        let decoded: GossipUpdate = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(update, decoded);
    }

    // 8. gossip_leave_serialization_roundtrip
    #[test]
    fn gossip_leave_serialization_roundtrip() {
        let update = GossipUpdate::Leave {
            node_id: 55,
            addr: test_addr(8000),
            incarnation: 2,
        };
        let bytes = rmp_serde::to_vec(&update).unwrap();
        let decoded: GossipUpdate = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(update, decoded);
    }

    // 9. distinct-node updates are all retained and drained
    #[test]
    fn distinct_nodes_all_drained() {
        let mut q = GossipQueue::new();
        q.add(GossipUpdate::Alive {
            node_id: 1,
            addr: test_addr(4001),
            incarnation: 0,
            cert_hash: None,
        });
        q.add(GossipUpdate::Suspect {
            node_id: 2,
            addr: test_addr(4002),
            incarnation: 1,
        });
        q.add(GossipUpdate::Dead {
            node_id: 3,
            addr: test_addr(4003),
            incarnation: 1,
        });
        let items = q.drain(3);
        assert_eq!(items.len(), 3);
        let nodes: std::collections::HashSet<u64> = items.iter().map(GossipUpdate::node_id).collect();
        assert_eq!(nodes, [1, 2, 3].into_iter().collect());
    }

    #[test]
    fn dead_carries_incarnation() {
        let update = GossipUpdate::Dead {
            node_id: 7,
            addr: test_addr(4007),
            incarnation: 9,
        };
        assert_eq!(update.incarnation(), 9);
        assert_eq!(update.node_id(), 7);
    }

    // 10. gossip_addr_preserved_in_roundtrip
    #[test]
    fn gossip_addr_preserved_in_roundtrip() {
        let addr: SocketAddr = "192.168.1.100:9999".parse().unwrap();
        let update = GossipUpdate::Alive {
            node_id: 1,
            addr,
            incarnation: 0,
            cert_hash: None,
        };
        let bytes = rmp_serde::to_vec(&update).unwrap();
        let decoded: GossipUpdate = rmp_serde::from_slice(&bytes).unwrap();
        if let GossipUpdate::Alive {
            addr: decoded_addr, ..
        } = decoded
        {
            assert_eq!(decoded_addr, addr);
            assert_eq!(decoded_addr.ip().to_string(), "192.168.1.100");
            assert_eq!(decoded_addr.port(), 9999);
        } else {
            panic!("Expected Alive variant");
        }
    }

    #[test]
    fn gossip_alive_with_cert_hash_roundtrip() {
        let hash = [0xABu8; 32];
        let update = GossipUpdate::Alive {
            node_id: 1,
            addr: test_addr(4000),
            incarnation: 0,
            cert_hash: Some(hash),
        };
        let bytes = rmp_serde::to_vec(&update).unwrap();
        let decoded: GossipUpdate = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(update, decoded);
        if let GossipUpdate::Alive { cert_hash, .. } = decoded {
            assert_eq!(cert_hash, Some(hash));
        }
    }
}
