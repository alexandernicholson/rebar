use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::registry::Registry;
use crate::swim::gossip::{GossipQueue, GossipUpdate};

/// Configuration for the three-phase drain protocol.
#[derive(Debug, Clone)]
pub struct DrainConfig {
    /// Time to propagate Leave gossip (phase 1).
    pub announce_timeout: Duration,
    /// Time to wait for in-flight messages (phase 2).
    pub drain_timeout: Duration,
    /// Time for supervisor shutdown (phase 3).
    pub shutdown_timeout: Duration,
}

impl Default for DrainConfig {
    fn default() -> Self {
        Self {
            announce_timeout: Duration::from_secs(5),
            drain_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(10),
        }
    }
}

/// Result of a completed drain operation.
#[derive(Debug)]
pub struct DrainResult {
    /// Number of processes stopped during shutdown.
    pub processes_stopped: usize,
    /// Number of outbound messages successfully delivered during the drain.
    pub messages_drained: usize,
    /// Number of outbound messages that could NOT be delivered (route error)
    /// or were still pending when the drain timed out. Non-zero means data was
    /// potentially lost; the caller should treat the drain as unclean.
    pub messages_undrained: usize,
    /// Duration of each phase: [drain, announce, shutdown].
    pub phase_durations: [Duration; 3],
    /// Whether any phase hit its timeout (an unclean drain).
    pub timed_out: bool,
}

impl DrainResult {
    /// Whether the drain completed cleanly: no timeout and nothing undrained.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        !self.timed_out && self.messages_undrained == 0
    }
}

/// Outcome of draining the outbound queue.
#[derive(Debug)]
pub struct OutboundDrainOutcome {
    /// Messages successfully delivered.
    pub delivered: usize,
    /// Messages that failed to route or were left pending at timeout.
    pub undrained: usize,
    /// Whether the drain hit its timeout instead of reaching a clean empty.
    pub timed_out: bool,
}

/// Mutable handles to the cluster components a drain operates on.
pub struct DrainContext<'a> {
    /// Gossip queue used to broadcast the Leave announcement.
    pub gossip: &'a mut GossipQueue,
    /// Registry to unregister this node's names from.
    pub registry: &'a mut Registry,
    /// Channel of outbound router commands to drain.
    pub remote_rx: &'a mut tokio::sync::mpsc::Receiver<crate::router::RouterCommand>,
    /// Connection manager used to flush and close connections.
    pub connection_manager: &'a mut crate::connection::manager::ConnectionManager,
}

/// Orchestrates the three-phase drain protocol.
pub struct NodeDrain {
    config: DrainConfig,
}

impl NodeDrain {
    #[must_use]
    pub const fn new(config: DrainConfig) -> Self {
        Self { config }
    }

    /// Phase 1: Announce departure to the cluster.
    /// - Broadcasts Leave via SWIM gossip
    /// - Unregisters all names from the registry
    ///
    /// Returns the number of names unregistered.
    ///
    /// The `incarnation` is this node's current SWIM incarnation; it is carried
    /// on the Leave so a replayed stale Leave cannot evict a rejoined node.
    pub fn announce(
        &self,
        node_id: u64,
        addr: SocketAddr,
        incarnation: u64,
        gossip: &mut GossipQueue,
        registry: &mut Registry,
    ) -> usize {
        gossip.add(GossipUpdate::Leave {
            node_id,
            addr,
            incarnation,
        });

        let names_before = registry.registered().len();
        registry.remove_by_node(node_id);
        let names_after = registry.registered().len();

        names_before - names_after
    }

