//! Blocking (preemptive) synchronization primitives.
//!
//! Provides OS-level synchronization tools for use outside the cooperative
//! scheduler, such as `CountdownEvent` and `FutureExtension`.

pub mod countdown_event;
pub mod future_extension;
