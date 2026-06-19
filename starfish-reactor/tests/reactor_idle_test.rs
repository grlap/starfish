//! Idle-sleep integration tests (former trackers #5/#59): the reactor must
//! block in the kernel instead of busy-spinning when nothing is runnable,
//! wake promptly on cross-reactor spawns, and honor timer deadlines while
//! asleep.
//!
//! Runs on every platform whose I/O backend implements `wait_for_io`/`wake`:
//! Linux io_uring (eventfd wake), Windows IOCP (wake packet), macOS kqueue
//! (`EVFILT_USER` wake).

#![cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]

use std::sync::Arc;
use std::time::{Duration, Instant};

use starfish_core::preemptive_synchronization::countdown_event::CountdownEvent;
use starfish_reactor::cooperative_synchronization::delayed_future::cooperative_sleep;
use starfish_reactor::coordinator::Coordinator;
use starfish_reactor::reactor::Reactor;

/// CPU time (user + system) consumed by the CALLING THREAD, in milliseconds.
#[cfg(target_os = "linux")]
fn thread_cpu_time_ms() -> u64 {
    // SAFETY: zeroed rusage is a valid out-param; RUSAGE_THREAD is
    // Linux-specific and this branch is cfg(target_os = "linux").
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut usage) };

    let to_ms = |time: libc::timeval| time.tv_sec as u64 * 1000 + time.tv_usec as u64 / 1000;
    to_ms(usage.ru_utime) + to_ms(usage.ru_stime)
}

