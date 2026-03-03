# Rebar Roadmap Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement the three remaining roadmap features — QUIC transport, distribution layer integration, and graceful node drain — using strict TDD.

**Architecture:** Three features with a dependency chain: QUIC transport is standalone, distribution layer uses the existing TCP transport for testing (QUIC for production), and graceful drain builds on top of distribution. Each feature follows the `MessageRouter` trait pattern to keep `rebar-core` decoupled from `rebar-cluster`.

**Tech Stack:** Rust 2024, tokio 1, quinn 0.11, rustls 0.23, rmpv 1, async-trait 0.1, dashmap 6

**Cargo path:** `~/.cargo/bin/cargo`

**Baseline:** 229 tests passing across all crates.

---

## Feature 1: QUIC Transport

### Task 1: Self-signed certificate generation

**Files:**
- Create: `crates/rebar-cluster/src/transport/quic.rs`
- Modify: `crates/rebar-cluster/src/transport/mod.rs:1-5`
- Modify: `crates/rebar-cluster/Cargo.toml:9-22`

**Step 1: Add rcgen dependency for cert generation**

In `crates/rebar-cluster/Cargo.toml`, add `rcgen` after `rustls`:

```toml
rcgen = "0.13"
```

**Step 2: Write the failing tests**

Create `crates/rebar-cluster/src/transport/quic.rs`:

```rust
use std::net::SocketAddr;
use std::sync::Arc;

use quinn::{ClientConfig, Endpoint, ServerConfig};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::protocol::Frame;
use crate::transport::{TransportConnection, TransportError, TransportListener};

/// SHA-256 fingerprint of a DER-encoded certificate.
pub type CertHash = [u8; 32];

/// Generate a self-signed certificate and private key for QUIC transport.
/// Returns (certificate, private key, SHA-256 fingerprint).
pub fn generate_self_signed_cert() -> (CertificateDer<'static>, PrivateKeyDer<'static>, CertHash) {
    todo!()
}

/// Compute the SHA-256 fingerprint of a DER-encoded certificate.
pub fn cert_fingerprint(cert: &CertificateDer<'_>) -> CertHash {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_cert_returns_valid_cert() {
        let (cert, key, hash) = generate_self_signed_cert();
        assert!(!cert.is_empty());
        // Key should be non-empty PKCS8
        match &key {
            PrivateKeyDer::Pkcs8(k) => assert!(!k.secret_pkcs8_der().is_empty()),
            _ => panic!("expected PKCS8 key"),
        }
        // Hash should be non-zero
        assert_ne!(hash, [0u8; 32]);
    }

    #[test]
    fn cert_fingerprint_is_deterministic() {
        let (cert, _, hash1) = generate_self_signed_cert();
        let hash2 = cert_fingerprint(&cert);
        // Same cert -> same hash
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn different_certs_have_different_fingerprints() {
        let (_, _, hash1) = generate_self_signed_cert();
        let (_, _, hash2) = generate_self_signed_cert();
        // Two independently generated certs should differ
        assert_ne!(hash1, hash2);
    }
}
```

**Step 3: Register module**

In `crates/rebar-cluster/src/transport/mod.rs`, replace contents with:

```rust
pub mod traits;
pub mod tcp;
pub mod quic;

pub use traits::*;
pub use tcp::TcpTransport;
pub use quic::{generate_self_signed_cert, cert_fingerprint, CertHash};
```

**Step 4: Run tests to verify they fail**

Run: `~/.cargo/bin/cargo test -p rebar-cluster transport::quic::tests --no-default-features 2>&1 | tail -10`
Expected: FAIL with "not yet implemented"

**Step 5: Implement certificate generation**

Replace the `todo!()` bodies in `quic.rs`:

```rust
pub fn generate_self_signed_cert() -> (CertificateDer<'static>, PrivateKeyDer<'static>, CertHash) {
    let key_pair = rcgen::KeyPair::generate().expect("key generation failed");
    let cert = rcgen::CertificateParams::new(vec!["rebar-node".to_string()])
        .expect("cert params failed")
        .self_signed(&key_pair)
        .expect("self-sign failed");

    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
    let hash = cert_fingerprint(&cert_der);

    (cert_der, key_der, hash)
}

pub fn cert_fingerprint(cert: &CertificateDer<'_>) -> CertHash {
    use std::hash::Hasher;
    // SHA-256 via ring (rustls dependency) or manual
    // Use simple approach: rustls already pulls in ring
    let digest = ring::digest::digest(&ring::digest::SHA256, cert.as_ref());
    let mut hash = [0u8; 32];
    hash.copy_from_slice(digest.as_ref());
    hash
}
```

Note: If `ring` is not directly available, add `ring = "0.17"` to `Cargo.toml`, or use `sha2` crate. Check which is available via rustls transitive deps. Alternative using `sha2`:

```toml
# In Cargo.toml [dependencies]:
sha2 = "0.10"
```

```rust
pub fn cert_fingerprint(cert: &CertificateDer<'_>) -> CertHash {
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    hasher.update(cert.as_ref());
    hasher.finalize().into()
}
```

**Step 6: Run tests to verify they pass**

Run: `~/.cargo/bin/cargo test -p rebar-cluster transport::quic::tests 2>&1 | tail -10`
Expected: 3 tests PASS

**Step 7: Commit**

```bash
~/.cargo/bin/cargo fmt --all
git add crates/rebar-cluster/src/transport/quic.rs crates/rebar-cluster/src/transport/mod.rs crates/rebar-cluster/Cargo.toml
git commit -m "feat(quic): add self-signed cert generation with SHA-256 fingerprinting"
```

---

### Task 2: QUIC connection — send and recv

**Files:**
- Modify: `crates/rebar-cluster/src/transport/quic.rs`

**Step 1: Write the failing tests**

Append to the `tests` module in `quic.rs`:

```rust
    #[tokio::test]
    async fn quic_send_recv_single_frame() {
        let (server_cert, server_key, server_hash) = generate_self_signed_cert();
        let server = QuicTransport::new(server_cert.clone(), server_key);

        let listener = server.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let server_addr = listener.local_addr();

        let (client_cert, client_key, _) = generate_self_signed_cert();
        let client = QuicTransport::new(client_cert, client_key);

        let server_handle = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            conn.recv().await.unwrap()
        });

        let mut client_conn = client.connect(server_addr, server_hash).await.unwrap();
        let frame = Frame {
            version: 1,
            msg_type: crate::protocol::MsgType::Heartbeat,
            request_id: 0,
            header: rmpv::Value::Nil,
            payload: rmpv::Value::Nil,
        };
        client_conn.send(&frame).await.unwrap();
        // Close to signal we're done sending
        client_conn.close().await.unwrap();

        let received = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            server_handle,
        ).await.unwrap().unwrap();
        assert_eq!(received.msg_type, crate::protocol::MsgType::Heartbeat);
        assert_eq!(received.version, 1);
    }

    #[tokio::test]
    async fn quic_send_recv_multiple_frames() {
        let (server_cert, server_key, server_hash) = generate_self_signed_cert();
        let server = QuicTransport::new(server_cert.clone(), server_key);

        let listener = server.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let server_addr = listener.local_addr();

        let (client_cert, client_key, _) = generate_self_signed_cert();
        let client = QuicTransport::new(client_cert, client_key);

        let server_handle = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let mut frames = Vec::new();
            for _ in 0..3 {
                frames.push(conn.recv().await.unwrap());
            }
            frames
        });

        let mut client_conn = client.connect(server_addr, server_hash).await.unwrap();
        for i in 0..3u64 {
            let frame = Frame {
                version: 1,
                msg_type: crate::protocol::MsgType::Send,
                request_id: i,
                header: rmpv::Value::Nil,
                payload: rmpv::Value::Integer(i.into()),
            };
            client_conn.send(&frame).await.unwrap();
        }
        client_conn.close().await.unwrap();

        let frames = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            server_handle,
        ).await.unwrap().unwrap();
        assert_eq!(frames.len(), 3);
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f.request_id, i as u64);
        }
    }

    #[tokio::test]
    async fn quic_large_payload() {
        let (server_cert, server_key, server_hash) = generate_self_signed_cert();
        let server = QuicTransport::new(server_cert.clone(), server_key);

        let listener = server.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let server_addr = listener.local_addr();

        let (client_cert, client_key, _) = generate_self_signed_cert();
        let client = QuicTransport::new(client_cert, client_key);

        let large_data = vec![0xABu8; 64 * 1024]; // 64KB payload
        let frame = Frame {
            version: 1,
            msg_type: crate::protocol::MsgType::Send,
            request_id: 42,
            header: rmpv::Value::Nil,
            payload: rmpv::Value::Binary(large_data.clone()),
        };

        let server_handle = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            conn.recv().await.unwrap()
        });

        let mut client_conn = client.connect(server_addr, server_hash).await.unwrap();
        client_conn.send(&frame).await.unwrap();
        client_conn.close().await.unwrap();

        let received = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            server_handle,
        ).await.unwrap().unwrap();
        assert_eq!(received.payload, rmpv::Value::Binary(large_data));
    }
```

