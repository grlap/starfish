//! Custom epoch-based memory reclamation for the Starfish runtime.
//!
//! Uses a **3-state epoch model**: garbage tagged at epoch E is safe to
//! reclaim once the global epoch reaches E+2. Three intrusive deferred
//! lists per reactor (indexed by `epoch % 3`) store freed allocations.
//!
//! Integrates with the `Coordinator` and `Reactor` via the
//! `CooperativeInitializer` trait to track per-reactor epoch counters.

/*
TODO:
 - [x] Move Epoch to its own crate
 - [x] Integrate Epoch with the Coordinator and Reactor
 - [x] 3-state epoch cycling with incremental reclamation
 - [x] Drop EpochCounters on Coordinator join_all
 - [ ] Design resource model (EpochToken acquire/release)
*/

pub mod epoch;
pub mod epoch_alloc;

pub use epoch::{Epoch, EpochGuard};
