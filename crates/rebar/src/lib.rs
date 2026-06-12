pub use rebar_core::router::{LocalRouter, MessageRouter};
pub use rebar_core::*;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

use rebar_cluster::connection::manager::{ConnectionEvent, ConnectionManager};
use rebar_cluster::protocol::{Frame, MsgType};
use rebar_cluster::router::{DeliverError, DistributedRouter, RouterCommand, deliver_inbound_frame};
use rebar_cluster::swim::{Outgoing, SwimConfig, SwimService};
use rebar_core::process::table::ProcessTable;
use rebar_core::runtime::Runtime;

/// Outcome of pumping one outbound remote message via [`DistributedRuntime::process_outbound`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboundOutcome {
    /// No message was pending.
    Idle,
    /// A message was delivered to the transport for the given node.
    Delivered(u64),
    /// A message was dequeued but the transport route to the node failed; the
    /// frame could not be delivered. Surfaced (not silently dropped) so the
    /// caller can react (e.g. trigger reconnect / alert).
    Failed(u64),
}

/// A fully wired distributed runtime bridging rebar-core and rebar-cluster.
pub struct DistributedRuntime {
    runtime: Runtime,
    table: Arc<ProcessTable>,
    connection_manager: ConnectionManager,
    remote_rx: mpsc::Receiver<RouterCommand>,
    swim: Option<Arc<SwimService>>,
}

impl DistributedRuntime {
    /// Create a distributed runtime for the given node, routing remote
    /// messages through the given connection manager.
    #[must_use]
    pub fn new(node_id: u64, connection_manager: ConnectionManager) -> Self {
        let table = Arc::new(ProcessTable::new(node_id));
        let (remote_tx, remote_rx) = mpsc::channel(1024);
        let router = Arc::new(DistributedRouter::new(
            node_id,
            Arc::clone(&table),
            remote_tx,
        ));
        let runtime = Runtime::with_router(node_id, Arc::clone(&table), router);

        Self {
            runtime,
            table,
            connection_manager,
            remote_rx,
            swim: None,
        }
    }

    /// Enable SWIM failure detection for this node, listening at `self_addr`.
    ///
    /// After enabling, seed known peers with [`swim_add_seed`](Self::swim_add_seed),
    /// drive [`swim_tick`](Self::swim_tick) once per `protocol_period`, and feed
    /// inbound frames through [`handle_inbound_frame`](Self::handle_inbound_frame).
    pub fn enable_swim(&mut self, self_addr: SocketAddr, config: SwimConfig) {
        self.swim = Some(Arc::new(SwimService::new(
            self.runtime.node_id(),
            self_addr,
            config,
            None,
        )));
    }

    /// The SWIM service, if enabled (for seeding and diagnostics).
    #[must_use]
    pub const fn swim(&self) -> Option<&Arc<SwimService>> {
        self.swim.as_ref()
    }

    /// Register a known peer with the SWIM service so it can be probed.
    pub fn swim_add_seed(&self, node_id: u64, addr: SocketAddr) {
        if let Some(swim) = &self.swim {
            swim.add_seed(node_id, addr);
        }
    }

    /// Send a batch of SWIM frames, connecting to peers as needed.
    async fn send_swim(&mut self, outgoing: Vec<Outgoing>) {
        for o in outgoing {
            if !self.connection_manager.is_connected(o.node_id) {
                let _ = self
                    .connection_manager
                    .on_node_discovered(o.node_id, o.addr)
                    .await;
            }
            let _ = self.connection_manager.route(o.node_id, &o.frame).await;
        }
    }

    /// Run one SWIM protocol period: probe, expire timers, gossip. Returns any
    /// connection events (e.g. `NodeDown`) produced for nodes newly declared
    /// dead, so the caller can react (the connection manager is already
    /// notified). A no-op if SWIM is not enabled.
    pub async fn swim_tick(&mut self) -> Vec<ConnectionEvent> {
        let Some(swim) = self.swim.clone() else {
            return Vec::new();
        };
        let outcome = swim.tick(Instant::now());
        self.send_swim(outcome.outgoing).await;
        let mut events = Vec::new();
        for node_id in outcome.newly_dead {
            events.extend(self.connection_manager.on_connection_lost(node_id));
        }
        events
    }

    /// The local runtime.
    #[must_use]
    pub const fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// The local process table.
    #[must_use]
    pub const fn table(&self) -> &Arc<ProcessTable> {
        &self.table
    }

    /// Mutable access to the connection manager.
    pub const fn connection_manager_mut(&mut self) -> &mut ConnectionManager {
        &mut self.connection_manager
    }

