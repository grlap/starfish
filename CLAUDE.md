# Starfish - Project Instructions

## Architecture

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for system overview, crate dependency graph, and key design decisions.

- **starfish-core**: Lock-free data structures (SkipList, SortedList, SplitOrderedHashMap, SkipTrie, Treap, YFastTrie)
- **starfish-crossbeam**: Epoch-based memory-safe wrappers using crossbeam-epoch
- **starfish-epoch**: Custom epoch-based memory reclamation integrated with Coordinator/Reactor
- **starfish-reactor**: Cross-platform async I/O reactor (io_uring, kqueue, IOCP) + cooperative task scheduler
- **starfish-http**: HTTP/1.1 and HTTP/3 client and server (RFC 9112, RFC 9114)
- **starfish-quic**: QUIC transport protocol (RFC 9000, RFC 9001, RFC 9002)
- **starfish-tls**: Async TLS/SSL using rustls
- **starfish-storage**: Transactional storage engine (WAL, MVCC, buffer pool, indexes)
- **starfish-db**: Transactional database with stream processing

## Documentation Rules

Hierarchical documentation — all three levels must stay in sync.

**Level 1 — File-level** (`//!` module docs): Every `.rs` file begins with `//!` explaining what it does, key types, and important invariants.

**Level 2 — Crate-level** (`<crate>/README.md`): Purpose, modules list, key types & traits, usage example.

**Level 3 — Root** (`README.md`): Project overview, crate table with links.

When modifying code:
- **Changing a file's purpose or public API** → update its `//!` doc comment
- **Adding/removing a module or public type** → update the crate `README.md`
- **Adding/removing a crate** → update root `README.md`

## Design Docs

Design docs live per-crate in `<crate>/docs/`. Central index: [docs/DESIGN-INDEX.md](docs/DESIGN-INDEX.md).

Every design doc should have a status header:
```markdown
**Status:** NOT IMPLEMENTED | IN PROGRESS | IMPLEMENTED | SUPERSEDED
**Crate:** starfish-core
**Key files:** src/data_structures/sorted/skip_list.rs
```

## Testing Conventions

- Integration tests: `<crate>/tests/`
- Reusable test harness: `starfish-core/src/common_tests/` (trait-based tests for all SortedCollection implementations)
- Every public trait should have tests exercising each method
- Use `cargo nextest run` for parallel test execution

### Lock-free reclamation validation gate

Debug-only runs are insufficient for lock-free reclamation changes — `DeferredGuard` never
frees (masks UAF/ABA), and even debug+ASan misses windows that need release-speed churn (a
real release-only SIGSEGV shipped past both, 2026-06-10). Any change touching marks, unlink,
retirement, or the `linked_height` handshake must pass ALL of:

1. **EpochGuard + ASan** on the epoch stress tests:
   `RUSTFLAGS=-Zsanitizer=address cargo +nightly test -p starfish-crossbeam --target x86_64-unknown-linux-gnu --test skip_list_map_epoch --test skip_list_pair_epoch --test sorted_list_map_epoch --test skip_trie_map_epoch`
2. **Release-mode loops** (the windows are timing-dependent; loop ≥10×, ideally with
   freed-memory poisoning): `MALLOC_PERTURB_=<1-255> cargo test --release -p starfish-crossbeam`
3. **Release + debug-assertions** so the retire-while-reachable canary
   (`SkipList::debug_assert_unreachable`, with per-path `unlink_at_level` reason codes) runs at
   release timing: `RUSTFLAGS="-C debug-assertions=yes" cargo test --release ...`

For crash hunts: gdb -batch loops beat core dumps (apport eats them on Ubuntu).

## Bug Tracking

Per-crate bug files: `<crate>/docs/bugs.md`. **Track only active bugs** — remove an entry once it is fixed (git history preserves the record); do not keep resolved items as ~~strikethrough~~.

Before removing a fixed entry, lift any durable, non-obvious engineering insight it carries — invariants, memory-ordering/linearizability rationale, RFC-compliance subtleties, security reasoning — into the relevant code docs (`//!` headers, doc comments, or `// SAFETY:` comments) so the "why" survives at the code.

## Build & Test

```bash
cargo test              # Run all tests
cargo bench             # Run benchmarks
cargo clippy            # Lint
```

## Code Review

`/review-local` runs parallel specialized reviewers on staged/unstaged changes, then consolidates findings into a single note.

Reviewer lenses live in `.claude/reviewers/*.md`. Add a new `.md` file = add a new reviewer. Current lenses:
- **correctness** — logic errors, edge cases, concurrency bugs
- **architecture** — crate boundaries, pattern consistency, public API
- **safety** — unsafe code, SAFETY comments, aliasing, UB, memory ordering
- **documentation** — doc sync (//! comments, READMEs, design docs, bug trackers)
- **testing** — test coverage for changes, edge cases, regression tests

## Agent Commands

| Command | Domain | Purpose |
|---------|--------|---------|
| `/review-local` | Review | Multi-lens review of local changes |
| `/sync-docs` | Docs | Sync all 3 documentation levels across workspace |
| `/sync-docs-crate <crate>` | Docs | Sync one crate's documentation |
| `/audit-docs` | Docs | Report doc staleness (read-only) |
| `/sync-design-index` | Design | Regenerate `docs/DESIGN-INDEX.md` |
| `/check-design-drift` | Design | Detect code vs design doc divergence (read-only) |
| `/audit-test-coverage` | Tests | Find untested public APIs (read-only) |
| `/gen-test-stubs <crate>` | Tests | Generate test skeletons for gaps |
| `/sync-test-docs` | Tests | Match design doc test sections to actual tests |
