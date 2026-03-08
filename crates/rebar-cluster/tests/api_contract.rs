//! API contract regression tests for rebar-cluster.
//!
//! These tests guard the public API surface that downstream crates (especially
//! the `rebar` facade) depend on.  If any of these tests break, it means the
//! cluster crate's contract has changed in a way that would break consumers.

// ---------------------------------------------------------------------------
// Transport trait contracts
// ---------------------------------------------------------------------------

/// Guards that TcpConnection implements TransportConnection + Send + Sync.
/// The v5 fork removed Send + Sync bounds from the transport traits, which
/// broke tokio::spawn usage across the codebase.
#[test]
fn transport_connection_requires_send_sync() {
    fn assert_send_sync<T: rebar_cluster::transport::TransportConnection + Send + Sync>() {}
    assert_send_sync::<rebar_cluster::transport::tcp::TcpConnection>();
}

/// Guards that TcpTransportListener implements TransportListener + Send + Sync.
/// Listeners must be Send + Sync so they can be moved into spawned tasks.
#[test]
fn transport_listener_requires_send_sync() {
    fn assert_send_sync<T: rebar_cluster::transport::TransportListener + Send + Sync>() {}
    assert_send_sync::<rebar_cluster::transport::tcp::TcpTransportListener>();
}

/// Guards that QuicTransportConnector implements TransportConnector + Send + Sync.
/// The ConnectionManager holds a Box<dyn TransportConnector> which requires
/// Send + Sync for use across async task boundaries.
#[test]
fn transport_connector_requires_send_sync() {
    fn assert_send_sync<T: rebar_cluster::connection::manager::TransportConnector + Send + Sync>() {
    }
    assert_send_sync::<rebar_cluster::transport::quic::QuicTransportConnector>();
}

/// Guards that TransportConnector can be used as a trait object (Box<dyn>).
/// The ConnectionManager accepts Box<dyn TransportConnector>, so the trait
/// must remain object-safe.
#[test]
fn transport_connector_returns_boxed_dyn_connection() {
    fn accepts_boxed(_connector: Box<dyn rebar_cluster::connection::manager::TransportConnector>) {}
    // Compiling this function is the test — it proves the trait is object-safe.
}

// ---------------------------------------------------------------------------
// QUIC transport
// ---------------------------------------------------------------------------

/// Guards that QuicTransport and related types exist and can be constructed
/// from a generated certificate.  The v5 fork removed quinn/rustls/rcgen
/// dependencies entirely.
#[test]
fn quic_transport_types_exist() {
    let (cert, key, _hash) = rebar_cluster::transport::quic::generate_self_signed_cert();
    let _transport = rebar_cluster::transport::quic::QuicTransport::new(cert, key);
}

/// Guards that certificate generation produces non-empty certs with
/// deterministic fingerprints.  The fingerprint is used for peer
/// authentication in the QUIC transport.
#[test]
fn quic_cert_generation_works() {
    let (cert, _key, hash) = rebar_cluster::transport::quic::generate_self_signed_cert();

    // Cert must be non-empty
    assert!(
        !cert.as_ref().is_empty(),
        "generated certificate must not be empty"
    );

    // Hash must be non-zero (SHA-256 of any non-empty input is never all-zero)
    assert_ne!(hash, [0u8; 32], "certificate hash must not be all zeros");

    // Fingerprint must be deterministic: recomputing from the same cert
    // must yield the same hash.
    let recomputed = rebar_cluster::transport::quic::cert_fingerprint(&cert);
    assert_eq!(
        hash, recomputed,
        "cert_fingerprint must be deterministic for the same certificate"
    );
}

/// Guards the QUIC send/recv roundtrip: listen on 127.0.0.1:0, send a Frame
/// from a client, receive it on the server, and verify all fields.
/// This ensures the full QUIC transport stack (quinn + rustls + rcgen) works.
#[tokio::test]
async fn quic_send_recv_roundtrip() {
    use rebar_cluster::protocol::{Frame, MsgType};
    use rebar_cluster::transport::traits::{TransportConnection, TransportListener};
    use rebar_cluster::transport::quic::{QuicTransport, generate_self_signed_cert};

    let (cert, key, hash) = generate_self_signed_cert();
    let transport = QuicTransport::new(cert, key);
    let listener = transport
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr();

    let server = tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap();
        conn.recv().await.unwrap()
    });

    let mut client = transport.connect(addr, hash).await.unwrap();
    let frame = Frame {
        version: 1,
        msg_type: MsgType::Send,
        request_id: 42,
        header: rmpv::Value::Nil,
        payload: rmpv::Value::String("quic-roundtrip".into()),
    };
    client.send(&frame).await.unwrap();

    let received = server.await.unwrap();
    assert_eq!(received.version, 1);
    assert_eq!(received.msg_type, MsgType::Send);
    assert_eq!(received.request_id, 42);
    assert_eq!(
        received.payload,
        rmpv::Value::String("quic-roundtrip".into())
    );
}

