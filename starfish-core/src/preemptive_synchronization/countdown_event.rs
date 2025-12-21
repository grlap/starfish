use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};

pub struct CountdownEvent {
    count: Mutex<usize>,
    condvar: Condvar,
    notified: AtomicBool,
}

impl CountdownEvent {
    // Create a new CountdownEvent with initial count.
    //
    pub fn new(count: usize) -> Self {
        CountdownEvent {
            count: Mutex::new(count),
            condvar: Condvar::new(),
            notified: AtomicBool::new(false),
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
            self.notified.store(true, Ordering::Release);
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
