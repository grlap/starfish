# Documentation Review

Focus: Are docs in sync with the code changes?

## What to check

1. **File-level `//!` docs**: If a file's purpose or public API changed, does the `//!` comment still match?

2. **Crate README**: If public types/traits were added, removed, or renamed:
   - Is the crate README updated?
   - Is the "Key Types" or "Modules" section still accurate?

3. **Design doc drift**: If the change implements something from a design doc:
   - Is the design doc's Status header updated? (e.g., NOT IMPLEMENTED → IN PROGRESS)
   - Does the implementation match the design, or has it diverged?

4. **Bug tracker updates**: If the change fixes a tracked bug:
   - Is the bug marked resolved (~~strikethrough~~) in the relevant `docs/bugs.md`?

5. **Root README**: If a crate was added/removed, is the root README updated?

6. **DESIGN-INDEX.md**: If a design doc status changed, flag that `/sync-design-index` should be run.

## What NOT to flag

- Missing inline code comments (// regular comments) — that's a style choice
- Documentation quality/wording preferences
- Missing doc tests (/// examples) — focus on structural doc sync only
