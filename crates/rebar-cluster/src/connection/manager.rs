use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use crate::protocol::Frame;
use crate::transport::TransportConnection;

/// Errors from the `ConnectionManager`.
#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error("transport error: {0}")]
    Transport(#[from] crate::transport::TransportError),
    #[error("unknown node: {0}")]
    UnknownNode(u64),
    #[error("already connected to node: {0}")]
    AlreadyConnected(u64),
    #[error("gave up reconnecting to node: {0}")]
    GaveUp(u64),
}

/// Events emitted by the `ConnectionManager`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionEvent {
    /// A node has gone down (connection lost).
    NodeDown(u64),
    /// A reconnect attempt is being triggered for the given node.
    ReconnectTriggered(u64),
    /// The manager has exhausted its reconnect budget for the node and reaped
    /// its bookkeeping; no further automatic reconnects will be attempted.
    ReconnectGaveUp(u64),
}

/// Computes exponential backoff delay for reconnection attempts.
///
/// Formula: `min(base_delay * 2^attempt, max_delay)`
#[derive(Debug, Clone)]
pub struct ReconnectPolicy {
    pub base_delay: Duration,
    pub max_delay: Duration,
    /// Maximum number of failed reconnect attempts before the manager gives up
    /// on a node and reaps its bookkeeping (address + attempt counter). This
    /// bounds the growth of the per-node maps for nodes that never come back.
    pub max_attempts: u32,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            max_attempts: 10,
        }
    }
}

impl ReconnectPolicy {
    /// Compute the backoff delay for the given attempt number (0-indexed).
    #[must_use]
    pub fn backoff_delay(&self, attempt: u32) -> Duration {
        let multiplier = 2u64.saturating_pow(attempt);
        let multiplier = u32::try_from(multiplier).unwrap_or(u32::MAX);
        let delay = self.base_delay.saturating_mul(multiplier);
        if delay > self.max_delay {
            self.max_delay
        } else {
            delay
        }
    }
}