    /// Process one pending outbound remote message.
    ///
    /// Returns [`OutboundOutcome`] describing whether a message was pending,
    /// delivered, or failed to route. A routing failure is reported (rather
    /// than silently dropped) so the caller can react.
    pub async fn process_outbound(&mut self) -> OutboundOutcome {
        match self.remote_rx.try_recv() {
            Ok(RouterCommand::Send { node_id, frame }) => {
                if self.connection_manager.route(node_id, &frame).await.is_ok() {
                    OutboundOutcome::Delivered(node_id)
                } else {
                    OutboundOutcome::Failed(node_id)
                }
            }
            Err(_) => OutboundOutcome::Idle,
        }
    }

    /// Deliver an inbound application (`Send`) frame to a local process.
    ///
    /// # Errors
    ///
    /// Returns [`DeliverError::Malformed`] if the peer frame is malformed, or
    /// [`DeliverError::Send`] if the destination process is not reachable.
    pub fn deliver_inbound(&self, frame: &Frame) -> Result<(), DeliverError> {
        deliver_inbound_frame(&self.table, frame)
    }

    /// Handle any inbound frame from a peer, dispatching by message type:
    /// `Swim` frames drive failure detection (and any responses are sent back),
    /// everything else is delivered to a local process.
    ///
    /// This is the single entry point a transport accept-loop should call for
    /// every received frame.
    ///
    /// # Errors
    ///
    /// Returns [`DeliverError`] only for non-SWIM frames that fail delivery;
    /// SWIM frames never error (malformed ones are ignored).
    pub async fn handle_inbound_frame(&mut self, frame: &Frame) -> Result<(), DeliverError> {
        if frame.msg_type == MsgType::Swim {
            if let Some(swim) = self.swim.clone() {
                let responses = swim.handle_frame(frame);
                self.send_swim(responses).await;
            }
            Ok(())
        } else {
            self.deliver_inbound(frame)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebar_cluster::connection::manager::TransportConnector;
    use rebar_cluster::protocol::MsgType;
    use rebar_cluster::transport::{TransportConnection, TransportError};
    use rebar_core::process::mailbox::Mailbox;
    use rebar_core::process::table::ProcessHandle;
    use std::sync::Mutex;

    struct MockConnector;

    #[async_trait::async_trait]
    impl TransportConnector for MockConnector {
        async fn connect(
            &self,
            _addr: std::net::SocketAddr,
        ) -> Result<Box<dyn TransportConnection>, TransportError> {
            Ok(Box::new(MockConn {
                sent: Arc::new(Mutex::new(Vec::new())),
            }))
        }
    }

    struct MockConn {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    #[async_trait::async_trait]
    impl TransportConnection for MockConn {
        async fn send(&mut self, frame: &Frame) -> Result<(), TransportError> {
            self.sent.lock().unwrap().push(frame.encode());
            Ok(())
        }
        async fn recv(&mut self) -> Result<Frame, TransportError> {
            Err(TransportError::ConnectionClosed)
        }
        async fn close(&mut self) -> Result<(), TransportError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn distributed_runtime_local_send() {
        let mgr = ConnectionManager::new(Box::new(MockConnector));
        let drt = DistributedRuntime::new(1, mgr);

        let (done_tx, done_rx) = tokio::sync::oneshot::channel();

        let receiver = drt
            .runtime()
            .spawn(move |mut ctx| async move {
                let msg = ctx.recv().await.unwrap();
                done_tx
                    .send(msg.payload().as_str().unwrap().to_string())
                    .unwrap();
            })
            .await;

        drt.runtime()
            .spawn(move |ctx| async move {
                ctx.send(receiver, rmpv::Value::String("local".into()))
                    .await
                    .unwrap();
            })
            .await;

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), done_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result, "local");
    }

    #[tokio::test]
    async fn distributed_runtime_inbound_delivery() {
        let mgr = ConnectionManager::new(Box::new(MockConnector));
        let drt = DistributedRuntime::new(2, mgr);

        let pid = drt.table().allocate_pid();
        let (tx, mut rx) = Mailbox::unbounded();
        drt.table().insert(pid, ProcessHandle::new(tx));

        let frame = Frame {
            version: 1,
            msg_type: MsgType::Send,
            request_id: 0,
            header: rmpv::Value::Map(vec![
                (
                    rmpv::Value::String("from_node".into()),
                    rmpv::Value::Integer(1u64.into()),
                ),
                (
                    rmpv::Value::String("from_local".into()),
                    rmpv::Value::Integer(5u64.into()),
                ),
                (
                    rmpv::Value::String("to_node".into()),
                    rmpv::Value::Integer(pid.node_id().into()),
                ),
                (
                    rmpv::Value::String("to_local".into()),
                    rmpv::Value::Integer(pid.local_id().into()),
                ),
            ]),
            payload: rmpv::Value::String("from-remote-node".into()),
        };

        drt.deliver_inbound(&frame).unwrap();

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.payload().as_str().unwrap(), "from-remote-node");
    }
}