/// CPU time (kernel + user) consumed by the CALLING THREAD, in milliseconds.
#[cfg(target_os = "windows")]
fn thread_cpu_time_ms() -> u64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};

    let mut creation_time = FILETIME::default();
    let mut exit_time = FILETIME::default();
    let mut kernel_time = FILETIME::default();
    let mut user_time = FILETIME::default();

    // SAFETY: GetCurrentThread returns a pseudo-handle (no close needed) with
    // THREAD_QUERY_INFORMATION access; all four out-params are valid.
    unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut creation_time,
            &mut exit_time,
            &mut kernel_time,
            &mut user_time,
        )
    }
    .expect("GetThreadTimes failed for the current thread");

    // FILETIME counts 100ns units; 10_000 of them per millisecond.
    let to_ms = |time: FILETIME| {
        (((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64) / 10_000
    };
    to_ms(kernel_time) + to_ms(user_time)
}

/// CPU time (user + system) consumed by the CALLING THREAD, in milliseconds.
#[cfg(target_os = "macos")]
// libc deprecates its Mach API in favor of the `mach2` crate — not worth a
// dev-dependency for one test helper.
#[allow(deprecated)]
fn thread_cpu_time_ms() -> u64 {
    // Not in the libc crate (the rest of the Mach thread_info surface is).
    unsafe extern "C" {
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
    }

    let mut info: libc::thread_basic_info = unsafe { std::mem::zeroed() };
    let mut count = libc::THREAD_BASIC_INFO_COUNT;

    // SAFETY: mach_thread_self returns a valid port for the calling thread;
    // info/count are a valid THREAD_BASIC_INFO out-buffer pair. The port
    // reference mach_thread_self allocates is released afterwards.
    let kern_result = unsafe {
        let thread_port = libc::mach_thread_self();
        let result = libc::thread_info(
            thread_port,
            libc::THREAD_BASIC_INFO as libc::thread_flavor_t,
            (&raw mut info) as libc::thread_info_t,
            &mut count,
        );
        mach_port_deallocate(libc::mach_task_self(), thread_port);
        result
    };
    assert_eq!(kern_result, libc::KERN_SUCCESS, "thread_info failed");

    let to_ms =
        |time: libc::time_value_t| time.seconds as u64 * 1000 + time.microseconds as u64 / 1000;
    to_ms(info.user_time) + to_ms(info.system_time)
}

/// A reactor waiting out a 400ms timer must sleep, not spin: spinning burns
/// CPU time ~equal to wall time; sleeping burns only the bounded spin window
/// plus bookkeeping.
#[test]
fn reactor_sleeps_instead_of_spinning_during_timer_wait() {
    let mut reactor = Reactor::new();

    _ = reactor.spawn(async {
        cooperative_sleep(Duration::from_millis(400)).await;
    });

    let cpu_before_ms = thread_cpu_time_ms();
    let wall_start = Instant::now();

    reactor.run();

    let wall_ms = wall_start.elapsed().as_millis() as u64;
    let cpu_ms = thread_cpu_time_ms() - cpu_before_ms;

    assert!(wall_ms >= 380, "timer fired early: {wall_ms}ms");
    assert!(
        cpu_ms < wall_ms / 2,
        "reactor burned {cpu_ms}ms CPU over {wall_ms}ms wall — busy-spinning instead of sleeping"
    );
}

/// A timer must fire close to its deadline even though the reactor is
/// blocked in the kernel (the sleep is bounded by the earliest deadline).
#[test]
fn timer_fires_on_time_while_reactor_sleeps() {
    let mut reactor = Reactor::new();

    _ = reactor.spawn(async {
        cooperative_sleep(Duration::from_millis(150)).await;
    });

    let start = Instant::now();
    reactor.run();
    let elapsed = start.elapsed();

    assert!(
        elapsed >= Duration::from_millis(140),
        "timer fired early: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(600),
        "timer fired far too late (sleep not bounded by deadline?): {elapsed:?}"
    );
}

/// Repeated sleep → external-wake cycles: exercises the sleeping-flag
/// handshake and each backend's wake-channel re-arm/coalescing path (Linux:
/// eventfd + multishot poll and the `wake_observed` pre-block re-check — a
/// wake reaped during `wait_for_io`'s prologue must abort the sleep, not be
/// destroyed; macOS: `EVFILT_USER` `EV_CLEAR` auto-reset across sleeps —
/// the sibling stale-wake skip in `completed_io` is covered by unit tests
/// in `kqueue_io_manager.rs`; Windows: wake packets). Any lost wake hangs
/// the round and fails via harness timeout.
#[test]
fn repeated_external_wakes_survive_sleep_cycles() {
    let mut coordinator = Coordinator::new();
    coordinator.initialize(1);

    for round in 0..100 {
        // Let the reactor pass its spin window and block (no timers, no I/O
        // => unbounded sleep interruptible only by the wake channel).
        std::thread::sleep(Duration::from_millis(2));

        let done_event = Arc::new(CountdownEvent::new(1));
        let done = done_event.clone();

        drop(Coordinator::reactor(0).spawn_external(async move {
            done.signal();
        }));

        done_event.wait();
        std::hint::black_box(round);
    }

    coordinator.join_all().unwrap();
}

/// A `spawn_external` onto a reactor that has gone to sleep (no timers, no
/// I/O — unbounded wait) must wake it via the backend's wake channel. Hangs
/// (and is killed by the test harness) if the wake path is broken. `join_all`
/// at the end additionally covers shutdown-while-sleeping: the second reactor
/// never receives work and must be woken by the shutdown flag to exit.
#[test]
fn external_spawn_wakes_sleeping_reactor() {
    let mut coordinator = Coordinator::new();
    coordinator.initialize(2);

    // Give both reactors time to pass the spin window and block.
    std::thread::sleep(Duration::from_millis(100));

    let done_event = Arc::new(CountdownEvent::new(1));
    let done = done_event.clone();

    let wake_start = Instant::now();
    drop(Coordinator::reactor(0).spawn_external(async move {
        done.signal();
    }));

    done_event.wait();
    let wake_latency = wake_start.elapsed();

    coordinator.join_all().unwrap();

    assert!(
        wake_latency < Duration::from_secs(2),
        "wake took {wake_latency:?} — wake channel broken?"
    );
}
