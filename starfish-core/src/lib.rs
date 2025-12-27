#![allow(dead_code)]

pub mod common_tests;
pub mod data_structures;
pub mod guard;
pub mod preemptive_synchronization;

// Re-export guard types for convenience
pub use guard::{DeferredGuard, DeferredRef, Guard};

/*
Task list:

Benchmark:

- [ ] https://github.com/bheisler/iai

*/

/*

cargo llvm-cov --html

sudo CARGO_PROFILE_RELEASE_DEBUG=true cargo flamegraph --bench my_benchmark --root --

cargo valgrind test

WSL:

- [ ] Upgrade to kernel v6.6x:
https://learn.microsoft.com/en-us/community/content/wsl-user-msft-kernel-v6
*/
