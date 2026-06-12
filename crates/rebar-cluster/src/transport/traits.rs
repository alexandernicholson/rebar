use std::net::SocketAddr;

use async_trait::async_trait;

use crate::protocol::Frame;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("connection closed")]
    ConnectionClosed,
    #[error("declared frame length {declared} exceeds maximum {max}")]
    FrameTooLarge { declared: usize, max: usize },
    #[error("frame error: {0}")]
    Frame(#[from] crate::protocol::FrameError),
}

#[async_trait]
pub trait TransportConnection: Send + Sync {
    async fn send(&mut self, frame: &Frame) -> Result<(), TransportError>;
    async fn recv(&mut self) -> Result<Frame, TransportError>;
    async fn close(&mut self) -> Result<(), TransportError>;
}

#[async_trait]
pub trait TransportListener: Send + Sync {
    type Connection: TransportConnection;
    fn local_addr(&self) -> SocketAddr;
    async fn accept(&self) -> Result<Self::Connection, TransportError>;
}
