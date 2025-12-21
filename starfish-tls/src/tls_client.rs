use std::error;
use std::io::{self, Read, Write};
use std::sync::Arc;

use rustls::{pki_types::ServerName, ClientConfig, ClientConnection};
use starfish_reactor::cooperative_io::async_read::AsyncRead;
use starfish_reactor::cooperative_io::async_write::AsyncWrite;
use starfish_reactor::cooperative_io::async_write::AsyncWriteExtension;
use starfish_reactor::cooperative_io::io_timeout::IOTimeout;

// Create a TLS client that wraps an async stream
pub struct TlsClient<S> {
    connection: ClientConnection,
    stream: S,
}

const TLS_READ_BUFFER_SIZE: usize = 1024;

impl<S> TlsClient<S>
where
    S: AsyncRead + AsyncWrite,
{
    /// Create a new TLS client.
    ///
    pub async fn new(
        stream: S,
        config: ClientConfig,
        server_name_string: impl Into<String>,
    ) -> Result<Self, Box<dyn error::Error>> {
        // Parse server name.
        //
        let server_name = ServerName::try_from(server_name_string.into())?;

        // Create TLS connection.
        //
        let connection = ClientConnection::new(Arc::new(config), server_name)?;

        // Create client.
        //
        let mut client = Self { connection, stream };

        // Perform TLS handshake.
        //
        client.handshake_with_timeout(&None).await?;

        Ok(client)
    }

    /// Perform TLS handshake.
    ///
    async fn handshake_with_timeout(&mut self, timeout: &Option<IOTimeout>) -> io::Result<()> {
        loop {
            // Process any buffered TLS messages.
            //
            self.connection
                .process_new_packets()
                .map_err(io::Error::other)?;

            // Check if handshake is complete.
            //
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

                // Read handshake response.
                //
                let mut buffer = [0u8; 1024];
                let count = self.stream.read_with_timeout(&mut buffer, timeout).await?;
                if count == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "EOF during handshake",
                    ));
                }

                // Feed received data to TLS connection.
                //
                self.connection
                    .read_tls(&mut &buffer[..count])
                    .map_err(io::Error::other)?;
            } else {
                return Ok(());
            }
        }
    }
}

impl<S> AsyncRead for TlsClient<S>
where
    S: AsyncRead + AsyncWrite,
{
    async fn read_with_timeout(
        &mut self,
        buf: &mut [u8],
        timeout: &Option<IOTimeout>,
    ) -> io::Result<usize> {
        let mut buf_len = 0;

        let mut stream_buf = vec![0u8; buf.len()];

        // Main read loop.
        //
        loop {
            // Check if the TLS connection is closed or not expecting more data.
            //
            if !self.connection.wants_read() && !self.connection.is_handshaking() {
                // Return buffered data if available.
                //
                match self.connection.reader().read(&mut buf[buf_len..]) {
                    Ok(count) if count > 0 => {
                        buf_len += count;
                    }
                    _ => {
                        // No more data available.
                        //
                        return Ok(buf_len);
                    }
                }
            }

            if buf_len > 0 {
                return Ok(buf_len);
            }

            // Read from the underlying stream with a controlled buffer size.
            //
            let count = match self
                .stream
                .read_with_timeout(&mut stream_buf, timeout)
                .await
            {
                Ok(count) => {
                    // EOF on underlying stream.
                    //
                    if self.connection.is_handshaking() {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "EOF during handshake",
                        ));
                    }

                    count
                }
                Err(e) => return Err(e),
            };

            // Process the buffer read from the underlying stream in the smaller chunks.
            //
            let mut buf_index = 0;
            while buf_index < count {
                let tls_read_buf_len = std::cmp::min(count - buf_index, TLS_READ_BUFFER_SIZE);

                // Feed data to TLS connection.
                //
                match self
                    .connection
                    .read_tls(&mut &stream_buf[buf_index..buf_index + tls_read_buf_len])
                {
                    Ok(_) => {
                        // Process the new TLS packets.
                        //
                        match self.connection.process_new_packets() {
                            Ok(io_state) => {
                                // If there is available plaintext data, copy to the buffer.
                                //
                                if io_state.plaintext_bytes_to_read() > 0 {
                                    match self.connection.reader().read(&mut buf[buf_len..]) {
                                        Ok(count) => {
                                            buf_len += count;
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

                    Err(e) => {
                        return Err(io::Error::other(e));
                    }
                }

                buf_index += tls_read_buf_len;
            }
        }
    }
}

impl<S> AsyncWrite for TlsClient<S>
where
    S: AsyncRead + AsyncWrite,
{
    async fn write_with_timeout(
        &mut self,
        buf: &[u8],
        timeout: &Option<IOTimeout>,
    ) -> io::Result<usize> {
        // Write plaintext data to TLS connection.
        //
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
