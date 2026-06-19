//! I/O timeout specification for async operations.
//!
//! Provides `IOTimeout`, an enum representing either a relative `Duration` or an
//! absolute `SystemTime` deadline for bounding the duration of I/O operations.

use std::time::{Duration, SystemTime};

#[derive(Clone, Copy)]
pub enum IOTimeout {
    Duration(Duration),
    AbsoluteTime(SystemTime),
}

impl IOTimeout {
    /// Creates a new IOTimeout from a Duration, which adds the Duration to the current SystemTime.
    ///
    pub fn from_duration(duration: Duration) -> Self {
        Self::AbsoluteTime(SystemTime::now() + duration)
    }

    /// Creates a new IOTimeout with a specific SystemTime.
    ///
    pub fn at_time(time: SystemTime) -> Self {
        Self::AbsoluteTime(time)
    }

    pub fn cancel_at_time(&self) -> SystemTime {
        match self {
            Self::Duration(duration) => SystemTime::now() + *duration,
            Self::AbsoluteTime(time) => *time,
        }
    }

    /// Captures a relative timeout as an absolute deadline.
    ///
    /// This should be called when an I/O operation is created so queueing delay
    /// does not silently extend the timeout budget.
    pub fn normalize(self) -> Self {
        match self {
            Self::Duration(duration) => Self::AbsoluteTime(SystemTime::now() + duration),
            absolute => absolute,
        }
    }

    /// Returns the remaining duration until the timeout expires.
    /// For `Duration` variants, returns the duration directly.
    /// For `AbsoluteTime` variants, computes the time remaining from now.
    ///
    pub fn remaining_duration(&self) -> Duration {
        match self {
            Self::Duration(duration) => *duration,
            Self::AbsoluteTime(time) => time
                .duration_since(SystemTime::now())
                .unwrap_or(Duration::from_secs(0)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::IOTimeout;
    use std::thread;
    use std::time::{Duration, SystemTime};

    #[test]
    fn from_duration_captures_deadline_immediately() {
        let timeout = IOTimeout::from_duration(Duration::from_millis(50));

        thread::sleep(Duration::from_millis(10));

        let remaining = timeout.remaining_duration();
        assert!(remaining <= Duration::from_millis(50));
        assert!(remaining >= Duration::from_millis(20));
    }

    #[test]
    fn normalize_turns_relative_timeout_into_absolute_deadline() {
        let normalized = IOTimeout::Duration(Duration::from_millis(25)).normalize();

        assert!(matches!(normalized, IOTimeout::AbsoluteTime(_)));

        let deadline = normalized.cancel_at_time();
        assert!(deadline >= SystemTime::now());
    }
}
