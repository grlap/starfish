use std::fs::OpenOptions;
use std::io;
use std::path::Path;

use super::file::File;

pub trait AsyncOpenOptions {
    fn as_async_io(&mut self) -> FileOpenOptions<'_>;
}

pub struct FileOpenOptions<'a> {
    inner: &'a mut OpenOptions,
}

impl<'a> FileOpenOptions<'a> {
    pub fn new(inner: &'a mut OpenOptions) -> Self {
        FileOpenOptions { inner }
    }

    pub fn open<P: AsRef<Path>>(&mut self, path: P) -> io::Result<File> {
        File::open(self.inner, path)
    }
}
