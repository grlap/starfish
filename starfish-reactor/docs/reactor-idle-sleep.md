# Reactor Idle Sleep Design

**Bug:** former trackers #5 (busy-spin when idle) and #59 (busy-spin while I/O is in flight or timers not yet due) — both removed from `bugs.md` 2026-06-12 when the fix landed on the last platform

**Problem:** When no futures, no I/O, and no delayed futures are pending, the reactor loop spins at 100% CPU with no backoff or parking.

**Reference:** Seastar C++ framework (per-core shared-nothing architecture).

**Status:** IMPLEMENTED — all three platforms. Linux landed 2026-06-11: `IOManager::wait_for_io(timeout)`/`wake()` (spin-preserving defaults for other backends), io_uring implementation (eventfd + multishot `PollAdd` wake channel, `EXT_ARG` timespec bounded waits), and the reactor's three-phase idle path (spin window → deadline-bounded sleep → SeqCst-fence sleeping-flag handshake), validated by `tests/reactor_idle_test.rs` (CPU-time, wake latency, timer punctuality). **Windows landed 2026-06-12** exactly per the design below: dedicated IOCP handler thread, its `SegQueue`, and the CAS-on-`hEvent` rendezvous removed; per-reactor port; the reactor blocks directly in `GetQueuedCompletionStatusEx` and `wake()` posts a `WAKEUP_KEY` sentinel (`IoCompletionPort::close()`'s shutdown sentinel was subsumed by it and deleted). **macOS landed 2026-06-12** (the last platform): the reactor blocks directly in `kevent`; the wake channel is an `EVFILT_USER` event registered `EV_CLEAR` at construction, triggered cross-thread via `NOTE_TRIGGER` (no extra fd; triggers coalesce). Because macOS I/O kevents are edge-triggered (`EV_CLEAR`) and I/O-timeout timers one-shot, real events reaped by the blocking wait are buffered for `completed_io` rather than discarded, and delivering a completion purges the future's remaining buffered events (a timed wait's two knotes can be batch-reaped together; an unpurged sibling would be delivered after the task is freed) — see the module invariants and unit tests in `kqueue_io_manager.rs`. `tests/reactor_idle_test.rs` now runs on all three platforms; a manual macOS measurement of 4 idle reactors over 3s showed ~85ms total CPU, vs ~12 CPU-seconds when spinning. (History: a partial `thread::park()`/`unpark()` mitigation was prototyped and reverted 2026-03-08 — per-`enqueue_external` Mutex overhead for a partial fix. The spin also destabilized the multi-reactor benchmarks while it lived — `cross_reactor_chain_depth_10_chains_100` varied ±45% across identical runs, `pingpong` ±11%, single-threaded baselines ±0.4% — so cross-reactor bench results predating 2026-06-12 cannot validate scheduling changes finer than ±50%. NOTE: the `cross_reactor`/`spawn_external` benches build their coordinators with `NoopIOManagerCreateOptions`, which inherits the spin-preserving `wait_for_io`/`wake` defaults — those benchmark reactors therefore still busy-spin by design (isolating scheduling cost from real I/O), so the idle-sleep fix does NOT change their behavior and the ±50% variance caveat still applies to them. Only reactors on a real backend (`UringIOManager` etc.) sleep.)

---

## ⚠ As-built correction (2026-06-13): the `sleeping` flag is NOT "purely an optimization"

The design body below repeatedly states the reactor-level `sleeping` flag is
"purely an optimization" and that `Release`/`Acquire` ordering suffices.
**That holds for the I/O-completion path only** — the kernel wait
(`io_uring_enter` / `kevent` / `GetQueuedCompletionStatusEx`) observes
already-arrived completions atomically, so no flag is needed there. It is
**WRONG for the external-futures queue path that actually shipped**:

- Sender: `external_futures.push(work)` → **`fence(SeqCst)`** → `if sleeping { wake() }`
- Sleeper: `sleeping.store(true)` → **`fence(SeqCst)`** → re-check queue → block

