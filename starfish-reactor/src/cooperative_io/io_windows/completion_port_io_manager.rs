use std::{io, ptr, sync::Arc, thread};

use crossbeam::queue::SegQueue;
use ctor::{ctor, dtor};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Networking::WinSock::{WSACleanup, WSADATA, WSAStartup};
use windows::Win32::System::IO::CancelIoEx;

use crate::cooperative_io::io_manager::{IOManager, IOManagerCreateOptions, IOManagerHolder};
use crate::reactor::FutureRuntime;

use super::iocp::IoCompletionPort;
use super::{io_wait_future::IOWaitFuture, iocp::CompletionStatus};

pub(crate) struct CompletionPortIOManager {
    iocp_handler: Arc<CompletionPortIOHandler>,
    completed_io: SegQueue<CompletionStatus>,
    active_io: i32,
}

impl CompletionPortIOManager {
    pub fn try_new() -> io::Result<Self> {
        Ok(Self::with_iocp_handler(Arc::new(
            CompletionPortIOHandler::try_new()?,
        )))
    }

    /// Creates a new instance of CompletionPortIOManager.
    ///
    pub fn with_iocp_handler(iocp_handler: Arc<CompletionPortIOHandler>) -> Self {
        CompletionPortIOManager {
            iocp_handler,
            completed_io: SegQueue::new(),
            active_io: 0,
        }
    }

    pub fn prepare_io(&mut self, handle: HANDLE) -> Result<(), io::Error> {
        let iocp_hander = self.iocp_handler.as_ref();

        let io_manager_ptr: *const CompletionPortIOManager = ptr::addr_of!(*self);

        // Registers handle with IO completion port.
        //
        iocp_hander
            .io_completion_port
            .associate(handle, io_manager_ptr as usize)
    }
}

impl IOManager for CompletionPortIOManager {
    fn has_active_io(&self) -> bool {
        self.active_io != 0
    }

    /// See [`IOManager::completed_io`].
    ///
    /// The hEvent field in OVERLAPPED points to the IOWaitFuture that owns it.
    /// hEvent is set in register_io_wait() AFTER I/O starts.
    /// The low-order bit is set to 1 to prevent Windows from treating it as an event handle.
    ///
    fn completed_io(&mut self) -> Option<(FutureRuntime, *const IOWaitFuture)> {
        let io_completed_status = self.completed_io.pop()?;

        // hEvent contains pointer to IOWaitFuture with low bit set to 1.
        // Clear the low bit to get the actual pointer.
        //
        let tagged_ptr = unsafe { (*io_completed_status.overlapped).hEvent.0 };
        let completed_io_wait_future_ptr = (tagged_ptr & !1) as *mut IOWaitFuture;

        // hEvent may not be set yet because I/O completed before register_io_wait() was called.
        // Re-queue and try again later.
        //
        if completed_io_wait_future_ptr.is_null() {
            self.completed_io.push(io_completed_status);
            return None;
        }

        let completed_io_wait_future = unsafe { &mut *completed_io_wait_future_ptr };

        if !completed_io_wait_future.has_future_runtime() {
            // FutureRuntime not yet attached (reactor hasn't processed the poll yet).
            // Re-queue and try again later.
            //
            self.completed_io.push(io_completed_status);
            return None;
        }

        // Update the future with results.
        //
        let is_completed = completed_io_wait_future.is_completed();

        if !is_completed {
            // The future has been completed on IOCP but OVERLAPPED struct does not reflect that.
            //
            panic!("Invalid OVERLAPPED completion status")
        }

        self.active_io -= 1;

        Some((
            completed_io_wait_future.take_future_runtime().unwrap(),
            completed_io_wait_future_ptr,
        ))
    }

    /// See [`IOManager::register_io_wait`].
    ///
    /// Sets hEvent in the external OVERLAPPED to point to the IOWaitFuture.
    /// The low bit is set to prevent Windows from treating it as a real event handle.
    ///
    fn register_io_wait(
        &mut self,
        io_wait_future_ptr: *const IOWaitFuture,
    ) -> Result<(), io::Error> {
        let io_wait_future = unsafe { &mut *(io_wait_future_ptr as *mut IOWaitFuture) };

        // Set hEvent in the OVERLAPPED that Windows knows about.
        //
        let overlapped = io_wait_future.overlapped;
        let overlapped_ref = unsafe { &mut *overlapped };
        if overlapped_ref.hEvent.0 == 0 {
            // Set hEvent to IOWaitFuture pointer with low bit set to prevent
            // Windows from treating it as a real event handle.
            let tagged_ptr = (io_wait_future_ptr as isize) | 1;
            overlapped_ref.hEvent = HANDLE(tagged_ptr);
        }

        self.active_io += 1;
        Ok(())
    }

