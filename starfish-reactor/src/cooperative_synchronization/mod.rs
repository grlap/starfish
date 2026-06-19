//! Cooperative synchronization primitives for the reactor.
//!
//! Provides futures-aware synchronization: `CooperativeEventFuture` for multi-waiter
//! events, `CooperativeWaitOneFuture` for one-shot signaling, `DelayedFuture` for
//! cooperative sleep, `Lock` for async mutual exclusion, and `InitBarrier` for startup sync.

pub mod delayed_future;
pub mod event_future;
pub(crate) mod init_barrier;
pub mod lock;
pub mod wait_one_future;
