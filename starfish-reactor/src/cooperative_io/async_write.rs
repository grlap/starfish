use std::future::Future;
use std::io;

use super::io_timeout::IOTimeout;

pub trait AsyncWrite {
    /// Attempts to write the contents of `buf` into this writer.
    /// Returns the number of bytes written or an error.
    ///
    fn write_with_timeout(
        &mut self,
        buf: &[u8],
        timeout: &Option<IOTimeout>,
    ) -> impl Future<Output = io::Result<usize>>;

    fn flush_with_timeout(
        &mut self,
        _: &Option<IOTimeout>,
    ) -> impl std::future::Future<Output = io::Result<()>> {
        async { Ok(()) }
    }

    fn close(&mut self) -> impl std::future::Future<Output = io::Result<()>> {
        self.flush_with_timeout(&None)
    }
}

pub trait AsyncWriteExtension: AsyncWrite {
    fn write_all_with_timeout(
        &mut self,
        buf: &[u8],
        timeout: &Option<IOTimeout>,
    ) -> impl Future<Output = io::Result<()>> {
        async move {
            let mut written = 0;
            while written < buf.len() {
                match self.write_with_timeout(&buf[written..], timeout).await? {
                    0 => {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "failed to write whole buffer",
                        ));
                    }
                    n => written += n,
                }
            }
            Ok(())
        }
    }

    fn write(&mut self, buf: &[u8]) -> impl Future<Output = io::Result<usize>> {
        self.write_with_timeout(buf, &None)
    }

    fn write_all(&mut self, buf: &[u8]) -> impl Future<Output = io::Result<()>> {
        self.write_all_with_timeout(buf, &None)
    }

    fn flush(&mut self) -> impl std::future::Future<Output = io::Result<()>> {
        self.flush_with_timeout(&None)
    }
}

// Implement the extension trait for all types that implement AsyncWrite.
//
impl<T: AsyncWrite> AsyncWriteExtension for T {}
