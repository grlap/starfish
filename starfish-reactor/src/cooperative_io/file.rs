//! Cooperative async file I/O.
//!
//! Provides `File`, a wrapper around `std::fs::File` that implements `AsyncRead`
//! and `AsyncWrite` for non-blocking file operations within the reactor.
//!
//! # File-position contract
//!
//! Sequential reads and writes follow AND advance a single shared file
//! position, and reads at EOF return `Ok(0)` — `read(2)`/`write(2)` semantics.
//! `AsyncReadExtension::read_exact` / `AsyncWriteExtension::write_all` depend
//! on this to make progress across partial transfers. Each backend implements
//! the contract its own way: io_uring passes offset `-1` (kernel file
//! position), macOS uses plain cursor syscalls, and Windows tracks an
//! explicit `FilePosition` stamped into each `OVERLAPPED`, because overlapped
//! handles ignore the kernel file pointer (the per-backend `io_file` module
//! docs carry the details; `FilePosition` is zero-sized where the kernel
//! already provides the cursor).
//!
//! Caveat: the EOF clause is fully honored on Linux and Windows
//! (`test_read_at_eof_returns_zero`), but the macOS backend has an open
//! continuation-loop defect (`bugs.md` #48) that can mis-handle a read at
//! EOF — the regression test excludes macOS until #48 is fixed.

use std::fs::OpenOptions;
use std::future::Future;
use std::io;
use std::path::Path;

use super::{
    async_read::AsyncRead,
    async_write::AsyncWrite,
    io_file::{FilePosition, prepare_io_with_file, read_file, write_file},
    io_timeout::IOTimeout,
};

pub struct File {
    inner: std::fs::File,
    position: FilePosition,
}

impl File {
    pub(crate) fn open<P: AsRef<Path>>(options: &mut OpenOptions, path: P) -> io::Result<File> {
        let file = options.open(path)?;
        prepare_io_with_file(&file)?;

        Ok(File {
            inner: file,
            position: FilePosition::new(),
        })
    }
}

impl AsyncRead for File {
    fn read_with_timeout(
        &mut self,
        buf: &mut [u8],
        timeout: &Option<IOTimeout>,
    ) -> impl Future<Output = io::Result<usize>> {
        read_file(&mut self.inner, &mut self.position, buf, timeout)
    }
}

impl AsyncWrite for File {
    fn write_with_timeout(
        &mut self,
        buf: &[u8],
        timeout: &Option<IOTimeout>,
    ) -> impl Future<Output = io::Result<usize>> {
        write_file(&mut self.inner, &mut self.position, buf, timeout)
    }
}
