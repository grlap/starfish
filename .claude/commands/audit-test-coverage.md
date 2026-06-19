Audit test coverage by analyzing the public API surface vs existing tests. Do NOT modify files — report only.

Arguments: $ARGUMENTS (optional crate name, e.g., "starfish-core". If omitted, audit all crates with real implementation.)

## Step 1: Identify target crates

If a crate name is provided, audit only that crate.
Otherwise, audit: starfish-core, starfish-crossbeam, starfish-epoch, starfish-reactor, starfish-tls.
Skip starfish-storage and starfish-db (aspirational — no real implementation yet).
Skip starfish-http (minimal stub).

## Step 2: Map public API surface

For each target crate:

1. Read `src/lib.rs` to find `pub mod` and `pub use` declarations
2. For each public module, read the source file and find:
   - `pub fn` — public functions
   - `pub struct` — public structs
   - `pub trait` — public traits (and their required methods)
   - `pub enum` — public enums
   - `impl Trait for Type` — trait implementations
3. Focus on items re-exported at the crate root (the true public API)

## Step 3: Map existing tests

For each target crate, find all tests:
1. Integration tests in `<crate>/tests/*.rs` — read and list `#[test]` functions
2. Inline test modules: grep for `#[cfg(test)]` in source files, list test functions
3. Common test framework: check `starfish-core/src/common_tests/` for reusable trait tests

## Step 4: Match API to tests

For each public item, search test files for:
- Direct usage (type name, function name appears in a test)
- Indirect usage via the common test framework (trait tests that exercise all implementations)

## Step 5: Report

Present per-crate findings:

### starfish-core

**Coverage Summary**: X/Y public items tested

| Public Item | Kind | Tested | Test Location |
|-------------|------|--------|---------------|
| `SkipList::insert` | fn | ✅ | tests/collection_stress_tests.rs |
| `SortedList::range` | fn | ❌ | — |

**Untested items** (critical gaps):
- List items with no test coverage

**Modules with no integration tests**:
- List modules that have zero test files

**Stale tests** (reference removed/renamed items):
- List test files that import items that no longer exist
