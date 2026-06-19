Synchronize all project documentation across all three levels. Follow these steps:

## Step 1: Audit file-level docs

For each crate in the workspace, read every `.rs` file under `src/`.
Check if it has a `//!` module-level doc comment at the top.

- If missing: read the file contents, understand its purpose, and add a concise `//!` doc comment (2-5 lines) at the very top.
- If present but outdated (doesn't match the file's actual contents/public API): update it.
- If present and accurate: leave it alone.

## Step 2: Audit crate READMEs

For each crate, read (or create) its `README.md`. It must contain:

1. **Title**: `# <crate-name>`
2. **Purpose**: 1 paragraph explaining what the crate does
3. **Modules**: List each module (`.rs` file or `mod` directory) with a one-line description derived from its `//!` doc comment
4. **Key Types & Traits**: List the main public types and traits
5. **Usage Example**: A short code snippet (if the crate has a clear entry point)
6. **Design Notes**: Key architectural decisions (only if relevant)

Cross-reference the `//!` comments from Step 1 to build the module list.
If a README exists, update it to match current code. If it doesn't exist, create it.

## Step 3: Update root README.md

Read the root `README.md`. Update the crate table to:
- Include all workspace members (check `Cargo.toml` [workspace] members)
- Each crate links to its README: `[crate-name](./crate-name)`
- One-line descriptions match the crate README's purpose paragraph

## Step 4: Summary

Report what was changed:
- Files where `//!` docs were added or updated
- Crate READMEs created or updated
- Root README changes

Do NOT change any functional code. Only modify documentation comments and markdown files.