That is a store-buffering (Dekker) pattern; plain `Release`/`Acquire` permits
the classic lost wakeup (sleeper misses the push AND sender misses the flag),
the same hazard as tracked lock bug #77. The paired `SeqCst` fences are
**load-bearing for correctness**, not an optimization — do not weaken them on
the strength of the body prose. See `reactor.rs` `run()` and
`enqueue_external_future_runtime` for the as-built code.

As-built vs. design body, also: the Windows backend uses a **per-reactor**
`IoCompletionPort` (not the `Arc<IoCompletionPort>` the body sketches), and
`wake()` reaches the port through the reactor rather than a separate
`waker()` handle.

---

## Current Behavior

```
reactor loop:
    check timeouts
    check external_futures (SegQueue)
    check io completions (IOManager::completed_io)
    check delayed_futures (BinaryHeap)
    check active_futures (VecDeque)

    if nothing found:
        should_run = !is_in_shutdown || has_active_io || ...
        continue  // <-- spins here at 100% CPU
```

---

## Seastar's Three-Phase Model

```
Active Polling  -->  Idle Polling (~200us)  -->  Sleep (kernel block)
      ^                                              |
      +---------- wakeup (eventfd / kqueue) ---------+
```

1. **Active polling** — tight loop, 100% CPU, minimum latency.
2. **Idle polling** — still spinning but lightweight checks only (~200us window). Absorbs transient lulls without paying the kernel sleep/wake cost (~1-10us round-trip).
3. **Sleep** — each event source arms for wakeup, then blocks in the kernel.

Key Seastar trait (`pollfn`):
```cpp
struct pollfn {
    virtual bool poll() = 0;                     // do work, return true if any
    virtual bool pure_poll() = 0;                // check only, no work
    virtual bool try_enter_interrupt_mode() = 0; // arm for wakeup
    virtual void exit_interrupt_mode() = 0;      // back to polling
};
```

If **any** poller finds work during `try_enter_interrupt_mode()`, the entire sleep aborts.

---

## Two Things to Solve

### 1. Cross-Reactor Wakeup (external futures)

When core A enqueues a future onto core B's `SegQueue`, core B needs to wake up.

**Current path:**
```rust
// reactor.rs:474
pub(crate) fn enqueue_external_feature_runtime(&self, future_runtime: FutureRuntime) {
    self.external_futures.push(future_runtime);
    // nothing wakes the reactor
}
```

**Solution:** Each reactor holds a wakeup primitive. After pushing to the queue, the sender signals it.

### 2. I/O Completion Wakeup

When the OS completes an I/O operation, the sleeping reactor needs to wake up.

This is platform-specific and ties into `IOManager::completed_io()`.

---

## Platform Analysis

### Windows (IOCP) — Eliminate Handler Thread

Currently `CompletionPortIOManager` spawns a **dedicated handler thread** that blocks in `GetQueuedCompletionStatusEx(INFINITE)` and pushes completions to a `SegQueue<CompletionStatus>` that the reactor busy-polls. This is an unnecessary indirection — the reactor can block directly in `GetQueuedCompletionStatusEx` itself.

**Refactor:** Remove the handler thread and `SegQueue`. The reactor thread blocks directly in `GetQueuedCompletionStatusEx(timeout)`. Cross-reactor wakeup uses `PostQueuedCompletionStatus` with a sentinel packet (we already use this pattern for shutdown in `IoCompletionPort::close()`).

**Solution:** `GetQueuedCompletionStatusEx` + `PostQueuedCompletionStatus` (sentinel)