    /// Drain in-flight outbound messages.
    ///
    /// Processes `RouterCommand`s from the channel until the channel is closed
    /// (`recv` returns `None` — the real "no more producers" signal) or the
    /// drain timeout elapses. A route error does NOT count as drained; the
    /// message is counted as undrained (potentially lost). On timeout, any
    /// messages still buffered in the channel are counted as undrained and the
    /// outcome is flagged `timed_out`.
    ///
    /// This must be run BEFORE announcing Leave / unregistering, so producers
    /// have stopped enqueuing only because the upstream is shutting down, not
    /// because we silently dropped them.
    pub async fn drain_outbound(
        &self,
        remote_rx: &mut tokio::sync::mpsc::Receiver<crate::router::RouterCommand>,
        connection_manager: &mut crate::connection::manager::ConnectionManager,
    ) -> OutboundDrainOutcome {
        let start = Instant::now();
        let mut delivered = 0;
        let mut undrained = 0;
        let mut timed_out = false;

        loop {
            let remaining = self
                .config
                .drain_timeout
                .checked_sub(start.elapsed())
                .unwrap_or(Duration::ZERO);
            if remaining.is_zero() {
                timed_out = true;
                break;
            }

            match tokio::time::timeout(remaining, remote_rx.recv()).await {
                Ok(Some(crate::router::RouterCommand::Send { node_id, frame })) => {
                    match connection_manager.route(node_id, &frame).await {
                        Ok(()) => delivered += 1,
                        // A route error means the message was NOT delivered;
                        // count it as undrained rather than as drained.
                        Err(_) => undrained += 1,
                    }
                }
                // Channel closed: all producers gone, queue truly empty. This is
                // the only clean termination.
                Ok(None) => break,
                Err(_) => {
                    timed_out = true;
                    break;
                }
            }
        }

        // On timeout, account for whatever is still buffered as undrained.
        if timed_out {
            while remote_rx.try_recv().is_ok() {
                undrained += 1;
            }
        }

        OutboundDrainOutcome {
            delivered,
            undrained,
            timed_out,
        }
    }

