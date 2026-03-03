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
    /// Number of outbound messages drained.
    pub messages_drained: usize,
    /// Duration of each phase: [announce, drain, shutdown].
    pub phase_durations: [Duration; 3],
    /// Whether any phase hit its timeout.
    pub timed_out: bool,
}

/// Orchestrates the three-phase drain protocol.
pub struct NodeDrain {
    config: DrainConfig,
}

impl NodeDrain {
    pub fn new(config: DrainConfig) -> Self {
        Self { config }
    }

    /// Phase 1: Announce departure to the cluster.
    /// - Broadcasts Leave via SWIM gossip
    /// - Unregisters all names from the registry
    /// Returns the number of names unregistered.
    pub fn announce(
        &self,
        node_id: u64,
        addr: SocketAddr,
        gossip: &mut GossipQueue,
        registry: &mut Registry,
    ) -> usize {
        gossip.add(GossipUpdate::Leave { node_id, addr });

        let names_before = registry.registered().len();
        registry.remove_by_node(node_id);
        let names_after = registry.registered().len();

        names_before - names_after
    }

    /// Phase 2: Drain in-flight outbound messages.
    /// Processes RouterCommands from the channel until empty or timeout.
    /// Returns (count, timed_out).
    pub async fn drain_outbound(
        &self,
        remote_rx: &mut tokio::sync::mpsc::Receiver<crate::router::RouterCommand>,
        connection_manager: &mut crate::connection::manager::ConnectionManager,
    ) -> (usize, bool) {
        let start = Instant::now();
        let mut drained = 0;
        let mut timed_out = false;

        loop {
            if start.elapsed() >= self.config.drain_timeout {
                timed_out = true;
                break;
            }

            let remaining = self.config.drain_timeout - start.elapsed();

            match tokio::time::timeout(remaining, remote_rx.recv()).await {
                Ok(Some(crate::router::RouterCommand::Send { node_id, frame })) => {
                    let _ = connection_manager.route(node_id, &frame).await;
                    drained += 1;
                }
                Ok(None) => break,
                Err(_) => {
                    timed_out = true;
                    break;
                }
            }
        }

        (drained, timed_out)
    }

    /// Execute the full three-phase drain protocol.
    pub async fn drain(
        &self,
        node_id: u64,
        addr: SocketAddr,
        gossip: &mut GossipQueue,
        registry: &mut Registry,
        remote_rx: &mut tokio::sync::mpsc::Receiver<crate::router::RouterCommand>,
        connection_manager: &mut crate::connection::manager::ConnectionManager,
        process_count: usize,
    ) -> DrainResult {
        let mut phase_durations = [Duration::ZERO; 3];
        let mut timed_out = false;

        // Phase 1: Announce
        let phase1_start = Instant::now();
        let _names_removed = self.announce(node_id, addr, gossip, registry);
        phase_durations[0] = phase1_start.elapsed();

        // Phase 2: Drain outbound
        let phase2_start = Instant::now();
        let (messages_drained, phase2_timed_out) =
            self.drain_outbound(remote_rx, connection_manager).await;
        phase_durations[1] = phase2_start.elapsed();
        if phase2_timed_out {
            timed_out = true;
        }

        // Phase 3: Shutdown connections
        let phase3_start = Instant::now();
        let _connections_closed = connection_manager.drain_connections().await;
        phase_durations[2] = phase3_start.elapsed();

        DrainResult {
            processes_stopped: process_count,
            messages_drained,
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

        drain.announce(1, test_addr(), &mut gossip, &mut registry);

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

        let removed = drain.announce(1, test_addr(), &mut gossip, &mut registry);

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

        let (count, timed_out) = drain.drain_outbound(&mut rx, &mut mgr).await;
        assert_eq!(count, 3);
        assert!(!timed_out);
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
        let (count, timed_out) = drain.drain_outbound(&mut rx, &mut mgr).await;
        let elapsed = start.elapsed();

        assert_eq!(count, 0);
        assert!(timed_out);
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
                &mut gossip,
                &mut registry,
                &mut rx,
                &mut mgr,
                5,
            )
            .await;

        assert_eq!(result.messages_drained, 1);
        assert_eq!(result.processes_stopped, 5);
        assert!(!result.timed_out);
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
        let (_tx, mut rx) = tokio::sync::mpsc::channel::<RouterCommand>(64);
        drop(_tx);
        let mut mgr = ConnectionManager::new(Box::new(NullConnector));

        let drain = NodeDrain::new(DrainConfig::default());
        let result = drain
            .drain(
                1,
                test_addr(),
                &mut gossip,
                &mut registry,
                &mut rx,
                &mut mgr,
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
