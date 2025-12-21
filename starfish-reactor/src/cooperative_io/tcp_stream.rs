use std::{
    future::Future,
    io,
    net::{self, SocketAddr},
};

use super::{
    async_read::AsyncRead,
    async_write::AsyncWrite,
    io_network::{tcp_stream_connect, tcp_stream_read, tcp_stream_write},
    io_timeout::IOTimeout,
};

pub struct TcpStream {
    inner: net::TcpStream,
}

impl TcpStream {
    pub(super) fn new(inner: net::TcpStream) -> TcpStream {
        TcpStream { inner }
    }

    pub async fn connect(socket_address: SocketAddr) -> io::Result<TcpStream> {
        TcpStream::connect_with_timeout(socket_address, &None).await
    }

    pub async fn connect_with_timeout(
        socket_address: SocketAddr,
        timeout: &Option<IOTimeout>,
    ) -> io::Result<TcpStream> {
        Ok(TcpStream::new(
            tcp_stream_connect(socket_address, timeout).await?,
        ))
    }
}

impl AsyncRead for TcpStream {
    fn read_with_timeout(
        &mut self,
        buf: &mut [u8],
        timeout: &Option<IOTimeout>,
    ) -> impl Future<Output = io::Result<usize>> {
        tcp_stream_read(&mut self.inner, buf, timeout)
    }
}

impl AsyncWrite for TcpStream {
    fn write_with_timeout(
        &mut self,
        buf: &[u8],
        timeout: &Option<IOTimeout>,
    ) -> impl Future<Output = io::Result<usize>> {
        tcp_stream_write(&mut self.inner, buf, timeout)
    }
}
