# Starfish-Reactor Bug Report

**Last verified:** 2026-06-13 — refreshed after the cross-platform idle-sleep landing and its
review. The entries the recent reactor / idle-sleep / Windows+macOS backend work touched were
re-checked against the current tree: #47 carries a 2026-06-12 mechanics update (handler thread
gone, per-reactor port), #48 a 2026-06-12 EOF-test note, #71 narrowed (`uring_io_manager.rs`
now fully `SAFETY`-commented), #70 resolved (DESIGN-INDEX count/status fixed and the dead
`../bug.md` link removed), and review #85–88 added for the new platform-code gaps. The
lock-deep-dive (#74–82) and core-review (#52–73) entries were not re-audited here — they were
verified at their review dates and untouched since. (Earlier audit baseline: 2026-06-05.)
Fixed-and-removed history follows — e.g. #51 the Linux file-offset bug, and the io_uring lifecycle cluster
#54/#56/#58/#60, all fixed 2026-06-11 with regression tests in `tests/io_uring_lifecycle_test.rs`
and `uring_io_manager.rs` unit tests; #39 the IOCP CAS-protocol doc note, mooted 2026-06-12 when
the handler thread and the protocol itself were removed — the replacement invariants live in the
`completion_port_io_manager.rs` module docs; #49 the Windows file-offset twin and #84 its
contract-doc note, fixed 2026-06-12 — `FilePosition` cursor stamped into
`OVERLAPPED.Offset`/`OffsetHigh`, `ERROR_HANDLE_EOF` mapped to `Ok(0)` for `read(2)` parity, the
platform-neutral contract now in `cooperative_io/file.rs`'s `//!` docs, regression tests
`test_sequential_writes_and_reads_advance_file_position` + `test_read_at_eof_returns_zero`).
Further removals since: the idle busy-spin pair #5/#59, closed 2026-06-12 when the macOS kqueue
backend (`EVFILT_USER` wake + blocking `kevent` in `wait_for_io`) landed idle sleep on the last
platform — see `reactor-idle-sleep.md` for the full per-platform history, including the
benchmark-variance fallout the spin caused; and #45 the kqueue error-path `udata` dereference,
fixed by the same change (both kqueue reap sites — `next_event` and `wait_for_io`'s inline
batch reap — now guard the `kevent()` error path and never interpret the eventlist on
failure; the lesson lives in `kqueue_io_manager.rs`'s module invariant 3).

## Low (from code review 2026-03-08, review 3)

### 36. macOS ECN loopback test missing — io_networking_test.rs

The ECN integration test (`test_udp_loopback_preserves_ecn_codepoint`) is `#[cfg(target_os = "linux")]` only. macOS has a full ECN cmsg implementation but no integration test, so platform regressions in the macOS cmsg path would be invisible.

**Key files:** `tests/io_networking_test.rs`

---

## Low (from code review 2026-03-09, review 4)

### 37. `IOTimeout::Duration` variant is semi-dead and misleading — io_timeout.rs

`from_duration()` now eagerly converts to `AbsoluteTime`, and `normalize()` does the same. The `Duration` variant is still publicly constructible but `remaining_duration()` on it returns the original duration without accounting for elapsed time. Code constructing `IOTimeout::Duration(d)` directly (bypassing `from_duration`) would get subtly wrong timeout behavior, potentially reintroducing BUG-18.

**Key files:** `src/cooperative_io/io_timeout.rs`

### 38. Unix ECN helpers duplicated across Linux and macOS backends — io_network.rs

`ControlMessageBuffer`, `ignore_unsupported_sockopt_error`, `populate_send_ecn_msghdr`, and `udp_socket_enable_ecn` are near-identical in `io_linux/io_network.rs` and `io_macos/io_network.rs`. A bug fix in one (e.g., BUG-23 Linux `IP_TOS` `c_int` fix) must be manually ported to the other. Consider extracting into a shared `cooperative_io/unix_common.rs` module behind `#[cfg(unix)]`.

**Key files:** `src/cooperative_io/io_linux/io_network.rs`, `src/cooperative_io/io_macos/io_network.rs`

### 40. Missing `normalize()` idempotency test — io_timeout.rs

No test verifies that calling `normalize()` on an already-absolute timeout preserves the exact deadline. An accidental double-normalization would silently shift deadlines.

**Key files:** `src/cooperative_io/io_timeout.rs`

---

## Critical (from code review 2026-03-14, review 6)

### 43. `Coordinator::reactor()` forges aliased `ExternalReactor<'static>` handles — coordinator.rs, reactor.rs

`Coordinator::reactor()` widens a borrow of `self.reactors[index]` to `ExternalReactor<'static>`, while `ExternalReactor` stores `&'a mut Reactor`. That is unsound even before shutdown: safe code can call `Coordinator::reactor(0)` twice and manufacture two mutable references to the same reactor. The earlier BUG-17 "accepted risk" note only addressed post-`join_all()` dangling handles; it does not cover this immediate aliasing unsoundness.

**Key files:** `src/coordinator.rs`, `src/reactor.rs`

### 44. TLS singleton accessors mint fresh `&mut` references from raw pointers — reactor.rs, coordinator.rs

`Reactor::local_instance()` and `Coordinator::instance()` are safe APIs that reconstruct `&mut` from raw thread-local pointers on every call. Callers can therefore obtain multiple simultaneous `&mut` aliases to the same object. `Reactor::local_instance()` additionally relies on `debug_assert!` for null checking, so release builds still turn misuse into undefined behavior. BUG-19 only addressed the null-pointer aspect in debug builds; the aliasing unsoundness remains.

**Key files:** `src/reactor.rs`, `src/coordinator.rs`

---

## High (from code review 2026-03-14, review 6)

### 46. Non-Starfish `Future`s can be silently dropped after `Poll::Pending` — reactor.rs

The reactor accepts arbitrary `Future`s, but only preserves tasks that set a Starfish-specific `ScheduleReason`. If a future returns `Poll::Pending` without doing that, the `ScheduleReason::Pending` arm does nothing and the runtime falls out of scope. The custom waker is also a no-op, so ordinary wake-driven futures can never reschedule themselves.

**Key files:** `src/reactor.rs`

### 47. Windows IOCP handle affinity is implicit and not tracked by the public I/O types — io_windows, file.rs, udp_socket.rs

Windows handle association happens opportunistically via the current reactor's `CompletionPortIOManager` (`prepare_io`), permanently binding the handle to that reactor's completion port. `File`, `TcpStream`, `TcpListener`, and `UdpSocket` do not carry reactor affinity, and `UdpSocket::new()` / `FileOpenOptions::open()` can be called outside any reactor thread. Safe code can therefore open/create on the wrong thread (no association at all — completions go nowhere and the awaiting task hangs) or move a handle to a different reactor.

**Mechanics update (2026-06-12, handler-thread removal):** each reactor now owns its port and dequeues it itself, so a handle used from a foreign reactor delivers its packet to the *associating* reactor's port, where the dequeue path reads `OVERLAPPED.hEvent` that the foreign reactor's `register_io_wait` wrote without synchronization — a cross-thread data race violating the manager's single-threaded invariant 1, on top of the wrong-queue delivery (the dequeuing reactor would poll/own a task parked on another reactor). A `debug_assert` in `completed_io` catches the unregistered-packet symptom in debug builds. The completion key no longer routes packets (routing is via `hEvent`), so fixing this no longer needs to thread a manager pointer through the key.

**Key files:** `src/cooperative_io/io_windows/completion_port_io_manager.rs`, `src/cooperative_io/io_windows/io_file.rs`, `src/cooperative_io/io_windows/io_network.rs`, `src/cooperative_io/file.rs`, `src/cooperative_io/udp_socket.rs`

### 48. macOS file I/O reuses a single callback result in a continuation loop — io_macos/io_file.rs, io_macos/io_wait_future.rs

macOS file read/write passes `should_continue = true`, intending to keep draining until the buffer is full. But `IOWaitFuture::handle_kevent()` computes the callback result once before the loop and then reuses that stale `Ok(length)` or `Err` on every iteration. Short reads/writes therefore get counted repeatedly, and `Ok(0)` at EOF degenerates into a spin loop.

**Note (2026-06-12):** the platform-generic EOF-contract test `test_read_at_eof_returns_zero` (`tests/io_file_test.rs`) is `cfg`'d to linux+windows solely because of this bug — add `target_os = "macos"` to its cfg as part of the fix; it is the ready-made regression test for the EOF spin.

**Key files:** `src/cooperative_io/io_macos/io_file.rs`, `src/cooperative_io/io_macos/io_wait_future.rs`, `tests/io_file_test.rs`

---

## Medium (from code review 2026-03-14, review 6)

### 50. `EventFuture::new_no_reactor()` creates waiters that can never be woken — event_future.rs

`new_no_reactor()` is still public, but `signal()` only re-enqueues parked waiters through `assigned_reactor()`. If an event built with `new_no_reactor()` is awaited cooperatively and then signaled, the waiting task is left parked forever because there is no reactor to receive the wake-up.

**Key files:** `src/cooperative_synchronization/event_future.rs`

---

## Critical (from code review 2026-06-05, review 7)

_Source: multi-agent review (25 lenses + adversarial verification). Findings below were either confirmed by independent verifiers or hand-verified; overstated UAF claims for the drop/panic paths were reframed (see #55; the other, #58, has since been fixed), and the refuted "lock double-ownership data race" and Windows `BOOL`/`bool` compile claims were intentionally excluded._

### 52. `EventFuture::cooperative_wait` fabricates aliasing `&mut` via `RcPointer::get_raw_mut` — event_future.rs, rc_pointer.rs

`cooperative_wait()` does `let mut event_clone = self.clone(); Pin::new_unchecked(event_clone.get_raw_mut())`. `RcPointer::get_raw_mut` is inherently unsound (documented as such) — it hands out `&mut EventFuture` to the shared inner allocation while other clones (the signaler and any other waiter) alias the same object. Two tasks awaiting the same event each hold a live `&mut EventFuture` across their `.await`, so the unique references overlap (Stacked/Tree Borrows UB), even on the single reactor thread. Distinct concrete instance of the aliasing-from-raw-pointer class in #44. Fix: drive the wait through a shared-reference path; never mint `&mut` from a shared `RcPointer`.

**Key files:** `src/cooperative_synchronization/event_future.rs`, `src/rc_pointer.rs`

### 53. `Coordinator::initialize` forms multiple simultaneous `&mut Coordinator` across worker threads — coordinator.rs

Each spawned worker does `let local_coordinator = &mut *coordinator_ptr;` and then `local_coordinator.reactors[i] = Some(reactor)` while the main thread still holds `&mut self` (and other workers hold their own `&mut`). Multiple unique references to the same `Coordinator` are live concurrently — aliasing/provenance UB. (The element writes target disjoint `Vec` slots, so the corruption is the `&mut` aliasing rather than a slot-level data race, but it is still UB.) Same root as #44, different code path. Fix: hand workers a `*const`/`*mut` and synchronize slot initialization without ever materializing `&mut Coordinator`, or use per-slot atomics / a thread-safe init structure.

**Key files:** `src/coordinator.rs`

---

## High (from code review 2026-06-05, review 7)

### 55. Reactor `run()` has no panic isolation — one task panic destroys the whole reactor thread — reactor.rs

`run()` polls tasks without `catch_unwind` (workspace is `panic = "unwind"`). A panic in any task's user code (or in a reactor-thread `.unwrap()` such as `completed_io`'s `take_future_runtime().unwrap()` or `register_io_wait`'s `queue_entry.take().unwrap()`) unwinds the entire loop, killing every other task on that reactor; their awaiters hang forever, and `reset_local_instance()` at the end of `run()` is skipped, leaving a dangling `*const Reactor` in TLS. Fix: wrap each poll in `catch_unwind`; on a caught panic, signal the task's completion so awaiters don't hang, drain/cancel its in-flight I/O, and continue; reset TLS on the unwind path.

**Key files:** `src/reactor.rs`, `src/cooperative_io/io_linux/uring_io_manager.rs`

### 57. `ArcPointer::clone` has no overflow-to-abort guard — rc_pointer.rs

`clone()` does `fetch_add(1, Relaxed)` with no saturation or abort. A reference-count wraparound (which `std::Arc` defends against by aborting) yields a premature free and subsequent use-after-free / double-free. (The `Relaxed` increment / `Release` decrement + `Acquire` fence ordering is itself correct.) Fix: detect overflow and `process::abort()`, matching `std::Arc`.

**Key files:** `src/rc_pointer.rs`

---

## Medium (from code review 2026-06-05, review 7)

### 61. `read_exact` / `write_all` reset the timeout budget on every partial transfer — async_read.rs

For relative `Duration` timeouts, the extension loops call the per-op I/O with a fresh timeout each iteration, so a slow-but-steady peer can keep the call alive indefinitely — the intended end-to-end deadline is never enforced. Fix: normalize to an absolute deadline once at entry and pass the remaining budget into each partial op.

**Key files:** `src/cooperative_io/async_read.rs`, `src/cooperative_io/async_write.rs`

### 62. `IOTimeout` uses wall-clock `SystemTime` — overflow panics on the reactor thread and clock steps mass-expire — io_timeout.rs

`from_duration` / `cancel_at_time` / `normalize` all compute `SystemTime::now() + duration`, which **panics** if the result exceeds `SystemTime`'s range (reachable with a user-supplied `Duration::MAX`), unwinding the reactor thread. Using wall-clock time also means a backward NTP step makes `remaining_duration` saturate to 0 and fires all I/O timeouts at once. Extends #37. Fix: use `checked_add` with a clamped far-future deadline, and migrate the timeout/timer subsystem to monotonic `Instant`.

**Key files:** `src/cooperative_io/io_timeout.rs`

### 63. `FairLock` is not strictly FIFO and `ReactorAwareLock` reorders waiters — lock.rs

`FairLock`: the pre-queue `try_acquire` / `try_acquire_owner` lets a new acquirer barge ahead of already-queued waiters, so ordering is not strictly FIFO despite the name and docs. `ReactorAwareLock`: `release()` selects a waiter with `swap_remove`, which moves the last waiter into the chosen slot, scrambling the queue order that the round-based scan window relies on — weakening the documented anti-starvation guarantee. (Note: the separate "barging grants two simultaneous owners / data race" claim was investigated and refuted — ownership stays serialized via the signal handoff.) Fix: enqueue before attempting acquisition for strict FIFO; use order-preserving removal (e.g. `remove`/`VecDeque`) in the reactor-aware path.

**Key files:** `src/cooperative_synchronization/lock.rs`

### 64. `WaitOne::signal` can move a `!Send` `FutureRuntime` across threads — wait_one_future.rs

`CooperativeWaitOneFuture` / `Signaler` carry unconditional `unsafe impl Send/Sync` (no `SAFETY` comment, no bound). When a cross-reactor `signal()` runs on thread B, it takes the parked `FutureRuntime` (a `Pin<Box<dyn FutureRuntimeTrait>>`, which erases `Send`) and pushes it to the owning reactor's queue — moving a possibly-`!Send` task box across threads. Reachable via a lock awaited by a non-`Send` (`spawn`) task released from another reactor. Fix: require the parked future to be `Send` on the cross-thread path, or restrict cross-reactor signaling to `Send` tasks; add the missing `SAFETY` justification.

**Key files:** `src/cooperative_synchronization/wait_one_future.rs`

### 65. Blocking DNS resolution on the reactor thread — io_linux/io_network.rs

`tcp_listener_bind` / `udp_socket_bind` are `async fn`s that take `ToSocketAddrs` and call the std `bind`, which performs **synchronous, blocking `getaddrinfo`** for hostname inputs — stalling every other task on the cooperative reactor for the full DNS timeout. (Same audit applies to other blocking std calls reachable from a reactor thread: `File::open`'s `openat`, `CountdownEvent::wait`.) Fix: accept an already-resolved `SocketAddr` (or resolve off-thread); audit blocking syscalls reachable from reactor context.

**Key files:** `src/cooperative_io/io_linux/io_network.rs`

### 66. `InitBarrier` Drop can double-wait a released barrier and hang — init_barrier.rs

`InitBarrierGuard::ready()` sets `synchronized = true` only *after* `countdown.wait()` + `barrier.wait()`. `Drop` re-runs both whenever `!synchronized`. A panic in the window between `barrier.wait()` returning and the `synchronized = true` assignment makes `Drop` perform a **second** `barrier.wait()` on a `Barrier(n+1)` generation that has already released — blocking that thread forever (and the main thread's `InitBarrier::wait()` has already returned, so the thread leaks). The module doc claims full panic-safety but only handles pre-signal panics. Fix: set a `barrier_entered` flag immediately before `barrier.wait()` so `Drop` never re-enters the same generation; add panic-at-each-phase tests.

**Key files:** `src/cooperative_synchronization/init_barrier.rs`

---

## Low (from code review 2026-06-05, review 7)

### 67. read/write opcode length truncated to `u32` — io_linux/io_network.rs

`opcode::Read`/`Write` are built with `buf.len() as u32`, silently truncating transfers ≥ 4 GiB. Fix: cap the per-op length explicitly and loop, or document the per-op limit.

**Key files:** `src/cooperative_io/io_linux/io_network.rs`

### 68. `delayed_future` timers use wall-clock `SystemTime` — delayed_future.rs

Timers compare against `SystemTime::now()` (non-monotonic; affected by clock steps), and `SystemTime + Duration` can panic on extreme durations. Same root as #62. Fix: use monotonic `Instant`.

**Key files:** `src/cooperative_synchronization/delayed_future.rs`, `src/reactor.rs`

### 69. README examples do not compile / reference non-dependencies — README.md

The "Custom I/O Manager" example uses `DefaultIOManagerCreateOptions`, which is `pub(crate)` and unreachable by users; the Coordinator example calls `num_cpus::get()` though `num_cpus` is not a dependency; the Benchmarks section omits the `lock_benchmark` target. Relatedly, `IOManagerCreateOptions` is effectively sealed, so external crates cannot supply a custom backend despite the docs implying it. Fix: make the example compile (expose a public create-options type or change the example) and align the dependency/bench lists.

**Key files:** `README.md`, `src/cooperative_io/io_manager.rs`

### 71. Multiple `unsafe` blocks lack `// SAFETY:` comments

The `Pin::new_unchecked` blocks in `io_linux/io_file.rs`, the raw-pointer reactor/coordinator reconstruction blocks in `coordinator.rs`, and the `Send`/`Sync` impls + raw derefs in `rc_pointer.rs` have no `SAFETY` justification, contrary to project convention. Fix: add `SAFETY` comments stating the invariant each block relies on. (`io_linux/uring_io_manager.rs` was the original worst offender; the 2026-06-11 io_uring lifecycle rewrite gave it a `SAFETY` comment per `unsafe` block, so it is no longer in scope here.)

**Key files:** `src/cooperative_io/io_linux/io_file.rs`, `src/coordinator.rs`, `src/rc_pointer.rs`

### 72. `lib.rs` ships a large commented-out TODO block and crate-wide `#![allow(dead_code)]` — lib.rs

`lib.rs` contains ~160 lines of commented-out internal task-list and shell commands, and `#![allow(dead_code)]` masks genuinely dead code (e.g. the Linux `IOWaitFuture::set_timeout`). Fix: move the task list out of source; scope or remove the blanket `allow` so dead code surfaces.

**Key files:** `src/lib.rs`

### 73. Test-coverage gaps for core invariants

No unit tests cover: `RcPointer`/`ArcPointer` clone/drop refcount and last-drop dealloc (zero tests); the reactor `register_io_wait` failure path and timeout-cancellation path; `Coordinator` worker-panic propagation through `join_all`; cross-reactor `EventFuture` / `WaitOne` signal-before-wait ordering; delayed-future tie-break ordering. Fix: add targeted regression tests (these would have caught #57 directly — and the since-fixed #51/#54/#56/#58/#60 io_uring lifecycle bugs, whose regression tests landed 2026-06-11 in `tests/io_uring_lifecycle_test.rs` and the `uring_io_manager.rs` unit tests; see also the since-fixed Linux file-offset bug, whose targeted regression test `test_sequential_writes_and_reads_advance_file_position` was added 2026-06-11 exactly as this entry prescribes — and whose Windows twin #49 was fixed 2026-06-12 the same way, plus `test_read_at_eof_returns_zero` for the EOF parity it exposed).

**Key files:** `src/rc_pointer.rs`, `src/reactor.rs`, `src/coordinator.rs`, `tests/`

---

## High (from code review 2026-06-11, review 8 — lock.rs deep dive)

_Source: focused multi-agent review of `cooperative_synchronization/lock.rs` (6 lenses + 3 paired protocol adjudications, every finding adversarially verified; 26 raw → 18 confirmed / 8 refuted). **Settled as SOUND — do not re-litigate:** (a) the `acquire_internal` None-branch head-signal is ownership DONATION (the CAS winner donates to the popped waiter and stays queued; conservation proven, empty-pop unreachable) — the disputed "wrong-waiter wakeup / hung task" claim from review 7 is definitively refuted; (b) UnfairLock/ReactorAwareLock two-queue stranding is impossible (`state == None` ⇒ RefCell Vec empty on all paths); (c) the wait-one double-swap rendezvous is sound under its (unenforced) one-shot invariants; (d) RefCell-only-under-ownership and the `T: Send`-only bound are std::Mutex-equivalent._

### 74. Reactor shutdown is blind to lock-parked waiters — task loss, lock wedged `Owned`, and UAF — lock.rs, reactor.rs, coordinator.rs

A task parked on a lock has its `FutureRuntime` moved out of every reactor queue into `WaitOneSharedState`, so `run()`'s exit condition (`reactor.rs:319-325`) cannot see it. Verified interleavings: (a) `join_all()` while a waiter is parked → its reactor exits → a later `release()` pops the signaler, `transfer_ownership` (→ `Owned`), `signal()` pushes onto the exited reactor's queue → the task never runs, the lock is permanently `Owned`, the task is silently dropped at `reactors.clear()`, and `join_all` returns `Ok`; (b) hold a `try_acquire()` guard on the main thread across `join_all()` (legal safe code), then drop it → `signal()` derefs the waiter's now-dangling `reactor_ptr` (`wait_one_future.rs:42,87-94,101-110`) and pushes onto a freed `SegQueue` — **use-after-free**. Tests dodge this only by counting completions with `CountdownEvent` before joining; nothing in the API forbids the bad order. Fix: count parked futures in the reactor's liveness condition (incremented at `set_waiting`, decremented on re-enqueue) so a reactor cannot exit while it owns parked tasks; replace the raw `reactor_ptr` with a handle invalidated at teardown so post-join `signal()` fails loudly instead of corrupting memory.

**Key files:** `src/cooperative_synchronization/lock.rs`, `src/cooperative_synchronization/wait_one_future.rs`, `src/reactor.rs`, `src/coordinator.rs`

### 75. `acquire()` is not cancellation-safe: a dropped acquire future strands the lock `Owned` forever — lock.rs

If an `acquire()` future is dropped after pushing its signaler (`lock.rs:311`) but before parking, a later `release()` pops the orphaned signaler, calls `transfer_ownership` (→ `Owned`), and `signal()` merely sets the rendezvous flag — the dropped task never parks and never releases. The ownership token is handed to a ghost: the lock is permanently `Owned`, and every later acquirer queues behind it forever. Reachable whenever an acquire future can be dropped mid-flight (reactor teardown with in-flight acquires, or any future composition that drops branches). Fix: an acknowledged handoff (releaser re-pops and re-signals if the granted waiter never claims ownership within the rendezvous), or a `Drop` impl on the acquire path that marks the queued signaler dead so `release()` skips it; at minimum document + debug-assert the no-drop contract.

**Key files:** `src/cooperative_synchronization/lock.rs`, `src/cooperative_synchronization/wait_one_future.rs`

### 76. `ReactorAwareLock::release` materializes `&Reactor` to foreign live reactors — aliasing UB on every contended release — lock.rs

`min_by_key` calls `signaler.assigned_reactor().map(|r| r.external_queue_len())` for **every** queued waiter (`lock.rs:705-716`), creating a `&Reactor` on the releasing thread while the target reactor's own thread is inside `run(&mut self)` — a live `&mut Reactor` and a foreign `&Reactor` to the same `!Sync` object coexist (aliasing/provenance UB), independent of which fields are read. Fires on the workhorse path of the in-tree benches/tests, for waiters that are not even selected; also inherits the #74 dangling-pointer case once a reactor is torn down. The `Sync` SAFETY comment (`lock.rs:744-752`) never addresses cross-thread reactor access. Fix: store the load metric behind a properly shared handle (e.g. an `Arc<AtomicUsize>` queue-length the reactor publishes) instead of dereferencing `*mut Reactor` cross-thread.

**Key files:** `src/cooperative_synchronization/lock.rs`, `src/cooperative_synchronization/wait_one_future.rs`

---

## Medium (from code review 2026-06-11, review 8 — lock.rs deep dive)

### 77. Releasing-handshake is a store-buffering race: formally-permitted lost wakeup, currently masked by crossbeam internals — lock.rs

Releaser: `store(Releasing, Release)` then pop; acquirer: push then `load(state, Acquire)`. The header's race-handling argument (`lock.rs:139-159`) assumes sequential consistency these orderings do not provide (classic SB litmus): the pop can legitimately miss the push **and** the acquirer can re-read stale `Owned` (coherence permits re-reading the value its failed CAS saw; no synchronizes-with edge exists when the pop read nothing the acquirer wrote), so the waiter parks forever while the lock sits free (`state == None`, signaler stranded). Two lenses independently derived the same C++20 `[atomics.order]` analysis and both verifications confirmed it. Cannot fire today: crossbeam `SegQueue::pop` executes an internal `fence(SeqCst)` before its empty-check and x86/ARMv8 are multi-copy-atomic — i.e. liveness rests on a dependency's private internals and hardware stronger than the Rust memory model (POWER would break it), justified nowhere. Fix: paired `std::sync::atomic::fence(SeqCst)` — between the push (`lock.rs:311`) and the state loop, and between `start_release()` and the first pop in each `release()` — and document the Dekker-style argument in the header.

**Key files:** `src/cooperative_synchronization/lock.rs`

### 78. `Releasing` spin stalls the entire spinning reactor — unbounded under OS preemption of the releaser — lock.rs

The acquire loop's `std::hint::spin_loop()` on `Releasing` (`lock.rs:334-337`) blocks the whole cooperative reactor — every task, timer, and I/O completion on that thread — for the full duration of a cross-thread `release()`. Bounded in the happy path (release is short), but unbounded if the releaser's OS thread is preempted mid-release; a same-reactor releaser is impossible (serialized), so this is purely the cross-thread case. Fix: bound the spin (yield to the reactor after N iterations by re-queuing the acquirer), or make release() publish its decision with a single state transition so acquirers never need to wait out a window.

**Key files:** `src/cooperative_synchronization/lock.rs`

### 79. ReactorAwareLock anti-starvation guarantee is void — concrete unbounded starvation schedule (extends #63) — lock.rs

Beyond #63's order-scrambling: `swap_remove` never shifts a mid-vector waiter toward index 0, so the round counter's `counter == 1` "forced pick of the front" keeps selecting around a waiter parked on a persistently loaded reactor indefinitely — the documented round-window guarantee (`lock.rs:176-188`) provides no bound at all. Fix (with #63): order-preserving removal (`VecDeque` + `remove`) and a pick rule that guarantees the oldest waiter is selected at least every K releases.

**Key files:** `src/cooperative_synchronization/lock.rs`

### 80. Lock `Send`/`Sync` SAFETY comments rest on the unjustified Signaler impls as an unstated premise (extends #64) — lock.rs

The three locks' `unsafe impl Send/Sync` justifications (`lock.rs:458-465, 584-592, 744-752`) are correct about the RefCell/UnsafeCell discipline but silently depend on `CooperativeWaitOneSignaler`'s unconditional `unsafe impl Send/Sync` (#64) being sound — which is exactly the unproven link (it is what permits moving a `!Send` `FutureRuntime` cross-thread). They also omit the load-bearing parts of the two-queue argument (acquire paths never touch the RefCell fields; the `is_signaled` AcqRel / SegQueue happens-before chain serializing successive cross-thread releases). Fix alongside #64; strengthen the comments to state the full invariant chain.

**Key files:** `src/cooperative_synchronization/lock.rs`, `src/cooperative_synchronization/wait_one_future.rs`

---

## Low (from code review 2026-06-11, review 8 — lock.rs deep dive)

### 81. lock.rs documentation drift: header race argument incomplete, release-path comments factually wrong, `mem::forget` semantics undocumented — lock.rs

(a) The header "Race Condition Handling" section (`lock.rs:139-159`) asserts the post-push state load observes `Releasing`-or-later without the ordering that would guarantee it (see #77) and never describes the donation path (#74-adjacent None-branch behavior, settled sound in this review). (b) The release-path comments "signal() can synchronously wake a task on the same thread" (`lock.rs:436-439, 542-545, 674-677`) are factually wrong — `signal()` only enqueues; the re-entrancy precaution they justify is still fine but for the wrong reason. (c) `mem::forget(LockResult)` permanently strands the lock `Owned` and leaks queued waiters — std-like, but undocumented. (d) The None-branch donation bypasses UnfairLock random / ReactorAwareLock least-loaded selection (signals the SegQueue head unconditionally) — document or route through the policy. (e) Add a `debug_assert` on the provably-unreachable empty-pop after a None-branch CAS win (if it ever became reachable it would be a silent parked-owner deadlock).

**Key files:** `src/cooperative_synchronization/lock.rs`

### 82. Allocation churn on the contention path: fresh `WaitOneSharedState` ArcPointer per contended acquire — lock.rs

Every contended `acquire()` heap-allocates a new `WaitOneSharedState` (ArcPointer) plus a SegQueue slot (`lock.rs:310-311`); under heavy contention this is allocator pressure on the hot path. Fix: pool/reuse rendezvous states per task, or intrusively embed the wait state in the acquire future.

**Key files:** `src/cooperative_synchronization/lock.rs`, `src/cooperative_synchronization/wait_one_future.rs`

---

## From code review 2026-06-11 (pending-changes review of the #51 fix)

### 83. `read_exact`/`write_all` short-transfer continuation loops are untested

The new offset regression test covers single-transfer ops only: 8KB reads/writes on a regular
file complete in one io_uring transfer, so the continuation loops (`async_read.rs` `&mut
buf[read..]` re-slicing + count accumulation + the `0 => UnexpectedEof` arm; `async_write.rs`
likewise with `WriteZero`) execute exactly one iteration. A regression in the loop logic — or an
offset bug affecting only continuation ops within one call — would pass the suite. Short
transfers cannot be reliably forced on regular files; the right tool is a deterministic unit
test of `read_exact_with_timeout`/`write_all_with_timeout` against a mock
`AsyncRead`/`AsyncWrite` returning short counts and a terminal 0 (no reactor needed).

**Key files:** `src/cooperative_io/async_read.rs`, `src/cooperative_io/async_write.rs`, `tests/io_file_test.rs`

---

## From code review 2026-06-13 (review of the macOS/Windows idle-sleep + Windows file-cursor commits)

_Platform-code findings from a multi-agent review of commits 964cb59 / 7d13fc3 / 6aa4f9e. The implementations are sound (no lost-wakeup, accounting, or UAF defects — the critical classes from the Linux idle-sleep review). These four gaps need the Windows/macOS machines to fix and compile-verify; doc-accuracy findings from the same review were fixed in place._

### 85. Medium: IOCP manager has no `Drop` drain — teardown with in-flight I/O silently leaks parked tasks, undocumented (Linux parity gap) — io_windows

The handler-thread removal (964cb59) replaced `CompletionPortIOHandler::drop` with nothing: `CompletionPortIOManager` has no `Drop`; the only teardown is `IocpImp::drop` (`iocp.rs`) doing a bare `CloseHandle`. If the manager drops with in-flight ops (reachable via the panic-unwind path #55), the kernel keeps writing into `OVERLAPPED`s/buffers inside parked task frames — memory-safe ONLY because those frames are self-owned and therefore leak permanently and silently. This is the same deliberate-leak situation Linux documents (invariant 3 / pre-5.19 path), but the Windows module docs mirror the Linux invariant numbering (1–3) with no teardown clause. The leak is the load-bearing safety mechanism; its absence from the docs invites a future "fix" that frees task boxes without draining → real UAF. Fix: add a module invariant documenting the deliberate leak (who closes the port, ops not drained, self-referential leak = safety argument), or add a bounded `GQCSEx` drain loop before `CloseHandle` mirroring the Linux Drop.

**Key files:** `src/cooperative_io/io_windows/completion_port_io_manager.rs`, `src/cooperative_io/io_windows/iocp.rs`

### 86. Low: Windows `wake()` ignores `PostQueuedCompletionStatus` failure — failed post is a lost wakeup with no fallback — io_windows

`wake()` does `let _ = self.io_completion_port.post_queued(...)`. `PostQueuedCompletionStatus` allocates a kernel completion packet and can fail (FALSE) under quota/resource exhaustion; the reactor's liveness for externally enqueued work depends on this one syscall when it sleeps with `timeout=None`. A swallowed failure strands the pushed future until an unrelated event arrives — unbounded hang. The Linux backend is failure-proof by construction (eventfd write) and additionally has a bounded-sleep degraded mode (`wake_poll_armed == false` caps sleeps at 1ms); the Windows backend has neither. Reachability is low (kernel pool exhaustion) → low severity, but silent and permanent when hit. Fix: record post failure (`wake_post_failed`) and clamp `None` timeouts to a short bounded wait while set, mirroring the io_uring degraded mode.

**Key files:** `src/cooperative_io/io_windows/completion_port_io_manager.rs`

### 87. Low: new kqueue unit tests fabricate `&mut [u8]` from an immutable stack buffer (aliasing UB on the delivery path) — io_macos tests

`delivery_purges_buffered_sibling_events` (and sibling) declare `let buffer = [0u8; 8];` (immutable) and pass `&buffer` into `new_read_wait`, which forwards it `*const [u8]` → stored as `*mut [u8]` (`io_wait_future.rs`); on delivery `handle_kevent` reconstitutes `&mut (*self.buffer)[..]` — a `&mut` whose provenance is a shared borrow of an immutable local (Stacked/Tree Borrows UB, even though the stub callback never writes). The production write path has the same `*const`→`*mut` wart, but the test adds a fresh READ-path instance. No runtime misbehavior today; Miri can't run it (FFI). Fix: `let mut buffer`, `new_read_wait(buffer: &mut [u8])`, pass `buffer as *mut [u8] as *const [u8]`.

**Key files:** `src/cooperative_io/io_macos/kqueue_io_manager.rs` (tests), `src/cooperative_io/io_macos/io_wait_future.rs`

### 88. Low: file append-mode divergence is reachable through the public API; dismissal is workspace-scoped — io_file

The explicit-cursor file I/O (`FilePosition`) assumes non-append writes; the in-code dismissal of append-mode divergence is justified only against current workspace callers, but `as_async_io()` / `FileOpenOptions` are public, so an external caller opening a file in append mode gets cursor/offset behavior that diverges from `O_APPEND` semantics. Fix: either document the unsupported append mode on the public open path, or detect and reject/handle it.

**Key files:** `src/cooperative_io/file.rs`, `src/cooperative_io/io_windows/io_file.rs`, `src/cooperative_io/file_open_options.rs`
