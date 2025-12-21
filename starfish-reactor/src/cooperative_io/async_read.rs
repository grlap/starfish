use std::future::Future;
use std::io;

use super::io_timeout::IOTimeout;

pub trait AsyncRead {
    /// Attempts to read from the source into the provided buffer.
    /// Returns the number of bytes read or an error.
    ///
    fn read_with_timeout(
        &mut self,
        buf: &mut [u8],
        timeout: &Option<IOTimeout>,
    ) -> impl Future<Output = io::Result<usize>>;
}

pub trait AsyncReadExtension: AsyncRead {
    fn read(&mut self, buf: &mut [u8]) -> impl Future<Output = io::Result<usize>> {
        self.read_with_timeout(buf, &None)
    }

    fn read_exact_with_timeout(
        &mut self,
        buf: &mut [u8],
        timeout: &Option<IOTimeout>,
    ) -> impl Future<Output = io::Result<()>> {
        async {
            // Implementation from previous example
            let mut read = 0;
            while read < buf.len() {
                match self.read_with_timeout(&mut buf[read..], timeout).await? {
                    0 => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "failed to fill whole buffer",
                        ));
                    }
                    n => read += n,
                }
            }
            Ok(())
        }
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> impl Future<Output = io::Result<()>> {
        self.read_exact_with_timeout(buf, &None)
    }
}

// Implement the extension trait for all types that implement AsyncRead.
//
impl<T: AsyncRead> AsyncReadExtension for T {}
