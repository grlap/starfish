//! Multi-reactor coordination for parallel cooperative scheduling.
//!
//! Provides the `Coordinator` struct for spawning and managing multiple `Reactor`
//! instances across threads, and the `CooperativeInitializer` trait for user-defined
//! per-reactor initialization logic.

use std::cell::UnsafeCell;
use std::future::Future;
use std::ops::Index;
use std::ptr;
use std::sync::atomic::{self, AtomicPtr};
use std::sync::{Arc, BarrierWaitResult};

use crate::arc_pointer_dyn;
use crate::cooperative_io::DefaultIOManagerCreateOptions;
use crate::cooperative_io::io_manager::IOManagerCreateOptions;
use crate::cooperative_synchronization::init_barrier::InitBarrier;
use crate::rc_pointer::ArcPointer;
use crate::reactor::{ExternalReactor, Reactor};

pub trait CooperativeInitializer {
    fn init(&self);
    fn uninit(&self);

    fn init_reactor(&self, reactor: &Reactor);
    fn uninit_reactor(&self, reactor: &Reactor);
}

// Defines a Cooperative Coordinator.
//
pub struct Coordinator {
    reactors: Vec<Option<Reactor>>,
    handles: Vec<std::thread::JoinHandle<()>>,
    initializers: Vec<ArcPointer<dyn CooperativeInitializer + Send + Sync>>,
}

impl Default for Coordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl Coordinator {
    pub fn new() -> Self {
        Coordinator {
            reactors: vec![],
            handles: vec![],
            initializers: vec![],
        }
    }

    /// Initializes the Coordinator with multiple reactor threads.
    ///
    pub fn initialize(&mut self, reactor_count: usize) -> BarrierWaitResult {
        self.initialize_with_io_manager(DefaultIOManagerCreateOptions::new(), reactor_count)
    }

    pub fn initialize_with_io_manager<
        T: IOManagerCreateOptions + std::marker::Send + std::marker::Sync + 'static,
    >(
        &mut self,
        io_manager_create_options: T,
        reactor_count: usize,
    ) -> BarrierWaitResult {
        Coordinator::set_local_instance(self);

        let io_manager_create_options: Arc<T> = Arc::new(io_manager_create_options);

        // Pre-fill with None. Each thread will write Some(reactor) to its slot.
        //
        self.reactors = (0..reactor_count).map(|_| None).collect();

        // Run Initializers.
        //
        for initializer in &self.initializers {
            initializer.init();
        }

        // Pass Coordinator to the Reactor threads.
        //
        let init_barrier = InitBarrier::new(reactor_count);

        let coordinator_ptr = Arc::new(AtomicPtr::new(self));

        // For each reactor create a thread to and store the handles.
        //
        self.handles = (0..reactor_count)
            .map(|reactor_index| {
                let coordinator_ptr_clone = Arc::clone(&coordinator_ptr);
                let mut guard = init_barrier.guard();
                let io_manager_create_options_clone = io_manager_create_options.clone();

                std::thread::spawn(move || {
                    unsafe {
                        let local_reactors_ptr =
                            coordinator_ptr_clone.load(atomic::Ordering::Acquire);

                        let local_coordinator = &mut *local_reactors_ptr;

                        // Sets Coordinator instance in local thread storage.
                        //
                        Coordinator::set_local_instance(local_coordinator);

                        let io_manager = io_manager_create_options_clone
                            .as_ref()
                            .try_new_io_manager()
                            .unwrap();

                        // Create a Reactor and store it in the reactors vector.
                        //
                        let mut local_reactor = Reactor::with_io_manager(io_manager);

                        // Reactor will continue to run if there are no futures in the queue.
                        //
                        local_reactor.set_reactor_index(reactor_index);
                        local_reactor.set_is_in_shutdown_flag(false);

                        // Write the reactor to its slot.
                        //
                        local_coordinator.reactors[reactor_index] = Some(local_reactor);

                        // Set reactor local thread instance.
                        //
                        Reactor::set_local_instance(
                            local_coordinator.reactors[reactor_index].as_ref().unwrap(),
                        );

                        // Read Reactor Instance from thread local storage.
                        //
                        let local_reactor = Reactor::local_instance();

                        // Call Initializers to init reactor.
                        //
                        for initializer in &local_coordinator.initializers {
                            initializer.init_reactor(local_reactor);
                        }

                        // Signal init complete and wait for all threads + main.
                        //
                        guard.ready();

                        local_reactor.run(); // Call run() on each reactor in its own thread

                        // Call initializers to uninit reactor.
                        //
                        for initializer in &local_coordinator.initializers {
                            initializer.uninit_reactor(local_reactor);
                        }
                    }

                    Coordinator::reset_local_instance();
                })
            })
            .collect();

        init_barrier.wait()
    }

