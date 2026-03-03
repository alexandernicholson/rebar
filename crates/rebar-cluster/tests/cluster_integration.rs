//! Cluster integration tests.
//!
//! These tests exercise the rebar-cluster components working together:
//! SWIM membership + gossip, TCP transport + wire protocol, OR-Set registry
//! with delta replication, and the ConnectionManager with mock transport.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rebar_cluster::connection::{ConnectionEvent, ConnectionManager, TransportConnector};
use rebar_cluster::protocol::{Frame, MsgType};
use rebar_cluster::registry::{Registry, RegistryDelta};
use rebar_cluster::router::{DistributedRouter, RouterCommand, deliver_inbound_frame};
use rebar_cluster::swim::{GossipQueue, GossipUpdate, Member, MembershipList, NodeState};
use rebar_cluster::transport::tcp::TcpTransport;
use rebar_cluster::transport::{TransportConnection, TransportError, TransportListener};
use rebar_core::process::ProcessId;
use rebar_core::process::table::ProcessTable;
use rebar_core::runtime::Runtime;
use tokio::sync::mpsc;

// ─── Mock infrastructure for ConnectionManager tests ───────────────────────

/// A mock transport connection that records sent frames.
struct MockConnection {
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl MockConnection {
    fn new(sent: Arc<Mutex<Vec<Vec<u8>>>>) -> Self {
        Self { sent }
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
        Ok(())
    }
}

/// A mock transport connector that can be configured to succeed or fail.
struct MockConnector {
    should_fail: Arc<Mutex<bool>>,
    sent_data: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl MockConnector {
    fn new() -> Self {
        Self {
            should_fail: Arc::new(Mutex::new(false)),
            sent_data: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[allow(dead_code)]
    fn set_should_fail(&self, fail: bool) {
        *self.should_fail.lock().unwrap() = fail;
    }
}

#[async_trait::async_trait]
impl TransportConnector for MockConnector {
    async fn connect(
        &self,
        _addr: SocketAddr,
    ) -> Result<Box<dyn TransportConnection>, TransportError> {
        if *self.should_fail.lock().unwrap() {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "mock connection refused",
            )));
        }
        Ok(Box::new(MockConnection::new(self.sent_data.clone())))
    }
}

/// Wraps Arc<MockConnector> so it can be used as Box<dyn TransportConnector>
/// while still allowing shared access for configuration and assertions.
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

fn pid(node: u64, local: u64) -> ProcessId {
    ProcessId::new(node, local)
}

// ─── Test 1: two_nodes_discover_via_swim ───────────────────────────────────

/// Two nodes discover each other via SWIM gossip.
///
/// Node A gossips about itself so Node B can discover it.
/// Node B gossips about itself so Node A can discover it.
/// After the exchange, both membership lists see the other node as Alive.
#[tokio::test]
async fn two_nodes_discover_via_swim() {
    // Node A (id=1) starts with itself in its membership list
    let mut list_a = MembershipList::new();
    list_a.add(Member::new(1, test_addr(5000)));

    // Node B (id=2) starts with itself in its membership list
    let mut list_b = MembershipList::new();
    list_b.add(Member::new(2, test_addr(5001)));

    // Node A prepares gossip about itself to send to B
    let mut gossip_from_a = GossipQueue::new();
    gossip_from_a.add(GossipUpdate::Alive {
        node_id: 1,
        addr: test_addr(5000),
        incarnation: 0,
        cert_hash: None,
    });

    // Node B prepares gossip about itself to send to A
    let mut gossip_from_b = GossipQueue::new();
    gossip_from_b.add(GossipUpdate::Alive {
        node_id: 2,
        addr: test_addr(5001),
        incarnation: 0,
        cert_hash: None,
    });

    // Apply gossip from A to B's list — B discovers node 1
    let updates_a_to_b = gossip_from_a.drain(10);
    for update in &updates_a_to_b {
        if let GossipUpdate::Alive {
            node_id,
            addr,
            incarnation,
            cert_hash: _,
        } = update
        {
            let member = Member::new(*node_id, *addr);
            list_b.add(member);
            if let Some(m) = list_b.get_mut(*node_id) {
                m.alive(*incarnation);
            }
        }
    }

    // Apply gossip from B to A's list — A discovers node 2
    let updates_b_to_a = gossip_from_b.drain(10);
    for update in &updates_b_to_a {
        if let GossipUpdate::Alive {
            node_id,
            addr,
            incarnation,
            cert_hash: _,
        } = update
        {
            let member = Member::new(*node_id, *addr);
            list_a.add(member);
            if let Some(m) = list_a.get_mut(*node_id) {
                m.alive(*incarnation);
            }
        }
    }

    // Verify: Node A's list has node 2 as Alive
    let b_in_a = list_a.get(2).expect("A should know about node 2");
    assert_eq!(b_in_a.state, NodeState::Alive);

    // Verify: Node B's list has node 1 as Alive
    let a_in_b = list_b.get(1).expect("B should know about node 1");
    assert_eq!(a_in_b.state, NodeState::Alive);

    // Both lists have 2 alive members (self + peer)
    assert_eq!(list_a.alive_count(), 2);
    assert_eq!(list_b.alive_count(), 2);
}

// ─── Test 2: send_message_across_nodes ─────────────────────────────────────

/// Start two TCP listeners on ephemeral ports, connect from node A to node B,
/// send a frame, and verify node B receives it correctly.
#[tokio::test]
async fn send_message_across_nodes() {
    let transport = TcpTransport::new();

    // Node B listens on ephemeral port
    let listener_b = transport
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr_b = listener_b.local_addr();

    // Server task: Node B accepts and receives one frame
    let server = tokio::spawn(async move {
        let mut conn = listener_b.accept().await.unwrap();
        conn.recv().await.unwrap()
    });

    // Node A connects to Node B and sends a message
    let mut client = transport.connect(addr_b).await.unwrap();
    let frame = Frame {
        version: 1,
        msg_type: MsgType::Send,
        request_id: 42,
        header: rmpv::Value::Map(vec![(
            rmpv::Value::String("from".into()),
            rmpv::Value::Integer(1.into()),
        )]),
        payload: rmpv::Value::String("hello from node A".into()),
    };
    client.send(&frame).await.unwrap();
    client.close().await.unwrap();

    // Verify the received frame
    let timeout = tokio::time::timeout(Duration::from_secs(5), server);
    let received = timeout.await.expect("server timed out").unwrap();
    assert_eq!(received.version, 1);
    assert_eq!(received.msg_type, MsgType::Send);
    assert_eq!(received.request_id, 42);
    assert_eq!(
        received.payload,
        rmpv::Value::String("hello from node A".into())
    );
}

// ─── Test 3: remote_process_monitor_fires_on_exit ──────────────────────────

/// Simulate the monitor protocol flow over TCP:
/// 1. Node A sends a Monitor request frame to Node B.
/// 2. Node B receives it and verifies the message type.
/// 3. Node B sends a ProcessDown frame back to Node A.
/// 4. Node A verifies the ProcessDown frame.
#[tokio::test]
async fn remote_process_monitor_fires_on_exit() {
    let transport = TcpTransport::new();

    let listener_b = transport
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr_b = listener_b.local_addr();

    // Node B: accept connection, receive Monitor request, send ProcessDown back
    let server = tokio::spawn(async move {
        let mut conn = listener_b.accept().await.unwrap();

        // Receive the monitor request
        let monitor_req = conn.recv().await.unwrap();
        assert_eq!(monitor_req.msg_type, MsgType::Monitor);

        // Extract the monitored PID from the payload
        let monitored_pid = monitor_req.payload.clone();

        // Send ProcessDown back (simulating the monitored process exiting)
        let down_frame = Frame {
            version: 1,
            msg_type: MsgType::ProcessDown,
            request_id: monitor_req.request_id,
            header: rmpv::Value::Map(vec![(
                rmpv::Value::String("reason".into()),
                rmpv::Value::String("normal".into()),
            )]),
            payload: monitored_pid,
        };
        conn.send(&down_frame).await.unwrap();
    });

    // Node A: connect, send Monitor request, receive ProcessDown
    let mut client = transport.connect(addr_b).await.unwrap();

    let monitor_frame = Frame {
        version: 1,
        msg_type: MsgType::Monitor,
        request_id: 100,
        header: rmpv::Value::Nil,
        payload: rmpv::Value::Map(vec![
            (
                rmpv::Value::String("node_id".into()),
                rmpv::Value::Integer(2.into()),
            ),
            (
                rmpv::Value::String("local_id".into()),
                rmpv::Value::Integer(42.into()),
            ),
        ]),
    };
    client.send(&monitor_frame).await.unwrap();

    // Receive the ProcessDown response
    let timeout = tokio::time::timeout(Duration::from_secs(5), client.recv());
    let down = timeout
        .await
        .expect("timed out waiting for ProcessDown")
        .unwrap();

    assert_eq!(down.msg_type, MsgType::ProcessDown);
    assert_eq!(down.request_id, 100);

    // Verify the reason header
    let reason = down
        .header
        .as_map()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_str() == Some("reason"))
        .map(|(_, v)| v.as_str().unwrap().to_string());
    assert_eq!(reason, Some("normal".to_string()));

