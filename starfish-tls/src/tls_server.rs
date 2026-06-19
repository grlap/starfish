//! TLS server implementation for async streams.
//!
//! Provides `TlsServer<S>`, a wrapper that adds server-side TLS encryption
//! to any stream implementing `AsyncRead + AsyncWrite`. Performs automatic
//! handshake on construction and implements async read/write with timeout support.

use std::io::{self, Read, Write};
use std::sync::Arc;

use rustls::pki_types::CertificateDer;
use rustls::{ProtocolVersion, ServerConfig, ServerConnection, SupportedCipherSuite};
use starfish_reactor::cooperative_io::async_read::AsyncRead;
use starfish_reactor::cooperative_io::async_write::AsyncWrite;
use starfish_reactor::cooperative_io::async_write::AsyncWriteExtension;
use starfish_reactor::cooperative_io::io_timeout::IOTimeout;

use crate::error::TlsError;

/// TLS server that wraps an async stream
pub struct TlsServer<S> {
    connection: ServerConnection,
    stream: S,
    read_buf: Vec<u8>,
    write_buf: Vec<u8>,
}

impl<S> TlsServer<S>
where
    S: AsyncRead + AsyncWrite,
{
    /// Create a new TLS server with the provided certificate and private key
    pub async fn new(stream: S, config: ServerConfig) -> Result<Self, TlsError> {
        // Create server connection
        let connection = ServerConnection::new(Arc::new(config))?;

        // Create server
        let mut server = Self {
            connection,
            stream,
            read_buf: Vec::new(),
            write_buf: Vec::new(),
        };

        // Perform TLS handshake
        server.handshake_with_timeout(&None).await?;

        Ok(server)
    }

    /// Perform TLS handshake
    async fn handshake_with_timeout(&mut self, timeout: &Option<IOTimeout>) -> io::Result<()> {
        loop {
            // Process any buffered TLS messages
            self.connection
                .process_new_packets()
                .map_err(io::Error::other)?;

            // Check if handshake is complete
            if self.connection.is_handshaking() {
                // Write handshake data if needed
                let mut buffer = Vec::new();
                let wrote = self
                    .connection
                    .write_tls(&mut buffer)
                    .map_err(io::Error::other)?;

                if wrote > 0 {
                    self.stream.write_all_with_timeout(&buffer, timeout).await?;
                    self.stream.flush_with_timeout(timeout).await?;
                }

                // Read handshake response
                let mut buffer = [0u8; 4096];
                let count = self.stream.read_with_timeout(&mut buffer, timeout).await?;
                if count == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "EOF during handshake",
                    ));
                }

                // Feed received data to TLS connection
                self.connection
                    .read_tls(&mut &buffer[..count])
                    .map_err(io::Error::other)?;
            } else {
                // Flush any remaining handshake data that was generated
                // by process_new_packets but not yet written.
                let mut buffer = Vec::new();
                let wrote = self
                    .connection
                    .write_tls(&mut buffer)
                    .map_err(io::Error::other)?;
                if wrote > 0 {
                    self.stream.write_all_with_timeout(&buffer, timeout).await?;
                    self.stream.flush_with_timeout(timeout).await?;
                }
                return Ok(());
            }
        }
    }
}

impl<S> TlsServer<S> {
    /// Returns the negotiated TLS protocol version, if the handshake has completed.
    pub fn protocol_version(&self) -> Option<ProtocolVersion> {
        self.connection.protocol_version()
    }

    /// Returns the negotiated cipher suite, if the handshake has completed.
    pub fn negotiated_cipher_suite(&self) -> Option<SupportedCipherSuite> {
        self.connection.negotiated_cipher_suite()
    }

    /// Returns the peer's certificate chain, if available.
    pub fn peer_certificates(&self) -> Option<&[CertificateDer<'static>]> {
        self.connection.peer_certificates()
    }

    /// Returns the negotiated ALPN protocol, if any.
    pub fn alpn_protocol(&self) -> Option<&[u8]> {
        self.connection.alpn_protocol()
    }
}