```
Setup:
    // No handler thread. No SegQueue. No wake_event.
    // IOCP handle owned directly by IOManager.
    // Define a sentinel completion_key for wakeup packets.
    const WAKEUP_KEY: usize = 0;  // no real handle uses key=0

Sleep (reactor thread):
    GetQueuedCompletionStatusEx(iocp, &entries, timeout)
    // blocks until IO completes, wakeup posted, or timeout

    for each entry:
        if entry.completion_key == WAKEUP_KEY && entry.overlapped == null:
            // cross-reactor wake or shutdown — check external_futures
        else:
            // real IO completion — process directly (no SegQueue)

Cross-reactor wake (sender thread):
    external_futures.push(future_runtime);
    if sleeping.load(Acquire) {
        PostQueuedCompletionStatus(iocp, 0, WAKEUP_KEY, null)
    }

Shutdown:
    PostQueuedCompletionStatus(iocp, 0, WAKEUP_KEY, null)  // already done today
```

This aligns Windows with macOS/Linux — the reactor thread blocks directly in the kernel I/O API. `GetQueuedCompletionStatusEx` atomically dequeues completions, so there is no lost-wakeup window between checking for work and entering the wait (same as `io_uring_enter`). The `sleeping` flag is an optimization only.

`PostQueuedCompletionStatus` can be called from any thread — it's thread-safe by design.

| Mechanism | API |
|-----------|-----|
| Block for IO | `GetQueuedCompletionStatusEx(iocp, &entries, timeout)` |
| Cross-reactor wake | `PostQueuedCompletionStatus(iocp, 0, WAKEUP_KEY, null)` |
| Shutdown wake | `PostQueuedCompletionStatus(iocp, 0, WAKEUP_KEY, null)` |

### Linux (io_uring) — Needs Work

Currently `completed_io()` polls the completion queue via `completion().next()` (non-blocking).

**Solution:** `eventfd` + **multishot** `io_uring_prep_poll_add`

```
Setup:
    wake_fd = eventfd(0, EFD_NONBLOCK)
    submit multishot poll_add SQE on wake_fd with user_data = WAKEUP_TOKEN
    // multishot (kernel 5.13+): kernel re-arms automatically, no re-submission needed

Sleep:
    // submit_with_args passes timeout as EXT_ARG — single syscall, no extra timeout SQE
    io_uring_enter(GETEVENTS | EXT_ARG, min_complete=1, timeout=&ts)

    for each cqe:
        if cqe.user_data == WAKEUP_TOKEN:
            drain eventfd (read 8 bytes)
            if !(cqe.flags & IORING_CQE_F_MORE):
                re-submit poll_add  // multishot ended, re-arm
            // process external futures
        else:
            // normal IO completion

Cross-reactor wake:
    write(wake_fd, &1u64, 8)          // completes the poll SQE, unblocks wait
```

The blocking call is `io_uring_enter(IORING_ENTER_GETEVENTS, min_complete=1)`. This is **atomic**: the kernel checks for pending CQEs as part of entering the wait state, so there is no lost-wakeup window between "check completions" and "go to sleep." The `AtomicBool sleeping` flag at the reactor level is purely an optimization to avoid redundant `write()` syscalls from senders — not needed for correctness on this path.

For timeouts, prefer `submit_with_args` with `IORING_ENTER_EXT_ARG` + `Timespec` over `io_uring_wait_cqe_timeout()`. The latter submits a timeout SQE that consumes a CQE slot; `EXT_ARG` passes the timeout as a parameter to `io_uring_enter` directly.

| Mechanism | API |
|-----------|-----|
| Block for IO | `io_uring_enter(GETEVENTS, min_complete=1)` |
| Block with timeout | `submit_with_args(1, &args)` with `IORING_ENTER_EXT_ARG` + `Timespec` |
| Create wake channel | `eventfd(0, EFD_NONBLOCK)` + multishot `io_uring_prep_poll_add()` |
| Cross-reactor wake | `write(wake_fd, &1u64, 8)` |

### macOS (kqueue) — Straightforward

Currently `completed_io()` calls `kevent()` with zero timeout (non-blocking poll).

**Solution:** `EVFILT_USER` for cross-reactor wakeup, non-zero timeout for blocking.

