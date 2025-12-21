use std::io::{self, Read, Write};
use std::sync::Arc;

use rustls::{ServerConfig, ServerConnection};
use starfish_reactor::cooperative_io::async_read::AsyncRead;
use starfish_reactor::cooperative_io::async_write::AsyncWrite;
use starfish_reactor::cooperative_io::async_write::AsyncWriteExtension;
use starfish_reactor::cooperative_io::io_timeout::IOTimeout;

/// TLS server that wraps an async stream
pub struct TlsServer<S> {
    connection: ServerConnection,
    stream: S,
}

impl<S> TlsServer<S>
where
    S: AsyncRead + AsyncWrite,
{
    /// Create a new TLS server with the provided certificate and private key
    pub async fn new(stream: S, config: ServerConfig) -> Result<Self, Box<dyn std::error::Error>> {
        // Create server connection
        let connection = ServerConnection::new(Arc::new(config))?;

        // Create server
        let mut server = Self { connection, stream };

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
                return Ok(());
            }
        }
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
        loop {
            // First try reading from already decrypted data
            match self.connection.reader().read(buf) {
                Ok(count) => {
                    // Successfully read some data (or got 0 with empty buffer)
                    if count > 0 || buf.is_empty() {
                        return Ok(count);
                    }

                    // Got 0 bytes with non-empty buffer, need more data
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // Need more data, continue to read from stream.
                }
                Err(e) => {
                    // Other error, propagate it
                    return Err(io::Error::other(e));
                }
            }

            // Need more data from the TCP stream
            // Check if connection is closed AND no more buffered data
            if !self.connection.wants_read() && !self.connection.is_handshaking() {
                // Check for any pending data after closing
                match self.connection.reader().read(buf) {
                    Ok(0) => return Ok(0),  // Truly done
                    Ok(n) => return Ok(n),  // Some final data
                    Err(_) => return Ok(0), // No more data
                }
            }

            // Read more encrypted data from the underlying stream
            let mut tls_buffer = [0u8; 8192]; // Larger buffer
            let count = self
                .stream
                .read_with_timeout(&mut tls_buffer, timeout)
                .await?;

            if count == 0 {
                // EOF on underlying stream
                if self.connection.is_handshaking() {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "EOF during handshake",
                    ));
                }

                // Process any final packets (errors expected during connection close)
                let _ = self.connection.process_new_packets();

                // Try one more read for any buffered data
                match self.connection.reader().read(buf) {
                    Ok(n) => return Ok(n),
                    Err(_) => return Ok(0),
                }
            }

            // Feed received data to TLS connection
            self.connection
                .read_tls(&mut &tls_buffer[..count])
                .map_err(io::Error::other)?;

            // Process the new TLS data
            if let Err(e) = self.connection.process_new_packets() {
                return Err(io::Error::other(e));
            }

            // Try reading again after processing new data
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
        let mut buffer = Vec::new();
        loop {
            buffer.clear(); // Reuse allocation

            // Use write_tls with our buffer as the writer
            let wrote = self
                .connection
                .write_tls(&mut buffer)
                .map_err(io::Error::other)?;

            // If no data was written, we're done
            if wrote == 0 {
                break;
            }

            // Write the buffered data to the underlying stream
            self.stream.write_all_with_timeout(&buffer, timeout).await?;
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
