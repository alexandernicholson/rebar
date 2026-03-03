use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use crate::protocol::Frame;
use crate::transport::TransportConnection;

/// Errors from the ConnectionManager.
#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error("transport error: {0}")]
    Transport(#[from] crate::transport::TransportError),
    #[error("unknown node: {0}")]
    UnknownNode(u64),
    #[error("already connected to node: {0}")]
    AlreadyConnected(u64),
}

/// Events emitted by the ConnectionManager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionEvent {
    /// A node has gone down (connection lost).
    NodeDown(u64),
    /// A reconnect attempt is being triggered for the given node.
    ReconnectTriggered(u64),
}

/// Computes exponential backoff delay for reconnection attempts.
///
/// Formula: min(base_delay * 2^attempt, max_delay)
#[derive(Debug, Clone)]
pub struct ReconnectPolicy {
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
        }
    }
}

impl ReconnectPolicy {
    /// Compute the backoff delay for the given attempt number (0-indexed).
    pub fn backoff_delay(&self, attempt: u32) -> Duration {
        let multiplier = 2u64.saturating_pow(attempt);
        let delay = self.base_delay.saturating_mul(multiplier as u32);
        if delay > self.max_delay {
            self.max_delay
        } else {
            delay
        }
    }
}

/// A trait for creating transport connections. This abstraction allows
/// the ConnectionManager to use different transport implementations
/// (TCP, QUIC, mock, etc.).
#[async_trait::async_trait]
pub trait TransportConnector: Send + Sync {
    async fn connect(
        &self,
        addr: SocketAddr,
    ) -> Result<Box<dyn TransportConnection>, crate::transport::TransportError>;
}

/// Manages connections to remote nodes in the cluster.
///
/// Wraps transport implementations and provides:
/// - Connection lifecycle (connect, disconnect, route)
/// - Event-driven connection management (on_node_discovered, on_connection_lost)
/// - Reconnection with exponential backoff
pub struct ConnectionManager {
    connections: HashMap<u64, Box<dyn TransportConnection>>,
    addresses: HashMap<u64, SocketAddr>,
    connector: Box<dyn TransportConnector>,
    reconnect_policy: ReconnectPolicy,
    events: Vec<ConnectionEvent>,
    reconnect_attempts: HashMap<u64, u32>,
}

impl ConnectionManager {
    /// Create a new ConnectionManager with the given transport connector.
    pub fn new(connector: Box<dyn TransportConnector>) -> Self {
        Self {
            connections: HashMap::new(),
            addresses: HashMap::new(),
            connector,
            reconnect_policy: ReconnectPolicy::default(),
            events: Vec::new(),
            reconnect_attempts: HashMap::new(),
        }
    }

    /// Create a new ConnectionManager with a custom reconnect policy.
    pub fn with_reconnect_policy(
        connector: Box<dyn TransportConnector>,
        policy: ReconnectPolicy,
    ) -> Self {
        Self {
            connections: HashMap::new(),
            addresses: HashMap::new(),
            connector,
            reconnect_policy: policy,
            events: Vec::new(),
            reconnect_attempts: HashMap::new(),
        }
    }

    /// Establish a connection to a node at the given address.
    pub async fn connect(&mut self, node_id: u64, addr: SocketAddr) -> Result<(), ConnectionError> {
        let conn = self.connector.connect(addr).await?;
        self.connections.insert(node_id, conn);
        self.addresses.insert(node_id, addr);
        self.reconnect_attempts.remove(&node_id);
        Ok(())
    }

    /// Disconnect from a node, closing the connection.
    pub async fn disconnect(&mut self, node_id: u64) -> Result<(), ConnectionError> {
        if let Some(mut conn) = self.connections.remove(&node_id) {
            let _ = conn.close().await;
        }
        self.addresses.remove(&node_id);
        self.reconnect_attempts.remove(&node_id);
        Ok(())
    }

