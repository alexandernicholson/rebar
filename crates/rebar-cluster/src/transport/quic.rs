use std::net::SocketAddr;
use std::sync::Arc;

use crate::protocol::{Frame, MAX_FRAME_SIZE};
use crate::transport::traits::{TransportConnection, TransportError, TransportListener};
use async_trait::async_trait;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use sha2::{Digest, Sha256};

/// SHA-256 fingerprint of a DER-encoded certificate.
pub type CertHash = [u8; 32];

/// Generate a self-signed certificate for node-to-node QUIC transport.
///
/// Returns the DER-encoded certificate, its PKCS8 private key, and a SHA-256
/// fingerprint, or a [`TransportError`] if the underlying key/certificate
/// generation fails.
///
/// # Errors
///
/// Returns `TransportError::Io` if certificate generation fails.
pub fn try_generate_self_signed_cert()
-> Result<(CertificateDer<'static>, PrivateKeyDer<'static>, CertHash), TransportError> {
    let certified_key = rcgen::generate_simple_self_signed(vec!["rebar-node".to_string()])
        .map_err(|e| TransportError::Io(std::io::Error::other(e)))?;

    let cert_der = certified_key.cert.der().clone();
    let key_der = PrivatePkcs8KeyDer::from(certified_key.key_pair.serialize_der());

    let hash = cert_fingerprint(&cert_der);

    Ok((cert_der, PrivateKeyDer::Pkcs8(key_der), hash))
}

/// Generate a self-signed certificate for node-to-node QUIC transport.
///
/// Returns the DER-encoded certificate, its PKCS8 private key, and a SHA-256 fingerprint.
///
/// This is a convenience wrapper around [`try_generate_self_signed_cert`] for
/// callers (e.g. tests, startup paths) that treat key generation failure as
/// fatal. Prefer [`try_generate_self_signed_cert`] on any peer-facing path.
///
/// # Panics
///
/// Panics if certificate generation fails.
#[must_use]
pub fn generate_self_signed_cert() -> (CertificateDer<'static>, PrivateKeyDer<'static>, CertHash) {
    try_generate_self_signed_cert().expect("certificate generation failed")
}

/// Compute the SHA-256 fingerprint of a DER-encoded certificate.
#[must_use]
pub fn cert_fingerprint(cert: &CertificateDer<'_>) -> CertHash {
    let mut hasher = Sha256::new();
    hasher.update(cert.as_ref());
    hasher.finalize().into()
}

// ---------------------------------------------------------------------------
// QUIC Transport
// ---------------------------------------------------------------------------

/// QUIC transport using stream-per-frame with length-prefixed framing.
pub struct QuicTransport {
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
}

impl QuicTransport {
    #[must_use]
    pub const fn new(cert: CertificateDer<'static>, key: PrivateKeyDer<'static>) -> Self {
        Self { cert, key }
    }

    /// Create a QUIC server endpoint bound to `addr`.
    ///
    /// # Errors
    ///
    /// Returns `TransportError::Io` if the TLS configuration is invalid or
    /// the endpoint cannot be bound to `addr`.
    pub async fn listen(&self, addr: SocketAddr) -> Result<QuicListener, TransportError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server_crypto = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| TransportError::Io(std::io::Error::other(e)))?
            .with_no_client_auth()
            .with_single_cert(vec![self.cert.clone()], self.key.clone_key())
            .map_err(|e| TransportError::Io(std::io::Error::other(e)))?;

        let server_config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)
                .map_err(|e| TransportError::Io(std::io::Error::other(e)))?,
        ));

        let socket = tokio::net::UdpSocket::bind(addr)
            .await
            .map_err(TransportError::Io)?;
        let endpoint = quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            Some(server_config),
            socket.into_std().map_err(TransportError::Io)?,
            Arc::new(quinn::TokioRuntime),
        )
        .map_err(TransportError::Io)?;

        // Cache the bound address now so `local_addr` is infallible.
        let local_addr = endpoint.local_addr().map_err(TransportError::Io)?;

        Ok(QuicListener {
            endpoint,
            local_addr,
        })
    }

    /// Connect to a remote QUIC endpoint, verifying the server certificate fingerprint.
    ///
    /// # Errors
    ///
    /// Returns `TransportError::Io` if the TLS configuration is invalid, the
    /// connection cannot be initiated, or the handshake fails (including a
    /// certificate fingerprint mismatch).
    pub async fn connect(
        &self,
        addr: SocketAddr,
        expected_cert_hash: CertHash,
    ) -> Result<QuicConnection, TransportError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Arc::new(FingerprintVerifier {
            expected_cert_hash,
            provider: provider.clone(),
        });

        let client_crypto = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| TransportError::Io(std::io::Error::other(e)))?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();

        let client_config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(client_crypto)
                .map_err(|e| TransportError::Io(std::io::Error::other(e)))?,
        ));

        let local_addr = SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, 0));
        let mut endpoint = quinn::Endpoint::client(local_addr)
            .map_err(|e| TransportError::Io(std::io::Error::other(e)))?;
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