```
Setup:
    register EVFILT_USER with EV_ADD | EV_CLEAR on kqueue (ident = WAKEUP_IDENT)

Sleep:
    kevent(kq, NULL, 0, events, max, NULL)   // NULL timeout = block forever
    // or with timeout:
    kevent(kq, NULL, 0, events, max, &ts)

    for each event:
        if filter == EVFILT_USER:
            // process external futures
        else:
            // normal IO completion

Cross-reactor wake:
    kevent(kq, &{EVFILT_USER, NOTE_TRIGGER}, 1, NULL, 0, &zero_ts)
```

`EVFILT_USER` is purely internal to the kqueue — no extra fd needed. `EV_CLEAR` auto-resets after delivery. Multiple triggers coalesce.

| Mechanism | API |
|-----------|-----|
| Block for IO | `kevent(kq, ..., NULL)` (NULL timeout) |
| Block with timeout | `kevent(kq, ..., &ts)` |
| Create wake channel | `kevent()` with `EVFILT_USER`, `EV_ADD \| EV_CLEAR` |
| Cross-reactor wake | `kevent()` with `EVFILT_USER`, `NOTE_TRIGGER` |

---

## Proposed Design

### IOManager Trait Extension

```rust
pub(crate) trait IOManager: Any {
    // existing
    fn has_active_io(&self) -> bool;
    fn completed_io(&mut self) -> Option<(FutureRuntime, *const IOWaitFuture)>;
    fn register_io_wait(&mut self, ...) -> Result<(), io::Error>;
    fn cancel_io_wait(&mut self, ...) -> Result<(), io::Error>;
    fn as_any(&mut self) -> &mut dyn Any;

    // new: sleep/wake support
    fn wait_for_io(&mut self, timeout: Option<Duration>) -> WaitResult;
    fn wake(&self);  // must be Send + Sync safe (called from other threads)
}

enum WaitResult {
    IoCompleted,       // at least one IO completion ready
    Woken,             // woken by wake() call
    TimedOut,          // timeout expired
}
```

### Reactor Changes

```rust
// New field on Reactor:
wake_flag: Arc<AtomicBool>,  // or platform-specific waker

// Modified idle path (reactor.rs:279-287):
if option_future_runtime.is_none() {
    if self.is_in_shutdown.load(Ordering::Acquire)
        && !self.io_manager.has_active_io()
        && self.external_futures.is_empty()
        && self.delayed_futures.is_empty()
    {
        should_run = false;
        continue;
    }

    // Phase 1: Quick spin check (a few iterations)
    // This catches work that arrives within microseconds.

    // Phase 2: Compute sleep timeout from delayed_futures
    let timeout = self.delayed_futures.peek()
        .map(|d| d.activate_system_time.duration_since(SystemTime::now()).ok())
        .flatten();

    // Phase 3: Block in IOManager (waits for IO or cross-reactor wake)
    self.io_manager.wait_for_io(timeout);

    continue;
}
```

**Comment (2026-03-07):** After the reactor sets its sleeping flag and re-checks state, the sleep path should be aborted only if runnable work appeared. If there is active I/O or a delayed timer, the reactor should still proceed into `wait_for_io(timeout)` instead of bouncing back into the outer polling loop.

### External Enqueue Wakeup

```rust
pub(crate) fn enqueue_external_future_runtime(&self, future_runtime: FutureRuntime) {
    self.external_futures.push(future_runtime);
    self.io_manager.wake();  // wake sleeping reactor
}
```

### Coordinator join_all Wakeup

```rust
pub fn join_all(&mut self) -> Result<(), ...> {
    for reactor in &self.reactors {
        reactor.set_is_in_shutdown_flag(true);
        reactor.io_manager.wake();  // ensure reactor exits sleep to see shutdown
    }
    // ...
}
```

Note: `wake()` on IOManager needs to be callable from outside the reactor thread. This means it either needs a `&self` method using interior mutability, or we extract the wake handle into a separate `Send + Sync` object (like `Arc<WakeHandle>`).