/// A trait for creating transport connections. This abstraction allows
/// the `ConnectionManager` to use different transport implementations
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
/// - Event-driven connection management (`on_node_discovered`, `on_connection_lost`)
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
    /// Create a new `ConnectionManager` with the given transport connector.
    #[must_use]
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

    /// Create a new `ConnectionManager` with a custom reconnect policy.
    #[must_use]
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
    ///
    /// # Errors
    ///
    /// Returns `ConnectionError::Transport` if the underlying transport
    /// fails to connect.
    pub async fn connect(&mut self, node_id: u64, addr: SocketAddr) -> Result<(), ConnectionError> {
        let conn = self.connector.connect(addr).await?;
        self.connections.insert(node_id, conn);
        self.addresses.insert(node_id, addr);
        self.reconnect_attempts.remove(&node_id);
        Ok(())
    }

    /// Disconnect from a node, closing the connection.
    ///
    /// # Errors
    ///
    /// Currently infallible; the `Result` is reserved for future transport
    /// close errors.
    pub async fn disconnect(&mut self, node_id: u64) -> Result<(), ConnectionError> {
        if let Some(mut conn) = self.connections.remove(&node_id) {
            let _ = conn.close().await;
        }
        self.addresses.remove(&node_id);
        self.reconnect_attempts.remove(&node_id);
        Ok(())
    }

    /// Route a frame to a connected node.
    ///
    /// # Errors
    ///
    /// Returns `ConnectionError::UnknownNode` if the node is not connected,
    /// or `ConnectionError::Transport` if sending the frame fails.
    pub async fn route(&mut self, node_id: u64, frame: &Frame) -> Result<(), ConnectionError> {
        let conn = self
            .connections
            .get_mut(&node_id)
            .ok_or(ConnectionError::UnknownNode(node_id))?;
        conn.send(frame).await?;
        Ok(())
    }

    /// Check if a node is currently connected.
    #[must_use]
    pub fn is_connected(&self, node_id: u64) -> bool {
        self.connections.contains_key(&node_id)
    }

    /// Return the number of active connections.
    #[must_use]
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Called when a node is discovered (e.g., via SWIM gossip).
    /// Connects to the node if not already connected.
    ///
    /// # Errors
    ///
    /// Returns `ConnectionError::Transport` if connecting to the node fails.
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
    ///
    /// Removes (and closes) any live connection, emits a `NodeDown` event, and
    /// — if we still have an address to dial — emits `ReconnectTriggered` and
    /// resets the attempt counter to 0 so backoff starts at the first tier. The
    /// attempt counter is bumped only by [`Self::attempt_reconnect`] on failure,
    /// so it is incremented in exactly one place.
    pub fn on_connection_lost(&mut self, node_id: u64) -> Vec<ConnectionEvent> {
        self.connections.remove(&node_id);

        let mut events = vec![ConnectionEvent::NodeDown(node_id)];

        // If we have the address, trigger reconnect starting from attempt 0.
        if self.addresses.contains_key(&node_id) {
            events.push(ConnectionEvent::ReconnectTriggered(node_id));
            self.reconnect_attempts.insert(node_id, 0);
        }

        events
    }

    /// Attempt to reconnect to a node.
    ///
    /// On success the new connection replaces (and closes) any prior connection
    /// for the node and the attempt counter is cleared. On failure the attempt
    /// counter is incremented (this is the only place it grows) and the backoff
    /// `Duration` the caller should wait before retrying is returned via
    /// `ConnectionError`'s context — the returned `Ok(Duration)` is the delay to
    /// honor before the *next* attempt (always `Duration::ZERO` on success).
    ///
    /// The caller is responsible for honoring the returned/contained `Duration`
    /// before retrying; the manager does not itself sleep. Once the policy's
    /// `max_attempts` is exhausted the node is reaped (address + counter
    /// dropped) and `ConnectionError::GaveUp` is returned so the caller stops.
    ///
    /// # Errors
    ///
    /// Returns `ConnectionError::UnknownNode` if no address is known for the
    /// node, `ConnectionError::GaveUp` if the reconnect budget is exhausted, or
    /// `ConnectionError::Transport` if this attempt fails (more remain).
    pub async fn attempt_reconnect(&mut self, node_id: u64) -> Result<Duration, ConnectionError> {
        let addr = self
            .addresses
            .get(&node_id)
            .copied()
            .ok_or(ConnectionError::UnknownNode(node_id))?;

        match self.connector.connect(addr).await {
            Ok(conn) => {
                // Replace any existing live connection, closing it first so we
                // never silently leak/overwrite an open transport. This also
                // guards against a concurrent reconnect having installed one.
                if let Some(mut old) = self.connections.remove(&node_id) {
                    let _ = old.close().await;
                }
                self.connections.insert(node_id, conn);
                self.reconnect_attempts.remove(&node_id);
                Ok(Duration::ZERO)
            }
            Err(e) => {
                let attempt = self.reconnect_attempts.entry(node_id).or_insert(0);
                *attempt += 1;
                let attempts = *attempt;
                if attempts >= self.reconnect_policy.max_attempts {
                    // Give up: reap this node's bookkeeping so the maps cannot
                    // grow unbounded for a node that never reconnects.
                    self.addresses.remove(&node_id);
                    self.reconnect_attempts.remove(&node_id);
                    return Err(ConnectionError::GaveUp(node_id));
                }
                // Backoff for the *next* attempt is keyed on the now-incremented
                // count; surface it so the caller can wait before retrying.
                Err(ConnectionError::Transport(e))
            }
        }
    }

    /// Backoff delay the caller should wait before the next reconnect attempt
    /// for `node_id`, based on the current attempt count and the policy.
    #[must_use]
    pub fn next_backoff(&self, node_id: u64) -> Duration {
        let attempt = self.reconnect_attempts.get(&node_id).copied().unwrap_or(0);
        self.reconnect_policy.backoff_delay(attempt)
    }

    /// Reap bookkeeping for any node whose reconnect budget is exhausted, or for
    /// nodes in `stale` (e.g. exceeded a caller-tracked TTL with no live
    /// connection). Closes nothing (these have no live connection) and clears
    /// the address + attempt counter so the maps stay bounded. Returns the node
    /// ids that were reaped.
    pub fn reap(&mut self, stale: &[u64]) -> Vec<u64> {
        let mut reaped: Vec<u64> = self
            .reconnect_attempts
            .iter()
            .filter(|&(_, &n)| n >= self.reconnect_policy.max_attempts)
            .map(|(&id, _)| id)
            .collect();
        for &id in stale {
            if !self.connections.contains_key(&id) && !reaped.contains(&id) {
                reaped.push(id);
            }
        }
        for id in &reaped {
            self.addresses.remove(id);
            self.reconnect_attempts.remove(id);
        }
        reaped
    }

    /// Get the current reconnect attempt count for a node.
    #[must_use]
    pub fn reconnect_attempt_count(&self, node_id: u64) -> u32 {
        self.reconnect_attempts.get(&node_id).copied().unwrap_or(0)
    }

    /// Drain all pending events.
    pub fn drain_events(&mut self) -> Vec<ConnectionEvent> {
        std::mem::take(&mut self.events)
    }

    /// Get the reconnect policy.
    #[must_use]
    pub const fn reconnect_policy(&self) -> &ReconnectPolicy {
        &self.reconnect_policy
    }

    /// Drain all connections. Closes each connection and clears the table.
    /// Returns the number of connections closed.
    pub async fn drain_connections(&mut self) -> usize {
        let count = self.connections.len();
        let node_ids: Vec<u64> = self.connections.keys().copied().collect();
        for node_id in node_ids {
            if let Some(mut conn) = self.connections.remove(&node_id) {
                let _ = conn.close().await;
            }
        }
        self.addresses.clear();
        self.reconnect_attempts.clear();
        count
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
        format!("127.0.0.1:{port}").parse().unwrap()
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
            other => panic!("expected UnknownNode, got: {other:?}"),
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
        let events = mgr.on_connection_lost(1);

        assert!(!mgr.is_connected(1));
        assert!(events.contains(&ConnectionEvent::NodeDown(1)));
    }

    #[tokio::test]
    async fn on_connection_lost_triggers_reconnect() {
        let setup = MockSetup::new();
        let (mut mgr, _mock) = setup.manager();

        mgr.connect(1, test_addr(4001)).await.unwrap();
        let events = mgr.on_connection_lost(1);

        assert!(events.contains(&ConnectionEvent::ReconnectTriggered(1)));
    }

    // ─── Reconnection Tests ───────────────────────────────────────

    #[tokio::test]
    async fn exponential_backoff_timing() {
        let policy = ReconnectPolicy {
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            ..ReconnectPolicy::default()
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
            ..ReconnectPolicy::default()
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
        mgr.on_connection_lost(1);
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

    // ─── Reconnect backoff / lifecycle regression tests ───────────

    #[tokio::test]
    async fn lost_connection_resets_attempts_to_zero() {
        // Fix 3: after a loss, backoff must start at attempt 0 (first tier), and
        // on_connection_lost must NOT bump the counter (that happens only on a
        // failed attempt_reconnect).
        let setup = MockSetup::new();
        let (mut mgr, _mock) = setup.manager();

        mgr.connect(1, test_addr(4001)).await.unwrap();
        let events = mgr.on_connection_lost(1);
        assert!(events.contains(&ConnectionEvent::ReconnectTriggered(1)));
        assert_eq!(mgr.reconnect_attempt_count(1), 0);
        // First backoff is the base delay (attempt 0), not skipped.
        assert_eq!(mgr.next_backoff(1), mgr.reconnect_policy().base_delay);
    }

    #[tokio::test]
    async fn failed_reconnect_increments_attempts_once() {
        // Fix 3: exactly one increment per failed attempt.
        let setup = MockSetup::new();
        let (mut mgr, mock) = setup.manager();

        mgr.connect(1, test_addr(4001)).await.unwrap();
        mgr.on_connection_lost(1);
        mock.set_should_fail(true);

        assert!(mgr.attempt_reconnect(1).await.is_err());
        assert_eq!(mgr.reconnect_attempt_count(1), 1);
        assert!(mgr.attempt_reconnect(1).await.is_err());
        assert_eq!(mgr.reconnect_attempt_count(1), 2);
    }

    #[tokio::test]
    async fn reconnect_gives_up_after_max_attempts_and_reaps() {
        // Fix 5: after the budget is exhausted, the node is reaped (address +
        // counter dropped) and GaveUp is returned so the caller stops.
        let policy = ReconnectPolicy {
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(10),
            max_attempts: 3,
        };
        let connector = Arc::new(MockConnector::new());
        let mut mgr =
            ConnectionManager::with_reconnect_policy(Box::new(ArcConnector(connector.clone())), policy);

        mgr.connect(1, test_addr(4001)).await.unwrap();
        mgr.on_connection_lost(1);
        connector.set_should_fail(true);

        // attempts 1 and 2 are transport errors; attempt 3 hits the budget.
        assert!(matches!(
            mgr.attempt_reconnect(1).await,
            Err(ConnectionError::Transport(_))
        ));
        assert!(matches!(
            mgr.attempt_reconnect(1).await,
            Err(ConnectionError::Transport(_))
        ));
        assert!(matches!(
            mgr.attempt_reconnect(1).await,
            Err(ConnectionError::GaveUp(1))
        ));
        // Reaped: no address, counter cleared, further reconnect is UnknownNode.
        assert_eq!(mgr.reconnect_attempt_count(1), 0);
        assert!(matches!(
            mgr.attempt_reconnect(1).await,
            Err(ConnectionError::UnknownNode(1))
        ));
    }

    #[tokio::test]
    async fn reap_drops_stale_unconnected_nodes() {
        // Fix 5: explicit reaping path for never-reconnected nodes bounds map growth.
        let setup = MockSetup::new();
        let (mut mgr, _mock) = setup.manager();

        mgr.connect(1, test_addr(4001)).await.unwrap();
        mgr.connect(2, test_addr(4002)).await.unwrap();
        // Node 1 lost (no live connection); node 2 stays connected.
        mgr.on_connection_lost(1);

        let reaped = mgr.reap(&[1, 2]);
        assert_eq!(reaped, vec![1]); // node 2 still connected, not reaped
        assert!(mgr.is_connected(2));
    }

    #[tokio::test]
    async fn reconnect_replaces_and_closes_existing_connection() {
        // Fix 4: a successful reconnect must close any live connection it
        // replaces rather than silently dropping/overwriting it.
        let closes: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let connector = Arc::new(CloseTrackingConnector {
            closes: closes.clone(),
        });
        let mut mgr = ConnectionManager::new(Box::new(ArcCloseConnector(connector)));

        mgr.connect(1, test_addr(4001)).await.unwrap();
        // Simulate the manager still holding a live connection (no on_connection_lost)
        // and a reconnect racing in; the old connection must be closed.
        mgr.attempt_reconnect(1).await.unwrap();
        assert_eq!(*closes.lock().unwrap(), 1, "old connection was not closed");
        assert!(mgr.is_connected(1));
    }

    // Connection + connector that count close() calls on the connections they hand out.
    struct CloseCountingConnection {
        closes: Arc<Mutex<u32>>,
    }

    #[async_trait::async_trait]
    impl TransportConnection for CloseCountingConnection {
        async fn send(&mut self, _frame: &Frame) -> Result<(), TransportError> {
            Ok(())
        }
        async fn recv(&mut self) -> Result<Frame, TransportError> {
            Err(TransportError::ConnectionClosed)
        }
        async fn close(&mut self) -> Result<(), TransportError> {
            *self.closes.lock().unwrap() += 1;
            Ok(())
        }
    }

    struct CloseTrackingConnector {
        closes: Arc<Mutex<u32>>,
    }

    struct ArcCloseConnector(Arc<CloseTrackingConnector>);

    #[async_trait::async_trait]
    impl TransportConnector for ArcCloseConnector {
        async fn connect(
            &self,
            _addr: SocketAddr,
        ) -> Result<Box<dyn TransportConnection>, TransportError> {
            Ok(Box::new(CloseCountingConnection {
                closes: self.0.closes.clone(),
            }))
        }
    }

    #[tokio::test]
    async fn drain_connections_closes_all() {
        let setup = MockSetup::new();
        let (mut mgr, _mock) = setup.manager();

        mgr.connect(1, test_addr(4001)).await.unwrap();
        mgr.connect(2, test_addr(4002)).await.unwrap();
        mgr.connect(3, test_addr(4003)).await.unwrap();

        let closed = mgr.drain_connections().await;
        assert_eq!(closed, 3);
        assert_eq!(mgr.connection_count(), 0);
        assert!(!mgr.is_connected(1));
        assert!(!mgr.is_connected(2));
        assert!(!mgr.is_connected(3));
    }
}