// ---------------------------------------------------------------------------
// QuicTransportConnector
// ---------------------------------------------------------------------------

/// A [`crate::connection::manager::TransportConnector`] implementation backed
/// by QUIC.
///
/// Each call to `connect` creates a fresh [`QuicTransport`] with cloned
/// credentials and dials the given address while verifying the remote
/// certificate fingerprint.
pub struct QuicTransportConnector {
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    expected_cert_hash: CertHash,
}

impl QuicTransportConnector {
    #[must_use]
    pub const fn new(
        cert: CertificateDer<'static>,
        key: PrivateKeyDer<'static>,
        expected_cert_hash: CertHash,
    ) -> Self {
        Self {
            cert,
            key,
            expected_cert_hash,
        }
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

// ---------------------------------------------------------------------------
// QuicListener
// ---------------------------------------------------------------------------

pub struct QuicListener {
    endpoint: quinn::Endpoint,
    local_addr: SocketAddr,
}

#[async_trait]
impl TransportListener for QuicListener {
    type Connection = QuicConnection;

    fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    async fn accept(&self) -> Result<Self::Connection, TransportError> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or(TransportError::ConnectionClosed)?;

        let connection = incoming
            .await
            .map_err(|e| TransportError::Io(std::io::Error::other(e)))?;

        Ok(QuicConnection {
            connection,
            _endpoint: None,
        })
    }
}

// ---------------------------------------------------------------------------
// QuicConnection
// ---------------------------------------------------------------------------

pub struct QuicConnection {
    connection: quinn::Connection,
    _endpoint: Option<quinn::Endpoint>,
}

#[async_trait]
impl TransportConnection for QuicConnection {
    async fn send(&mut self, frame: &Frame) -> Result<(), TransportError> {
        let encoded = frame.encode();
        let len = u32::try_from(encoded.len())
            .map_err(|e| TransportError::Io(std::io::Error::other(e)))?;

        let mut send_stream = self
            .connection
            .open_uni()
            .await
            .map_err(|e| TransportError::Io(std::io::Error::other(e)))?;

        send_stream
            .write_all(&len.to_be_bytes())
            .await
            .map_err(|e| TransportError::Io(std::io::Error::other(e)))?;
        send_stream
            .write_all(&encoded)
            .await
            .map_err(|e| TransportError::Io(std::io::Error::other(e)))?;
        send_stream
            .finish()
            .map_err(|e| TransportError::Io(std::io::Error::other(e)))?;

        Ok(())
    }

