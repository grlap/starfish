//! Completion signaling for spawned futures.
//!
//! Provides `CompletionSignaler` and `ExternalCompletionWaiter`, which unify
//! cooperative (reactor-thread) and preemptive (non-reactor-thread) completion
//! signaling for `spawn_external`.

use std::sync::Arc;

use crate::cooperative_synchronization::wait_one_future::CooperativeWaitOneFuture;
use crate::cooperative_synchronization::wait_one_future::CooperativeWaitOneSignaler;
use crate::reactor::{Reactor, ReactorAssigned};
use starfish_core::preemptive_synchronization::countdown_event::CountdownEvent;

pub(crate) enum CompletionSignaler {
    Cooperative(CooperativeWaitOneSignaler),
    Preemptive(Arc<CountdownEvent>),
}

impl CompletionSignaler {
    pub(crate) fn signal(&self) {
        match self {
            CompletionSignaler::Cooperative(signaler) => {
                signaler.signal();
            }
            CompletionSignaler::Preemptive(completed_event) => {
                let signaled = completed_event.signal();
                debug_assert!(signaled, "CompletionSignaler double-signal");
            }
        }
    }
}

impl ReactorAssigned for CompletionSignaler {
    fn assigned_reactor(&self) -> Option<&Reactor> {
        match self {
            CompletionSignaler::Cooperative(signaler) => signaler.assigned_reactor(),
            CompletionSignaler::Preemptive(_) => None,
        }
    }
}

pub(crate) enum ExternalCompletionWaiter {
    Cooperative(CooperativeWaitOneFuture),
    Preemptive(Arc<CountdownEvent>),
}

impl ExternalCompletionWaiter {
    pub(crate) async fn wait(self) {
        match self {
            ExternalCompletionWaiter::Cooperative(mut wait_one) => {
                wait_one.cooperative_wait().await;
            }
            ExternalCompletionWaiter::Preemptive(completed_event) => {
                assert!(
                    !Reactor::has_local_instance(),
                    "Preemptive wait must not be called from a reactor thread"
                );
                completed_event.wait();
            }
        }
    }
}
