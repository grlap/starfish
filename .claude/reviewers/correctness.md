# Correctness Review

Focus: Logic errors, edge cases, off-by-one errors, incorrect assumptions.

## What to check

1. **Logic errors**: Does the code do what it claims? Follow each code path.
2. **Edge cases**: Empty collections, zero values, overflow, null/None handling, single-element cases.
3. **Error handling**: Are errors propagated correctly? Any swallowed errors? Panics in library code?
4. **Concurrency bugs**: Data races, ordering issues, ABA problems, lost updates.
   - For atomic operations: is the memory ordering correct (Relaxed vs Acquire/Release vs SeqCst)?
   - For lock-free code: is the CAS loop correct? Can it livelock?
5. **Resource leaks**: Opened handles/sockets not closed? Allocated memory not freed?
6. **Off-by-one**: Loop bounds, slice indexing, fence-post errors.
7. **Type confusion**: Incorrect casts, truncation (u64 → u32), sign issues.
8. **Contract violations**: Does the code uphold the invariants documented in `//!` comments?

## What NOT to flag

- Style preferences (naming, formatting) — not your lane
- Missing documentation — there's a separate doc reviewer for that
- Performance suggestions unless they indicate a correctness bug (e.g., busy-wait that should block)
