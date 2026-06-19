Fix a bug from a crate's `docs/bugs.md` by tag (e.g., `BUG-1`, `DOC-2`, `TRD-3`).

Arguments: $ARGUMENTS (required: `<crate> <tag>`, e.g., `starfish-epoch BUG-1`). If omitted, ask the user which crate and bug to fix.

**IMPORTANT: NEVER `git commit` or `git push` without explicit user approval. All other git commands (diff, status, stash, add, etc.) may be executed freely.**

## Bug tracker write policy (append-only)

When updating `<crate>/docs/bugs.md`, preserve history:
- Do not delete prior bug entries or old reviewer notes.
- Add new findings at the end of the appropriate severity section.
- When assessing an existing bug (agree/disagree/severity adjustment), add an inline `Reviewer Note (YYYY-MM-DD): ...` under that bug entry.
- Keep existing wording unless clearly wrong; prefer additive comments over rewrites.

## Step 1: Parse the bug

Read `<crate>/docs/bugs.md` and find the bug entry matching the tag from `$ARGUMENTS`.

- If the file doesn't exist → tell the user and **stop**.
- If the tag is not found → tell the user and **stop**.
- If the bug is already closed (strikethrough / `FIXED` / `Resolved`) → tell the user and **stop**.
- Extract: **severity**, **component/file**, **code locations** (with line numbers), **description**, and **recommended fix** (if any).
- Present a brief summary to the user: crate, tag, severity, one-line description.

## Step 2: Assess the bug (push-back gate)

Read every file listed in the bug entry. Examine the actual code and surrounding context.

Evaluate whether the bug is **valid** and whether the **severity is accurate**. Consider:
- Has the bug already been fixed in a later change?
- Is the described behavior actually a bug, or expected/harmless?
- Is the severity too high or too low?

**If you agree** the bug is valid and severity is correct:
1. Add an inline `Reviewer Note (YYYY-MM-DD): Agree ...` under that bug entry.
2. Proceed to Step 3.

**If you disagree** (false positive, wrong severity, already fixed, or not worth fixing) → use AskUserQuestion to present your reasoning and offer options:
- Agree with the original assessment and proceed
- Downgrade/upgrade severity and proceed
- Close as false positive / already fixed
- Proceed with the fix anyway

If the user chooses to close or downgrade without fixing:
1. Update `<crate>/docs/bugs.md` accordingly (append reviewer note + strikethrough/reason, or append note with new severity)
2. Update the `Last updated:` header with today's date and the tag
3. Present what was changed and **stop**

## Step 3: Fix the bug

Implement the fix:

1. Read the referenced files and any surrounding code needed for context
2. Follow existing project patterns — check nearby code for conventions
3. Keep the fix **minimal and focused** — do not refactor unrelated code
4. If the approach is ambiguous or there are multiple valid solutions → use AskUserQuestion to let the user choose before writing code

**Related bugs:** If 2–3 other open bugs share the same root cause or touch the same file, mention them to the user and offer to fix them together in one pass. Do not batch more than 3 bugs.

## Step 4: Verify the fix

Run these checks sequentially:

1. `cargo fmt --all` — fix any formatting issues
2. `cargo clippy --all-targets` — if errors appear, fix them and re-run
3. `cargo nextest run` (or `cargo test` if nextest is unavailable) — if a specific test file is relevant, run that first with `-p <crate>`; otherwise run the full suite
   - If tests fail due to the fix, update or add tests as needed
   - If tests fail for unrelated reasons, note it but continue

All three must pass before proceeding.

## Step 5: Review via /review-local

Invoke the `/review-local` skill to get multi-reviewer sign-off on the changes.

After the review completes:
- **Critical or High findings** → fix them, re-run Step 4, and re-review
- **Medium or Low findings** → present to the user; proceed if they accept
- **No findings** → proceed

## Step 6: Close the bug in bugs.md

Once the fix is verified and reviewed:

1. Strikethrough the bug title and add `(FIXED: <brief rationale>)`
   - Example: `#### ~~BUG-1. Infinite loop in read_to_end~~ (FIXED: added EOF break condition)`
2. Update the `Last updated:` line at the top of the crate's bugs.md — append today's date and the fixed tag
   - Example: `| 2026-02-28 (BUG-1 fixed)`
3. Present a final summary:
   - Bug tag, crate, and title
   - What was changed (files modified)
   - How it was verified (tests, clippy, review)
   - Any related bugs the user may want to address next
