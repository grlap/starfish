//! Unified HTTP server.
//!
//! [`HttpServer`] listens for incoming HTTP/1.1 connections over TCP.
//! HTTP/3 server functionality is available via [`crate::h3::server::H3Server`].

use std::net::SocketAddr;

use starfish_reactor::cooperative_io::tcp_listener::TcpListener;
use starfish_reactor::cooperative_io::tcp_stream::TcpStream;

use crate::error::HttpError;
use crate::h1::server::H1Server;

/// HTTP server listening for incoming connections.
pub struct HttpServer {
    listener: TcpListener,
}

impl HttpServer {
    /// Bind the server to the given address.
    pub async fn bind(addr: SocketAddr) -> Result<Self, HttpError> {
        let listener = TcpListener::bind(addr).await.map_err(HttpError::Io)?;
        Ok(Self { listener })
    }

    /// Accept the next HTTP/1.1 connection.
    ///
    /// Returns an `H1Server` ready to receive requests on the accepted connection.
    pub async fn accept(&mut self) -> Result<H1Server<TcpStream>, HttpError> {
        let stream = self.listener.accept().await.map_err(HttpError::Io)?;
        Ok(H1Server::new(stream))
    }
}