    /// Execute the full three-phase drain protocol.
    pub async fn drain(
        &self,
        node_id: u64,
        addr: SocketAddr,
        incarnation: u64,
        ctx: DrainContext<'_>,
        process_count: usize,
    ) -> DrainResult {
        let mut phase_durations = [Duration::ZERO; 3];
        let mut timed_out = false;

        // Phase 1: Drain outbound to empty FIRST, while we are still a full
        // cluster member. In-flight replies/messages must be delivered before
        // we announce departure or unregister — otherwise they are lost.
        let phase1_start = Instant::now();
        let outcome = self
            .drain_outbound(ctx.remote_rx, ctx.connection_manager)
            .await;
        phase_durations[0] = phase1_start.elapsed();
        if outcome.timed_out {
            timed_out = true;
        }

        // Phase 2: Announce departure (gossip Leave + unregister names) only
        // after the outbound queue has been drained.
        let phase2_start = Instant::now();
        let _names_removed =
            self.announce(node_id, addr, incarnation, ctx.gossip, ctx.registry);
        phase_durations[1] = phase2_start.elapsed();

        // Phase 3: Shutdown connections
        let phase3_start = Instant::now();
        let _connections_closed = ctx.connection_manager.drain_connections().await;
        phase_durations[2] = phase3_start.elapsed();

        DrainResult {
            processes_stopped: process_count,
            messages_drained: outcome.delivered,
            messages_undrained: outcome.undrained,
            phase_durations,
            timed_out,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Registry;
    use crate::swim::gossip::{GossipQueue, GossipUpdate};
    use rebar_core::process::ProcessId;
    use std::net::SocketAddr;

    fn test_addr() -> SocketAddr {
        "127.0.0.1:4000".parse().unwrap()
    }

    #[test]
    fn drain_config_defaults() {
        let config = DrainConfig::default();
        assert_eq!(config.announce_timeout, Duration::from_secs(5));
        assert_eq!(config.drain_timeout, Duration::from_secs(30));
        assert_eq!(config.shutdown_timeout, Duration::from_secs(10));
    }

    #[test]
    fn drain_config_custom() {
        let config = DrainConfig {
            announce_timeout: Duration::from_secs(1),
            drain_timeout: Duration::from_secs(10),
            shutdown_timeout: Duration::from_secs(5),
        };
        assert_eq!(config.announce_timeout, Duration::from_secs(1));
    }

    #[test]
    fn drain_result_fields() {
        let result = DrainResult {
            processes_stopped: 10,
            messages_drained: 50,
            messages_undrained: 0,
            phase_durations: [
                Duration::from_millis(100),
                Duration::from_millis(500),
                Duration::from_millis(200),
            ],
            timed_out: false,
        };
        assert_eq!(result.processes_stopped, 10);
        assert_eq!(result.messages_drained, 50);
        assert!(!result.timed_out);
    }

    #[test]
    fn drain_broadcasts_leave() {
        let drain = NodeDrain::new(DrainConfig::default());
        let mut gossip = GossipQueue::new();
        let mut registry = Registry::default();

        drain.announce(1, test_addr(), 0, &mut gossip, &mut registry);

        let updates = gossip.drain(10);
        assert_eq!(updates.len(), 1);
        assert!(matches!(updates[0], GossipUpdate::Leave { node_id: 1, .. }));
    }

    #[test]
    fn drain_unregisters_names() {
        let drain = NodeDrain::new(DrainConfig::default());
        let mut gossip = GossipQueue::new();
        let mut registry = Registry::default();

        registry.register("service_a", ProcessId::new(1, 1), 1, 100);
        registry.register("service_b", ProcessId::new(1, 2), 1, 101);
        registry.register("service_c", ProcessId::new(2, 1), 2, 102);

        assert_eq!(registry.registered().len(), 3);

        let removed = drain.announce(1, test_addr(), 0, &mut gossip, &mut registry);

        assert_eq!(removed, 2);
        assert_eq!(registry.registered().len(), 1);
        assert!(registry.lookup("service_c").is_some());
        assert!(registry.lookup("service_a").is_none());
        assert!(registry.lookup("service_b").is_none());
    }

    #[tokio::test]
    async fn drain_waits_for_inflight() {
        use crate::connection::manager::ConnectionManager;
        use crate::protocol::{Frame, MsgType};
        use crate::router::RouterCommand;
        use crate::transport::{TransportConnection, TransportError};

        struct NullConn;
        #[async_trait::async_trait]
        impl TransportConnection for NullConn {
            async fn send(&mut self, _frame: &Frame) -> Result<(), TransportError> {
                Ok(())
            }
            async fn recv(&mut self) -> Result<Frame, TransportError> {
                Err(TransportError::ConnectionClosed)
            }
            async fn close(&mut self) -> Result<(), TransportError> {
                Ok(())
            }
        }

        struct NullConnector;
        #[async_trait::async_trait]
        impl crate::connection::manager::TransportConnector for NullConnector {
            async fn connect(
                &self,
                _: SocketAddr,
            ) -> Result<Box<dyn TransportConnection>, TransportError> {
                Ok(Box::new(NullConn))
            }
        }

        let (tx, mut rx) = tokio::sync::mpsc::channel::<RouterCommand>(64);
        let mut mgr = ConnectionManager::new(Box::new(NullConnector));
        mgr.connect(2, test_addr()).await.unwrap();

        for i in 0..3 {
            tx.send(RouterCommand::Send {
                node_id: 2,
                frame: Frame {
                    version: 1,
                    msg_type: MsgType::Send,
                    request_id: i,
                    header: rmpv::Value::Nil,
                    payload: rmpv::Value::Nil,
                },
            })
            .await
            .unwrap();
        }
        drop(tx);

        let drain = NodeDrain::new(DrainConfig {
            drain_timeout: Duration::from_secs(5),
            ..DrainConfig::default()
        });

        let outcome = drain.drain_outbound(&mut rx, &mut mgr).await;
        assert_eq!(outcome.delivered, 3);
        assert_eq!(outcome.undrained, 0);
        assert!(!outcome.timed_out);
    }

    #[tokio::test]
    async fn route_error_counts_as_undrained_not_drained() {
        use crate::connection::manager::ConnectionManager;
        use crate::protocol::{Frame, MsgType};
        use crate::router::RouterCommand;
        use crate::transport::{TransportConnection, TransportError};

        struct NullConn;
        #[async_trait::async_trait]
        impl TransportConnection for NullConn {
            async fn send(&mut self, _: &Frame) -> Result<(), TransportError> {
                Ok(())
            }
            async fn recv(&mut self) -> Result<Frame, TransportError> {
                Err(TransportError::ConnectionClosed)
            }
            async fn close(&mut self) -> Result<(), TransportError> {
                Ok(())
            }
        }
        struct NullConnector;
        #[async_trait::async_trait]
        impl crate::connection::manager::TransportConnector for NullConnector {
            async fn connect(
                &self,
                _: SocketAddr,
            ) -> Result<Box<dyn TransportConnection>, TransportError> {
                Ok(Box::new(NullConn))
            }
        }

        let (tx, mut rx) = tokio::sync::mpsc::channel::<RouterCommand>(64);
        // No connection registered for node 99 -> route() returns UnknownNode.
        let mut mgr = ConnectionManager::new(Box::new(NullConnector));

        tx.send(RouterCommand::Send {
            node_id: 99,
            frame: Frame {
                version: 1,
                msg_type: MsgType::Send,
                request_id: 0,
                header: rmpv::Value::Nil,
                payload: rmpv::Value::Nil,
            },
        })
        .await
        .unwrap();
        drop(tx);

        let drain = NodeDrain::new(DrainConfig {
            drain_timeout: Duration::from_secs(1),
            ..DrainConfig::default()
        });
        let outcome = drain.drain_outbound(&mut rx, &mut mgr).await;
        assert_eq!(outcome.delivered, 0, "failed route must not count as drained");
        assert_eq!(outcome.undrained, 1);
        assert!(!outcome.timed_out);
    }

    #[tokio::test]
    async fn timeout_reports_undrained_count() {
        use crate::connection::manager::ConnectionManager;
        use crate::protocol::{Frame, MsgType};
        use crate::router::RouterCommand;
        use crate::transport::{TransportConnection, TransportError};

        struct StallConn;
        #[async_trait::async_trait]
        impl TransportConnection for StallConn {
            async fn send(&mut self, _: &Frame) -> Result<(), TransportError> {
                Ok(())
            }
            async fn recv(&mut self) -> Result<Frame, TransportError> {
                Err(TransportError::ConnectionClosed)
            }
            async fn close(&mut self) -> Result<(), TransportError> {
                Ok(())
            }
        }
        struct StallConnector;
        #[async_trait::async_trait]
        impl crate::connection::manager::TransportConnector for StallConnector {
            async fn connect(
                &self,
                _: SocketAddr,
            ) -> Result<Box<dyn TransportConnection>, TransportError> {
                Ok(Box::new(StallConn))
            }
        }

        let (tx, mut rx) = tokio::sync::mpsc::channel::<RouterCommand>(64);
        let mut mgr = ConnectionManager::new(Box::new(StallConnector));

        // Two messages to an unconnected node -> route() fails (undrained). The
        // sender is kept alive so the channel never closes and the drain must
        // time out; nothing is silently dropped.
        for i in 0..2 {
            tx.send(RouterCommand::Send {
                node_id: 99,
                frame: Frame {
                    version: 1,
                    msg_type: MsgType::Send,
                    request_id: i,
                    header: rmpv::Value::Nil,
                    payload: rmpv::Value::Nil,
                },
            })
            .await
            .unwrap();
        }

        let drain = NodeDrain::new(DrainConfig {
            drain_timeout: Duration::from_millis(50),
            ..DrainConfig::default()
        });
        let outcome = drain.drain_outbound(&mut rx, &mut mgr).await;
        assert!(outcome.timed_out, "open channel with no close must time out");
        // Nothing delivered (unknown node); both counted as undrained, not lost.
        assert_eq!(outcome.delivered, 0);
        assert_eq!(outcome.undrained, 2);
        drop(tx);
    }

    #[tokio::test]
    async fn drain_respects_timeout() {
        use crate::connection::manager::ConnectionManager;
        use crate::router::RouterCommand;
        use crate::transport::{TransportConnection, TransportError};

        struct NullConn;
        #[async_trait::async_trait]
        impl TransportConnection for NullConn {
            async fn send(&mut self, _: &crate::protocol::Frame) -> Result<(), TransportError> {
                Ok(())
            }
            async fn recv(&mut self) -> Result<crate::protocol::Frame, TransportError> {
                Err(TransportError::ConnectionClosed)
            }
            async fn close(&mut self) -> Result<(), TransportError> {
                Ok(())
            }
        }

        struct NullConnector;
        #[async_trait::async_trait]
        impl crate::connection::manager::TransportConnector for NullConnector {
            async fn connect(
                &self,
                _: SocketAddr,
            ) -> Result<Box<dyn TransportConnection>, TransportError> {
                Ok(Box::new(NullConn))
            }
        }

        let (tx, mut rx) = tokio::sync::mpsc::channel::<RouterCommand>(64);
        let mut mgr = ConnectionManager::new(Box::new(NullConnector));

        let drain = NodeDrain::new(DrainConfig {
            drain_timeout: Duration::from_millis(100),
            ..DrainConfig::default()
        });

        let start = Instant::now();
        let outcome = drain.drain_outbound(&mut rx, &mut mgr).await;
        let elapsed = start.elapsed();

        assert_eq!(outcome.delivered, 0);
        assert!(outcome.timed_out);
        assert!(elapsed >= Duration::from_millis(100));
        assert!(elapsed < Duration::from_secs(1));

        drop(tx);
    }

    #[tokio::test]
    async fn full_drain_protocol() {
        use crate::connection::manager::ConnectionManager;
        use crate::protocol::{Frame, MsgType};
        use crate::router::RouterCommand;
        use crate::transport::{TransportConnection, TransportError};

        struct NullConn;
        #[async_trait::async_trait]
        impl TransportConnection for NullConn {
            async fn send(&mut self, _: &Frame) -> Result<(), TransportError> {
                Ok(())
            }
            async fn recv(&mut self) -> Result<Frame, TransportError> {
                Err(TransportError::ConnectionClosed)
            }
            async fn close(&mut self) -> Result<(), TransportError> {
                Ok(())
            }
        }

        struct NullConnector;
        #[async_trait::async_trait]
        impl crate::connection::manager::TransportConnector for NullConnector {
            async fn connect(
                &self,
                _: SocketAddr,
            ) -> Result<Box<dyn TransportConnection>, TransportError> {
                Ok(Box::new(NullConn))
            }
        }

        let mut gossip = GossipQueue::new();
        let mut registry = Registry::new();
        registry.register("svc", ProcessId::new(1, 1), 1, 100);

        let (tx, mut rx) = tokio::sync::mpsc::channel::<RouterCommand>(64);
        let mut mgr = ConnectionManager::new(Box::new(NullConnector));
        mgr.connect(2, test_addr()).await.unwrap();

        tx.send(RouterCommand::Send {
            node_id: 2,
            frame: Frame {
                version: 1,
                msg_type: MsgType::Send,
                request_id: 0,
                header: rmpv::Value::Nil,
                payload: rmpv::Value::Nil,
            },
        })
        .await
        .unwrap();
        drop(tx);

        let drain = NodeDrain::new(DrainConfig {
            announce_timeout: Duration::from_millis(100),
            drain_timeout: Duration::from_secs(1),
            shutdown_timeout: Duration::from_millis(100),
        });

        let result = drain
            .drain(
                1,
                test_addr(),
                0,
                DrainContext {
                    gossip: &mut gossip,
                    registry: &mut registry,
                    remote_rx: &mut rx,
                    connection_manager: &mut mgr,
                },
                5,
            )
            .await;

        assert_eq!(result.messages_drained, 1);
        assert_eq!(result.messages_undrained, 0);
        assert_eq!(result.processes_stopped, 5);
        assert!(!result.timed_out);
        assert!(result.is_clean());
        assert!(registry.lookup("svc").is_none());

        let updates = gossip.drain(10);
        assert!(
            updates
                .iter()
                .any(|u| matches!(u, GossipUpdate::Leave { node_id: 1, .. }))
        );
        assert_eq!(mgr.connection_count(), 0);
    }

    #[tokio::test]
    async fn drain_returns_stats() {
        use crate::connection::manager::ConnectionManager;
        use crate::router::RouterCommand;
        use crate::transport::{TransportConnection, TransportError};

        struct NullConn;
        #[async_trait::async_trait]
        impl TransportConnection for NullConn {
            async fn send(&mut self, _: &crate::protocol::Frame) -> Result<(), TransportError> {
                Ok(())
            }
            async fn recv(&mut self) -> Result<crate::protocol::Frame, TransportError> {
                Err(TransportError::ConnectionClosed)
            }
            async fn close(&mut self) -> Result<(), TransportError> {
                Ok(())
            }
        }

        struct NullConnector;
        #[async_trait::async_trait]
        impl crate::connection::manager::TransportConnector for NullConnector {
            async fn connect(
                &self,
                _: SocketAddr,
            ) -> Result<Box<dyn TransportConnection>, TransportError> {
                Ok(Box::new(NullConn))
            }
        }

        let mut gossip = GossipQueue::new();
        let mut registry = Registry::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<RouterCommand>(64);
        drop(tx);
        let mut mgr = ConnectionManager::new(Box::new(NullConnector));

        let drain = NodeDrain::new(DrainConfig::default());
        let result = drain
            .drain(
                1,
                test_addr(),
                0,
                DrainContext {
                    gossip: &mut gossip,
                    registry: &mut registry,
                    remote_rx: &mut rx,
                    connection_manager: &mut mgr,
                },
                0,
            )
            .await;

        for d in &result.phase_durations {
            assert!(*d >= Duration::ZERO);
        }
        assert_eq!(result.messages_drained, 0);
        assert_eq!(result.processes_stopped, 0);
        assert!(!result.timed_out);
    }
}