**Step 2: Run tests to verify they fail**

Run: `~/.cargo/bin/cargo test -p rebar-cluster transport::quic::tests 2>&1 | tail -10`
Expected: FAIL — `QuicTransport` not found

**Step 3: Implement QuicTransport, QuicListener, QuicConnection**

Add to `quic.rs` (above the tests module):

```rust
use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// QUIC transport using quinn. Each send opens a new unidirectional stream
/// (stream-per-frame model), eliminating head-of-line blocking.
pub struct QuicTransport {
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
}

impl QuicTransport {
    pub fn new(cert: CertificateDer<'static>, key: PrivateKeyDer<'static>) -> Self {
        Self { cert, key }
    }

    fn server_config(&self) -> ServerConfig {
        let cert_chain = vec![self.cert.clone()];
        let key = self.key.clone_key();
        let server_crypto = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain, key)
            .expect("server TLS config failed");
        ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)
                .expect("QUIC server config failed"),
        ))
    }

    fn client_config(expected_hash: CertHash) -> ClientConfig {
        let crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(FingerprintVerifier { expected_hash }))
            .with_no_client_auth();
        ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
                .expect("QUIC client config failed"),
        ))
    }

    pub async fn listen(&self, addr: SocketAddr) -> Result<QuicListener, TransportError> {
        let config = self.server_config();
        let endpoint = Endpoint::server(config, addr)
            .map_err(|e| TransportError::Io(e))?;
        Ok(QuicListener { endpoint })
    }

    pub async fn connect(
        &self,
        addr: SocketAddr,
        expected_cert_hash: CertHash,
    ) -> Result<QuicConnection, TransportError> {
        let client_config = Self::client_config(expected_cert_hash);
        let mut endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap())
            .map_err(|e| TransportError::Io(e))?;
        endpoint.set_default_client_config(client_config);
        let connection = endpoint
            .connect(addr, "rebar-node")
            .map_err(|e| TransportError::Io(std::io::Error::other(e)))?
            .await
            .map_err(|e| TransportError::Io(std::io::Error::other(e)))?;
        Ok(QuicConnection {
            connection,
            _endpoint: Some(endpoint),
        })
    }
}

pub struct QuicListener {
    endpoint: Endpoint,
}

#[async_trait]
impl TransportListener for QuicListener {
    type Connection = QuicConnection;

    fn local_addr(&self) -> SocketAddr {
        self.endpoint.local_addr().expect("endpoint has no local addr")
    }

    async fn accept(&self) -> Result<QuicConnection, TransportError> {
        let incoming = self.endpoint.accept().await
            .ok_or_else(|| TransportError::ConnectionClosed)?;
        let connection = incoming.await
            .map_err(|e| TransportError::Io(std::io::Error::other(e)))?;
        Ok(QuicConnection {
            connection,
            _endpoint: None,
        })
    }
}

pub struct QuicConnection {
    connection: quinn::Connection,
    /// Keep client endpoint alive for the connection's lifetime.
    _endpoint: Option<Endpoint>,
}

#[async_trait]
impl TransportConnection for QuicConnection {
    async fn send(&mut self, frame: &Frame) -> Result<(), TransportError> {
        let data = frame.encode();
        let mut stream = self.connection.open_uni().await
            .map_err(|e| TransportError::Io(std::io::Error::other(e)))?;
        // Write length prefix (4 bytes BE) then data
        stream.write_all(&(data.len() as u32).to_be_bytes()).await
            .map_err(|e| TransportError::Io(e.into()))?;
        stream.write_all(&data).await
            .map_err(|e| TransportError::Io(e.into()))?;
        stream.finish()
            .map_err(|e| TransportError::Io(std::io::Error::other(e)))?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Frame, TransportError> {
        let mut stream = self.connection.accept_uni().await
            .map_err(|e| TransportError::Io(std::io::Error::other(e)))?;
        // Read 4-byte length prefix
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await
            .map_err(|e| TransportError::Io(e.into()))?;
        let len = u32::from_be_bytes(len_buf) as usize;
        // Read frame data
        let mut data = vec![0u8; len];
        stream.read_exact(&mut data).await
            .map_err(|e| TransportError::Io(e.into()))?;
        Frame::decode(&data).map_err(TransportError::Frame)
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        self.connection.close(0u32.into(), b"done");
        Ok(())
    }
}

/// Custom certificate verifier that accepts a cert only if its SHA-256
/// fingerprint matches the expected hash (from SWIM gossip).
#[derive(Debug)]
struct FingerprintVerifier {
    expected_hash: CertHash,
}

impl rustls::client::danger::ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let actual_hash = cert_fingerprint(end_entity);
        if actual_hash == self.expected_hash {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("cert fingerprint mismatch".into()))
        }
    }

    fn verify_tls12_signature(
        &self, _: &[u8], _: &CertificateDer<'_>, _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self, _: &[u8], _: &CertificateDer<'_>, _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
```

**Step 4: Run tests to verify they pass**

Run: `~/.cargo/bin/cargo test -p rebar-cluster transport::quic::tests 2>&1 | tail -20`
Expected: 6 tests PASS (3 cert + 3 transport)

**Step 5: Run full test suite to verify no regressions**

Run: `~/.cargo/bin/cargo test --workspace 2>&1 | grep "^test result"`
Expected: All existing 229 tests still pass, plus new tests

**Step 6: Commit**

```bash
~/.cargo/bin/cargo fmt --all
git add -A
git commit -m "feat(quic): implement QUIC transport with stream-per-frame model"
```

---

### Task 3: QUIC connection close and concurrent streams

**Files:**
- Modify: `crates/rebar-cluster/src/transport/quic.rs` (tests module)

**Step 1: Write the failing tests**

Append to tests module in `quic.rs`:

