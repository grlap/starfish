Audit documentation freshness across the workspace. Do NOT modify any files — report only.

## Step 1: File-level `//!` doc audit

For each crate in the workspace, glob all `.rs` files under `src/` (recursively).
Read the first 5 lines of each file and check for a `//!` module-level doc comment.

Report files that are **missing** `//!` docs, grouped by crate:
```
starfish-core:
  - src/lib.rs ❌ missing
  - src/data_structures/sorted/skip_list.rs ❌ missing
```

## Step 2: Crate README audit

For each crate, check if `README.md` exists. If it exists, verify it contains these required sections:
- A purpose paragraph (first paragraph after the title)
- A "Modules" or module listing section
- A "Key Types" or "Key Traits" section

Report missing READMEs and READMEs with missing sections.

## Step 3: Root README vs Cargo.toml

Read `Cargo.toml` workspace members. Read root `README.md` crate table.
Report any crates in the workspace but missing from the README, or listed in the README but not in the workspace.

## Step 4: Design doc status check

Read `docs/DESIGN-INDEX.md`. For each design doc marked "NOT IMPLEMENTED":
- Quickly check if any of its key types/files now exist in the source code
- Flag potential staleness (the design may have been implemented)

## Step 5: Summary

Present a summary table:
| Check | Status | Details |
|-------|--------|---------|
| File `//!` docs | X/Y files covered | N missing |
| Crate READMEs | X/Y present | N missing sections |
| Root README | ✅ or ❌ | Missing crates: ... |
| Design doc freshness | X docs may be stale | ... |
