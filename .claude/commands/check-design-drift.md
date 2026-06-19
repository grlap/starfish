Check for drift between design docs and implementation. Do NOT modify any files — report only.

## Step 1: Load design docs

Read `docs/DESIGN-INDEX.md` to get the list of design documents (skip bug trackers and references).

## Step 2: Analyze each design doc

For each design document:

1. **Read the document** and extract:
   - Proposed types, traits, structs, enums (from code blocks and descriptions)
   - Proposed method signatures
   - Referenced source files (file paths mentioned in the doc)
   - The claimed implementation status

2. **Check against source code**:
   - For each proposed type/trait: does it exist in the codebase? (use grep/glob)
   - For each referenced file: does it exist? Have the public items changed?
   - For "NOT IMPLEMENTED" docs: do any of the described types actually exist now? (doc may be stale)
   - For "IMPLEMENTED" docs: have key types been removed or renamed? (code drifted from doc)

3. **Classify drift**:
   - `STALE_STATUS` — doc says NOT IMPLEMENTED but code exists
   - `MISSING_CODE` — doc says IMPLEMENTED but code is missing
   - `SIGNATURE_DRIFT` — type/method exists but signature differs from doc
   - `UNDOCUMENTED_ADDITIONS` — source file has new public items not in the doc
   - `NO_DRIFT` — doc and code are consistent

## Step 3: Report

Present findings as a table:

| Design Doc | Status in Doc | Drift Type | Details |
|------------|---------------|------------|---------|
| Iterators | NOT IMPLEMENTED | STALE_STATUS | `SortedCollectionIter` exists in skip_list.rs |
| MVCC | NOT IMPLEMENTED | NO_DRIFT | Types not found in source |

Then list actionable items:
- Docs that need status updates
- Docs that need content updates to match changed code
- Code that needs documentation (new public items)