```rust
    #[tokio::test]
    async fn quic_connection_close() {
        let (server_cert, server_key, server_hash) = generate_self_signed_cert();
        let server = QuicTransport::new(server_cert.clone(), server_key);

        let listener = server.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let server_addr = listener.local_addr();

        let (client_cert, client_key, _) = generate_self_signed_cert();
        let client = QuicTransport::new(client_cert, client_key);

        let server_handle = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            // After client closes, recv should return an error
            let result = conn.recv().await;
            result.is_err()
        });

        let mut client_conn = client.connect(server_addr, server_hash).await.unwrap();
        client_conn.close().await.unwrap();

        let got_error = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            server_handle,
        ).await.unwrap().unwrap();
        assert!(got_error);
    }

    #[tokio::test]
    async fn quic_concurrent_streams() {
        let (server_cert, server_key, server_hash) = generate_self_signed_cert();
        let server = QuicTransport::new(server_cert.clone(), server_key);

        let listener = server.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let server_addr = listener.local_addr();

        let (client_cert, client_key, _) = generate_self_signed_cert();
        let client = QuicTransport::new(client_cert, client_key);

        let server_handle = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let mut frames = Vec::new();
            for _ in 0..10 {
                frames.push(conn.recv().await.unwrap());
            }
            frames
        });

        let mut client_conn = client.connect(server_addr, server_hash).await.unwrap();

        // Send 10 frames as fast as possible (concurrent streams)
        let mut handles = Vec::new();
        for i in 0..10u64 {
            let frame = Frame {
                version: 1,
                msg_type: crate::protocol::MsgType::Send,
                request_id: i,
                header: rmpv::Value::Nil,
                payload: rmpv::Value::Integer(i.into()),
            };
            // Send sequentially but each opens a new stream internally
            client_conn.send(&frame).await.unwrap();
        }
        client_conn.close().await.unwrap();

        let frames = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            server_handle,
        ).await.unwrap().unwrap();
        assert_eq!(frames.len(), 10);
        // All request IDs should be present (order may vary with QUIC)
        let mut ids: Vec<u64> = frames.iter().map(|f| f.request_id).collect();
        ids.sort();
        assert_eq!(ids, (0..10).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn quic_cert_fingerprint_mismatch() {
        let (server_cert, server_key, _server_hash) = generate_self_signed_cert();
        let server = QuicTransport::new(server_cert.clone(), server_key);

        let listener = server.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let server_addr = listener.local_addr();

        let (client_cert, client_key, _) = generate_self_signed_cert();
        let client = QuicTransport::new(client_cert, client_key);

        // Use a wrong hash
        let wrong_hash = [0xFFu8; 32];
        let result = client.connect(server_addr, wrong_hash).await;
        assert!(result.is_err());
    }
```

**Step 2: Run tests to verify they pass (these should pass with existing impl)**

Run: `~/.cargo/bin/cargo test -p rebar-cluster transport::quic::tests 2>&1 | tail -15`
Expected: All 9 tests PASS. If any fail, fix the implementation.

**Step 3: Commit**

```bash
~/.cargo/bin/cargo fmt --all
git add crates/rebar-cluster/src/transport/quic.rs
git commit -m "test(quic): add connection close, concurrent stream, and cert mismatch tests"
```

---

### Task 4: QUIC TransportConnector for ConnectionManager

**Files:**
- Modify: `crates/rebar-cluster/src/transport/quic.rs`
- Modify: `crates/rebar-cluster/src/transport/mod.rs`

**Step 1: Write the failing test**

Append to tests module in `quic.rs`:

```rust
    #[tokio::test]
    async fn quic_connector_works_with_connection_manager() {
        use crate::connection::manager::{ConnectionManager, TransportConnector};

        let (server_cert, server_key, server_hash) = generate_self_signed_cert();
        let server = QuicTransport::new(server_cert.clone(), server_key);
        let listener = server.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let server_addr = listener.local_addr();

        let (client_cert, client_key, _) = generate_self_signed_cert();
        let connector = QuicTransportConnector::new(client_cert, client_key, server_hash);
        let mut mgr = ConnectionManager::new(Box::new(connector));

        let server_handle = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            conn.recv().await.unwrap()
        });

        mgr.connect(1, server_addr).await.unwrap();
        assert!(mgr.is_connected(1));

        let frame = Frame {
            version: 1,
            msg_type: crate::protocol::MsgType::Heartbeat,
            request_id: 0,
            header: rmpv::Value::Nil,
            payload: rmpv::Value::Nil,
        };
        mgr.route(1, &frame).await.unwrap();

        let received = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            server_handle,
        ).await.unwrap().unwrap();
        assert_eq!(received.msg_type, crate::protocol::MsgType::Heartbeat);
    }
```

**Step 2: Run test to verify it fails**

Run: `~/.cargo/bin/cargo test -p rebar-cluster quic_connector_works 2>&1 | tail -10`
Expected: FAIL — `QuicTransportConnector` not found

**Step 3: Implement QuicTransportConnector**

Add to `quic.rs` (above tests module):

```rust
/// Transport connector for QUIC, used by ConnectionManager.
/// Holds a client endpoint and the expected cert hash for all peers.
pub struct QuicTransportConnector {
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    /// For now, all peers share the same expected hash. In production,
    /// this would be looked up per-node from the SWIM membership list.
    expected_cert_hash: CertHash,
}

impl QuicTransportConnector {
    pub fn new(
        cert: CertificateDer<'static>,
        key: PrivateKeyDer<'static>,
        expected_cert_hash: CertHash,
    ) -> Self {
        Self { cert, key, expected_cert_hash }
    }
}

#[async_trait]
impl crate::connection::manager::TransportConnector for QuicTransportConnector {
    async fn connect(
        &self,
        addr: SocketAddr,
    ) -> Result<Box<dyn TransportConnection>, TransportError> {
        let transport = QuicTransport::new(self.cert.clone(), self.key.clone_key());
        let conn = transport.connect(addr, self.expected_cert_hash).await?;
        Ok(Box::new(conn))
    }
}
```

Update `crates/rebar-cluster/src/transport/mod.rs` to export it:

```rust
pub mod traits;
pub mod tcp;
pub mod quic;

pub use traits::*;
pub use tcp::TcpTransport;
pub use quic::{generate_self_signed_cert, cert_fingerprint, CertHash, QuicTransport, QuicTransportConnector};
```

**Step 4: Run test to verify it passes**

Run: `~/.cargo/bin/cargo test -p rebar-cluster quic_connector_works 2>&1 | tail -10`
Expected: PASS

**Step 5: Run full suite**

Run: `~/.cargo/bin/cargo test --workspace 2>&1 | grep "^test result"`
Expected: All tests pass

**Step 6: Commit**

```bash
~/.cargo/bin/cargo fmt --all
git add -A
git commit -m "feat(quic): add QuicTransportConnector for ConnectionManager integration"
```

---

### Task 5: Add cert_hash to SWIM gossip

**Files:**
- Modify: `crates/rebar-cluster/src/swim/member.rs:13-19`
- Modify: `crates/rebar-cluster/src/swim/gossip.rs:6-26`

**Step 1: Write the failing tests**

Append to tests in `member.rs`:

```rust
    #[test]
    fn member_with_cert_hash() {
        let hash = [42u8; 32];
        let mut member = Member::new(1, "127.0.0.1:4000".parse().unwrap());
        member.cert_hash = Some(hash);
        assert_eq!(member.cert_hash, Some(hash));
    }
```

Append to tests in `gossip.rs`:

```rust
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
```

**Step 2: Run tests to verify they fail**

Run: `~/.cargo/bin/cargo test -p rebar-cluster member_with_cert_hash 2>&1 | tail -5`
Expected: FAIL — no field `cert_hash`

**Step 3: Add cert_hash field to Member**

In `member.rs`, modify the `Member` struct (lines 13-19):

```rust
#[derive(Debug, Clone)]
pub struct Member {
    pub node_id: u64,
    pub addr: SocketAddr,
    pub state: NodeState,
    pub incarnation: u64,
    pub cert_hash: Option<[u8; 32]>,
}
```

Update `Member::new()` (lines 22-29):

```rust
    pub fn new(node_id: u64, addr: SocketAddr) -> Self {
        Self {
            node_id,
            addr,
            state: NodeState::Alive,
            incarnation: 0,
            cert_hash: None,
        }
    }
```

**Step 4: Add cert_hash to GossipUpdate::Alive**

In `gossip.rs`, modify the `Alive` variant (lines 8-12):

```rust
    Alive {
        node_id: u64,
        addr: SocketAddr,
        incarnation: u64,
        cert_hash: Option<[u8; 32]>,
    },
```

**Step 5: Fix all compilation errors**

