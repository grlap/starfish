use std::cell::UnsafeCell;
use std::future::Future;
use std::ops::Index;
use std::ptr;
use std::sync::atomic::{self, AtomicPtr};
use std::sync::{Arc, Barrier, BarrierWaitResult};

use crate::arc_pointer_dyn;
use crate::cooperative_io::DefaultIOManagerCreateOptions;
use crate::cooperative_io::io_manager::IOManagerCreateOptions;
use crate::rc_pointer::ArcPointer;
use crate::reactor::{ExternalReactor, Reactor};
use starfish_core::preemptive_synchronization::countdown_event::CountdownEvent;

pub trait CooperativeIntitializer {
    fn init(&mut self);
    fn uninit(&mut self);

    fn init_reactor(&self, reactor: &Reactor);
    fn uninit_reactor(&self, reactor: &Reactor);
}

// Defines a Cooperative Coordinator.
//
pub struct Coordinator {
    reactors: Vec<Reactor>,
    handles: Vec<std::thread::JoinHandle<()>>,
    initializers: Vec<ArcPointer<dyn CooperativeIntitializer + Send + Sync>>,
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

        // Reserve capacity for reactors. Each thread will write directly to its slot.
        // We use set_len to avoid creating placeholder reactors that would need to be dropped.
        //
        self.reactors = Vec::with_capacity(reactor_count);
        // SAFETY: Each slot will be initialized exactly once by its corresponding thread
        // before any reads occur. The countdown_init barrier ensures all slots are written
        // before any thread proceeds to use them.
        #[allow(clippy::uninit_vec)]
        unsafe {
            self.reactors.set_len(reactor_count)
        };

        // Run Initializers.
        //
        for initalizer in &mut self.initializers {
            initalizer.init();
        }

        // Pass Coordinator to the Reactor threads.
        //
        let start_barrier = Arc::new(Barrier::new(reactor_count + 1));

        let coordinator_ptr = Arc::new(AtomicPtr::new(self));

        let countdown_init = Arc::new(CountdownEvent::new(reactor_count));

        // For each reactor create a thread to and store the handles.
        //
        self.handles = (0..reactor_count)
            .map(|reactor_index| {
                let coordinator_ptr_clone = Arc::clone(&coordinator_ptr);
                let countdown_clone = Arc::clone(&countdown_init);
                let start_barrier_clone = start_barrier.clone();
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

                        // Write the reactor directly to the uninitialized slot.
                        // SAFETY: The slot is uninitialized (set_len was called without init),
                        // and each thread writes exactly once to its own slot.
                        ptr::write(
                            &mut local_coordinator.reactors[reactor_index],
                            local_reactor,
                        );

                        // Set reactor local thread instance.
                        //
                        Reactor::set_local_instance(&local_coordinator.reactors[reactor_index]);

                        // Read Reactor Instance from thread local storage.
                        //
                        let local_reactor = Reactor::local_instance();

                        // Call Initalizers to init reactor.
                        //
                        for initalizer in &local_coordinator.initializers {
                            initalizer.init_reactor(local_reactor);
                        }

                        // Signal that a reactor was created.
                        //
                        countdown_clone.signal();
                        countdown_clone.wait();

                        start_barrier_clone.wait();

                        local_reactor.run(); // Call run() on each reactor in its own thread

                        // Call initalizers to uninit reactor.
                        //
                        for initalizer in &local_coordinator.initializers {
                            initalizer.uninit_reactor(local_reactor);
                        }
                    }

                    Coordinator::reset_local_instance();
                })
            })
            .collect();

        countdown_init.wait();

        start_barrier.wait()
    }

    pub fn register_initializer<T: CooperativeIntitializer + Sync + Send + 'static>(
        &mut self,
        initializer: T,
    ) {
        self.initializers.push(arc_pointer_dyn!(
            initializer,
            dyn CooperativeIntitializer + Send + Sync
        ));
    }

    // Joins for all Reactor threads.
    //
    pub fn join_all(&mut self) -> Result<(), Box<dyn std::any::Any + Send>> {
        for index in 0..self.reactors.len() {
            self.reactors[index].set_is_in_shutdown_flag(true);
        }

        // Take ownership of the handles vector, replacing it with an empty vector.
        //
        let handles = std::mem::take(&mut self.handles);

        // Join each thread and return early if any thread panicked.
        //
        for handle in handles {
            handle.join()?;
        }

        // Remove all the reactors from the list.
        //
        self.reactors.clear();
        self.handles.clear();

        // Reset the main thread's TLS to avoid stale pointers.
        //
        Coordinator::reset_local_instance();

        Ok(())
    }

    #[inline]
    pub fn reactor(index: usize) -> ExternalReactor<'static> {
        Coordinator::instance().reactor_instance(index)
    }

    // Gets a reference-counted pointer to the Reactor at the specified index.
    //
    #[inline]
    pub fn reactor_instance(&mut self, index: usize) -> ExternalReactor<'_> {
        ExternalReactor::new(&mut self.reactors[index])
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
        T: Default + 'static + Send,
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

    // Gets the Coordinator instance.
    //
    #[inline]
    pub fn instance<'a>() -> &'a mut Self {
        let coordinator_ptr =
            COORDINATOR_INSTANCE.with(|s| unsafe { *s.get() }) as *mut Coordinator;

        if coordinator_ptr.is_null() {
            panic!("Coordinator not initalized");
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
        &self.reactors[index]
    }
}

thread_local! {
    static COORDINATOR_INSTANCE: UnsafeCell<*const Coordinator> = const {UnsafeCell::new(ptr::null_mut())};
}
