use std::io;
use std::io::Error;
use std::ptr;
use std::result::Result;
use std::sync::Arc;

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Foundation::ERROR_ABANDONED_WAIT_0;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows::Win32::System::IO::CreateIoCompletionPort;
use windows::Win32::System::IO::GetQueuedCompletionStatus;
use windows::Win32::System::IO::GetQueuedCompletionStatusEx;
use windows::Win32::System::IO::OVERLAPPED;
use windows::Win32::System::IO::OVERLAPPED_ENTRY;
use windows::Win32::System::IO::PostQueuedCompletionStatus;
use windows::core::HRESULT;

/// Represents an I/O completion port.
///
pub struct IoCompletionPort {
    inner: Arc<IocpImp>,
}

unsafe impl Send for IoCompletionPort {}
unsafe impl Sync for IoCompletionPort {}

impl Clone for IoCompletionPort {
    fn clone(&self) -> IoCompletionPort {
        IoCompletionPort {
            inner: self.inner.clone(),
        }
    }
}

impl IoCompletionPort {
    /// Create a new IoCompletionPort with the specified number of concurrent threads.
    ///
    /// If zero threads are specified, the system allows as many concurrently running
    /// threads as there are processors in the system.
    ///
    pub fn new(concurrent_threads: u32) -> Result<IoCompletionPort, io::Error> {
        Ok(IoCompletionPort {
            inner: Arc::new(IocpImp::new(concurrent_threads)?),
        })
    }

    /// Assoicates the given file handle with this IoCompletionPort.
    ///
    /// The completion key is included in every I/O completion packet for the specified file handle.
    pub fn associate(&self, handle: HANDLE, completion_key: usize) -> Result<(), io::Error> {
        self.inner.associate(handle, completion_key)
    }

    /// Attempts to dequeue an I/O completion packet from the IoCompletionPort.
    ///
    pub fn get_queued(&self, timeout: u32) -> Result<CompletionStatus, io::Error> {
        self.inner.get_queued(timeout)
    }

    /// Attempts to dequeue multiple I/O completion packets from the IoCompletionPort simultaneously.
    /// Returns the number of CompletionStatus objects dequeued.
    ///
    pub fn get_many_queued(
        &self,
        buf: &mut [CompletionStatus],
        timeout: u32,
    ) -> Result<usize, io::Error> {
        self.inner.get_many_queued(buf, timeout)
    }

    /// Posts an I/O completion packet to the IoCompletionPort.
    ///
    /// Note that the OVERLAPPED structure in the CompletionStatus does not have to be valid (it can be a null pointer).
    /// Ensure that if you intend to post an OVERLAPPED structure, it is not freed until the CompletionStatus is dequeued.
    pub fn post_queued(&self, packet: CompletionStatus) -> Result<(), io::Error> {
        self.inner.post_queued(packet)
    }

    pub fn close(&mut self) {
        self.inner.close();
    }
}

/// Represents an I/O completion status packet.
///
#[derive(Copy, Clone)]
pub struct CompletionStatus {
    /// The number of bytes transferred during the operation.
    pub byte_count: usize,
    /// The completion key associated with this packet
    pub completion_key: usize,

    /// A pointer to the overlapped structure which may or may not be valid
    pub overlapped: *mut OVERLAPPED,
}

impl CompletionStatus {
    /// Creates a new CompletionStatus instance.
    ///
    pub fn new() -> CompletionStatus {
        CompletionStatus {
            byte_count: 0,
            completion_key: 0,
            overlapped: ptr::null_mut(),
        }
    }
}

impl Default for CompletionStatus {
    fn default() -> Self {
        Self::new()
    }
}

struct IocpImp {
    inner: HANDLE,
}

impl IocpImp {
    pub fn new(concurrent_threads: u32) -> Result<IocpImp, io::Error> {
        let handle = unsafe {
            CreateIoCompletionPort(INVALID_HANDLE_VALUE, HANDLE(0), 0, concurrent_threads)
        }?;

        Ok(IocpImp { inner: handle })
    }

    pub fn associate(&self, handle: HANDLE, completion_key: usize) -> Result<(), io::Error> {
        let handle = unsafe { CreateIoCompletionPort(handle, self.inner, completion_key, 0) }?;

        if handle.0 == 0 {
            return Err(Error::last_os_error());
        }

        Ok(())
    }

    pub fn get_queued(&self, timeout: u32) -> Result<CompletionStatus, io::Error> {
        let mut length: u32 = 0;
        let mut key: usize = 0;
        let mut overlapped = ptr::null_mut();

        let iocp_handle = self.inner;

        if iocp_handle == INVALID_HANDLE_VALUE {
            return Err(Error::from_raw_os_error(
                HRESULT::from_win32(ERROR_ABANDONED_WAIT_0.0).0,
            ));
        }

        unsafe {
            // Ignore the error. We will get the error in IOWaitFuture::is_completed using GetOverlappedResult.
            //
            let _ = GetQueuedCompletionStatus(
                iocp_handle,
                &mut length,
                &mut key,
                &mut overlapped,
                timeout,
            );
        };

        Ok(CompletionStatus {
            byte_count: length as usize,
            completion_key: key,
            overlapped,
        })
    }

    pub fn get_many_queued(
        &self,
        buf: &mut [CompletionStatus],
        timeout: u32,
    ) -> Result<usize, io::Error> {
        let n = buf.len();

        let mut entries: Vec<OVERLAPPED_ENTRY> = vec![OVERLAPPED_ENTRY::default(); n];

        let mut removed = 0;

        unsafe {
            GetQueuedCompletionStatusEx(
                self.inner,
                entries.as_mut_slice(),
                &mut removed,
                timeout,
                false,
            )?
        };

        for (status, entry) in buf.iter_mut().zip(entries.iter()).take(removed as usize) {
            *status = CompletionStatus {
                byte_count: entry.dwNumberOfBytesTransferred as usize,
                completion_key: entry.lpCompletionKey,
                overlapped: entry.lpOverlapped,
            };
        }

        Ok(removed as usize)
    }

    pub fn post_queued(&self, packet: CompletionStatus) -> Result<(), io::Error> {
        unsafe {
            PostQueuedCompletionStatus(
                self.inner,
                packet.byte_count as u32,
                packet.completion_key,
                Some(packet.overlapped),
            )?
        };

        Ok(())
    }

    pub fn close(&self) {
        // Post an empty status to signal termination.
        //
        self.post_queued(CompletionStatus {
            byte_count: 0,
            completion_key: 0,
            overlapped: ptr::null_mut(),
        })
        .unwrap();
    }
}

impl Drop for IocpImp {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.inner);
            self.inner = INVALID_HANDLE_VALUE;
        }
    }
}