    /// See [`IOManager::cancel_io_wait`].
    ///
    fn cancel_io_wait(&mut self, io_wait_future_ptr: *const IOWaitFuture) -> Result<(), io::Error> {
        let io_future_wait = unsafe { &*io_wait_future_ptr };

        unsafe { CancelIoEx(io_future_wait.handle, Some(io_future_wait.overlapped)) }?;

        Ok(())
    }

    fn as_any(&mut self) -> &mut dyn std::any::Any
    where
        Self: 'static,
    {
        self
    }
}

#[ctor]
fn initialize_wsa() {
    unsafe {
        let mut wsa_data = WSADATA::default();
        let result = WSAStartup(0x0202, &mut wsa_data);

        if result != 0 {
            panic!(
                "WSA initialization failed: {}",
                io::Error::from_raw_os_error(result)
            );
        }
    }
}

#[dtor]
fn cleanup_wsa() {
    let _ = unsafe { WSACleanup() };
}

pub(crate) struct CompletionPortIOHandler {
    io_completion_port: IoCompletionPort,
    io_handler_thread_handle: Option<thread::JoinHandle<()>>,
}

impl CompletionPortIOHandler {
    /// Creates a new instance of CompletionPortIOHandler.
    ///
    pub fn try_new() -> io::Result<Self> {
        // Create IOCompletionPort handle.
        //
        let io_completion_port = IoCompletionPort::new(1)?;

        let io_completion_port_clone = io_completion_port.clone();

        let io_handler_thread_handle = std::thread::spawn(move || {
            Self::handle_completion_events(io_completion_port);
        });

        let iocp_handler = CompletionPortIOHandler {
            io_completion_port: io_completion_port_clone,
            io_handler_thread_handle: Some(io_handler_thread_handle),
        };

        Ok(iocp_handler)
    }

    /// Handle completion events in a dedicated thread.
    ///
    fn handle_completion_events(io_completion_port: IoCompletionPort) {
        const IO_COMPLETED_ENTRIES_COUNT: usize = 16;

        let mut is_running = true;

        while is_running {
            let mut completion_statuses = [CompletionStatus::new(); IO_COMPLETED_ENTRIES_COUNT];

            let io_completed_result =
                io_completion_port.get_many_queued(&mut completion_statuses, u32::MAX);

            match io_completed_result {
                Ok(count) => {
                    completion_statuses[..count]
                        .iter()
                        .for_each(|io_completed_status| {
                            match io_completed_status.overlapped.is_null() {
                                true => {
                                    // The IO Completion port handle was closed, so event processing has been terminated.
                                    //
                                    is_running = false;
                                }
                                false => {
                                    // Register completed IO.
                                    //
                                    let io_manager = unsafe {
                                        &*(io_completed_status.completion_key
                                            as *const CompletionPortIOManager)
                                    };

                                    io_manager.completed_io.push(*io_completed_status);
                                }
                            }
                        })
                }
                Err(error) => {
                    panic!("Error: {:?} {:?}", error, std::panic::Location::caller());
                }
            }
        }
    }
}

impl Drop for CompletionPortIOHandler {
    fn drop(&mut self) {
        // Close IO Completion port.
        //
        self.io_completion_port.close();

        // Wait for the IOCP Hanlder thread.
        //
        self.io_handler_thread_handle
            .take()
            .unwrap()
            .join()
            .unwrap();
    }
}

// Implements IOManager create options.
//
pub struct CompletionPortIOManagerCreateOptions {
    iocp_handler: Arc<CompletionPortIOHandler>,
}

impl Default for CompletionPortIOManagerCreateOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl CompletionPortIOManagerCreateOptions {
    pub fn new() -> Self {
        CompletionPortIOManagerCreateOptions {
            iocp_handler: Arc::new(CompletionPortIOHandler::try_new().unwrap()),
        }
    }
}

impl IOManagerCreateOptions for CompletionPortIOManagerCreateOptions {
    fn try_new_io_manager(&self) -> io::Result<IOManagerHolder> {
        Ok(IOManagerHolder {
            io_manager: Box::new(CompletionPortIOManager::with_iocp_handler(
                self.iocp_handler.clone(),
            )),
        })
    }
}