---

## Implementation Per Platform

### Windows

Eliminates `CompletionPortIOHandler` (handler thread) and `SegQueue<CompletionStatus>`. The reactor blocks directly in `GetQueuedCompletionStatusEx`. The `IoCompletionPort` is shared via `Arc` so that `wake()` can call `PostQueuedCompletionStatus` from any thread.

```rust
const WAKEUP_KEY: usize = 0;  // sentinel — real handles use &IOManager as key

struct CompletionPortIOManager {
    io_completion_port: Arc<IoCompletionPort>,  // shared for cross-thread wake()
    active_io: usize,
    // No handler thread. No SegQueue. No wake_event.
}

impl CompletionPortIOManager {
    pub fn try_new() -> io::Result<Self> {
        Ok(Self {
            io_completion_port: Arc::new(IoCompletionPort::new(1)?),
            active_io: 0,
        })
    }

    pub fn prepare_io(&mut self, handle: HANDLE) -> Result<(), io::Error> {
        let io_manager_ptr: *const CompletionPortIOManager = ptr::addr_of!(*self);
        self.io_completion_port.associate(handle, io_manager_ptr as usize)
    }
}

impl IOManager for CompletionPortIOManager {
    fn completed_io(&mut self) -> Option<(FutureRuntime, *const IOWaitFuture)> {
        // Non-blocking: call get_many_queued with timeout=0
        // Filter out WAKEUP_KEY sentinels
        // Process real completions via OVERLAPPED -> IOWaitFuture (same as today)
    }

    fn wait_for_io(&mut self, timeout: Option<Duration>) -> WaitResult {
        let ms = timeout.map(|d| d.as_millis() as u32).unwrap_or(INFINITE);

        let mut entries = [CompletionStatus::new(); 16];
        match self.io_completion_port.get_many_queued(&mut entries, ms) {
            Ok(count) => {
                let mut had_io = false;
                for status in &entries[..count] {
                    if status.completion_key == WAKEUP_KEY && status.overlapped.is_null() {
                        // Wakeup sentinel — ignore
                    } else {
                        had_io = true;
                        // Process IO completion directly here,
                        // or stash for completed_io() to return
                    }
                }
                if had_io { WaitResult::IoCompleted } else { WaitResult::Woken }
            }
            Err(_) => WaitResult::TimedOut,
        }
    }

    fn wake(&self) {
        // PostQueuedCompletionStatus with sentinel — same as existing close() pattern
        self.io_completion_port.post_queued(CompletionStatus {
            byte_count: 0,
            completion_key: WAKEUP_KEY,
            overlapped: ptr::null_mut(),
        }).ok();
    }
}
```

`PostQueuedCompletionStatus` is thread-safe by design — `wake()` can be called from any thread via `Arc<IoCompletionPort>`. The `GetQueuedCompletionStatusEx` call atomically dequeues completions, so there is no lost-wakeup window. The `sleeping` flag is an optimization only, same as io_uring.

### Linux

