use std::{
    io,
    net::{self, ToSocketAddrs},
};

use super::{
    io_network::{tcp_listener_accept, tcp_listener_bind},
    io_timeout::IOTimeout,
    tcp_stream::TcpStream,
};

pub struct TcpListener {
    inner: net::TcpListener,
}

impl TcpListener {
    pub async fn bind<A: ToSocketAddrs>(socket_address: A) -> io::Result<TcpListener> {
        Ok(TcpListener {
            inner: tcp_listener_bind(socket_address).await?,
        })
    }

    pub async fn accept(&mut self) -> io::Result<TcpStream> {
        self.accept_with_timeout(&None).await
    }

    pub async fn accept_with_timeout(
        &mut self,
        timeout: &Option<IOTimeout>,
    ) -> io::Result<TcpStream> {
        Ok(TcpStream::new(
            tcp_listener_accept(&mut self.inner, timeout).await?,
        ))
    }
}