Every place that constructs `GossipUpdate::Alive` or pattern-matches on it needs updating. Search with:
`grep -rn "GossipUpdate::Alive" crates/rebar-cluster/src/`

Add `cert_hash: None` to all existing constructions, and `cert_hash: _` or `cert_hash` to all pattern matches. Similarly update test constructions to include `cert_hash: None`.

**Step 6: Run tests to verify they pass**

Run: `~/.cargo/bin/cargo test --workspace 2>&1 | grep "^test result"`
Expected: All tests pass including new ones

**Step 7: Commit**

```bash
~/.cargo/bin/cargo fmt --all
git add -A
git commit -m "feat(swim): add cert_hash field to Member and GossipUpdate::Alive"
```

---

## Feature 2: Distribution Layer Integration

### Task 6: MessageRouter trait in rebar-core

**Files:**
- Create: `crates/rebar-core/src/router.rs`
- Modify: `crates/rebar-core/src/lib.rs:1-3`
- Modify: `crates/rebar-core/src/process/types.rs:77-83` (SendError)

**Step 1: Write the failing tests**

Create `crates/rebar-core/src/router.rs`:

```rust
use crate::process::{Message, ProcessId, SendError};
use crate::process::table::ProcessTable;
use std::sync::Arc;

/// Trait for routing messages between processes. Implementations decide
/// whether to deliver locally or over the network.
pub trait MessageRouter: Send + Sync {
    fn route(&self, from: ProcessId, to: ProcessId, payload: rmpv::Value) -> Result<(), SendError>;
}

/// Default router that delivers messages to the local ProcessTable.
pub struct LocalRouter {
    table: Arc<ProcessTable>,
}

impl LocalRouter {
    pub fn new(table: Arc<ProcessTable>) -> Self {
        Self { table }
    }
}

impl MessageRouter for LocalRouter {
    fn route(&self, from: ProcessId, to: ProcessId, payload: rmpv::Value) -> Result<(), SendError> {
        let msg = Message::new(from, payload);
        self.table.send(to, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::mailbox::Mailbox;
    use crate::process::table::ProcessHandle;

    #[test]
    fn local_router_delivers_locally() {
        let table = Arc::new(ProcessTable::new(1));
        let pid = table.allocate_pid();
        let (tx, mut rx) = Mailbox::unbounded();
        table.insert(pid, ProcessHandle::new(tx));

        let router = LocalRouter::new(table);
        let from = ProcessId::new(1, 0);
        router.route(from, pid, rmpv::Value::String("hello".into())).unwrap();

        // Verify message arrived
        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.payload().as_str().unwrap(), "hello");
    }

    #[test]
    fn local_router_rejects_unknown_pid() {
        let table = Arc::new(ProcessTable::new(1));
        let router = LocalRouter::new(table);
        let from = ProcessId::new(1, 0);
        let dead_pid = ProcessId::new(1, 999);

        let result = router.route(from, dead_pid, rmpv::Value::Nil);
        assert!(matches!(result, Err(SendError::ProcessDead(_))));
    }

    #[test]
    fn local_router_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LocalRouter>();
    }
}
```

**Step 2: Add try_recv to MailboxRx**

In `crates/rebar-core/src/process/mailbox.rs`, add a synchronous try_recv if it doesn't exist. Check first — if it does, skip this. If not, add to `MailboxRx` impl:

```rust
    pub fn try_recv(&mut self) -> Option<Message> {
        match &mut self.inner {
            RxInner::Unbounded(rx) => rx.try_recv().ok(),
            RxInner::Bounded(rx) => rx.try_recv().ok(),
        }
    }
```

**Step 3: Add NodeUnreachable variant to SendError**

In `crates/rebar-core/src/process/types.rs`, modify `SendError` (lines 77-83):

```rust
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("process dead: {0}")]
    ProcessDead(ProcessId),
    #[error("mailbox full for: {0}")]
    MailboxFull(ProcessId),
    #[error("node unreachable: {0}")]
    NodeUnreachable(u64),
}
```

**Step 4: Register module**

In `crates/rebar-core/src/lib.rs`:

```rust
pub mod process;
pub mod router;
pub mod runtime;
pub mod supervisor;
```

**Step 5: Run tests to verify they pass**

Run: `~/.cargo/bin/cargo test -p rebar-core router::tests 2>&1 | tail -10`
Expected: 3 tests PASS

**Step 6: Run full suite**

Run: `~/.cargo/bin/cargo test --workspace 2>&1 | grep "^test result"`
Expected: All tests pass

**Step 7: Commit**

```bash
~/.cargo/bin/cargo fmt --all
git add -A
git commit -m "feat(core): add MessageRouter trait and LocalRouter implementation"
```

---

### Task 7: Wire ProcessContext and Runtime to use MessageRouter

**Files:**
- Modify: `crates/rebar-core/src/runtime.rs:10-14` (ProcessContext struct)
- Modify: `crates/rebar-core/src/runtime.rs:38-41` (ProcessContext::send)
- Modify: `crates/rebar-core/src/runtime.rs:45-57` (Runtime struct and new)
- Modify: `crates/rebar-core/src/runtime.rs:72-104` (Runtime::spawn)
- Modify: `crates/rebar-core/src/runtime.rs:109-113` (Runtime::send)

**Step 1: Write the failing test**

Append to tests module in `runtime.rs`:

```rust
    #[tokio::test]
    async fn runtime_with_custom_router() {
        use crate::router::MessageRouter;
        use std::sync::atomic::{AtomicU64, Ordering};

        struct CountingRouter {
            count: AtomicU64,
            inner: crate::router::LocalRouter,
        }
        impl MessageRouter for CountingRouter {
            fn route(&self, from: ProcessId, to: ProcessId, payload: rmpv::Value) -> Result<(), SendError> {
                self.count.fetch_add(1, Ordering::Relaxed);
                self.inner.route(from, to, payload)
            }
        }

        let table = Arc::new(crate::process::table::ProcessTable::new(1));
        let counter = Arc::new(AtomicU64::new(0));
        let router = Arc::new(CountingRouter {
            count: AtomicU64::new(0),
            inner: crate::router::LocalRouter::new(Arc::clone(&table)),
        });
        let counter_ref = Arc::clone(&router);

        let rt = Runtime::with_router(1, Arc::clone(&table), router as Arc<dyn MessageRouter>);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();

        let receiver = rt.spawn(move |mut ctx| async move {
            let msg = ctx.recv().await.unwrap();
            done_tx.send(msg.payload().as_str().unwrap().to_string()).unwrap();
        }).await;

        rt.spawn(move |ctx| async move {
            ctx.send(receiver, rmpv::Value::String("routed".into())).await.unwrap();
        }).await;

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), done_rx).await.unwrap().unwrap();
        assert_eq!(result, "routed");
        assert!(counter_ref.count.load(Ordering::Relaxed) > 0);
    }
```

**Step 2: Run test to verify it fails**

Run: `~/.cargo/bin/cargo test -p rebar-core runtime_with_custom_router 2>&1 | tail -10`
Expected: FAIL — `with_router` not found

**Step 3: Modify ProcessContext to use router**

In `runtime.rs`, change ProcessContext:

```rust
use crate::router::{LocalRouter, MessageRouter};

pub struct ProcessContext {
    pid: ProcessId,
    rx: MailboxRx,
    router: Arc<dyn MessageRouter>,
}
```

Change `ProcessContext::send`:

```rust
    pub async fn send(&self, dest: ProcessId, payload: rmpv::Value) -> Result<(), SendError> {
        self.router.route(self.pid, dest, payload)
    }
```

**Step 4: Modify Runtime to use router**