// Implement AsyncRead for our TLS server
impl<S> AsyncRead for TlsServer<S>
where
    S: AsyncRead + AsyncWrite,
{
    async fn read_with_timeout(
        &mut self,
        buf: &mut [u8],
        timeout: &Option<IOTimeout>,
    ) -> io::Result<usize> {
        let mut buf_len = 0;

        self.read_buf
            .resize(buf.len().max(crate::MIN_READ_BUF_SIZE), 0);

        // Main read loop.
        //
        loop {
            // Drain any decrypted plaintext before touching the socket again.
            // Rustls can still report `wants_read()` even when application bytes
            // are already available, so gating this on `wants_read()` can hang
            // keep-alive protocols waiting for data that is already decrypted.
            if !self.connection.is_handshaking() {
                match self.connection.reader().read(&mut buf[buf_len..]) {
                    Ok(count) if count > 0 => {
                        buf_len += count;
                    }
                    _ => {}
                }
            }

            if buf_len > 0 {
                return Ok(buf_len);
            }

            // Read encrypted data from the underlying stream.
            //
            let count = match self
                .stream
                .read_with_timeout(&mut self.read_buf, timeout)
                .await
            {
                Ok(0) => {
                    if self.connection.is_handshaking() {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "EOF during handshake",
                        ));
                    }

                    // Stream EOF — drain any remaining plaintext.
                    //
                    let _ = self.connection.process_new_packets();
                    match self.connection.reader().read(&mut buf[buf_len..]) {
                        Ok(n) if n > 0 => return Ok(buf_len + n),
                        _ => return Ok(buf_len),
                    }
                }
                Ok(count) => count,
                Err(e) => return Err(e),
            };

            // Feed received data to the TLS connection, looping in case
            // read_tls cannot consume everything at once.
            //
            let mut offset = 0;
            while offset < count {
                let n = self
                    .connection
                    .read_tls(&mut &self.read_buf[offset..count])
                    .map_err(io::Error::other)?;
                if n == 0 {
                    break;
                }
                offset += n;

                // Process after each feed to free internal buffer space.
                //
                match self.connection.process_new_packets() {
                    Ok(io_state) => {
                        if io_state.plaintext_bytes_to_read() > 0 {
                            match self.connection.reader().read(&mut buf[buf_len..]) {
                                Ok(c) => {
                                    buf_len += c;
                                }
                                Err(err) => {
                                    return Err(io::Error::other(err));
                                }
                            }
                        }
                    }
                    Err(err) => {
                        return Err(io::Error::other(err));
                    }
                }
            }
        }
    }
}

// Implement AsyncWrite for our TLS server
impl<S> AsyncWrite for TlsServer<S>
where
    S: AsyncRead + AsyncWrite,
{
    async fn write_with_timeout(
        &mut self,
        buf: &[u8],
        timeout: &Option<IOTimeout>,
    ) -> io::Result<usize> {
        // Write plaintext data to TLS connection
        let written = self
            .connection
            .writer()
            .write(buf)
            .map_err(io::Error::other)?;

        // Send encrypted data
        self.flush_with_timeout(timeout).await?;

        Ok(written)
    }

    async fn flush_with_timeout(&mut self, timeout: &Option<IOTimeout>) -> io::Result<()> {
        // Process any pending TLS messages
        self.connection
            .process_new_packets()
            .map_err(io::Error::other)?;

        // Write any pending TLS data to the underlying stream
        loop {
            self.write_buf.clear();

            // Use write_tls with our buffer as the writer
            let wrote = self
                .connection
                .write_tls(&mut self.write_buf)
                .map_err(io::Error::other)?;

            // If no data was written, we're done
            if wrote == 0 {
                break;
            }

            // Write the buffered data to the underlying stream
            self.stream
                .write_all_with_timeout(&self.write_buf, timeout)
                .await?;
        }

        // Flush the underlying stream
        self.stream.flush_with_timeout(timeout).await?;

        Ok(())
    }

    async fn close(&mut self) -> io::Result<()> {
        // Initiate TLS close
        self.connection.send_close_notify();

        // Flush remaining data
        self.flush().await
    }
}