    async fn recv(&mut self) -> Result<Frame, TransportError> {
        let mut recv_stream = self
            .connection
            .accept_uni()
            .await
            .map_err(|e| TransportError::Io(std::io::Error::other(e)))?;

        let mut len_buf = [0u8; 4];
        recv_stream
            .read_exact(&mut len_buf)
            .await
            .map_err(|e| TransportError::Io(std::io::Error::other(e)))?;
        let len = u32::from_be_bytes(len_buf) as usize;
        // Reject an oversized length prefix BEFORE allocating, so a peer cannot
        // force a multi-gigabyte allocation per stream (remote OOM/DoS).
        if len > MAX_FRAME_SIZE {
            return Err(TransportError::FrameTooLarge {
                declared: len,
                max: MAX_FRAME_SIZE,
            });
        }

        let mut buf = vec![0u8; len];
        recv_stream
            .read_exact(&mut buf)
            .await
            .map_err(|e| TransportError::Io(std::io::Error::other(e)))?;

        let frame = Frame::decode(&buf)?;
        Ok(frame)
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        self.connection.close(0u32.into(), b"done");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// FingerprintVerifier — custom rustls ServerCertVerifier
// ---------------------------------------------------------------------------

/// Verifies a server certificate by pinning its SHA-256 fingerprint AND proving
/// that the peer possesses the corresponding private key.
///
/// The fingerprint is computed over the *public* certificate, which is sent in
/// the clear during the handshake; pinning it alone is NOT authentication, since
/// an attacker can replay that public cert with a different key pair. We
/// therefore also delegate the handshake-signature checks to the real rustls
/// crypto provider (`verify_tls12_signature` / `verify_tls13_signature`), which
/// verify the signature against the certificate's public key — proving
/// possession of the matching private key and closing the MITM bypass.
#[derive(Debug)]
struct FingerprintVerifier {
    expected_cert_hash: CertHash,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let fingerprint = cert_fingerprint(end_entity);
        if fingerprint == self.expected_cert_hash {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "certificate fingerprint mismatch".to_string(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_cert_returns_valid_cert() {
        let (cert, key, hash) = generate_self_signed_cert();
        assert!(!cert.is_empty());
        match &key {
            PrivateKeyDer::Pkcs8(k) => assert!(!k.secret_pkcs8_der().is_empty()),
            _ => panic!("expected PKCS8 key"),
        }
        assert_ne!(hash, [0u8; 32]);
    }

    #[test]
    fn cert_fingerprint_is_deterministic() {
        let (cert, _, hash1) = generate_self_signed_cert();
        let hash2 = cert_fingerprint(&cert);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn different_certs_have_different_fingerprints() {
        let (_, _, hash1) = generate_self_signed_cert();
        let (_, _, hash2) = generate_self_signed_cert();
        assert_ne!(hash1, hash2);
    }

    #[tokio::test]
    async fn quic_send_recv_single_frame() {
        use crate::protocol::{Frame, MsgType};
        use crate::transport::traits::{TransportConnection, TransportListener};

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
            msg_type: MsgType::Heartbeat,
            request_id: 0,
            header: rmpv::Value::Nil,
            payload: rmpv::Value::Nil,
        };
        client.send(&frame).await.unwrap();

        let received = server.await.unwrap();
        assert_eq!(received.msg_type, MsgType::Heartbeat);
        assert_eq!(received.version, 1);
    }

    #[tokio::test]
    async fn quic_send_recv_multiple_frames() {
        use crate::protocol::{Frame, MsgType};
        use crate::transport::traits::{TransportConnection, TransportListener};

        let (cert, key, hash) = generate_self_signed_cert();
        let transport = QuicTransport::new(cert, key);
        let listener = transport
            .listen("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr();

        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let mut frames = Vec::new();
            for _ in 0..3 {
                frames.push(conn.recv().await.unwrap());
            }
            frames
        });

        let mut client = transport.connect(addr, hash).await.unwrap();
        for i in 0..3u64 {
            client
                .send(&Frame {
                    version: 1,
                    msg_type: MsgType::Send,
                    request_id: i,
                    header: rmpv::Value::Nil,
                    payload: rmpv::Value::Nil,
                })
                .await
                .unwrap();
        }

        let frames = server.await.unwrap();
        assert_eq!(frames.len(), 3);
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f.request_id, i as u64);
        }
    }

    #[tokio::test]
    async fn quic_large_payload() {
        use crate::protocol::{Frame, MsgType};
        use crate::transport::traits::{TransportConnection, TransportListener};

        let (cert, key, hash) = generate_self_signed_cert();
        let transport = QuicTransport::new(cert, key);
        let listener = transport
            .listen("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr();

        let big_payload: Vec<u8> = (0..=u8::MAX).cycle().take(65536).collect();
        let big_payload_clone = big_payload.clone();

        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            conn.recv().await.unwrap()
        });

        let mut client = transport.connect(addr, hash).await.unwrap();
        client
            .send(&Frame {
                version: 1,
                msg_type: MsgType::Send,
                request_id: 0,
                header: rmpv::Value::Nil,
                payload: rmpv::Value::Binary(big_payload),
            })
            .await
            .unwrap();

        let received = server.await.unwrap();
        assert_eq!(received.payload, rmpv::Value::Binary(big_payload_clone));
    }

    #[tokio::test]
    async fn quic_connection_close() {
        use crate::transport::traits::{TransportConnection, TransportListener};
        use std::time::Duration;

        let (cert, key, hash) = generate_self_signed_cert();
        let transport = QuicTransport::new(cert, key);
        let listener = transport
            .listen("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr();

        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            // Server tries to receive — should error because client closes
            conn.recv().await
        });

        let mut client = transport.connect(addr, hash).await.unwrap();
        // Close the connection immediately
        client.close().await.unwrap();

        let result = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server task timed out")
            .expect("server task panicked");

        assert!(
            result.is_err(),
            "server recv should fail after client close"
        );
    }

    #[tokio::test]
    async fn quic_concurrent_streams() {
        use crate::protocol::{Frame, MsgType};
        use crate::transport::traits::{TransportConnection, TransportListener};
        use std::time::Duration;

        let (cert, key, hash) = generate_self_signed_cert();
        let transport = QuicTransport::new(cert, key);
        let listener = transport
            .listen("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr();

        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let mut frames = Vec::new();
            for _ in 0..10 {
                frames.push(conn.recv().await.unwrap());
            }
            frames
        });