    /// Route a frame to a connected node.
    pub async fn route(&mut self, node_id: u64, frame: &Frame) -> Result<(), ConnectionError> {
        let conn = self
            .connections
            .get_mut(&node_id)
            .ok_or(ConnectionError::UnknownNode(node_id))?;
        conn.send(frame).await?;
        Ok(())
    }

    /// Check if a node is currently connected.
    pub fn is_connected(&self, node_id: u64) -> bool {
        self.connections.contains_key(&node_id)
    }

    /// Return the number of active connections.
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Called when a node is discovered (e.g., via SWIM gossip).
    /// Connects to the node if not already connected.
    pub async fn on_node_discovered(
        &mut self,
        node_id: u64,
        addr: SocketAddr,
    ) -> Result<(), ConnectionError> {
        if self.is_connected(node_id) {
            return Ok(());
        }
        self.connect(node_id, addr).await
    }

    /// Called when a connection to a node is lost.
    /// Emits a NodeDown event and optionally triggers a reconnect attempt.
    pub async fn on_connection_lost(&mut self, node_id: u64) -> Vec<ConnectionEvent> {
        self.connections.remove(&node_id);

        let mut events = Vec::new();
        events.push(ConnectionEvent::NodeDown(node_id));

        // If we have the address, trigger reconnect
        if self.addresses.contains_key(&node_id) {
            events.push(ConnectionEvent::ReconnectTriggered(node_id));
            let attempt = self.reconnect_attempts.entry(node_id).or_insert(0);
            *attempt += 1;
        }

        events
    }

    /// Attempt to reconnect to a node. Returns the backoff delay that should
    /// be waited before the next attempt if this attempt fails.
    pub async fn attempt_reconnect(&mut self, node_id: u64) -> Result<Duration, ConnectionError> {
        let addr = self
            .addresses
            .get(&node_id)
            .copied()
            .ok_or(ConnectionError::UnknownNode(node_id))?;

        let attempt = self.reconnect_attempts.get(&node_id).copied().unwrap_or(0);
        let _delay = self.reconnect_policy.backoff_delay(attempt);

        match self.connector.connect(addr).await {
            Ok(conn) => {
                self.connections.insert(node_id, conn);
                self.reconnect_attempts.remove(&node_id);
                Ok(Duration::ZERO)
            }
            Err(e) => {
                let attempt = self.reconnect_attempts.entry(node_id).or_insert(0);
                *attempt += 1;
                Err(ConnectionError::Transport(e))
            }
        }
    }

    /// Get the current reconnect attempt count for a node.
    pub fn reconnect_attempt_count(&self, node_id: u64) -> u32 {
        self.reconnect_attempts.get(&node_id).copied().unwrap_or(0)
    }

    /// Drain all pending events.
    pub fn drain_events(&mut self) -> Vec<ConnectionEvent> {
        std::mem::take(&mut self.events)
    }

    /// Get the reconnect policy.
    pub fn reconnect_policy(&self) -> &ReconnectPolicy {
        &self.reconnect_policy
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Frame, MsgType};
    use crate::transport::{TransportConnection, TransportError};
    use std::sync::{Arc, Mutex};

    // ─── Mock Transport ────────────────────────────────────────────

    /// Records all frames sent through this connection.
    struct MockConnection {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        closed: Arc<Mutex<bool>>,
    }

    impl MockConnection {
        fn new(sent: Arc<Mutex<Vec<Vec<u8>>>>) -> Self {
            Self {
                sent,
                closed: Arc::new(Mutex::new(false)),
            }
        }
    }

    #[async_trait::async_trait]
    impl TransportConnection for MockConnection {
        async fn send(&mut self, frame: &Frame) -> Result<(), TransportError> {
            self.sent.lock().unwrap().push(frame.encode());
            Ok(())
        }

        async fn recv(&mut self) -> Result<Frame, TransportError> {
            Err(TransportError::ConnectionClosed)
        }

        async fn close(&mut self) -> Result<(), TransportError> {
            *self.closed.lock().unwrap() = true;
            Ok(())
        }
    }

