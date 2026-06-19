# Testing Review

Focus: Test coverage for the changes, test quality.

## What to check

1. **New public APIs**: Every new `pub fn`, `pub trait`, `pub struct` should have at least one test.
   - Check both `tests/` integration tests and inline `#[cfg(test)]` modules
   - Check `common_tests/` for trait-level test coverage

2. **Changed behavior**: If existing behavior changed, are the tests updated to match?
   - Tests that still pass but test the old behavior = false confidence

3. **Edge cases**: For new code, are these tested?
   - Empty input / zero values
   - Boundary conditions (max values, single element)
   - Error paths (what happens on failure?)

4. **Concurrency tests**: For lock-free code changes:
   - Stress tests with multiple threads?
   - Tests that exercise the specific race condition being fixed?

5. **Test quality**:
   - Tests actually assert something meaningful (not just "doesn't panic")
   - Test names describe the scenario, not the implementation
   - No flaky tests (time-dependent, order-dependent)
   - Prefer deterministic local resources: avoid public internet dependencies, fixed ports, and oversized temp files for default test runs

6. **Regression tests**: If this fixes a bug, is there a test that would catch the bug if it regressed?

7. **Reactor external-wait patterns**:
   - For `spawn_external` from preemptive threads, prefer direct waiting on returned handles (e.g., `unwrap_result`) over extra wrapper synchronization unless explicitly needed

## What NOT to flag

- Code coverage percentages
- Missing doc tests (/// examples)
- Test code style/formatting
- Absence of benchmarks in review/fix flows
