Review staged and unstaged changes using multiple specialized reviewers.
Use the strongest available reasoning model for this command.

**IMPORTANT: NEVER `git commit` or `git push` without explicit user approval. All other git commands (diff, status, stash, add, etc.) may be executed freely.**

---

## Phase 1 — Quick-scan loop (fix until clean)

### Step 1: Format and lint

Run `cargo fmt` to ensure all code is properly formatted.
Then run `cargo clippy -- -D warnings`. If clippy produces ANY errors or warnings — regardless of whether they are in changed files or pre-existing — **STOP immediately**. Present the full clippy output to the user and do NOT proceed to any further steps.
Do **not** run benchmarks as part of this review flow.

### Step 2: Get the changes

Run `git diff` and `git diff --cached` to get all staged and unstaged changes.
If there are no changes, tell the user and stop.

Also run `git diff --name-only` and `git diff --cached --name-only` to get the list of changed files.

### Step 3: Quick scan (single primary pass)

Perform a broad code review of the diff **yourself** (do NOT launch subagents).
Evaluate every changed file for the most impactful issues across all disciplines:
correctness, safety, architecture, documentation, and test coverage.

Read changed files on-demand as needed for context — don't rely on the diff alone.

Classify each finding with a severity:
- **Critical** — data loss, UB, unsound unsafe, crash
- **High** — incorrect behavior, race condition, memory ordering bug
- **Medium** — meaningful code-quality or maintainability issue
- **Low** — minor style, naming, or readability nit
- **Note** — informational observation, no action needed

Collect **all** findings (every severity level). Keep Low/Note findings aside — they will be merged into the final report later.

### Step 4: Fix-or-escalate loop

If there are **any Critical, High, or Medium** findings:

1. Present the findings to the user in a brief summary table.
2. For each finding, either:
   - **Fix it** if the fix is clear and safe, OR
   - **Ask the user** (via AskUserQuestion) for guidance if the fix is ambiguous, risky, or involves a design choice.
3. After applying fixes, **go back to Step 1** (re-format, re-lint, re-scan).
4. Cap at **3 iterations**. If issues persist after 3 rounds, present remaining findings to the user and ask how to proceed.

If there are **no** Critical, High, or Medium findings — proceed to Phase 2.

---

## Phase 2 — Specialized reviewers

### Step 5: Discover reviewers

Glob `.claude/reviewers/*.md` to find all available reviewer lens files.
Read each file to get:
- The reviewer name (from the filename, e.g., `correctness.md` → "Correctness")
- The review instructions (the file content)

### Step 6: Run reviewers in parallel

For each reviewer found in Step 5, launch a **Task agent** (subagent_type: general-purpose) with this prompt:

```
You are a code reviewer focusing on: [REVIEWER NAME]

## Your Review Instructions
[CONTENT OF THE REVIEWER .md FILE]

## Changes to Review
[THE GIT DIFF]

## Changed Files
[LIST OF CHANGED FILE PATHS — read files on-demand as needed for context, don't rely on diff alone]

## Project Context
This is the Starfish project — a Rust async runtime with lock-free data structures.
Read CLAUDE.md and docs/ARCHITECTURE.md for project context if needed.

## Known Accepted Patterns (do NOT flag these)
- `Reactor::local_instance()` unbounded lifetime / TLS raw pointer pattern is accepted **when called only on reactor-managed threads with initialized TLS**
- `Coordinator::instance()` unbounded lifetime / TLS raw pointer pattern is accepted **when called only on coordinator-managed threads with initialized TLS**
- `Coordinator::reactor()` returning `ExternalReactor<'static>` is accepted as an operational risk **as long as handles do not escape coordinator lifecycle and are never used after `join_all()`**
- `spawn_external`/`spawn_external_with_result` preemptive-thread waiting via `CountdownEvent` is accepted; tests may wait directly via returned handle (`unwrap_result`)

## Output Format
Return your findings as a structured list:

### [REVIEWER NAME] Review

**Findings:**

For each issue found:
- **[SEVERITY: Critical/High/Medium/Low/Note]** `file:line` — Description of the issue
  - Why it matters: [explanation]
  - Suggested fix: [if applicable]

If no issues found, say "No issues found."

**Summary:** [1-2 sentence overall assessment]
```

Run ALL reviewer agents in parallel using multiple Task tool calls in a single message.

### Step 7: Consolidate

After all reviewers complete, merge their findings **together with the Low/Note findings from the Phase 1 quick scan** into a single review note:

```markdown
# Code Review — [date]

## Changes Reviewed
- [list of changed files]

## Actionable
[Findings that require code changes, grouped by severity: Critical → High → Medium → Low]

## Informational
[Observations, style notes, and FYI items that don't require immediate action]
[Include Low/Note items from the Phase 1 quick scan here]

## Reviewer Summaries
- **Quick Scan**: [summary of what was found and fixed in Phase 1, if anything]
- **Correctness**: [summary]
- **Architecture**: [summary]
- **Safety**: [summary]
- **Documentation**: [summary]
- **Testing**: [summary]
```

Deduplicate: if two reviewers (or the quick scan and a reviewer) flag the same issue, merge them (note which reviewers caught it).

Present the consolidated note directly to the user (do NOT write it to a file unless asked).
