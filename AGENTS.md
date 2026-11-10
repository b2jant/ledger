# Working on LEDGER

## Standing user instruction: use Jujutsu for version control

All future work and branches in this folder must use Jujutsu (`jj`). This is a
colocated Jujutsu/Git repository; GitHub stores the Git history, and `jj` manages
local changes and bookmarks (the branches published to GitHub).

- Start with `jj status` and `jj log`. Preserve existing work and use `jj new`
  to start a change; use `jj workspace add` when an isolated workspace is needed.
- Use `jj diff`, `jj commit` / `jj describe`, `jj bookmark`, `jj rebase`,
  `jj git fetch`, and `jj git push` for the corresponding version-control work.
  Do not use Git's staging, commit, branch/checkout/switch, stash, merge/rebase,
  reset, cherry-pick, pull, or push commands for normal work in this repository.
  Read-only Git inspection, GitHub CLI operations, and Git commands in test
  fixtures remain allowed.
- The remote is `origin` (`https://github.com/b2jant/ledger.git`); the main
  bookmark is `main`. Publish named feature bookmarks explicitly with
  `jj git push --remote origin --bookmark <name>`.
- After `jj commit`, the completed commit is `@-` and the new working-copy
  commit is `@`. Create or advance the intended bookmark to the completed commit
  before pushing; do not accidentally publish the empty working-copy commit.
- Keep `.jj/`, build output, and local LEDGER database/runtime files out of
  commits. If `jj` is unavailable, restore it before changing repository history.
- Apply this workflow to any authorized implementation/review subagents too.

See [README.md](README.md#contributing-with-jujutsu) for setup and example commands.

## Standing user instruction: use LEDGER for its own upgrades

For future features, fixes, refactors and upgrades in this repository, use LEDGER
to ground the work and assess its usefulness on the actual task. Apply this to any
authorized implementation/review subagents as well; include it in their briefs.

1. Read existing repository instructions and relevant entries in `LEDGER_FEEDBACK.md`.
   Then query LEDGER before broad source exploration. Use the real task description,
   with genuinely known anchors; retain the initial query so later refinements are
   not mistaken for first-attempt success.
2. Prefer connected `ledger_context` and `ledger_expand` MCP tools. When unavailable,
   use the equivalent local CLI. From the repository root, for example:

   ```sh
   ./target/release/ledger --workspace . query "<task>" --mode enhance --fresh
   ./target/release/ledger --workspace . expand "<returned handle>" "<advertised item>"
   ```

   Use an available current binary. If it is missing, stale, or fails, record that
   limitation and build it when practical. Do not let a broken LEDGER block its repair.
   Never invent a tool call/result or claim the CLI is a connected MCP tool.
3. Inspect proposed edit targets, references/contracts and candidate tests. Use
   expansion IDs for needed source. Treat suggestions as hypotheses: independently
   verify relevant files, callers and tests using direct reads, targeted `rg`, and
   appropriate checks. A ready index or lack of coverage warnings is not proof that
   retrieval is complete or that the recommended changes are correct.
4. Refine with a known path/symbol/literal or use native search when results are
   irrelevant, missing, ambiguous, stale, unavailable, or insufficient for safe edits.
   Record the fallback and what it found. Independent verification and these fallbacks
   are required even when optional helper prompts say to rely only on returned files.
5. Before trusting a generated verification command, check that it selects real tests;
   a successful command that runs zero tests is not validation. Run checks appropriate
   to the actual changes, not just the suggested commands.
6. If changing LEDGER retrieval/indexing/output behavior, compare the original task
   against the updated binary with a fresh index. Use a clean owner lifetime when
   switching builds; re-query for new handles. Compare the same prompt/anchors/budget
   and disclose any changed conditions. Fixture/local results are not general benchmarks.
7. Append one concise entry per meaningful task to `LEDGER_FEEDBACK.md`, covering:
   task and query/anchors; CLI or MCP and freshness; helpful results; irrelevant/missed
   results or bad test commands; refinements/fallbacks; actual changed files and checks;
   and any same-query comparison after a retrieval change. Report failures honestly.

Do not paste secrets or giant source dumps into feedback. New files that did not exist
at query time are not retrieval misses. Keep observed defects separate from suspected
causes and proposed fixes. Repair task-blocking failures when necessary; otherwise
record unrelated retrieval improvements for a future upgrade rather than expanding
every task into a search-engine rewrite. Mention material LEDGER successes/misses in
the task handoff without claiming unmeasured time or token savings.