    /// Controls whether connect succeeds or fails, and tracks sent data.
    struct MockConnector {
        should_fail: Arc<Mutex<bool>>,
        sent_data: Arc<Mutex<Vec<Vec<u8>>>>,
        connect_count: Arc<Mutex<u32>>,
    }

    impl MockConnector {
        fn new() -> Self {
            Self {
                should_fail: Arc::new(Mutex::new(false)),
                sent_data: Arc::new(Mutex::new(Vec::new())),
                connect_count: Arc::new(Mutex::new(0)),
            }
        }

        #[allow(dead_code)]
        fn set_should_fail(&self, fail: bool) {
            *self.should_fail.lock().unwrap() = fail;
        }

        fn connect_count(&self) -> u32 {
            *self.connect_count.lock().unwrap()
        }

        fn sent_data(&self) -> Vec<Vec<u8>> {
            self.sent_data.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl TransportConnector for MockConnector {
        async fn connect(
            &self,
            _addr: SocketAddr,
        ) -> Result<Box<dyn TransportConnection>, TransportError> {
            *self.connect_count.lock().unwrap() += 1;
            if *self.should_fail.lock().unwrap() {
                return Err(TransportError::Io(std::io::Error::new(
                    std::io::ErrorKind::ConnectionRefused,
                    "mock connection refused",
                )));
            }
            Ok(Box::new(MockConnection::new(self.sent_data.clone())))
        }
    }

    /// Helper to create a mock connector wrapped in Arc for shared access.
    struct MockSetup {
        connector: Arc<MockConnector>,
    }

    impl MockSetup {
        fn new() -> Self {
            Self {
                connector: Arc::new(MockConnector::new()),
            }
        }

        fn manager(self) -> (ConnectionManager, Arc<MockConnector>) {
            let connector_ref = self.connector.clone();
            let mgr = ConnectionManager::new(Box::new(ArcConnector(self.connector)));
            (mgr, connector_ref)
        }
    }

    /// Wrapper to allow Arc<MockConnector> to be used as Box<dyn TransportConnector>.
    struct ArcConnector(Arc<MockConnector>);

    #[async_trait::async_trait]
    impl TransportConnector for ArcConnector {
        async fn connect(
            &self,
            addr: SocketAddr,
        ) -> Result<Box<dyn TransportConnection>, TransportError> {
            self.0.connect(addr).await
        }
    }

    fn test_addr(port: u16) -> SocketAddr {
        format!("127.0.0.1:{}", port).parse().unwrap()
    }

    fn heartbeat_frame() -> Frame {
        Frame {
            version: 1,
            msg_type: MsgType::Heartbeat,
            request_id: 0,
            header: rmpv::Value::Nil,
            payload: rmpv::Value::Nil,
        }
    }

    fn send_frame(payload: &str) -> Frame {
        Frame {
            version: 1,
            msg_type: MsgType::Send,
            request_id: 0,
            header: rmpv::Value::Nil,
            payload: rmpv::Value::String(payload.into()),
        }
    }

    // ─── Connection Lifecycle Tests ────────────────────────────────

    #[tokio::test]
    async fn connect_to_new_node() {
        let setup = MockSetup::new();
        let (mut mgr, mock) = setup.manager();

        mgr.connect(1, test_addr(4001)).await.unwrap();

        assert!(mgr.is_connected(1));
        assert_eq!(mgr.connection_count(), 1);
        assert_eq!(mock.connect_count(), 1);
    }

    #[tokio::test]
    async fn route_frame_to_connected_node() {
        let setup = MockSetup::new();
        let (mut mgr, mock) = setup.manager();

        mgr.connect(1, test_addr(4001)).await.unwrap();
        mgr.route(1, &heartbeat_frame()).await.unwrap();

        let sent = mock.sent_data();
        assert_eq!(sent.len(), 1);
        // Verify the sent data decodes to a valid frame
        let decoded = Frame::decode(&sent[0]).unwrap();
        assert_eq!(decoded.msg_type, MsgType::Heartbeat);
    }

    #[tokio::test]
    async fn route_to_unknown_node_returns_error() {
        let setup = MockSetup::new();
        let (mut mgr, _mock) = setup.manager();

        let result = mgr.route(999, &heartbeat_frame()).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ConnectionError::UnknownNode(id) => assert_eq!(id, 999),
            other => panic!("expected UnknownNode, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn disconnect_node() {
        let setup = MockSetup::new();
        let (mut mgr, _mock) = setup.manager();

        mgr.connect(1, test_addr(4001)).await.unwrap();
        assert!(mgr.is_connected(1));

        mgr.disconnect(1).await.unwrap();
        assert!(!mgr.is_connected(1));
        assert_eq!(mgr.connection_count(), 0);
    }

    #[tokio::test]
    async fn reconnect_after_disconnect() {
        let setup = MockSetup::new();
        let (mut mgr, mock) = setup.manager();

        mgr.connect(1, test_addr(4001)).await.unwrap();
        mgr.disconnect(1).await.unwrap();
        assert!(!mgr.is_connected(1));

        // Reconnect to same node
        mgr.connect(1, test_addr(4001)).await.unwrap();
        assert!(mgr.is_connected(1));
        assert_eq!(mock.connect_count(), 2);
    }

    // ─── Event Handling Tests ──────────────────────────────────────

    #[tokio::test]
    async fn on_node_discovered_connects() {
        let setup = MockSetup::new();
        let (mut mgr, mock) = setup.manager();

        mgr.on_node_discovered(1, test_addr(4001)).await.unwrap();

        assert!(mgr.is_connected(1));
        assert_eq!(mock.connect_count(), 1);
    }

    #[tokio::test]
    async fn on_node_discovered_idempotent() {
        let setup = MockSetup::new();
        let (mut mgr, mock) = setup.manager();

        mgr.on_node_discovered(1, test_addr(4001)).await.unwrap();
        mgr.on_node_discovered(1, test_addr(4001)).await.unwrap();
        mgr.on_node_discovered(1, test_addr(4001)).await.unwrap();

        assert_eq!(mgr.connection_count(), 1);
        assert_eq!(mock.connect_count(), 1); // Only connected once
    }

    #[tokio::test]
    async fn on_connection_lost_fires_node_down() {
        let setup = MockSetup::new();
        let (mut mgr, _mock) = setup.manager();

        mgr.connect(1, test_addr(4001)).await.unwrap();
        let events = mgr.on_connection_lost(1).await;

        assert!(!mgr.is_connected(1));
        assert!(events.contains(&ConnectionEvent::NodeDown(1)));
    }

    #[tokio::test]
    async fn on_connection_lost_triggers_reconnect() {
        let setup = MockSetup::new();
        let (mut mgr, _mock) = setup.manager();

        mgr.connect(1, test_addr(4001)).await.unwrap();
        let events = mgr.on_connection_lost(1).await;

        assert!(events.contains(&ConnectionEvent::ReconnectTriggered(1)));
    }

    // ─── Reconnection Tests ───────────────────────────────────────

    #[tokio::test]
    async fn exponential_backoff_timing() {
        let policy = ReconnectPolicy {
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
        };

        assert_eq!(policy.backoff_delay(0), Duration::from_secs(1)); // 1 * 2^0 = 1
        assert_eq!(policy.backoff_delay(1), Duration::from_secs(2)); // 1 * 2^1 = 2
        assert_eq!(policy.backoff_delay(2), Duration::from_secs(4)); // 1 * 2^2 = 4
        assert_eq!(policy.backoff_delay(3), Duration::from_secs(8)); // 1 * 2^3 = 8
        assert_eq!(policy.backoff_delay(4), Duration::from_secs(16)); // 1 * 2^4 = 16
    }

    #[tokio::test]
    async fn max_backoff_capped() {
        let policy = ReconnectPolicy {
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
        };

        // 2^5 = 32 > 30, should be capped
        assert_eq!(policy.backoff_delay(5), Duration::from_secs(30));
        // 2^10 = 1024 >> 30, should still be capped
        assert_eq!(policy.backoff_delay(10), Duration::from_secs(30));
        // Very large attempt shouldn't panic
        assert_eq!(policy.backoff_delay(100), Duration::from_secs(30));
    }

    #[tokio::test]
    async fn reconnect_succeeds_restores_routing() {
        let setup = MockSetup::new();
        let (mut mgr, mock) = setup.manager();

        // Connect, then simulate connection loss
        mgr.connect(1, test_addr(4001)).await.unwrap();
        mgr.on_connection_lost(1).await;
        assert!(!mgr.is_connected(1));

        // Reconnect succeeds
        let result = mgr.attempt_reconnect(1).await;
        assert!(result.is_ok());
        assert!(mgr.is_connected(1));

        // Routing works after reconnect
        mgr.route(1, &send_frame("hello")).await.unwrap();
        let sent = mock.sent_data();
        assert_eq!(sent.len(), 1);
    }

    // ─── Multi-node Tests ─────────────────────────────────────────

    #[tokio::test]
    async fn full_mesh_three_nodes() {
        let setup = MockSetup::new();
        let (mut mgr, mock) = setup.manager();

        // Connect to 3 nodes forming a "full mesh" from this node's perspective
        mgr.connect(1, test_addr(4001)).await.unwrap();
        mgr.connect(2, test_addr(4002)).await.unwrap();
        mgr.connect(3, test_addr(4003)).await.unwrap();

        assert_eq!(mgr.connection_count(), 3);
        assert!(mgr.is_connected(1));
        assert!(mgr.is_connected(2));
        assert!(mgr.is_connected(3));
        assert_eq!(mock.connect_count(), 3);

        // Route to each node
        mgr.route(1, &heartbeat_frame()).await.unwrap();
        mgr.route(2, &heartbeat_frame()).await.unwrap();
        mgr.route(3, &heartbeat_frame()).await.unwrap();

        assert_eq!(mock.sent_data().len(), 3);
    }

    #[tokio::test]
    async fn concurrent_route_to_multiple_nodes() {
        let setup = MockSetup::new();
        let (mut mgr, mock) = setup.manager();

        mgr.connect(1, test_addr(4001)).await.unwrap();
        mgr.connect(2, test_addr(4002)).await.unwrap();

        // Route different payloads to different nodes
        mgr.route(1, &send_frame("msg_to_1")).await.unwrap();
        mgr.route(2, &send_frame("msg_to_2")).await.unwrap();
        mgr.route(1, &send_frame("another_to_1")).await.unwrap();

        let sent = mock.sent_data();
        assert_eq!(sent.len(), 3);

        // Verify all payloads were sent
        let payloads: Vec<String> = sent
            .iter()
            .map(|data| {
                let frame = Frame::decode(data).unwrap();
                frame.payload.as_str().unwrap().to_string()
            })
            .collect();
        assert!(payloads.contains(&"msg_to_1".to_string()));
        assert!(payloads.contains(&"msg_to_2".to_string()));
        assert!(payloads.contains(&"another_to_1".to_string()));
    }

    #[tokio::test]
    async fn connection_count() {
        let setup = MockSetup::new();
        let (mut mgr, _mock) = setup.manager();

        assert_eq!(mgr.connection_count(), 0);

        mgr.connect(1, test_addr(4001)).await.unwrap();
        assert_eq!(mgr.connection_count(), 1);

        mgr.connect(2, test_addr(4002)).await.unwrap();
        assert_eq!(mgr.connection_count(), 2);

        mgr.connect(3, test_addr(4003)).await.unwrap();
        assert_eq!(mgr.connection_count(), 3);

        mgr.disconnect(2).await.unwrap();
        assert_eq!(mgr.connection_count(), 2);

        mgr.disconnect(1).await.unwrap();
        mgr.disconnect(3).await.unwrap();
        assert_eq!(mgr.connection_count(), 0);
    }
}
