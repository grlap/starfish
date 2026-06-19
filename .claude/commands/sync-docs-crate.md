Synchronize documentation for a single crate: $ARGUMENTS

If no crate name is provided, ask the user which crate to sync.

## Step 1: Audit file-level docs for this crate

Read every `.rs` file under `<crate>/src/`. For each file:
- If missing `//!` module doc comment: read the file, understand its purpose, add a concise `//!` comment (2-5 lines) at the top.
- If present but outdated: update it.
- If accurate: leave it alone.

## Step 2: Update crate README

Read (or create) `<crate>/README.md`. Ensure it contains:
1. **Title**: `# <crate-name>`
2. **Purpose**: 1 paragraph
3. **Modules**: List each module with one-line description from `//!` docs
4. **Key Types & Traits**: Main public API
5. **Usage Example**: Short code snippet (if applicable)
6. **Design Notes**: Architectural decisions (if relevant)

## Step 3: Check root README

Verify the root `README.md` crate table includes this crate with an accurate description. Update if needed.

## Step 4: Summary

Report what changed. Do NOT modify any functional code.
