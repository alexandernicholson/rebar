use std::net::SocketAddr;
use std::sync::Arc;

use crate::protocol::Frame;
use crate::transport::traits::{TransportConnection, TransportError, TransportListener};
use async_trait::async_trait;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use sha2::{Digest, Sha256};

/// SHA-256 fingerprint of a DER-encoded certificate.
pub type CertHash = [u8; 32];

/// Generate a self-signed certificate for node-to-node QUIC transport.
///
/// Returns the DER-encoded certificate, its PKCS8 private key, and a SHA-256 fingerprint.
pub fn generate_self_signed_cert() -> (CertificateDer<'static>, PrivateKeyDer<'static>, CertHash) {
    let certified_key = rcgen::generate_simple_self_signed(vec!["rebar-node".to_string()])
        .expect("certificate generation failed");

    let cert_der = certified_key.cert.der().clone();
    let key_der = PrivatePkcs8KeyDer::from(certified_key.key_pair.serialize_der());

    let hash = cert_fingerprint(&cert_der);

    (cert_der, PrivateKeyDer::Pkcs8(key_der), hash)
}

/// Compute the SHA-256 fingerprint of a DER-encoded certificate.
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
    pub fn new(cert: CertificateDer<'static>, key: PrivateKeyDer<'static>) -> Self {
        Self { cert, key }
    }

    /// Create a QUIC server endpoint bound to `addr`.
    pub async fn listen(&self, addr: SocketAddr) -> Result<QuicListener, TransportError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server_crypto = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| TransportError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?
            .with_no_client_auth()
            .with_single_cert(vec![self.cert.clone()], self.key.clone_key())
            .map_err(|e| TransportError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

        let server_config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto).map_err(|e| {
                TransportError::Io(std::io::Error::new(std::io::ErrorKind::Other, e))
            })?,
        ));

        let endpoint = quinn::Endpoint::server(server_config, addr)
            .map_err(|e| TransportError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

        Ok(QuicListener { endpoint })
    }

    /// Connect to a remote QUIC endpoint, verifying the server certificate fingerprint.
    pub async fn connect(
        &self,
        addr: SocketAddr,
        expected_cert_hash: CertHash,
    ) -> Result<QuicConnection, TransportError> {
        let verifier = Arc::new(FingerprintVerifier { expected_cert_hash });
        let provider = Arc::new(rustls::crypto::ring::default_provider());

        let client_crypto = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| TransportError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();

        let client_config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(client_crypto).map_err(|e| {
                TransportError::Io(std::io::Error::new(std::io::ErrorKind::Other, e))
            })?,
        ));

        let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap())
            .map_err(|e| TransportError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        endpoint.set_default_client_config(client_config);

        let connection = endpoint
            .connect(addr, "rebar-node")
            .map_err(|e| TransportError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?
            .await
            .map_err(|e| TransportError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

        Ok(QuicConnection {
            connection,
            _endpoint: Some(endpoint),
        })
    }
}

// ---------------------------------------------------------------------------
// QuicListener
// ---------------------------------------------------------------------------

pub struct QuicListener {
    endpoint: quinn::Endpoint,
}

#[async_trait]
impl TransportListener for QuicListener {
    type Connection = QuicConnection;

    fn local_addr(&self) -> SocketAddr {
        self.endpoint.local_addr().expect("endpoint has local addr")
    }

    async fn accept(&self) -> Result<Self::Connection, TransportError> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or(TransportError::ConnectionClosed)?;

        let connection = incoming
            .await
            .map_err(|e| TransportError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

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
        let len = encoded.len() as u32;

        let mut send_stream =
            self.connection.open_uni().await.map_err(|e| {
                TransportError::Io(std::io::Error::new(std::io::ErrorKind::Other, e))
            })?;

        send_stream
            .write_all(&len.to_be_bytes())
            .await
            .map_err(|e| TransportError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        send_stream
            .write_all(&encoded)
            .await
            .map_err(|e| TransportError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        send_stream
            .finish()
            .map_err(|e| TransportError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

        Ok(())
    }

    async fn recv(&mut self) -> Result<Frame, TransportError> {
        let mut recv_stream =
            self.connection.accept_uni().await.map_err(|e| {
                TransportError::Io(std::io::Error::new(std::io::ErrorKind::Other, e))
            })?;

        let mut len_buf = [0u8; 4];
        recv_stream
            .read_exact(&mut len_buf)
            .await
            .map_err(|e| TransportError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        let len = u32::from_be_bytes(len_buf) as usize;

        let mut buf = vec![0u8; len];
        recv_stream
            .read_exact(&mut buf)
            .await
            .map_err(|e| TransportError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

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

/// Verifies a server certificate by checking its SHA-256 fingerprint against an expected hash.
#[derive(Debug)]
struct FingerprintVerifier {
    expected_cert_hash: CertHash,
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
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
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

        let big_payload: Vec<u8> = (0..65536).map(|i| (i % 256) as u8).collect();
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
}
