# Architecture Review

Focus: Structural integrity, crate boundaries, patterns, consistency.

## What to check

1. **Crate dependency direction**: Dependencies flow downward only:
   ```
   starfish-core (foundation)
   ├→ starfish-reactor (core)
   │  ├→ starfish-tls (core, reactor)
   │  └→ starfish-epoch (core, reactor)
   │     └→ starfish-storage (core, epoch)
   ├→ starfish-http (core)
   └→ starfish-crossbeam (core)
   ```
   Flag any code that creates upward or circular dependencies.

2. **Module boundaries**: Is the change in the right module? Does it belong in a different crate?

3. **Pattern consistency**: Does the code follow existing patterns?
   - Collections implement `SortedCollection` or `MapCollection` traits
   - IO backends implement `IOManager` trait
   - Memory reclamation uses `Guard` trait
   - Tests use `common_tests/` framework for trait-level testing

4. **Public API surface**: New `pub` items should be intentional.
   - Prefer `pub(crate)` over `pub` unless it's a true public API
   - New public traits/types need to be re-exported from `lib.rs`

5. **Naming conventions**: Match existing patterns in the crate.

## What NOT to flag

- Internal implementation details (how, not where)
- Performance characteristics (unless architectural)
- Unsafe code specifics — there's a separate safety reviewer