    let timeout = tokio::time::timeout(Duration::from_secs(5), server);
    timeout.await.expect("server timed out").unwrap();
}

// ─── Test 4: registry_name_resolves_across_nodes ───────────────────────────

/// Two registries on different nodes. A name registered on Registry A is
/// replicated to Registry B via deltas. Registry B can then look up the name
/// and resolve it to the correct PID.
#[tokio::test]
async fn registry_name_resolves_across_nodes() {
    let mut reg_a = Registry::new();
    let mut reg_b = Registry::new();

    let server_pid = pid(1, 100);

    // Node A registers "global_server"
    reg_a.register("global_server", server_pid, 1, 1000);

    // Generate deltas from A's full state
    let deltas = reg_a.generate_deltas();
    assert!(!deltas.is_empty(), "should have at least one delta");

    // Apply deltas to B (simulating replication)
    for delta in deltas {
        // Only apply Add deltas (Remove deltas with empty name are tombstone markers)
        match &delta {
            RegistryDelta::Add(entry) => {
                assert_eq!(entry.name, "global_server");
                assert_eq!(entry.pid, server_pid);
                assert_eq!(entry.node_id, 1);
            }
            RegistryDelta::Remove { .. } => {
                // Tombstone markers from generate_deltas — still valid to merge
            }
        }
        reg_b.merge_delta(delta);
    }

    // Verify B can look up the name
    let entry = reg_b
        .lookup("global_server")
        .expect("B should resolve global_server");
    assert_eq!(entry.pid, server_pid);
    assert_eq!(entry.node_id, 1);
    assert_eq!(entry.timestamp, 1000);

    // Verify both registries agree
    let a_entry = reg_a.lookup("global_server").unwrap();
    assert_eq!(a_entry.pid, entry.pid);
}

