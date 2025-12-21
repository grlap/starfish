use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::raw::c_void;
use std::ptr;
use std::time::{Duration, UNIX_EPOCH};

use kqueue_sys::{EventFilter, EventFlag, FilterFlag, kevent, kqueue};
use libc::timespec;

use crate::cooperative_io::io_manager::{IOManager, IOManagerCreateOptions, IOManagerHolder};
use crate::reactor::FutureRuntime;

use super::io_wait_future::IOWaitFuture;

pub(crate) struct KqueueIOManager {
    kqueue_file: File,
    active_io: i32,
}

impl KqueueIOManager {
    pub(crate) fn try_new() -> io::Result<Self> {
        let kqueue_fd = unsafe { kqueue() };

        if kqueue_fd < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(KqueueIOManager {
            kqueue_file: unsafe { File::from_raw_fd(kqueue_fd) },
            active_io: 0,
        })
    }
}

impl IOManager for KqueueIOManager {
    fn has_active_io(&self) -> bool {
        self.active_io != 0
    }

    /// See [`IOManager::completed_io`].
    ///
    fn completed_io(&mut self) -> Option<(FutureRuntime, *const IOWaitFuture)> {
        const MAX_EVENTS: usize = 1;

        let mut k_event = kevent::new(
            0,
            EventFilter::EVFILT_SYSCOUNT,
            EventFlag::empty(),
            FilterFlag::empty(),
        );

        let timeout = timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };

        // Get a kevent from the queue if available.
        //
        let event_count = unsafe {
            kevent(
                self.kqueue_file.as_raw_fd(),
                ptr::null(),
                0,
                &mut k_event,
                MAX_EVENTS as i32,
                &timeout,
            )
        };

        if event_count == 0 {
            return None;
        }

        let io_wait_future_ptr = k_event.udata as *mut IOWaitFuture;
        let io_wait_future = unsafe { &mut (*io_wait_future_ptr) };

        io_wait_future.handle_kevent();

        if !io_wait_future.is_completed() {
            return None;
        }

        // Unregister kevent after the IOWaitFuture has completed.
        //
        let delete_k_event = kevent {
            ident: k_event.ident,
            filter: k_event.filter,
            fflags: FilterFlag::empty(),
            data: 0,
            udata: ptr::null_mut(),
            flags: EventFlag::EV_DELETE,
        };

        let _result = unsafe {
            kevent(
                self.kqueue_file.as_raw_fd(),
                &delete_k_event,
                1,
                ptr::null_mut(),
                0,
                ptr::null(),
            )
        };

        // TODO: handle kevent deletion failure.

        self.active_io -= 1;

        let future_runtime = io_wait_future.take_future_runtime().unwrap();

        Some((future_runtime, io_wait_future_ptr))
    }

    /// See [`IOManager::register_io_wait`].
    ///
    fn register_io_wait(
        &mut self,
        io_wait_future_ptr: *const IOWaitFuture,
    ) -> Result<(), std::io::Error> {
        // Store IOWaitFuture pointer in the Kevent struct.
        //
        let io_wait_future = unsafe { &mut (*(io_wait_future_ptr as *mut IOWaitFuture)) };
        io_wait_future.k_event.udata = ptr::addr_of!(*io_wait_future_ptr) as *mut c_void;

        let k_event_ptr: *const kevent = ptr::addr_of!(io_wait_future.k_event);

        let result = match io_wait_future.timeout() {
            Some(timeout) => {
                // Create a Timespec entry for timeout.
                //
                let duration = timeout
                    .cancel_at_time()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or(Duration::from_secs(0));

                let ts = timespec {
                    tv_sec: duration.as_secs() as i64,
                    tv_nsec: duration.subsec_nanos() as i64,
                };

                // Reset future timeout as it is no longer needed.
                // Reactor will not track the timeout.
                //
                io_wait_future.set_timeout(None);

                unsafe {
                    kevent(
                        self.kqueue_file.as_raw_fd(),
                        k_event_ptr,
                        1,
                        ptr::null_mut(),
                        0,
                        &ts,
                    )
                }
            }
            None => unsafe {
                kevent(
                    self.kqueue_file.as_raw_fd(),
                    k_event_ptr,
                    1,
                    ptr::null_mut(),
                    0,
                    ptr::null(),
                )
            },
        };

        if result != -1 {
            self.active_io += 1;
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// See [`IOManager::cancel_io_wait`].
    ///
    fn cancel_io_wait(&mut self, _: *const IOWaitFuture) -> Result<(), io::Error> {
        Ok(())
    }

    fn as_any(&mut self) -> &mut dyn std::any::Any
    where
        Self: 'static,
    {
        self
    }
}

// Implement reactor create options.
//
pub struct KqueueIOManagerReactorCreateOptions {}

impl Default for KqueueIOManagerReactorCreateOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl KqueueIOManagerReactorCreateOptions {
    pub fn new() -> Self {
        KqueueIOManagerReactorCreateOptions {}
    }
}

impl IOManagerCreateOptions for KqueueIOManagerReactorCreateOptions {
    fn try_new_io_manager(&self) -> io::Result<IOManagerHolder> {
        Ok(IOManagerHolder {
            io_manager: Box::new(KqueueIOManager::try_new()?),
        })
    }
}
