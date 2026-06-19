# Safety Review

Focus: Unsafe Rust correctness, undefined behavior, soundness.

## What to check

1. **Every `unsafe` block must have a `// SAFETY:` comment** explaining why the invariants hold.
   Flag any missing SAFETY comments.

2. **Pointer validity**: Raw pointers (*const T, *mut T):
   - Is the pointer non-null before dereference?
   - Is the pointed-to memory still alive (not freed/deallocated)?
   - Is the pointer properly aligned?
   - For lock-free code: is the node protected by an epoch guard before access?

3. **Aliasing rules** (Stacked Borrows / Tree Borrows):
   - No `&mut T` and `&T` to the same location at the same time
   - UnsafeCell used for interior mutability
   - RefCell/Cell used correctly (no nested borrow_mut)

4. **Memory ordering**:
   - `Relaxed`: Only for counters/stats where order doesn't matter
   - `Acquire`/`Release`: For publish/subscribe patterns (protect data behind the atomic)
   - `SeqCst`: Rarely needed — flag if used, verify necessity
   - Fence usage: is the fence placed correctly relative to the atomic operations?

5. **Lifetime soundness**:
   - Does transmuting lifetimes create dangling references?
   - Thread-local unbounded lifetimes: are they truly thread-local?
   - Guard lifetimes: does the guard outlive all references it protects?

6. **Send/Sync**:
   - Manual `unsafe impl Send/Sync`: are the requirements actually met?
   - Raw pointers in types: do they need explicit Send/Sync impls?

7. **Uninitialized memory**:
   - `MaybeUninit` used correctly? Not read before initialization?
   - `std::mem::zeroed()`: is all-zeros a valid bit pattern for this type?

8. **FFI safety** (platform IO code):
   - System call return values checked?
   - Buffer sizes correct?
   - OVERLAPPED/kevent structures initialized properly?

## What NOT to flag

- Safe Rust code (no unsafe block = no concern for this reviewer)
- Style or naming in unsafe blocks
- Known accepted risks documented in MEMORY.md under "Not Real Concerns"
- `Reactor::local_instance()`/`Coordinator::instance()` TLS raw-pointer lifetime widening, when guarded by documented thread-local preconditions and used only on initialized runtime threads
- `Coordinator::reactor()` returning `ExternalReactor<'static>` as an accepted shutdown-scoped risk, **unless** a handle can outlive coordinator lifecycle or be used after `join_all()`
