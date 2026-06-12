use std::net::SocketAddr;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::protocol::{Frame, MAX_FRAME_SIZE};
use crate::transport::traits::{TransportConnection, TransportError, TransportListener};

/// TCP transport using length-prefixed framing.
///
/// Wire format:
/// ```text
/// ┌──────────┬──────────────┐
/// │ len: u32 │ payload: [u8]│
/// └──────────┴──────────────┘
/// ```
#[derive(Default)]
pub struct TcpTransport;

impl TcpTransport {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Create a TCP listener bound to `addr`.
    ///
    /// # Errors
    ///
    /// Returns `TransportError::Io` if the listener cannot be bound to `addr`.
    pub async fn listen(&self, addr: SocketAddr) -> Result<TcpTransportListener, TransportError> {
        let listener = TcpListener::bind(addr).await?;
        // Cache the bound address now (while we can still surface a bind error)
        // so `local_addr` is infallible and cannot panic later.
        let local_addr = listener.local_addr()?;
        Ok(TcpTransportListener {
            inner: listener,
            local_addr,
        })
    }

    /// Connect to a remote TCP endpoint at `addr`.
    ///
    /// # Errors
    ///
    /// Returns `TransportError::Io` if the connection cannot be established.
    pub async fn connect(&self, addr: SocketAddr) -> Result<TcpConnection, TransportError> {
        let stream = TcpStream::connect(addr).await?;
        Ok(TcpConnection { stream })
    }
}

pub struct TcpTransportListener {
    inner: TcpListener,
    local_addr: SocketAddr,
}

#[async_trait]
impl TransportListener for TcpTransportListener {
    type Connection = TcpConnection;

    fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    async fn accept(&self) -> Result<Self::Connection, TransportError> {
        let (stream, _addr) = self.inner.accept().await?;
        Ok(TcpConnection { stream })
    }
}

pub struct TcpConnection {
    stream: TcpStream,
}

#[async_trait]
impl TransportConnection for TcpConnection {
    async fn send(&mut self, frame: &Frame) -> Result<(), TransportError> {
        let encoded = frame.encode();
        let len = u32::try_from(encoded.len())
            .map_err(|e| TransportError::Io(std::io::Error::other(e)))?;
        self.stream.write_all(&len.to_be_bytes()).await?;
        self.stream.write_all(&encoded).await?;
        self.stream.flush().await?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Frame, TransportError> {
        let mut len_buf = [0u8; 4];
        match self.stream.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(TransportError::ConnectionClosed);
            }
            Err(e) => return Err(TransportError::Io(e)),
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        // Reject an oversized length prefix BEFORE allocating, so a peer cannot
        // force a multi-gigabyte allocation (remote OOM/DoS).
        if len > MAX_FRAME_SIZE {
            return Err(TransportError::FrameTooLarge {
                declared: len,
                max: MAX_FRAME_SIZE,
            });
        }
        let mut buf = vec![0u8; len];
        self.stream.read_exact(&mut buf).await?;
        let frame = Frame::decode(&buf)?;
        Ok(frame)
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        self.stream.shutdown().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Frame, MsgType};

