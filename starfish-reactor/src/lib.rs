#![allow(dead_code)]

pub mod cooperative_io;
pub mod cooperative_synchronization;
pub mod coordinator;
pub mod rc_pointer;
pub mod reactor;

/*
Task list:

Reactor:

- [ ] Single thread allocator
- [ ] Optimize spawn_external_with_result
- [ ] Use pinning instead of ptr for IOWaitFuture ('a)
- [ ] Relay on Reactor::local_instance() to store information about the wait.

- [ ] Implement Semaphore (limits number of active future on a single Reactor)
- [ ] Poolable Futures (registers in Reactor only once)

- [x] Reactor loop exit
- [ ] Reactor handle future optimization.

Coordinator:

- [ ] Create Coordinator from the CoordinationOptions (similar to file)
 - [x] Implement IOManagerCreateOptions
 - [ ] Affinitize reactor's threads

- [x] Implement LockFuture (implements trait Send).
 - [ ] UnitTests

Benchmark:

- [ ] https://github.com/bheisler/iai

IO subsystem:

- [x] Cancelable IO
 - [x] Register pending IO with timeout
  - [x] Implement Treap, to keep cancelable IOWaitFuture
 - [x] Implement canceling IO
 - [x] Reactor cancel IO
 - [ ] Unit Tests

- [ ] Read Write improvements
 - [ ] Does file needs to be &mut
 - [ ] Multiple reads or writes on the same file instance
 - [ ] Support larger than 4GB files or 2GB Linux
  - [ ] This has multiple limitations both on Windows and Linux

- [ ] Direct IO

- [x] Pin all IOWaitFuture

- [ ] Test AsyncReadExtension
- [ ] Test AsyncWriteExtension

- [x] UDP support
 - [x] Windows
 - [x] Linux
 - [x] MacOS
 - [ ] Unit tests

- [x] TPC support
  - [x] Linux IO_URing
  - [x] Windows CooperativeIOManager.
  - [x] MacOS
  - [ ] Unit tests

Windows IO:
 - [ ] Implement IoRing
 - [ ] Implement RIO (Registered I/O)
 - [x] Register IO HANDLE (IOCP)

 Linux IO:
 - [x] Implement IO_URing
 - [ ] Improve IO_URing error handling
  - [ ] Resubmit queries
 - [ ] Implement pre-allocated buffers (zero copy)

 - [x] cargo valgrind test -p starfish --test io_networking_test

 ===
Done:
- [x] spawn_external requires Future + `Send
- [x] Add benchmark folder

MacOS IO:
- [x] MacOS IOWaitFuture deregister keventl
- [x] Handle registration events
- [x] Close Kqueue
- [x] Implement TCP
- [x] Implement UDP

Preemptive:
- [x] rename future_block_on

- [x] Implement Timers, Sleep
 - [x] Priority Queue
 - [x] Modify Reactor
 - [x] UnitTests
- [x] DualPointerWrapper add reference counter
- [x] Extend Drop trait for DualPointerWrapper<T:CooperativeFuture>
 - [x] On drop deregister future from Reactor
- [x] Convert project to lib
- [x] Add tests
- [x] Static instance of Reactor
- [x] Move EventFuture to new file
- [x] DualPointerWrapper should hold CooperativeScheduler
- [x] DualPointerWrapper implements Drop..
- [x] Future can return result
- [x] Implement Reactor
  - [-] Implement ArcPointer<T> so we could send Reactor to threads.
  - [-] Consider ref_counter on Reactor.
 - [x] spawn_external and spawn_external_with_result should return Future
- [x] Spawn_with_result from the preemptive thread

- [x] Communication between CooperativeSchedulers
  - [x] pass Coordinator to Reactor threads
  - [x] lockless queue to accept external futures
  - [x] result from external futures

  - [x] Implement CompletionPortIOManager
 - [x] dedicated thread to handle IOCP, will dispatch completed IO futures. We can take Reactor from the CompletedEvent.
 - [x] On completion move completed
 - [x] Assign handle to CompletionPort once
  - [x] do not use completion_key, use overlapped ptr to locate the IOWaitFuture

  - [x] Read of write to File needs to work on Buffers of Buffer Slices
 - [x] Do not pass buffer length to file read/write

- [x] Error handling
 - [x] Return errors on creating IOManager
 - [x] register_pending_io can fail

- [x] IOWaitFuture change output type, type Output = io::Result<usize>;
 - [x] Windows
 - [x] MacOS

- [x] Open file with async flag is different for Windows and MacOS
- [x] Define IOWaitFutures
- [x] Register IOWaitFuture with future_runtime in pending_io_futures
- [x] Check if IOWaitFuture is ready. Simplest implementation
- [x] Use std::File
  - [-] Add handle struct
  - [-] Implement Drop trait for the handle struct
- [x] Implement write to a file
- [x] split io_future and file functions

- [x] Define trait IOManager
- [x] Implement PoolingIOManager
  - [x] Reactor would register pending IOWaitFutures with IOManager
  - [x] Move checking IOWaitFutures to IOManager
*/

/*

cargo llvm-cov --html

sudo CARGO_PROFILE_RELEASE_DEBUG=true cargo flamegraph --bench my_benchmark --root --

cargo valgrind test

WSL:

- [ ] Upgrade to kernel v6.6x:
https://learn.microsoft.com/en-us/community/content/wsl-user-msft-kernel-v6
*/