// ─── Test 5: node_down_fires_when_node_disconnects ─────────────────────────

/// Use the ConnectionManager with a mock transport. Connect to a node, then
/// trigger on_connection_lost. Verify the NodeDown event is emitted.
#[tokio::test]
async fn node_down_fires_when_node_disconnects() {
    let connector = Arc::new(MockConnector::new());
    let mut mgr = ConnectionManager::new(Box::new(ArcConnector(connector.clone())));

    // Connect to node 5
    mgr.connect(5, test_addr(6000)).await.unwrap();
    assert!(mgr.is_connected(5));
    assert_eq!(mgr.connection_count(), 1);

    // Simulate connection loss
    let events = mgr.on_connection_lost(5).await;

    // Verify NodeDown event was emitted
    assert!(
        events.contains(&ConnectionEvent::NodeDown(5)),
        "should emit NodeDown(5), got: {:?}",
        events
    );

    // Node should no longer be connected
    assert!(!mgr.is_connected(5));
    assert_eq!(mgr.connection_count(), 0);
}

// ─── Test 6: reconnection_after_transient_failure ──────────────────────────

/// Connect to a node, simulate connection loss, then use attempt_reconnect
/// to restore the connection. Verify the node is connected again afterward.
#[tokio::test]
async fn reconnection_after_transient_failure() {
    let connector = Arc::new(MockConnector::new());
    let mut mgr = ConnectionManager::new(Box::new(ArcConnector(connector.clone())));

    // Connect to node 10
    mgr.connect(10, test_addr(7000)).await.unwrap();
    assert!(mgr.is_connected(10));

    // Simulate transient failure — connection lost
    let events = mgr.on_connection_lost(10).await;
    assert!(events.contains(&ConnectionEvent::NodeDown(10)));
    assert!(events.contains(&ConnectionEvent::ReconnectTriggered(10)));
    assert!(!mgr.is_connected(10));

    // Attempt reconnection (connector still succeeds)
    let result = mgr.attempt_reconnect(10).await;
    assert!(result.is_ok(), "reconnect should succeed");

    // Verify the node is connected again
    assert!(mgr.is_connected(10));
    assert_eq!(mgr.connection_count(), 1);

    // Verify reconnect attempt counter was reset
    assert_eq!(mgr.reconnect_attempt_count(10), 0);
}

// ─── Test 7: three_node_mesh_all_connected ─────────────────────────────────

