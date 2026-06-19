//! Cooperative async UDP socket.
//!
//! Provides `UdpSocket` for asynchronous `send_to` and `recv_from` operations
//! within the reactor's cooperative scheduler, including optional ECN send and
//! receive metadata for transports like QUIC.

use std::{
    io,
    net::{self, SocketAddr},
};

use super::{
    io_network::{udp_recv_from, udp_send_to, udp_socket_bind, udp_socket_enable_ecn},
    io_timeout::IOTimeout,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdpEcnCodepoint {
    Ect0,
    Ect1,
    Ce,
}

impl UdpEcnCodepoint {
    pub fn from_bits(bits: u8) -> Option<Self> {
        match bits & 0b11 {
            0b10 => Some(Self::Ect0),
            0b01 => Some(Self::Ect1),
            0b11 => Some(Self::Ce),
            _ => None,
        }
    }

    pub fn bits(self) -> u8 {
        match self {
            Self::Ect0 => 0b10,
            Self::Ect1 => 0b01,
            Self::Ce => 0b11,
        }
    }
}

pub struct UdpSocket {
    inner: net::UdpSocket,
}

impl UdpSocket {
    pub fn new(inner: net::UdpSocket) -> UdpSocket {
        let _ = udp_socket_enable_ecn(&inner);
        UdpSocket { inner }
    }
    pub async fn bind(socket_address: SocketAddr) -> io::Result<UdpSocket> {
        Ok(UdpSocket::new(udp_socket_bind(socket_address).await?))
    }

    /// Create a duplicate handle to the same underlying socket.
    ///
    /// The clone shares the same local address and port. Both handles
    /// can send independently, but only one should call `recv_from` at a time.
    pub fn try_clone(&self) -> io::Result<UdpSocket> {
        Ok(UdpSocket::new(self.inner.try_clone()?))
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    pub async fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let (len, addr, _) = self.recv_from_with_timeout_and_ecn(buf, &None).await?;
        Ok((len, addr))
    }

    pub async fn recv_from_with_timeout(
        &mut self,
        buf: &mut [u8],
        timeout: &Option<IOTimeout>,
    ) -> io::Result<(usize, SocketAddr)> {
        let (len, addr, _) = self.recv_from_with_timeout_and_ecn(buf, timeout).await?;
        Ok((len, addr))
    }

    pub async fn recv_from_with_ecn(
        &mut self,
        buf: &mut [u8],
    ) -> io::Result<(usize, SocketAddr, Option<UdpEcnCodepoint>)> {
        self.recv_from_with_timeout_and_ecn(buf, &None).await
    }

    pub async fn recv_from_with_timeout_and_ecn(
        &mut self,
        buf: &mut [u8],
        timeout: &Option<IOTimeout>,
    ) -> io::Result<(usize, SocketAddr, Option<UdpEcnCodepoint>)> {
        udp_recv_from(&self.inner, buf, timeout).await
    }

    pub async fn send_to(
        &mut self,
        destination_address: SocketAddr,
        buf: &mut [u8],
    ) -> io::Result<usize> {
        self.send_to_with_timeout_and_ecn(destination_address, buf, &None, None)
            .await
    }

    pub async fn send_to_with_timeout(
        &mut self,
        destination_address: SocketAddr,
        buf: &mut [u8],
        timeout: &Option<IOTimeout>,
    ) -> io::Result<usize> {
        self.send_to_with_timeout_and_ecn(destination_address, buf, timeout, None)
            .await
    }

    pub async fn send_to_with_ecn(
        &mut self,
        destination_address: SocketAddr,
        buf: &mut [u8],
        ecn: Option<UdpEcnCodepoint>,
    ) -> io::Result<usize> {
        self.send_to_with_timeout_and_ecn(destination_address, buf, &None, ecn)
            .await
    }

    pub async fn send_to_with_timeout_and_ecn(
        &mut self,
        destination_address: SocketAddr,
        buf: &mut [u8],
        timeout: &Option<IOTimeout>,
        ecn: Option<UdpEcnCodepoint>,
    ) -> io::Result<usize> {
        udp_send_to(&self.inner, destination_address, buf, timeout, ecn).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udp_ecn_codepoint_round_trips_known_bits() {
        for codepoint in [
            UdpEcnCodepoint::Ect0,
            UdpEcnCodepoint::Ect1,
            UdpEcnCodepoint::Ce,
        ] {
            assert_eq!(
                UdpEcnCodepoint::from_bits(codepoint.bits()),
                Some(codepoint)
            );
        }
    }

    #[test]
    fn udp_ecn_codepoint_masks_unrelated_bits_and_rejects_non_ecn() {
        assert_eq!(
            UdpEcnCodepoint::from_bits(0b10),
            Some(UdpEcnCodepoint::Ect0)
        );
        assert_eq!(
            UdpEcnCodepoint::from_bits(0b1010),
            Some(UdpEcnCodepoint::Ect0)
        );
        assert_eq!(
            UdpEcnCodepoint::from_bits(0b0101),
            Some(UdpEcnCodepoint::Ect1)
        );
        assert_eq!(
            UdpEcnCodepoint::from_bits(0b1111),
            Some(UdpEcnCodepoint::Ce)
        );
        assert_eq!(UdpEcnCodepoint::from_bits(0b00), None);
        assert_eq!(UdpEcnCodepoint::from_bits(0b1100), None);
    }

    #[test]
    fn try_clone_shares_local_addr() {
        let std_sock = net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let local_addr = std_sock.local_addr().unwrap();
        let sock = UdpSocket::new(std_sock);

        let cloned = sock.try_clone().unwrap();
        assert_eq!(cloned.inner.local_addr().unwrap(), local_addr);
    }
}
