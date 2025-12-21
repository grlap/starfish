use std::fs::OpenOptions;
use std::future::Future;
use std::io;
use std::path::Path;

use super::{
    async_read::AsyncRead,
    async_write::AsyncWrite,
    io_file::{prepare_io_with_file, read_file, write_file},
    io_timeout::IOTimeout,
};

pub struct File {
    inner: std::fs::File,
}

impl File {
    pub(crate) fn open<P: AsRef<Path>>(options: &mut OpenOptions, path: P) -> io::Result<File> {
        let file = options.open(path)?;
        prepare_io_with_file(&file)?;

        Ok(File { inner: file })
    }
}

impl AsyncRead for File {
    fn read_with_timeout(
        &mut self,
        buf: &mut [u8],
        timeout: &Option<IOTimeout>,
    ) -> impl Future<Output = io::Result<usize>> {
        read_file(&mut self.inner, buf, timeout)
    }
}

impl AsyncWrite for File {
    fn write_with_timeout(
        &mut self,
        buf: &[u8],
        timeout: &Option<IOTimeout>,
    ) -> impl Future<Output = io::Result<usize>> {
        write_file(&mut self.inner, buf, timeout)
    }
}
