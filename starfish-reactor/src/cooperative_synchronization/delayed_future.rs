use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, SystemTime},
};

use crate::reactor::{Reactor, ScheduleReason};

pub struct DelayedFuture {
    activate_system_time: SystemTime,
}

impl DelayedFuture {
    fn new(activate_system_time: SystemTime) -> Self {
        DelayedFuture {
            activate_system_time,
        }
    }
}

impl Future for DelayedFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        // Get the reactor from the context.
        //
        let reactor = Reactor::local_instance();

        let now = SystemTime::now();

        if now >= self.activate_system_time {
            Poll::Ready(())
        } else {
            // Update the reactor with rescheduling reason.
            //
            reactor.set_schedule_reason(ScheduleReason::DelayedWait {
                activate_system_time: self.activate_system_time,
            });

            Poll::Pending
        }
    }
}

#[inline]
pub async fn cooperative_sleep(duration: Duration) {
    let now = SystemTime::now();

    let activate_system_time = now + duration;

    DelayedFuture::new(activate_system_time).await;
}
