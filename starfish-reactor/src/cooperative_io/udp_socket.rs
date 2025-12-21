use std::{
    io,
    net::{self, SocketAddr},
};

use super::{
    io_network::{udp_recv_from, udp_send_to, udp_socket_bind},
    io_timeout::IOTimeout,
};

pub struct UdpSocket {
    inner: net::UdpSocket,
}

impl UdpSocket {
    pub fn new(inner: net::UdpSocket) -> UdpSocket {
        UdpSocket { inner }
    }
    pub async fn bind(socket_address: SocketAddr) -> io::Result<UdpSocket> {
        Ok(UdpSocket::new(udp_socket_bind(socket_address).await?))
    }

    pub async fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.recv_from_with_timeout(buf, &None).await
    }

    pub async fn recv_from_with_timeout(
        &mut self,
        buf: &mut [u8],
        timeout: &Option<IOTimeout>,
    ) -> io::Result<(usize, SocketAddr)> {
        udp_recv_from(&self.inner, buf, timeout).await
    }

    pub async fn send_to(
        &mut self,
        destination_address: SocketAddr,
        buf: &mut [u8],
    ) -> io::Result<usize> {
        self.send_to_with_timeout(destination_address, buf, &None)
            .await
    }

    pub async fn send_to_with_timeout(
        &mut self,
        destination_address: SocketAddr,
        buf: &mut [u8],
        timeout: &Option<IOTimeout>,
    ) -> io::Result<usize> {
        udp_send_to(&self.inner, destination_address, buf, timeout).await
    }
}
