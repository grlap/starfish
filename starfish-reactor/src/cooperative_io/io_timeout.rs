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
        Self::Duration(duration)
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
}