        let mut client = transport.connect(addr, hash).await.unwrap();
        // Send 10 frames as fast as possible (each send opens a new stream)
        for i in 0..10u64 {
            client
                .send(&Frame {
                    version: 1,
                    msg_type: MsgType::Send,
                    request_id: i,
                    header: rmpv::Value::Nil,
                    payload: rmpv::Value::Nil,
                })
                .await
                .unwrap();
        }

        let frames = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server task timed out")
            .expect("server task panicked");

        assert_eq!(frames.len(), 10);
        let mut ids: Vec<u64> = frames.iter().map(|f| f.request_id).collect();
        ids.sort_unstable();
        assert_eq!(ids, (0..10).collect::<Vec<u64>>());
    }

    #[tokio::test]
    async fn quic_connector_works_with_connection_manager() {
        use crate::connection::manager::ConnectionManager;
        use crate::protocol::{Frame, MsgType};
        use crate::transport::traits::TransportListener;

        // Server setup
        let (server_cert, server_key, server_hash) = generate_self_signed_cert();
        let server = QuicTransport::new(server_cert.clone(), server_key);
        let listener = server.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let server_addr = listener.local_addr();

        // Client connector
        let (client_cert, client_key, _) = generate_self_signed_cert();
        let connector = QuicTransportConnector::new(client_cert, client_key, server_hash);
        let mut mgr = ConnectionManager::new(Box::new(connector));

        // Server accepts in background, receives frame
        let server_handle = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            conn.recv().await.unwrap()
        });

        // Client connects via ConnectionManager and routes a frame
        mgr.connect(1, server_addr).await.unwrap();
        assert!(mgr.is_connected(1));

        let frame = Frame {
            version: 1,
            msg_type: MsgType::Heartbeat,
            request_id: 0,
            header: rmpv::Value::Nil,
            payload: rmpv::Value::Nil,
        };
        mgr.route(1, &frame).await.unwrap();

        let received = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.msg_type, MsgType::Heartbeat);
    }

    #[test]
    fn tls_signature_verifier_is_not_a_stub() {
        // Regression for the MITM-bypass bug: verify_tls{12,13}_signature used to
        // return HandshakeSignatureValid::assertion() unconditionally, so a
        // garbage signature was "valid" and possession of the private key was
        // never proven. With a real verifier, a bogus signature over a real cert
        // must be REJECTED. Constructing the wrong-key-correct-fingerprint MITM
        // in a unit test requires forging a TLS transcript, so we instead assert
        // the verifier actually checks the signature (rejects a known-bad one).
        use rustls::client::danger::ServerCertVerifier;
        use rustls::internal::msgs::codec::{Codec, Reader};

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let (cert, _key, hash) = generate_self_signed_cert();
        let verifier = FingerprintVerifier {
            expected_cert_hash: hash,
            provider,
        };

        // Build a DigitallySignedStruct on the wire: scheme (u16) + sig (u16-len
        // prefixed). ECDSA_NISTP256_SHA256 = 0x0403, with an all-zero signature.
        let mut wire = Vec::new();
        wire.extend_from_slice(&0x0403u16.to_be_bytes()); // scheme
        wire.extend_from_slice(&8u16.to_be_bytes()); // sig len
        wire.extend_from_slice(&[0u8; 8]); // bogus signature bytes
        let mut reader = Reader::init(&wire);
        let dss = rustls::DigitallySignedStruct::read(&mut reader)
            .expect("DigitallySignedStruct should decode");

        let message = b"transcript bytes that were never actually signed";
        let result13 = verifier.verify_tls13_signature(message, &cert, &dss);
        assert!(
            result13.is_err(),
            "TLS 1.3 verifier accepted a bogus signature — it is still the stub"
        );
        let result12 = verifier.verify_tls12_signature(message, &cert, &dss);
        assert!(
            result12.is_err(),
            "TLS 1.2 verifier accepted a bogus signature — it is still the stub"
        );
    }

    #[tokio::test]
    async fn quic_cert_fingerprint_mismatch() {
        use crate::transport::traits::TransportListener;
        use std::time::Duration;

        let (cert, key, _hash) = generate_self_signed_cert();
        let transport = QuicTransport::new(cert, key);
        let listener = transport
            .listen("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr();

        // Server must accept so the TLS handshake can proceed
        let _server = tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        // Use a wrong cert hash
        let wrong_hash: CertHash = [0xFF; 32];

        let result =
            tokio::time::timeout(Duration::from_secs(5), transport.connect(addr, wrong_hash))
                .await
                .expect("connect timed out");

        assert!(
            result.is_err(),
            "connection with wrong cert hash should fail"
        );
    }
}
