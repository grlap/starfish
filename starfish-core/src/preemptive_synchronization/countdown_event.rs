//! Countdown latch for thread synchronization.
//!
//! Provides `CountdownEvent`, a synchronization primitive that blocks
//! waiters until an internal counter reaches zero (similar to Java's
//! `CountDownLatch`).

use std::sync::{Condvar, Mutex};

pub struct CountdownEvent {
    count: Mutex<usize>,
    condvar: Condvar,
}

impl CountdownEvent {
    // Create a new CountdownEvent with initial count.
    //
    pub fn new(count: usize) -> Self {
        CountdownEvent {
            count: Mutex::new(count),
            condvar: Condvar::new(),
        }
    }

    // Signal the event, decrementing count by one.
    //
    pub fn signal(&self) -> bool {
        let mut count = self.count.lock().unwrap();
        if *count == 0 {
            return false;
        }
        *count -= 1;
        if *count == 0 {
            self.condvar.notify_all();
            true
        } else {
            false
        }
    }

    // Wait until count reaches zero.
    //
    pub fn wait(&self) {
        let mut count = self.count.lock().unwrap();
        while *count > 0 {
            count = self.condvar.wait(count).unwrap();
        }
    }
}