```rust
pub struct Runtime {
    node_id: u64,
    table: Arc<ProcessTable>,
    router: Arc<dyn MessageRouter>,
}

impl Runtime {
    pub fn new(node_id: u64) -> Self {
        let table = Arc::new(ProcessTable::new(node_id));
        let router = Arc::new(LocalRouter::new(Arc::clone(&table)));
        Self { node_id, table, router }
    }

    pub fn with_router(node_id: u64, table: Arc<ProcessTable>, router: Arc<dyn MessageRouter>) -> Self {
        Self { node_id, table, router }
    }

    pub fn node_id(&self) -> u64 {
        self.node_id
    }

    pub fn table(&self) -> &Arc<ProcessTable> {
        &self.table
    }

    pub async fn spawn<F, Fut>(&self, handler: F) -> ProcessId
    where
        F: FnOnce(ProcessContext) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let pid = self.table.allocate_pid();
        let (tx, rx) = Mailbox::unbounded();

        let handle = ProcessHandle::new(tx);
        self.table.insert(pid, handle);

        let ctx = ProcessContext {
            pid,
            rx,
            router: Arc::clone(&self.router),
        };

        let table = Arc::clone(&self.table);

        tokio::spawn(async move {
            let inner = tokio::spawn(handler(ctx));
            let _ = inner.await;
            table.remove(&pid);
        });

        pid
    }

    pub async fn send(&self, dest: ProcessId, payload: rmpv::Value) -> Result<(), SendError> {
        let from = ProcessId::new(self.node_id, 0);
        self.router.route(from, dest, payload)
    }
}
```

**Step 5: Run tests to verify they pass**

Run: `~/.cargo/bin/cargo test -p rebar-core 2>&1 | tail -15`
Expected: All rebar-core tests pass including the new one

**Step 6: Run full suite**

Run: `~/.cargo/bin/cargo test --workspace 2>&1 | grep "^test result"`
Expected: All tests pass (existing tests use `Runtime::new()` which defaults to LocalRouter)

**Step 7: Commit**

```bash
~/.cargo/bin/cargo fmt --all
git add -A
git commit -m "feat(core): wire ProcessContext and Runtime to use MessageRouter"
```

---

### Task 8: DistributedRouter in rebar-cluster

**Files:**
- Create: `crates/rebar-cluster/src/router.rs`
- Modify: `crates/rebar-cluster/src/lib.rs`

**Step 1: Write the failing tests**

Create `crates/rebar-cluster/src/router.rs`:

```rust
use std::sync::Arc;

use rebar_core::process::{Message, ProcessId, SendError};
use rebar_core::process::table::ProcessTable;
use rebar_core::router::MessageRouter;

use crate::protocol::{Frame, MsgType};

use tokio::sync::mpsc;

/// Command sent from the router to the connection handler for remote delivery.
#[derive(Debug)]
pub enum RouterCommand {
    Send { node_id: u64, frame: Frame },
}

/// Router that handles both local and remote message delivery.
/// Local messages go to ProcessTable. Remote messages are encoded as
/// Frames and sent via a channel to the connection handler.
pub struct DistributedRouter {
    node_id: u64,
    table: Arc<ProcessTable>,
    remote_tx: mpsc::Sender<RouterCommand>,
}

impl DistributedRouter {
    pub fn new(
        node_id: u64,
        table: Arc<ProcessTable>,
        remote_tx: mpsc::Sender<RouterCommand>,
    ) -> Self {
        Self { node_id, table, remote_tx }
    }

    fn encode_send_frame(from: ProcessId, to: ProcessId, payload: rmpv::Value) -> Frame {
        Frame {
            version: 1,
            msg_type: MsgType::Send,
            request_id: 0,
            header: rmpv::Value::Map(vec![
                (rmpv::Value::String("from_node".into()), rmpv::Value::Integer(from.node_id().into())),
                (rmpv::Value::String("from_local".into()), rmpv::Value::Integer(from.local_id().into())),
                (rmpv::Value::String("to_node".into()), rmpv::Value::Integer(to.node_id().into())),
                (rmpv::Value::String("to_local".into()), rmpv::Value::Integer(to.local_id().into())),
            ]),
            payload,
        }
    }
}

impl MessageRouter for DistributedRouter {
    fn route(&self, from: ProcessId, to: ProcessId, payload: rmpv::Value) -> Result<(), SendError> {
        if to.node_id() == self.node_id {
            // Local delivery
            let msg = Message::new(from, payload);
            self.table.send(to, msg)
        } else {
            // Remote delivery
            let frame = Self::encode_send_frame(from, to, payload);
            self.remote_tx.try_send(RouterCommand::Send {
                node_id: to.node_id(),
                frame,
            }).map_err(|_| SendError::NodeUnreachable(to.node_id()))
        }
    }
}

/// Decode an inbound Send frame and deliver to the local ProcessTable.
pub fn deliver_inbound_frame(table: &ProcessTable, frame: &Frame) -> Result<(), SendError> {
    // Extract addressing from header
    let header = frame.header.as_map()
        .ok_or_else(|| SendError::ProcessDead(ProcessId::new(0, 0)))?;

    let mut from_node = 0u64;
    let mut from_local = 0u64;
    let mut to_node = 0u64;
    let mut to_local = 0u64;

    for (k, v) in header {
        match k.as_str() {
            Some("from_node") => from_node = v.as_u64().unwrap_or(0),
            Some("from_local") => from_local = v.as_u64().unwrap_or(0),
            Some("to_node") => to_node = v.as_u64().unwrap_or(0),
            Some("to_local") => to_local = v.as_u64().unwrap_or(0),
            _ => {}
        }
    }

    let from = ProcessId::new(from_node, from_local);
    let to = ProcessId::new(to_node, to_local);
    let msg = Message::new(from, frame.payload.clone());
    table.send(to, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebar_core::process::mailbox::Mailbox;
    use rebar_core::process::table::ProcessHandle;

    #[test]
    fn distributed_router_routes_local() {
        let table = Arc::new(ProcessTable::new(1));
        let pid = table.allocate_pid();
        let (tx, mut rx) = Mailbox::unbounded();
        table.insert(pid, ProcessHandle::new(tx));

        let (remote_tx, _remote_rx) = mpsc::channel(10);
        let router = DistributedRouter::new(1, Arc::clone(&table), remote_tx);

        let from = ProcessId::new(1, 0);
        router.route(from, pid, rmpv::Value::String("local-msg".into())).unwrap();

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.payload().as_str().unwrap(), "local-msg");
    }

    #[tokio::test]
    async fn distributed_router_routes_remote() {
        let table = Arc::new(ProcessTable::new(1));
        let (remote_tx, mut remote_rx) = mpsc::channel(10);
        let router = DistributedRouter::new(1, Arc::clone(&table), remote_tx);

        let from = ProcessId::new(1, 5);
        let remote_pid = ProcessId::new(2, 10); // Different node!

        router.route(from, remote_pid, rmpv::Value::String("remote-msg".into())).unwrap();

        let cmd = remote_rx.recv().await.unwrap();
        match cmd {
            RouterCommand::Send { node_id, frame } => {
                assert_eq!(node_id, 2);
                assert_eq!(frame.msg_type, MsgType::Send);
                assert_eq!(frame.payload, rmpv::Value::String("remote-msg".into()));
                // Verify header contains addressing
                let header = frame.header.as_map().unwrap();
                let to_node = header.iter()
                    .find(|(k, _)| k.as_str() == Some("to_node"))
                    .unwrap().1.as_u64().unwrap();
                assert_eq!(to_node, 2);
            }
        }
    }

    #[test]
    fn distributed_router_node_unreachable() {
        let table = Arc::new(ProcessTable::new(1));
        let (remote_tx, _) = mpsc::channel(1);
        let router = DistributedRouter::new(1, Arc::clone(&table), remote_tx);

        // Fill up the channel
        let from = ProcessId::new(1, 0);
        let remote_pid = ProcessId::new(2, 1);
        router.route(from, remote_pid, rmpv::Value::Nil).unwrap();

        // Second send should fail (channel full)
        let result = router.route(from, remote_pid, rmpv::Value::Nil);
        assert!(matches!(result, Err(SendError::NodeUnreachable(2))));
    }

    #[test]
    fn inbound_frame_delivers_to_local_process() {
        let table = Arc::new(ProcessTable::new(2));
        let pid = table.allocate_pid(); // PID <2.1>
        let (tx, mut rx) = Mailbox::unbounded();
        table.insert(pid, ProcessHandle::new(tx));

        let frame = DistributedRouter::encode_send_frame(
            ProcessId::new(1, 5),  // from remote node 1
            pid,                    // to local pid
            rmpv::Value::String("from-remote".into()),
        );

        deliver_inbound_frame(&table, &frame).unwrap();

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.payload().as_str().unwrap(), "from-remote");
        assert_eq!(msg.from().node_id(), 1);
        assert_eq!(msg.from().local_id(), 5);
    }

    #[test]
    fn encode_send_frame_has_correct_fields() {
        let from = ProcessId::new(1, 5);
        let to = ProcessId::new(2, 10);
        let frame = DistributedRouter::encode_send_frame(from, to, rmpv::Value::Integer(42.into()));

        assert_eq!(frame.version, 1);
        assert_eq!(frame.msg_type, MsgType::Send);
        assert_eq!(frame.payload, rmpv::Value::Integer(42.into()));

        let header = frame.header.as_map().unwrap();
        assert_eq!(header.len(), 4);
    }
}
```