/// Simulate three nodes, each with a ConnectionManager, forming a full mesh.
/// Each node connects to the other two. Verify each has connection_count() == 2.
#[tokio::test]
async fn three_node_mesh_all_connected() {
    // Create three independent ConnectionManagers (one per node)
    let connector_1 = Arc::new(MockConnector::new());
    let connector_2 = Arc::new(MockConnector::new());
    let connector_3 = Arc::new(MockConnector::new());

    let mut mgr_1 = ConnectionManager::new(Box::new(ArcConnector(connector_1.clone())));
    let mut mgr_2 = ConnectionManager::new(Box::new(ArcConnector(connector_2.clone())));
    let mut mgr_3 = ConnectionManager::new(Box::new(ArcConnector(connector_3.clone())));

    // Node 1 connects to nodes 2 and 3
    mgr_1.connect(2, test_addr(8001)).await.unwrap();
    mgr_1.connect(3, test_addr(8002)).await.unwrap();

    // Node 2 connects to nodes 1 and 3
    mgr_2.connect(1, test_addr(8000)).await.unwrap();
    mgr_2.connect(3, test_addr(8002)).await.unwrap();

    // Node 3 connects to nodes 1 and 2
    mgr_3.connect(1, test_addr(8000)).await.unwrap();
    mgr_3.connect(2, test_addr(8001)).await.unwrap();

    // Verify each node has exactly 2 connections (full mesh)
    assert_eq!(
        mgr_1.connection_count(),
        2,
        "node 1 should be connected to 2 peers"
    );
    assert_eq!(
        mgr_2.connection_count(),
        2,
        "node 2 should be connected to 2 peers"
    );
    assert_eq!(
        mgr_3.connection_count(),
        2,
        "node 3 should be connected to 2 peers"
    );

    // Verify specific connections
    assert!(mgr_1.is_connected(2));
    assert!(mgr_1.is_connected(3));
    assert!(!mgr_1.is_connected(1)); // not connected to self

    assert!(mgr_2.is_connected(1));
    assert!(mgr_2.is_connected(3));
    assert!(!mgr_2.is_connected(2));

    assert!(mgr_3.is_connected(1));
    assert!(mgr_3.is_connected(2));
    assert!(!mgr_3.is_connected(3));
}

// ─── Test 8: end_to_end_cross_node_send_via_tcp ────────────────────────────

/// End-to-end: two DistributedRuntimes send messages via TCP transport.
#[tokio::test]
async fn end_to_end_cross_node_send_via_tcp() {
    struct TcpConnector;

    #[async_trait::async_trait]
    impl TransportConnector for TcpConnector {
        async fn connect(
            &self,
            addr: std::net::SocketAddr,
        ) -> Result<Box<dyn TransportConnection>, TransportError> {
            let transport = TcpTransport::new();
            let conn = transport.connect(addr).await?;
            Ok(Box::new(conn))
        }
    }

    // --- Node 1 setup ---
    let table1 = Arc::new(ProcessTable::new(1));
    let (remote_tx1, mut remote_rx1) = mpsc::channel::<RouterCommand>(64);
    let router1 = Arc::new(DistributedRouter::new(1, Arc::clone(&table1), remote_tx1));
    let rt1 = Runtime::with_router(1, Arc::clone(&table1), router1);

    // --- Node 2 setup ---
    let table2 = Arc::new(ProcessTable::new(2));
    let (remote_tx2, _remote_rx2) = mpsc::channel::<RouterCommand>(64);
    let router2 = Arc::new(DistributedRouter::new(2, Arc::clone(&table2), remote_tx2));
    let rt2 = Runtime::with_router(2, Arc::clone(&table2), router2);

    // Node 2: start TCP listener
    let transport2 = TcpTransport::new();
    let listener = transport2
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let node2_addr = listener.local_addr();

    // Node 2: spawn a receiver process
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let receiver_pid = rt2
        .spawn(move |mut ctx| async move {
            let msg = ctx.recv().await.unwrap();
            done_tx
                .send((
                    msg.from().node_id(),
                    msg.payload().as_str().unwrap().to_string(),
                ))
                .unwrap();
        })
        .await;

    // Node 2: accept connection and deliver inbound frames
    let table2_clone = Arc::clone(&table2);
    let server = tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap();
        loop {
            match conn.recv().await {
                Ok(frame) => {
                    deliver_inbound_frame(&table2_clone, &frame)
                        .expect("inbound frame delivery should succeed");
                }
                Err(_) => break,
            }
        }
    });

    // Node 1: connect to node 2
    let mut mgr1 = ConnectionManager::new(Box::new(TcpConnector));
    mgr1.connect(2, node2_addr).await.unwrap();

    // Node 1: send message to receiver_pid on node 2
    rt1.send(receiver_pid, rmpv::Value::String("cross-node-hello".into()))
        .await
        .unwrap();

    // Node 1: process the outbound message (drain remote_rx1 → connection_manager)
    if let Some(RouterCommand::Send { node_id, frame }) = remote_rx1.recv().await {
        assert_eq!(node_id, 2);
        mgr1.route(2, &frame).await.unwrap();
    }

    // Node 2: verify message received
    let (from_node, payload) = tokio::time::timeout(std::time::Duration::from_secs(5), done_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(from_node, 1);
    assert_eq!(payload, "cross-node-hello");

    // Ensure the background accept task completed cleanly
    let _ = tokio::time::timeout(Duration::from_secs(1), server).await;
}
