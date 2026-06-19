Generate test stubs for untested public APIs. Arguments: $ARGUMENTS (required crate name, e.g., "starfish-core").

If no crate name is provided, ask the user which crate to generate stubs for.

## Step 1: Run audit logic

Perform the same analysis as `/audit-test-coverage` for the specified crate:
1. Map the public API surface (pub fn, struct, trait, enum)
2. Map existing tests (tests/ directory + inline #[cfg(test)] + common_tests/)
3. Identify untested public items

## Step 2: Generate test stubs

For each untested public item:

1. Generate a `#[test]` function skeleton:
   ```rust
   #[test]
   fn test_<item_name>_<operation>() {
       // TODO: Test <description of what should be tested>
       todo!("Implement test for <item>")
   }
   ```

2. Include necessary imports (`use` statements for the type/trait)

3. Place the test in the appropriate location:
   - If the source file already has a `#[cfg(test)] mod tests {}` block → add there
   - If a `tests/<module>_test.rs` integration test exists for that module → add there
   - Otherwise → create a new file in `<crate>/tests/`

4. For trait implementations, follow the pattern in `starfish-core/src/common_tests/`:
   - Generate tests that exercise trait methods through the concrete type
   - Use the common test framework if the trait is `SortedCollection` or `MapCollection`

## Step 3: Special cases

- Do NOT generate tests for `pub(crate)` items (only truly public API)
- Do NOT generate tests for re-exports (test the original, not the re-export)
- Do NOT generate tests for trivial derives (Debug, Clone, etc.)
- For `unsafe` functions, add a `// SAFETY:` comment in the test explaining the preconditions

## Step 4: Report

List every test function generated and where it was placed:
```
Generated 5 test stubs:
  tests/skip_list_tests.rs:
    - test_skip_list_range_query_inclusive
    - test_skip_list_range_query_exclusive
  src/guard/mod.rs (inline #[cfg(test)]):
    - test_guard_pin_unpin_lifecycle
```