**Step 2: Register module**

In `crates/rebar-cluster/src/lib.rs`:

```rust
pub mod connection;
pub mod protocol;
pub mod registry;
pub mod router;
pub mod swim;
pub mod transport;
```

**Step 3: Run tests to verify they pass**

Run: `~/.cargo/bin/cargo test -p rebar-cluster router::tests 2>&1 | tail -15`
Expected: 5 tests PASS

**Step 4: Run full suite**

Run: `~/.cargo/bin/cargo test --workspace 2>&1 | grep "^test result"`
Expected: All tests pass

**Step 5: Commit**

```bash
~/.cargo/bin/cargo fmt --all
git add -A
git commit -m "feat(cluster): add DistributedRouter and inbound frame delivery"
```

---

### Task 9: DistributedRuntime in rebar crate

**Files:**
- Modify: `crates/rebar/src/lib.rs`
- Modify: `crates/rebar/Cargo.toml`

**Step 1: Write the failing tests**

Replace `crates/rebar/src/lib.rs`:

```rust
pub use rebar_core::*;
pub use rebar_core::router::{MessageRouter, LocalRouter};

use std::sync::Arc;
use tokio::sync::mpsc;

use rebar_core::process::table::ProcessTable;
use rebar_core::process::ProcessId;
use rebar_core::runtime::Runtime;
use rebar_cluster::connection::manager::ConnectionManager;
use rebar_cluster::router::{DistributedRouter, RouterCommand, deliver_inbound_frame};
use rebar_cluster::protocol::{Frame, MsgType};

/// A fully wired distributed runtime that bridges rebar-core and rebar-cluster.
/// Handles transparent local/remote message routing.
pub struct DistributedRuntime {
    runtime: Runtime,
    table: Arc<ProcessTable>,
    connection_manager: ConnectionManager,
    remote_rx: mpsc::Receiver<RouterCommand>,
}

impl DistributedRuntime {
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
        }
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    pub fn table(&self) -> &Arc<ProcessTable> {
        &self.table
    }

    pub fn connection_manager_mut(&mut self) -> &mut ConnectionManager {
        &mut self.connection_manager
    }

    /// Process one pending outbound remote message.
    /// Returns true if a message was processed, false if channel is empty.
    pub async fn process_outbound(&mut self) -> bool {
        match self.remote_rx.try_recv() {
            Ok(RouterCommand::Send { node_id, frame }) => {
                let _ = self.connection_manager.route(node_id, &frame).await;
                true
            }
            Err(_) => false,
        }
    }

    /// Deliver an inbound frame to a local process.
    pub fn deliver_inbound(&self, frame: &Frame) -> Result<(), rebar_core::process::SendError> {
        deliver_inbound_frame(&self.table, frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebar_cluster::connection::manager::TransportConnector;
    use rebar_cluster::transport::{TransportConnection, TransportError};
    use rebar_core::process::mailbox::Mailbox;
    use rebar_core::process::table::ProcessHandle;
    use std::sync::Mutex;

    // Minimal mock connector
    struct MockConnector;

    #[async_trait::async_trait]
    impl TransportConnector for MockConnector {
        async fn connect(
            &self,
            _addr: std::net::SocketAddr,
        ) -> Result<Box<dyn TransportConnection>, TransportError> {
            Ok(Box::new(MockConn { sent: Arc::new(Mutex::new(Vec::new())) }))
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

        let receiver = drt.runtime().spawn(move |mut ctx| async move {
            let msg = ctx.recv().await.unwrap();
            done_tx.send(msg.payload().as_str().unwrap().to_string()).unwrap();
        }).await;

        drt.runtime().spawn(move |ctx| async move {
            ctx.send(receiver, rmpv::Value::String("local".into())).await.unwrap();
        }).await;

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            done_rx,
        ).await.unwrap().unwrap();
        assert_eq!(result, "local");
    }

    #[tokio::test]
    async fn distributed_runtime_inbound_delivery() {
        let mgr = ConnectionManager::new(Box::new(MockConnector));
        let drt = DistributedRuntime::new(2, mgr);

        let pid = drt.table().allocate_pid();
        let (tx, mut rx) = Mailbox::unbounded();
        drt.table().insert(pid, ProcessHandle::new(tx));

        // Simulate inbound frame from remote node 1
        let frame = Frame {
            version: 1,
            msg_type: MsgType::Send,
            request_id: 0,
            header: rmpv::Value::Map(vec![
                (rmpv::Value::String("from_node".into()), rmpv::Value::Integer(1u64.into())),
                (rmpv::Value::String("from_local".into()), rmpv::Value::Integer(5u64.into())),
                (rmpv::Value::String("to_node".into()), rmpv::Value::Integer(pid.node_id().into())),
                (rmpv::Value::String("to_local".into()), rmpv::Value::Integer(pid.local_id().into())),
            ]),
            payload: rmpv::Value::String("from-remote-node".into()),
        };

        drt.deliver_inbound(&frame).unwrap();

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.payload().as_str().unwrap(), "from-remote-node");
    }
}
```

**Step 2: Add async-trait dependency to rebar crate**

In `crates/rebar/Cargo.toml`:

```toml
[package]
name = "rebar"
version = "0.1.0"
edition.workspace = true
license.workspace = true
repository.workspace = true
description = "A BEAM-inspired distributed actor runtime"

[dependencies]
rebar-core = { path = "../rebar-core" }
rebar-cluster = { path = "../rebar-cluster" }
async-trait = "0.1"
tokio = { version = "1", features = ["full"] }
rmpv = "1"

[dev-dependencies]
tokio-test = "0.4"
```

**Step 3: Run tests to verify they pass**

Run: `~/.cargo/bin/cargo test -p rebar 2>&1 | tail -15`
Expected: 2 tests PASS

**Step 4: Run full suite**

Run: `~/.cargo/bin/cargo test --workspace 2>&1 | grep "^test result"`
Expected: All tests pass

**Step 5: Commit**

```bash
~/.cargo/bin/cargo fmt --all
git add -A
git commit -m "feat(rebar): add DistributedRuntime wiring core and cluster"
```

---

### Task 10: End-to-end cross-node messaging test

**Files:**
- Modify: `crates/rebar-cluster/tests/cluster_integration.rs`

**Step 1: Write the end-to-end test**

Append to `crates/rebar-cluster/tests/cluster_integration.rs`:

```rust
/// End-to-end: two DistributedRuntimes send messages via TCP transport.
#[tokio::test]
async fn end_to_end_cross_node_send_via_tcp() {
    use rebar_cluster::transport::tcp::TcpTransport;
    use rebar_cluster::transport::{TransportConnection, TransportListener};
    use rebar_cluster::connection::manager::{ConnectionManager, TransportConnector};
    use rebar_cluster::router::{DistributedRouter, RouterCommand, deliver_inbound_frame};
    use rebar_cluster::protocol::{Frame, MsgType};
    use rebar_core::process::table::ProcessTable;
    use rebar_core::process::mailbox::Mailbox;
    use rebar_core::process::table::ProcessHandle;
    use rebar_core::process::ProcessId;
    use rebar_core::router::MessageRouter;
    use rebar_core::runtime::Runtime;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    struct TcpConnector;

    #[async_trait::async_trait]
    impl TransportConnector for TcpConnector {
        async fn connect(
            &self,
            addr: std::net::SocketAddr,
        ) -> Result<Box<dyn TransportConnection>, rebar_cluster::transport::TransportError> {
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
    let listener = transport2.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let node2_addr = listener.local_addr();

    // Node 2: spawn a receiver process
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let receiver_pid = rt2.spawn(move |mut ctx| async move {
        let msg = ctx.recv().await.unwrap();
        done_tx.send((
            msg.from().node_id(),
            msg.payload().as_str().unwrap().to_string(),
        )).unwrap();
    }).await;

    // Node 2: accept connection and deliver inbound frames
    let table2_clone = Arc::clone(&table2);
    tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap();
        loop {
            match conn.recv().await {
                Ok(frame) => {
                    let _ = deliver_inbound_frame(&table2_clone, &frame);
                }
                Err(_) => break,
            }
        }
    });

    // Node 1: connect to node 2
    let mut mgr1 = ConnectionManager::new(Box::new(TcpConnector));
    mgr1.connect(2, node2_addr).await.unwrap();

    // Node 1: send message to receiver_pid on node 2
    rt1.send(receiver_pid, rmpv::Value::String("cross-node-hello".into())).await.unwrap();

    // Node 1: process the outbound message (drain remote_rx1 → connection_manager)
    if let Some(RouterCommand::Send { node_id, frame }) = remote_rx1.recv().await {
        assert_eq!(node_id, 2);
        mgr1.route(2, &frame).await.unwrap();
    }

    // Node 2: verify message received
    let (from_node, payload) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        done_rx,
    ).await.unwrap().unwrap();
    assert_eq!(from_node, 1);
    assert_eq!(payload, "cross-node-hello");
}
```

**Step 2: Run the test**

Run: `~/.cargo/bin/cargo test -p rebar-cluster end_to_end_cross_node_send_via_tcp 2>&1 | tail -15`
Expected: PASS

**Step 3: Commit**

```bash
~/.cargo/bin/cargo fmt --all
git add -A
git commit -m "test: add end-to-end cross-node messaging via TCP transport"
```

---

## Feature 3: Graceful Node Drain

### Task 11: DrainConfig and DrainResult types

**Files:**
- Create: `crates/rebar-cluster/src/drain.rs`
- Modify: `crates/rebar-cluster/src/lib.rs`

**Step 1: Write the failing tests and types**

Create `crates/rebar-cluster/src/drain.rs`:

```rust
use std::time::Duration;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
```

**Step 2: Register module**

In `crates/rebar-cluster/src/lib.rs`:

```rust
pub mod connection;
pub mod drain;
pub mod protocol;
pub mod registry;
pub mod router;
pub mod swim;
pub mod transport;
```

**Step 3: Run tests**

Run: `~/.cargo/bin/cargo test -p rebar-cluster drain::tests 2>&1 | tail -10`
Expected: 3 tests PASS

**Step 4: Commit**

```bash
~/.cargo/bin/cargo fmt --all
git add -A
git commit -m "feat(drain): add DrainConfig and DrainResult types"
```

---

### Task 12: Phase 1 — Announce (Leave broadcast + registry cleanup)

**Files:**
- Modify: `crates/rebar-cluster/src/drain.rs`

**Step 1: Write the failing tests**

Add to `drain.rs` above the tests module:

```rust
use crate::swim::gossip::{GossipQueue, GossipUpdate};
use crate::registry::orset::Registry;
use std::net::SocketAddr;
use std::time::Instant;

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
        // Broadcast Leave
        gossip.add(GossipUpdate::Leave {
            node_id,
            addr,
        });

        // Count registered names for this node before cleanup
        let names_before = registry.registered().len();
        registry.remove_by_node(node_id);
        let names_after = registry.registered().len();

        names_before - names_after
    }
}
```

Add to tests module:

```rust
    use super::*;
    use rebar_core::process::ProcessId;

    fn test_addr() -> SocketAddr {
        "127.0.0.1:4000".parse().unwrap()
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

        // Register some names on node 1
        registry.register("service_a", ProcessId::new(1, 1), 1, 100);
        registry.register("service_b", ProcessId::new(1, 2), 1, 101);
        // Register a name on node 2 (should NOT be removed)
        registry.register("service_c", ProcessId::new(2, 1), 2, 102);

        assert_eq!(registry.registered().len(), 3);

        let removed = drain.announce(1, test_addr(), &mut gossip, &mut registry);

        assert_eq!(removed, 2);
        assert_eq!(registry.registered().len(), 1);
        // Only service_c on node 2 remains
        assert!(registry.lookup("service_c").is_some());
        assert!(registry.lookup("service_a").is_none());
        assert!(registry.lookup("service_b").is_none());
    }
```

**Step 2: Run tests**

Run: `~/.cargo/bin/cargo test -p rebar-cluster drain::tests 2>&1 | tail -10`
Expected: 5 tests PASS (3 from Task 11 + 2 new)

**Step 3: Run full suite**

Run: `~/.cargo/bin/cargo test --workspace 2>&1 | grep "^test result"`
Expected: All pass

**Step 4: Commit**

```bash
~/.cargo/bin/cargo fmt --all
git add -A
git commit -m "feat(drain): implement phase 1 — Leave broadcast and registry cleanup"
```

---

### Task 13: Phase 2 — Drain in-flight messages

**Files:**
- Modify: `crates/rebar-cluster/src/drain.rs`

**Step 1: Write the failing tests**

Add method to `NodeDrain`:

```rust
    /// Phase 2: Drain in-flight outbound messages.
    /// Processes RouterCommands from the channel until empty or timeout.
    /// Returns number of messages drained.
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
                Ok(None) => break, // Channel closed, all senders dropped
                Err(_) => {
                    timed_out = true;
                    break;
                }
            }
        }

        (drained, timed_out)
    }
```

Add tests:

```rust
    #[tokio::test]
    async fn drain_waits_for_inflight() {
        use crate::router::RouterCommand;
        use crate::protocol::{Frame, MsgType};
        use crate::connection::manager::ConnectionManager;
        use crate::transport::{TransportConnection, TransportError};

        struct NullConn;
        #[async_trait::async_trait]
        impl TransportConnection for NullConn {
            async fn send(&mut self, _frame: &Frame) -> Result<(), TransportError> { Ok(()) }
            async fn recv(&mut self) -> Result<Frame, TransportError> { Err(TransportError::ConnectionClosed) }
            async fn close(&mut self) -> Result<(), TransportError> { Ok(()) }
        }

        struct NullConnector;
        #[async_trait::async_trait]
        impl crate::connection::manager::TransportConnector for NullConnector {
            async fn connect(&self, _: SocketAddr) -> Result<Box<dyn TransportConnection>, TransportError> {
                Ok(Box::new(NullConn))
            }
        }

        let (tx, mut rx) = tokio::sync::mpsc::channel::<RouterCommand>(64);
        let mut mgr = ConnectionManager::new(Box::new(NullConnector));
        mgr.connect(2, test_addr()).await.unwrap();

        // Enqueue 3 messages
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
            }).await.unwrap();
        }
        drop(tx); // Close channel so drain completes

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
        use crate::router::RouterCommand;
        use crate::connection::manager::ConnectionManager;
        use crate::transport::{TransportConnection, TransportError};

        struct NullConn;
        #[async_trait::async_trait]
        impl TransportConnection for NullConn {
            async fn send(&mut self, _: &Frame) -> Result<(), TransportError> { Ok(()) }
            async fn recv(&mut self) -> Result<Frame, TransportError> { Err(TransportError::ConnectionClosed) }
            async fn close(&mut self) -> Result<(), TransportError> { Ok(()) }
        }

        struct NullConnector;
        #[async_trait::async_trait]
        impl crate::connection::manager::TransportConnector for NullConnector {
            async fn connect(&self, _: SocketAddr) -> Result<Box<dyn TransportConnection>, TransportError> {
                Ok(Box::new(NullConn))
            }
        }

        // Channel stays open (sender not dropped), so drain must timeout
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
        assert!(elapsed < Duration::from_secs(1)); // Didn't wait too long

        drop(tx); // Cleanup
    }
```

