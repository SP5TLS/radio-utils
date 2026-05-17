use std::net::SocketAddr;

use crate::Result;

/// Transport abstraction for UDP communication.
/// Currently only native UDP is supported. A WebSocket relay transport
/// can be added for WASM environments.
pub struct UdpTransport {
    socket: tokio::net::UdpSocket,
}

impl UdpTransport {
    pub async fn bind(addr: &str) -> Result<Self> {
        let socket = tokio::net::UdpSocket::bind(addr).await?;
        socket.set_broadcast(true)?;
        Ok(Self { socket })
    }

    pub async fn bind_any() -> Result<Self> {
        Self::bind("0.0.0.0:0").await
    }

    pub async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> Result<()> {
        self.socket.send_to(buf, addr).await?;
        Ok(())
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        let (len, addr) = self.socket.recv_from(buf).await?;
        Ok((len, addr))
    }

    /// Non-blocking receive. Returns `Err` if no data is available.
    pub fn try_recv_from(
        &self,
        buf: &mut [u8],
    ) -> std::result::Result<(usize, SocketAddr), std::io::Error> {
        self.socket.try_recv_from(buf)
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }

    pub fn socket(&self) -> &tokio::net::UdpSocket {
        &self.socket
    }
}