    pub fn register_initializer<T: CooperativeInitializer + Sync + Send + 'static>(
        &mut self,
        initializer: T,
    ) {
        self.initializers.push(arc_pointer_dyn!(
            initializer,
            dyn CooperativeInitializer + Send + Sync
        ));
    }

    // Joins for all Reactor threads.
    //
    pub fn join_all(&mut self) -> Result<(), Box<dyn std::any::Any + Send>> {
        for reactor in self.reactors.iter().flatten() {
            reactor.set_is_in_shutdown_flag(true);
        }

        // Take ownership of the handles vector, replacing it with an empty vector.
        //
        let handles = std::mem::take(&mut self.handles);

        // Join ALL threads before touching self.reactors. If we returned early
        // on the first panic, remaining threads would be detached and could
        // still reference reactors we're about to clear.
        //
        let mut first_error = None;
        for handle in handles {
            if let Err(e) = handle.join()
                && first_error.is_none()
            {
                first_error = Some(e);
            }
        }

        // Remove all the reactors from the list.
        //
        self.reactors.clear();

        // Reset the main thread's TLS to avoid stale pointers.
        //
        Coordinator::reset_local_instance();

        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// # Preconditions
    /// This intentionally widens the lifetime to `'static` for cross-thread convenience.
    /// Callers must ensure returned handles are only used while coordinator-owned
    /// reactors are initialized, and never after `join_all()` clears them.
    #[inline]
    pub fn reactor(index: usize) -> ExternalReactor<'static> {
        Coordinator::instance().reactor_instance(index)
    }

    // Gets a reference-counted pointer to the Reactor at the specified index.
    //
    #[inline]
    pub fn reactor_instance(&mut self, index: usize) -> ExternalReactor<'_> {
        ExternalReactor::new(
            self.reactors[index]
                .as_mut()
                .expect("reactor not initialized"),
        )
    }

    // Gets reactor count.
    //
    #[inline]
    pub fn reactor_count(&self) -> usize {
        self.reactors.len()
    }

    pub fn for_each_reactor<F: Fn() -> R, R: Future<Output = ()> + 'static + Send>(
        &mut self,
        future_factory: F,
    ) -> Vec<impl Future<Output = ()> + 'static + Send> {
        let reactor_count = self.reactor_count();

        (0..reactor_count)
            .map(|reactor_id| {
                let future = future_factory();
                self.reactor_instance(reactor_id).spawn_external(future)
            })
            .collect()
    }

    pub fn for_each_reactor_with_result<
        T: 'static + Send,
        F: Fn() -> R,
        R: Future<Output = T> + 'static + Send,
    >(
        &mut self,
        future_factory: F,
    ) -> Vec<impl Future<Output = T> + 'static> {
        let reactor_count = self.reactor_count();

        (0..reactor_count)
            .map(|reactor_id| {
                let future = future_factory();
                self.reactor_instance(reactor_id)
                    .spawn_external_with_result(future)
            })
            .collect()
    }

    /// Gets the Coordinator instance.
    ///
    /// # Preconditions
    /// Must be called only on threads where `set_local_instance` was executed and
    /// before `reset_local_instance`. The TLS raw pointer must remain valid.
    #[inline]
    pub fn instance<'a>() -> &'a mut Self {
        let coordinator_ptr =
            COORDINATOR_INSTANCE.with(|s| unsafe { *s.get() }) as *mut Coordinator;

        if coordinator_ptr.is_null() {
            panic!("Coordinator not initialized");
        }

        unsafe { &mut *coordinator_ptr }
    }

    pub(super) fn set_local_instance(&self) {
        COORDINATOR_INSTANCE.with(|s| {
            unsafe {
                *s.get() = self;
            };
        });
    }

    pub(super) fn reset_local_instance() {
        COORDINATOR_INSTANCE.with(|s| {
            unsafe {
                *s.get() = ptr::null_mut();
            };
        });
    }
}

impl Index<usize> for Coordinator {
    type Output = Reactor;

    #[inline]
    fn index(&self, index: usize) -> &Self::Output {
        self.reactors[index]
            .as_ref()
            .expect("reactor not initialized")
    }
}

thread_local! {
    static COORDINATOR_INSTANCE: UnsafeCell<*const Coordinator> = const {UnsafeCell::new(ptr::null_mut())};
}