```rust
const WAKEUP_TOKEN: u64 = u64::MAX;

struct UringIOManager {
    // existing...
    ring: IoUring,
    wake_fd: OwnedFd,              // eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC)
    need_rearm_notifier: bool,      // true if multishot PollAdd ended
}

impl UringIOManager {
    fn new() -> io::Result<Self> {
        let wake_fd = unsafe {
            OwnedFd::from_raw_fd(libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC))
        };
        // Submit multishot PollAdd on wake_fd
        let poll_sqe = opcode::PollAdd::new(types::Fd(wake_fd.as_raw_fd()), libc::POLLIN as _)
            .multi(true)
            .build()
            .user_data(WAKEUP_TOKEN);
        // ... push to SQ and submit ...
        Ok(Self { ring, wake_fd, need_rearm_notifier: false })
    }
}

impl IOManager for UringIOManager {
    fn wait_for_io(&mut self, timeout: Option<Duration>) -> WaitResult {
        // Re-arm multishot PollAdd if it ended (kernel can terminate multishot)
        if self.need_rearm_notifier {
            // push new PollAdd SQE for wake_fd
            self.need_rearm_notifier = false;
        }

        // Block: submit pending SQEs + wait for at least 1 CQE
        let res = match timeout {
            Some(duration) => {
                let ts = types::Timespec::new()/* from duration */;
                let args = types::SubmitArgs::new().timespec(&ts);
                self.ring.submitter().submit_with_args(1, &args)
            }
            None => self.ring.submit_and_wait(1),
        };

        // Drain CQEs
        let mut had_io = false;
        for cqe in self.ring.completion() {
            match cqe.user_data() {
                WAKEUP_TOKEN => {
                    // Drain eventfd counter
                    let mut buf = [0u8; 8];
                    let _ = libc::read(self.wake_fd.as_raw_fd(), buf.as_mut_ptr().cast(), 8);
                    // Check if multishot ended
                    if cqe.flags() & IORING_CQE_F_MORE == 0 {
                        self.need_rearm_notifier = true;
                    }
                }
                _ => {
                    had_io = true;
                    // process normal IO completion
                }
            }
        }

        if had_io { WaitResult::IoCompleted }
        else if res == Err(ETIME) { WaitResult::TimedOut }
        else { WaitResult::Woken }
    }

    fn wake(&self) {
        let val: u64 = 1;
        unsafe { libc::write(self.wake_fd.as_raw_fd(), &val as *const _ as *const _, 8) };
    }
}
```

`io_uring_enter` atomically submits and waits — no lost-wakeup window. The `sleeping` flag at the reactor level is purely an optimization to skip redundant `write(eventfd)` calls.

### macOS

```rust
struct KqueueIOManager {
    // existing...
    kq: RawFd,
}

const WAKEUP_IDENT: usize = usize::MAX;  // unlikely to collide with real fds

impl IOManager for KqueueIOManager {
    fn new() -> Self {
        // existing setup + register EVFILT_USER
        let kev = kevent {
            ident: WAKEUP_IDENT,
            filter: EVFILT_USER,
            flags: EV_ADD | EV_CLEAR,
            ..
        };
        kevent(kq, &kev, 1, null, 0, &zero_ts);
    }

    fn wait_for_io(&mut self, timeout: Option<Duration>) -> WaitResult {
        let ts = timeout.map(|d| timespec { ... });
        let n = kevent(kq, null, 0, events, max, ts.as_ref());
        // process events: filter EVFILT_USER vs real IO
    }

    fn wake(&self) {
        let kev = kevent {
            ident: WAKEUP_IDENT,
            filter: EVFILT_USER,
            fflags: NOTE_TRIGGER,
            ..
        };
        kevent(kq, &kev, 1, null, 0, &zero_ts);
    }
}
```

---

## Memory Barrier Concern

There is a potential race between the reactor checking `external_futures.is_empty()` and going to sleep, vs another thread pushing to the queue and calling `wake()`:

```
Reactor thread:              Sender thread:
  check queue (empty)
                               push to queue
                               wake()          // reactor not sleeping yet — lost!
  sleep()                      // missed wakeup
```

Seastar solves this with `membarrier()` syscall. A simpler approach for us:

1. Use `AtomicBool sleeping` flag on the reactor.
2. Reactor: set `sleeping = true` (Release), then re-check all queues. If still empty, sleep.
3. Sender: push to queue, then check `sleeping` (Acquire). If true, call `wake()`.
4. Either the reactor sees the item (and skips sleep), or the sender sees `sleeping=true` (and wakes).