**Step 2: Run tests**

Run: `~/.cargo/bin/cargo test -p rebar-cluster drain::tests 2>&1 | tail -15`
Expected: 7 tests PASS

**Step 3: Commit**

```bash
~/.cargo/bin/cargo fmt --all
git add -A
git commit -m "feat(drain): implement phase 2 — drain in-flight outbound messages"
```

---

### Task 14: Phase 3 — Shutdown and full drain orchestration

**Files:**
- Modify: `crates/rebar-cluster/src/drain.rs`
- Modify: `crates/rebar-cluster/src/connection/manager.rs`

**Step 1: Add drain method to ConnectionManager**

In `crates/rebar-cluster/src/connection/manager.rs`, add method to `impl ConnectionManager`:

```rust
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
```

Add test in `manager.rs` tests module:

```rust
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
```

**Step 2: Add full drain orchestration to NodeDrain**

In `drain.rs`, add the `drain` method that orchestrates all three phases:

```rust
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
```

**Step 3: Write the full drain test**

Add to tests module in `drain.rs`:

```rust
    #[tokio::test]
    async fn full_drain_protocol() {
        use crate::router::RouterCommand;
        use crate::protocol::{Frame, MsgType};
        use crate::connection::manager::ConnectionManager;
        use crate::transport::{TransportConnection, TransportError};

        struct NullConn;
        #[async_trait::async_trait]
        impl TransportConnection for NullConn {
            async fn send(&mut self, _: &Frame) -> Result<(), TransportError> { Ok(()) }
            async fn recv(&mut self) -> Result<Frame, TransportError> { Err(TransportError::ConnectionClosed) }
            async fn close(&mut self) -> Result<(), TransportError> { Ok(()) }
        }

        struct NullConnector;
        #[async_trait::async_trait]
        impl crate::connection::manager::TransportConnector for NullConnector {
            async fn connect(&self, _: SocketAddr) -> Result<Box<dyn TransportConnection>, TransportError> {
                Ok(Box::new(NullConn))
            }
        }

        let mut gossip = GossipQueue::new();
        let mut registry = Registry::default();
        registry.register("svc", ProcessId::new(1, 1), 1, 100);

        let (tx, mut rx) = tokio::sync::mpsc::channel::<RouterCommand>(64);
        let mut mgr = ConnectionManager::new(Box::new(NullConnector));
        mgr.connect(2, test_addr()).await.unwrap();

        // Enqueue a message
        tx.send(RouterCommand::Send {
            node_id: 2,
            frame: Frame {
                version: 1,
                msg_type: MsgType::Send,
                request_id: 0,
                header: rmpv::Value::Nil,
                payload: rmpv::Value::Nil,
            },
        }).await.unwrap();
        drop(tx);

        let drain = NodeDrain::new(DrainConfig {
            announce_timeout: Duration::from_millis(100),
            drain_timeout: Duration::from_secs(1),
            shutdown_timeout: Duration::from_millis(100),
        });

        let result = drain.drain(
            1,
            test_addr(),
            &mut gossip,
            &mut registry,
            &mut rx,
            &mut mgr,
            5, // 5 processes were running
        ).await;

        // Verify results
        assert_eq!(result.messages_drained, 1);
        assert_eq!(result.processes_stopped, 5);
        assert!(!result.timed_out);

        // Registry should be empty (for node 1)
        assert!(registry.lookup("svc").is_none());

        // Gossip should have Leave
        let updates = gossip.drain(10);
        assert!(updates.iter().any(|u| matches!(u, GossipUpdate::Leave { node_id: 1, .. })));

        // Connections should be drained
        assert_eq!(mgr.connection_count(), 0);
    }

    #[tokio::test]
    async fn drain_returns_stats() {
        use crate::router::RouterCommand;
        use crate::connection::manager::ConnectionManager;
        use crate::transport::{TransportConnection, TransportError};

        struct NullConn;
        #[async_trait::async_trait]
        impl TransportConnection for NullConn {
            async fn send(&mut self, _: &Frame) -> Result<(), TransportError> { Ok(()) }
            async fn recv(&mut self) -> Result<Frame, TransportError> { Err(TransportError::ConnectionClosed) }
            async fn close(&mut self) -> Result<(), TransportError> { Ok(()) }
        }

        struct NullConnector;
        #[async_trait::async_trait]
        impl crate::connection::manager::TransportConnector for NullConnector {
            async fn connect(&self, _: SocketAddr) -> Result<Box<dyn TransportConnection>, TransportError> {
                Ok(Box::new(NullConn))
            }
        }

        let mut gossip = GossipQueue::new();
        let mut registry = Registry::default();
        let (_tx, mut rx) = tokio::sync::mpsc::channel::<RouterCommand>(64);
        drop(_tx); // Close immediately
        let mut mgr = ConnectionManager::new(Box::new(NullConnector));

        let drain = NodeDrain::new(DrainConfig::default());
        let result = drain.drain(
            1, test_addr(), &mut gossip, &mut registry,
            &mut rx, &mut mgr, 0,
        ).await;

        // All phase durations should be non-negative
        for d in &result.phase_durations {
            assert!(*d >= Duration::ZERO);
        }
        assert_eq!(result.messages_drained, 0);
        assert_eq!(result.processes_stopped, 0);
        assert!(!result.timed_out);
    }
```

**Step 4: Run tests**

Run: `~/.cargo/bin/cargo test -p rebar-cluster drain::tests 2>&1 | tail -15`
Expected: 9 tests PASS

Run: `~/.cargo/bin/cargo test -p rebar-cluster manager::tests::drain_connections_closes_all 2>&1 | tail -5`
Expected: PASS

**Step 5: Run full suite**

Run: `~/.cargo/bin/cargo test --workspace 2>&1 | grep "^test result"`
Expected: All tests pass

**Step 6: Commit**

```bash
~/.cargo/bin/cargo fmt --all
git add -A
git commit -m "feat(drain): implement full three-phase drain protocol with stats"
```

---

### Task 15: Update README roadmap checkboxes

**Files:**
- Modify: `README.md:130-132`

**Step 1: Check the boxes**

Replace the unchecked items in README.md:

```markdown
- [x] QUIC transport (`quinn`-based, replacing TCP as production default)
- [x] Distribution layer integration (cluster <-> core, transparent remote `send()`)
- [x] Graceful node drain (coordinated shutdown with state migration)
```

**Step 2: Run full test suite one final time**

Run: `~/.cargo/bin/cargo test --workspace 2>&1 | grep "^test result"`
Expected: All tests pass

**Step 3: Commit**

```bash
git add README.md
git commit -m "docs: mark QUIC, distribution, and drain as complete in roadmap"
```

**Step 4: Push**

```bash
git push origin main
```
