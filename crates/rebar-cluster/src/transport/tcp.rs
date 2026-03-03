use std::net::SocketAddr;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::protocol::Frame;
use crate::transport::traits::{TransportConnection, TransportError, TransportListener};

/// TCP transport using length-prefixed framing.
///
/// Wire format:
/// ```text
/// ┌──────────┬──────────────┐
/// │ len: u32 │ payload: [u8]│
/// └──────────┴──────────────┘
/// ```
pub struct TcpTransport;

impl TcpTransport {
    pub fn new() -> Self {
        TcpTransport
    }

    pub async fn listen(&self, addr: SocketAddr) -> Result<TcpTransportListener, TransportError> {
        let listener = TcpListener::bind(addr).await?;
        Ok(TcpTransportListener { inner: listener })
    }

    pub async fn connect(&self, addr: SocketAddr) -> Result<TcpConnection, TransportError> {
        let stream = TcpStream::connect(addr).await?;
        Ok(TcpConnection { stream })
    }
}

pub struct TcpTransportListener {
    inner: TcpListener,
}

#[async_trait]
impl TransportListener for TcpTransportListener {
    type Connection = TcpConnection;

    fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr().expect("listener has local addr")
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
        let len = encoded.len() as u32;
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
        ids.sort();
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
