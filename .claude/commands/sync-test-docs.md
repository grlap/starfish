Synchronize test documentation with actual tests. Do NOT modify files — report only.

## Step 1: Find test sections in design docs

Read `docs/DESIGN-INDEX.md` to get the list of design documents.
For each design doc, search for sections titled "Testing", "Test", "Test Plan", "Verification", or "Test Cases".

Extract described test scenarios. Example:
```
From upsert.md → "Testing" section:
  1. Basic upsert (insert when missing)
  2. Idempotent upsert (replace when exists)
  3. Concurrent upsert (multiple threads)
  4. Mixed operations (upsert + delete interleaving)
```

## Step 2: Map to existing tests

For each described test scenario, search the codebase for matching tests:
- Search test function names for keywords from the scenario
- Search test comments/doc strings for matching descriptions
- Check `starfish-core/src/common_tests/` for trait-level tests that cover the scenario

## Step 3: Find undocumented tests

Search all test files for `#[test]` functions. Cross-reference against the design doc test descriptions.
Identify tests that exist in the code but are not described in any design doc.

## Step 4: Report

Present a three-column report:

### Matched (design doc test ↔ actual test)
| Design Doc | Described Test | Actual Test | Location |
|------------|---------------|-------------|----------|
| upsert.md | Basic upsert | test_upsert_insert | tests/sorted_list_tests.rs |

### Missing (described in design doc but no test exists)
| Design Doc | Described Test | Priority |
|------------|---------------|----------|
| upsert.md | Concurrent upsert | High |

### Undocumented (test exists but not in any design doc)
| Test Function | Location | Possible Design Doc |
|--------------|----------|---------------------|
| test_skip_list_batch_insert | tests/collection_stress_tests.rs | iterators-and-range-queries.md? |

## Step 5: Summary

- N described tests matched to actual tests
- N described tests have no implementation (gaps)
- N tests exist without design doc coverage
