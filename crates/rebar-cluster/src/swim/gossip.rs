use std::collections::VecDeque;
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    },
    Leave {
        node_id: u64,
        addr: SocketAddr,
    },
}

pub struct GossipQueue {
    queue: VecDeque<GossipUpdate>,
}

impl GossipQueue {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }

    pub fn add(&mut self, update: GossipUpdate) {
        self.queue.push_back(update);
    }

    pub fn drain(&mut self, max: usize) -> Vec<GossipUpdate> {
        let count = max.min(self.queue.len());
        self.queue.drain(..count).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_addr(port: u16) -> SocketAddr {
        format!("127.0.0.1:{}", port).parse().unwrap()
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
                addr: test_addr(4000 + i as u16),
                incarnation: 0,
                cert_hash: None,
            });
        }
        let items = q.drain(3);
        assert_eq!(items.len(), 3);
        // Remaining items should still be in the queue
        let rest = q.drain(10);
        assert_eq!(rest.len(), 2);
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
        });
        let items = q.drain(100);
        assert_eq!(items.len(), 2);
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
        };
        let bytes = rmp_serde::to_vec(&update).unwrap();
        let decoded: GossipUpdate = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(update, decoded);
    }

    // 9. gossip_queue_fifo_order
    #[test]
    fn gossip_queue_fifo_order() {
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
        });
        let items = q.drain(3);
        assert_eq!(
            items[0],
            GossipUpdate::Alive {
                node_id: 1,
                addr: test_addr(4001),
                incarnation: 0,
                cert_hash: None,
            }
        );
        assert_eq!(
            items[1],
            GossipUpdate::Suspect {
                node_id: 2,
                addr: test_addr(4002),
                incarnation: 1,
            }
        );
        assert_eq!(
            items[2],
            GossipUpdate::Dead {
                node_id: 3,
                addr: test_addr(4003),
            }
        );
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