    #[tokio::test]
    async fn connect_and_send_frame() {
        let transport = TcpTransport::new();
        let listener = transport
            .listen("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr();
        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            conn.recv().await.unwrap()
        });
        let mut client = transport.connect(addr).await.unwrap();
        let frame = Frame {
            version: 1,
            msg_type: MsgType::Heartbeat,
            request_id: 0,
            header: rmpv::Value::Nil,
            payload: rmpv::Value::Nil,
        };
        client.send(&frame).await.unwrap();
        client.close().await.unwrap();
        let received = server.await.unwrap();
        assert_eq!(received.msg_type, MsgType::Heartbeat);
    }

    #[tokio::test]
    async fn bidirectional_echo() {
        let transport = TcpTransport::new();
        let listener = transport
            .listen("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr();
        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let frame = conn.recv().await.unwrap();
            conn.send(&frame).await.unwrap();
        });
        let mut client = transport.connect(addr).await.unwrap();
        let frame = Frame {
            version: 1,
            msg_type: MsgType::Send,
            request_id: 42,
            header: rmpv::Value::Nil,
            payload: rmpv::Value::String("ping".into()),
        };
        client.send(&frame).await.unwrap();
        let response = client.recv().await.unwrap();
        assert_eq!(response.request_id, 42);
        assert_eq!(response.payload, rmpv::Value::String("ping".into()));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn multiple_frames_sequential() {
        let transport = TcpTransport::new();
        let listener = transport
            .listen("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr();
        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let mut frames = Vec::new();
            for _ in 0..5 {
                frames.push(conn.recv().await.unwrap());
            }
            frames
        });
        let mut client = transport.connect(addr).await.unwrap();
        for i in 0..5u64 {
            client
                .send(&Frame {
                    version: 1,
                    msg_type: MsgType::Send,
                    request_id: i,
                    header: rmpv::Value::Nil,
                    payload: rmpv::Value::Integer(i.into()),
                })
                .await
                .unwrap();
        }
        let frames = server.await.unwrap();
        assert_eq!(frames.len(), 5);
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f.request_id, i as u64);
        }
    }

    #[tokio::test]
    async fn large_frame() {
        let transport = TcpTransport::new();
        let listener = transport
            .listen("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr();
        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            conn.recv().await.unwrap()
        });
        let mut client = transport.connect(addr).await.unwrap();
        let big = "x".repeat(1_000_000);
        client
            .send(&Frame {
                version: 1,
                msg_type: MsgType::Send,
                request_id: 0,
                header: rmpv::Value::Nil,
                payload: rmpv::Value::String(big.into()),
            })
            .await
            .unwrap();
        let received = server.await.unwrap();
        assert_eq!(received.payload.as_str().unwrap().len(), 1_000_000);
    }

    #[tokio::test]
    async fn recv_after_close_returns_error() {
        let transport = TcpTransport::new();
        let listener = transport
            .listen("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr();
        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let result = conn.recv().await;
            assert!(result.is_err());
        });
        let mut client = transport.connect(addr).await.unwrap();
        client.close().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn multiple_clients() {
        let transport = TcpTransport::new();
        let listener = transport
            .listen("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr();
        let server = tokio::spawn(async move {
            let mut conn1 = listener.accept().await.unwrap();
            let mut conn2 = listener.accept().await.unwrap();
            let f1 = conn1.recv().await.unwrap();
            let f2 = conn2.recv().await.unwrap();
            (f1.request_id, f2.request_id)
        });
        let mut c1 = transport.connect(addr).await.unwrap();
        let mut c2 = transport.connect(addr).await.unwrap();
        c1.send(&Frame {
            version: 1,
            msg_type: MsgType::Send,
            request_id: 100,
            header: rmpv::Value::Nil,
            payload: rmpv::Value::Nil,
        })
        .await
        .unwrap();
        c2.send(&Frame {
            version: 1,
            msg_type: MsgType::Send,
            request_id: 200,
            header: rmpv::Value::Nil,
            payload: rmpv::Value::Nil,
        })
        .await
        .unwrap();
        let (r1, r2) = server.await.unwrap();
        let mut ids = vec![r1, r2];
        ids.sort_unstable();
        assert_eq!(ids, vec![100, 200]);
    }

    #[tokio::test]
    async fn high_throughput() {
        let transport = TcpTransport::new();
        let listener = transport
            .listen("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr();
        let count = 1000u64;
        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let mut received = 0u64;
            for _ in 0..count {
                conn.recv().await.unwrap();
                received += 1;
            }
            received
        });
        let mut client = transport.connect(addr).await.unwrap();
        for i in 0..count {
            client
                .send(&Frame {
                    version: 1,
                    msg_type: MsgType::Heartbeat,
                    request_id: i,
                    header: rmpv::Value::Nil,
                    payload: rmpv::Value::Nil,
                })
                .await
                .unwrap();
        }
        let received = server.await.unwrap();
        assert_eq!(received, count);
    }

    #[tokio::test]
    async fn connect_to_invalid_address_returns_error() {
        let transport = TcpTransport::new();
        let result = transport.connect("127.0.0.1:1".parse().unwrap()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn recv_rejects_oversized_length_prefix() {
        // A peer that sends FF FF FF FF as the length prefix must be rejected
        // BEFORE any ~4 GiB buffer is allocated. We assert recv returns
        // FrameTooLarge promptly without the test exhausting memory.
        let transport = TcpTransport::new();
        let listener = transport
            .listen("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr();
        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            conn.recv().await
        });
        let mut attacker = TcpStream::connect(addr).await.unwrap();
        attacker.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        attacker.flush().await.unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("recv hung — oversized length likely triggered a huge allocation")
            .unwrap();
        match result {
            Err(TransportError::FrameTooLarge { declared, max }) => {
                assert_eq!(declared, u32::MAX as usize);
                assert_eq!(max, crate::protocol::MAX_FRAME_SIZE);
            }
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn listener_local_addr() {
        let transport = TcpTransport::new();
        let listener = transport
            .listen("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr();
        assert_eq!(
            addr.ip(),
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        );
        assert_ne!(addr.port(), 0);
    }
}