// ---------------------------------------------------------------------------
// DistributedRuntime
// ---------------------------------------------------------------------------

/// Guards that DistributedRuntime is NOT generic — it is a concrete type
/// with no type parameters.  The v5 fork made it generic over the transport,
/// which broke the facade crate's public API.
#[test]
fn distributed_runtime_is_not_generic() {
    fn accepts_ref(_rt: &rebar::DistributedRuntime) {}
    // Compiling this function is the test — if DistributedRuntime were generic,
    // this would fail because the type parameters would be unresolved.
}

// ---------------------------------------------------------------------------
// ConnectionManager
// ---------------------------------------------------------------------------

/// Guards that ConnectionManager::new accepts Box<dyn TransportConnector>.
/// The v5 fork made ConnectionManager generic over the connector type,
/// which broke dynamic dispatch and the ability to swap transports at runtime.
#[test]
fn connection_manager_accepts_boxed_dyn_connector() {
    use rebar_cluster::connection::manager::{ConnectionManager, TransportConnector};
    use rebar_cluster::transport::{TransportConnection, TransportError};

    struct DummyConnector;

    #[async_trait::async_trait]
    impl TransportConnector for DummyConnector {
        async fn connect(
            &self,
            _addr: std::net::SocketAddr,
        ) -> Result<Box<dyn TransportConnection>, TransportError> {
            Err(TransportError::ConnectionClosed)
        }
    }

    let _mgr = ConnectionManager::new(Box::new(DummyConnector));
}

// ---------------------------------------------------------------------------
// Wire protocol
// ---------------------------------------------------------------------------

/// Guards that all 13 MsgType variants exist with correct u8 discriminants.
/// The wire protocol is critical for cluster interop — changing discriminant
/// values would silently corrupt cross-node communication.
#[test]
fn msg_type_variants_complete() {
    use rebar_cluster::protocol::MsgType;

    // All 13 variants with their expected discriminant values
    let variants: [(MsgType, u8); 13] = [
        (MsgType::Send, 0x01),
        (MsgType::Monitor, 0x02),
        (MsgType::Demonitor, 0x03),
        (MsgType::Link, 0x04),
        (MsgType::Unlink, 0x05),
        (MsgType::Exit, 0x06),
        (MsgType::ProcessDown, 0x07),
        (MsgType::NameLookup, 0x08),
        (MsgType::NameRegister, 0x09),
        (MsgType::NameUnregister, 0x0A),
        (MsgType::Heartbeat, 0x0B),
        (MsgType::HeartbeatAck, 0x0C),
        (MsgType::NodeInfo, 0x0D),
    ];

    for (variant, expected_value) in &variants {
        assert_eq!(
            *variant as u8, *expected_value,
            "MsgType::{:?} should have discriminant 0x{:02X}",
            variant, expected_value
        );
    }

    // Values outside the valid range must be rejected
    assert!(
        MsgType::from_u8(0x00).is_err(),
        "0x00 must not be a valid MsgType"
    );
    assert!(
        MsgType::from_u8(0x0E).is_err(),
        "0x0E must not be a valid MsgType"
    );
}

/// Guards that Frame encode/decode is deterministic: encoding the same frame
/// twice produces identical bytes, and decoding recovers all fields.
#[test]
fn frame_encode_decode_deterministic() {
    use rebar_cluster::protocol::{Frame, MsgType};

    let frame = Frame {
        version: 1,
        msg_type: MsgType::Send,
        request_id: 12345,
        header: rmpv::Value::Map(vec![(
            rmpv::Value::String("key".into()),
            rmpv::Value::Integer(99.into()),
        )]),
        payload: rmpv::Value::String("deterministic".into()),
    };

    let bytes1 = frame.encode();
    let bytes2 = frame.encode();
    assert_eq!(bytes1, bytes2, "encoding the same frame twice must produce identical bytes");

    let decoded = Frame::decode(&bytes1).unwrap();
    assert_eq!(decoded.version, 1);
    assert_eq!(decoded.msg_type, MsgType::Send);
    assert_eq!(decoded.request_id, 12345);
    assert_eq!(
        decoded.payload,
        rmpv::Value::String("deterministic".into())
    );
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Guards that DistributedRouter implements Send + Sync.  The router is
/// wrapped in Arc and shared across async tasks — losing Send + Sync
/// would break the runtime's message routing.
#[test]
fn distributed_router_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<rebar_cluster::router::DistributedRouter>();
}