```rust
// Reactor idle path:
self.sleeping.store(true, Ordering::Release);

// Re-check after setting flag (prevents lost wakeup)
if !self.external_futures.is_empty()
    || self.io_manager.has_active_io()
    || !self.delayed_futures.is_empty()
{
    self.sleeping.store(false, Ordering::Relaxed);
    continue;
}

self.io_manager.wait_for_io(timeout);
self.sleeping.store(false, Ordering::Relaxed);

// Sender path:
self.external_futures.push(future_runtime);
if self.sleeping.load(Ordering::Acquire) {
    self.io_manager.wake();
}
```

This is safe because `SegQueue::push` uses atomic operations internally, and the `Release/Acquire` on `sleeping` establishes the necessary happens-before relationship.

**Note on io_uring and IOCP:** On Linux, `io_uring_enter` atomically checks for pending CQEs as part of entering the wait state. On Windows, `GetQueuedCompletionStatusEx` similarly atomically dequeues completions. Both prevent lost wakeups at the kernel level — the `sleeping` flag is purely an optimization to avoid redundant `write(eventfd)` / `PostQueuedCompletionStatus` syscalls (~200-500ns each) from senders when the reactor is already awake.

On macOS (kqueue), `kevent()` is also atomic with respect to registered events, but `EVFILT_USER` wakeups from cross-reactor senders go through a separate `kevent()` call. The `sleeping` flag prevents redundant wakeup syscalls on this path as well.

In summary: the `sleeping` flag is an optimization on all three platforms — not needed for correctness, but avoids ~200-500ns per redundant wakeup syscall under high cross-reactor traffic.

---

## Open Questions

1. ~~**Idle poll window:** Skip for now. Compio doesn't use one and gets good results. Add in Phase 3 only if benchmarks show the kernel sleep/wake cost (~1-10μs) is a bottleneck for bursty workloads.~~

2. ~~**`wake()` on `&self`:** Extract the wake primitive into a separate `Send + Sync` handle stored on the Reactor. Each platform's wake handle is already thread-safe: Windows uses `Arc<IoCompletionPort>` (`PostQueuedCompletionStatus` is thread-safe), Linux uses an `OwnedFd` (eventfd, `write()` is thread-safe), macOS uses a kqueue fd (`kevent()` with `EVFILT_USER` is thread-safe). The `IOManager` trait adds `fn waker(&self) -> Arc<dyn ReactorWaker>` to vend a cloneable handle, keeping `IOManager` itself `&mut self` for all other methods.~~

3. ~~**NoopIOManager:** No IO support — `wait_for_io()` and `wake()` return errors (only external futures via cross-reactor calls are supported). The reactor's idle sleep path uses `thread::park_timeout()` / `thread.unpark()` directly, bypassing the IOManager entirely.~~

4. ~~**Shutdown wakeup:** Solved — `join_all()` calls `wake()` after setting `is_in_shutdown` flag (see Coordinator join_all Wakeup section).~~

5. ~~**Delayed futures timeout:** Solved — reactor computes timeout from `delayed_futures.peek()` and passes it to `wait_for_io(timeout)` (see Reactor Changes section).~~

6. **Scope note (2026-03-07):** The eventual implementation should land as one coordinated feature. A partial pure-thread park path is easy to write, but it makes the scheduler behavior look "fixed" while active-I/O idle still polls. Keep the design and implementation tied together.

---

## Phased Implementation

### Phase 1: Cross-reactor wakeup only (simplest)
- Add `sleeping: AtomicBool` to Reactor
- Add `thread: Option<Thread>` to Reactor (for `unpark()`)
- Idle path: `thread::park_timeout()` with delayed-future deadline
- `enqueue_external`: `thread.unpark()` if sleeping
- `join_all`: unpark all reactors after setting shutdown flag
- IO still polled on each park_timeout wake — adds latency but simple

### Phase 2: IOManager sleep integration
- Add `wait_for_io()` / `wake()` to IOManager trait
- Implement per-platform blocking wait
- Reactor uses `wait_for_io()` instead of `park_timeout()`
- IO completions now wake reactor immediately

### Phase 3: Idle poll window (optional)
- Configurable spin duration before sleeping
- `pure_poll()` style lightweight checks during window
