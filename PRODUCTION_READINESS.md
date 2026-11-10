# LEDGER production readiness roadmap

LEDGER is useful for locating code when a caller already knows a symbol or literal.
Its broader task understanding is inconsistent, and its ability to choose the right
real-world commits or pull requests has not been established. The next release
should close those gaps with five improvements below, while retaining its local,
bounded CLI/MCP design.

This is an assessment and implementation roadmap, not a release announcement.
The assessment inspected source at `dacf07f1`, the working checkout's qualitative
feedback, fresh CLI queries, and existing tests on November 9, 2026. It preserves
the separate unpublished VCS-planning and runner-workflow changes. The additional
feedback from that checkout informed this review without being copied into this PR.

## What has been useful

| Capability | Evidence | Current conclusion |
| --- | --- | --- |
| Exact code lookup | Earlier feedback records `install_harness_skills` and `VcsBackend` becoming useful after native exploration supplied precise anchors. This review retrieved `query_warm_snapshot` and successfully expanded its source after refinement. | Useful evidence locator; refinement success is not first-attempt understanding. |
| Broad task semantics | Workflow, publication and CI tasks returned unrelated code. This review's initial unanchored query returned fixture/framing tests, a GraphQL extractor and a history test instead of core ranking and distribution files. | Unreliable across task types; a ready index does not establish relevant coverage. |
| Commit selection | Local history FTS, co-change and hunk attribution exist, with synthetic Git integration coverage. The current repository has very little history; its large initial import is excluded from history search by the noise rule. | Implemented foundation; no measured real-task precision or recall yet. |
| PR selection | There is no forge ingestion or structured PR entity. `gh pr list --state all` returned no PRs before this assessment PR. | No demonstrated PR-selection usefulness. Commit-message references alone cannot establish PR identity or review state. |
| Evidence delivery | Scope checks, atomic generation publication, owner locking, bounded IPC and continuation expansion already exist. One advertised expansion failed; a later query and expansion succeeded. | Preserve the protections; improve diagnostics and evaluate realistic workloads. |

The feedback log is qualitative and selected, not a representative benchmark. No
percentage accuracy, engineering-time savings or production-scale performance is
claimed. Source and tests supporting these conclusions are linked below.

## 1. Understand the task and retrieve files beyond code symbols

**Problem.** [SQLite indexing](crates/ledger-store/src/sqlite.rs) populates both code
and documentation FTS inside the loop over `resolved.symbols`. The
[parser](crates/ledger-parser/src/extractors.rs) produces no AST symbols for ordinary
Markdown/config files. In [query_warm_snapshot](crates/ledger-daemon/src/engine.rs),
path anchors also seed only symbols. Such files can still appear through live
literal or contract paths, but are absent from the main file-selection mechanism.
[Anchor classification](crates/ledger-core/src/query.rs) misses root names such as
`README.md` and `Cargo.toml`. FTS uses the first twelve extracted terms; lexical
matches and graph propagation can crowd explicit anchors out of four target slots.
[Query terms](crates/ledger-core/src/tokens.rs) discard words such as `not`, so they
cannot serve as a representation of the user's constraints.

**First deliverable.** Introduce file and section candidates alongside symbols,
with bounded Markdown headings, configuration sections and plain-file slices.
Keep their kinds explicit rather than pretending every section is a function.
Resolve existing in-scope paths before guessing an anchor's kind; apply focus and
scope rules equally to every candidate type. Add task categories for docs,
configuration, tests and history. Preserve the original request and explicit
constraints in a small intent representation; caller-supplied follow-up context
must identify its source and stay bounded. Do not silently infer conversational
memory from an isolated tool call.

Prioritize explicit entities and informative terms before truncation. Reserve
target space for exact anchors, diversify files, and constrain high-degree graph
neighbors. Separate relevance confidence from index freshness: return a calibrated
low-evidence result or a narrow clarification instead of confidently filling all
four slots. Evaluate synonym expansion and optional local embeddings only after
the structural fixes; neither becomes a mandatory cloud dependency.

**Edit surface.** `ledger-core` query/tokens/model, parser extractors, store
SQLite/graph, daemon selection and the MCP request/response schema.

**Acceptance.** Known-path tests must retrieve `README.md`, `AGENTS.md`,
`Cargo.toml` and workflow YAML with zero AST symbols. Regression cases must retain
explicit anchors, respect exclusions/negation and retrieve the intended existing
files on the original unanchored prompts. Unknown or out-of-scope paths need honest
diagnostics. Measure held-out Recall@4 and ranking quality under improvement 3;
no predicted percentage gain is a release claim.

## 2. Return traceable commit and PR evidence

**Problem.** [History search](crates/ledger-store/src/sqlite.rs) searches summaries
and paths, excludes merge/dependency prefixes and revisions touching over fifty
allowed files, and stores no PR entity. The engine uses history FTS for history mode
or when other seeds are absent, then distributes scores across touched symbols.
This can hide useful rationale and cannot prove which PR caused a regression.
Co-change is correlation, not causation.

**First deliverable.** Add structured history evidence to
[ContextPack](crates/ledger-core/src/query.rs): repository identity, full revision
ID, parent/base identity, touched path or hunk, rationale for selection, source and
observation time. Keep history evidence distinct from current edit targets. Retain
merge/dependency metadata for direct history search while suppressing unhelpful
co-change propagation. Select bounded diffs using existing
[hunk attribution](crates/ledger-vcs/src/history.rs); rank using query relevance,
overlap and age without allowing age alone to erase an old relevant change.

Start offline with bounded commit bodies/trailers and reference candidates.
`#123` may denote an issue, a PR or another repository: retain it as unresolved
until the repository and object type are established. A trailer is untrusted
metadata, not proof of approval. Then add opt-in GitHub enrichment behind a
separate forge interface: PR title/body, base/head/merge SHAs, state, URL and fetch
time, with bounded additional review evidence when requested. Model many-to-many
commit associations and squash/rebase/merge cases. GitHub provides an
[associated-PR endpoint](https://docs.github.com/en/rest/commits/commits#list-pull-requests-associated-with-a-commit);
use its documented semantics rather than inventing mappings from commit subjects.

Fetch through a serialized, bounded queue with pagination checkpoints, conditional
requests, backoff and explicit partial/stale states, following
[GitHub API guidance](https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api).
Queries use the local cache. Network failure must preserve local retrieval. Scope
cached remote data by host, repository and access context; provide purge/retention
controls and keep credentials out of the index. Fork content and PR text remain
untrusted evidence. No automatic checkout, cherry-pick, approval or merge follows
from a relevance score.

**Edit surface.** Core history/query types, VCS Git/JJ/history/co-change, store
schema, daemon rendering, CLI/MCP configuration and a new forge adapter boundary.

**Acceptance.** Fixtures cover squash, rebase, merge, revert, rename, dependency
fixes, identical numbers in different repositories, missing credentials, revoked
access, pagination and rate limits. Known relevant commits/PRs must be retrievable
with verifiable identifiers and explicit uncertainty; a relevance match must not
be labeled a proven regression cause. Hold out real repository/PR tasks before
claiming useful PR selection. Network sync depends on improvement 4's budgets.

## 3. Make relevance and verification measurable

**Problem.** The [existing benchmark](crates/ledger-cli/src/phase2_bench.rs) uses a
small synthetic history and supplied anchors. It is useful regression coverage,
but does not measure broad natural-language task success. The
[resolver](crates/ledger-parser/src/resolver.rs) marks calls from test files as test
evidence even when the caller is a helper, then uses that helper name as a Cargo
filter. `parser_and_contracts buf` demonstrably selects zero tests.

**First deliverable.** Add a versioned evaluation manifest recording repository
revision, original task, genuinely supplied anchors, expected relevant files and
history, mode, budget, build and index conditions. Treat the existing feedback
cases as development regressions, and reserve separate repositories/tasks as a
held-out set. Split by repository/task family to reduce leakage. Replay through
the actual CLI and MCP, including expansions, instead of only benchmark-specific
selection code. Compare lexical, graph, history and any semantic channel under
matched conditions.

Report Recall@4, precision/rank quality, commit/PR relevance, low-confidence
calibration, expansion validity, actual test selection, cold/warm latency, peak
memory, bytes read and index size. Keep first attempts separate from refined
queries; retain failure cases. Establish a measured baseline before setting a
numeric performance SLO, and gate regressions by task category rather than hiding
them in an aggregate score.

Only emit a symbol test filter for a verified test declaration/discovered test.
Follow helper callers to actual tests where justified, or suggest the whole
verified test target. Use discovery for module-qualified exact selection. Cargo's
ordinary substring filters can validly match unqualified names; missing module
scope alone does not prove a zero-test bug. Run discovery in trusted fixture/CI
contexts: `cargo test -- --list` may compile and execute build scripts, so a
read-only context query must not silently execute arbitrary repository tooling.
Label commands as candidates unless discovery/execution evidence is available.

**Edit surface.** Parser resolver/extractors and tests; CLI benchmark runners;
daemon/MCP end-to-end fixtures; a new evaluation manifest and CI quality report.

**Acceptance.** Helper-name cases (`identity`, `socket`, `backend`, `buf`, `extract`)
cannot be presented as verified tests. Discovery must select at least one intended
test, with fixture cases for duplicate names and modules. Every advertised expansion
in the evaluation corpus must resolve or give a specific lifecycle/scope reason.
Create these gates before tuning improvements 1 and 2.

## 4. Bound indexing work and degrade optional sources safely

**Problem.** [run_index_pass](crates/ledger-daemon/src/engine.rs) reuses parses by
content hash, but still reads allowed files on each pass. Changed generations
rebuild graph/store state and request up to 500 revisions. VCS errors propagate
before publication. [SystemCommandRunner](crates/ledger-vcs/src/backend.rs) captures
process output without an enforced wall-clock or byte budget. Process-local
[handle storage](crates/ledger-store/src/graph.rs) gives the same error for several
failure causes. The observed failed expansion is not proof of any one cause.

**First deliverable.** Add process deadlines, stdout/stderr caps, cancellation and
reaping to the command runner. Publish source evidence with explicit absent,
partial, stale or failed history when optional VCS/forge work fails; never present
old history as belonging to a new head. Preserve atomic publication and the
single-owner lock. Add before/after source/head checks or bounded retry when the
checkout changes during indexing.

Move toward dirty-file batching, bounded work queues and incremental persistence,
using filesystem events as hints plus periodic full reconciliation. Metadata can
avoid routine reads but cannot prove unchanged content: cover same-size edits,
restored timestamps, missed events, ignore changes and atomic saves. `--fresh`
must retain a documented content-verification guarantee. Benchmark on declared
small/large repository fixtures and record hardware/profile before making scale
claims. Add retention limits for telemetry, cached history and WAL/disk growth.

Expose owner/build/DB identity and actionable handle-expiry diagnostics. First
retain the documented restart-invalidation contract and make re-query recovery
reliable. Durable handles are a later option requiring versioned baselines,
workspace/DB identity, scope revalidation and delta-idempotence tests; persisting a
bare handle table is insufficient. A cold or degraded result must state exactly
which evidence is unavailable.

**Edit surface.** Daemon engine/IPC, VCS backend/Git/JJ and store graph/SQLite;
extend lifecycle, expansion and history integration tests. This follows the same
bounded-runner and degraded-history direction as the separately prepared VCS plan.

**Acceptance.** Test hung/oversized child processes, disk/store failure, index
churn, daemon restart/build change, eviction, database replacement and cross-workspace
handles. Verify fresh source remains available with failed history, stale evidence
is labeled, and no change bypasses containment or ownership. Performance gates
must include CPU/I/O at idle and bounded memory under repeated queries.

## 5. Ship installable and verifiable releases

**Problem.** The [artifact workflow](.github/workflows/binary-artifacts.yml) uploads
temporary CI artifacts without publishing releases. The
[packager](.github/scripts/package_binary.py) omits version/target-triple identity
from archive names. Cargo declares dual licensing but this tree has no license
texts; internal dependencies are path-only. CI tests a newer compiler than the
declared minimum without a dedicated minimum-version job.

**First deliverable.** Adopt a release checklist covering license texts and notices,
repository/package metadata, changelog, support/security reporting and a tested
CLI/MCP/database compatibility policy. Test the declared minimum Rust version or
raise it based on results; using a newer pinned CI toolchain is not itself a bug.
Build immutable versioned archives from reviewed version tags with the lockfile,
explicit target triples and recorded compiler/commit metadata. Start with macOS
ARM64 and Linux x86_64; add other Unix targets only when build and runtime smoke
checks pass. Document OS/ABI floors. Windows needs a separate portability pass
because today's transport is Unix-specific.

Publish permanent GitHub Release assets with SHA-256 checksums and verifiable
[build attestations](https://docs.github.com/en/actions/how-tos/secure-your-work/use-artifact-attestations/use-artifact-attestations).
A checksum checks integrity; an attestation binds provenance. Pin release actions
to reviewed revisions, restrict publishing credentials, and test downloads,
tamper rejection and previous-version upgrades. Deterministic archive metadata
does not establish bit-identical compiled binaries.

Use an owned [Homebrew tap](https://docs.brew.sh/How-to-Create-and-Maintain-a-Tap)
first. Proposed package: `b2jant/tap/ledger-context`, installing a distinct
`ledger-context` executable. Homebrew already provides
[the accounting program ledger](https://formulae.brew.sh/formula/ledger), including
`bin/ledger`; changing only the formula name would not prevent that collision.
Do not install a competing `ledger` symlink by default. Document MCP configuration
using the packaged name and test daemon spawning through it. A formula should
build a tagged source release and use tap bottles for fast installs; ordinary
release tarballs are not automatically Homebrew bottles. Follow the
[formula test/audit workflow](https://docs.brew.sh/Formula-Cookbook).

Direct archives and local Cargo installs remain initial alternatives. Before a
registry release, check/reserve names, include license/readme files in each package,
add compatible version requirements to internal path dependencies, and verify
packaging and publication order in a staging registry. Cargo documents the
[path-plus-version requirement](https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html#multiple-locations).
Do not advertise `cargo install ledger-cli` until that package is actually ours and
published. Defer npm wrappers, system packages and containers until demand justifies
their maintenance; a container is optional CI packaging, not the easiest local MCP
installation.

**Edit surface.** Cargo manifests, license files, release workflow/packager,
installation docs and a separate tap repository. No tap or package is published
by this assessment PR.

**Acceptance.** On every supported target, install from the release/tap into a
clean environment and exercise version reporting, init, fresh query, expansion,
MCP handshake, status and stop. Test upgrades with an active owner, rollback/rebuild
of disposable index data, uninstall cleanup and coexistence with Homebrew's ledger.
Validate actual downloaded archives and bottles, not just local build output.

## Delivery order and release decision

Start improvement 3's regression harness and improvement 1's file/path retrieval
together. Deliver improvement 4's process budgets before improvement 2's network
sync; build offline history evidence in parallel. Release engineering in
improvement 5 can proceed alongside those changes, with preview releases clearly
labeled until quality gates pass.

A production release requires all five acceptance sets to pass on its declared
support matrix, held-out task results to be published, and unresolved relevance or
compatibility limitations to be documented. Each improvement should ship as
reviewable implementation slices with same-query before/after evidence. The
roadmap adds no runtime behavior. A follow-up CI repair in this PR synchronizes
the existing VCS-failure and panic-recovery tests with worker idleness.

## Round table decisions

The parent and Gemini 4 Argon, requested with `thinking_level=high` for both
rounds, independently inspected retrieval and production boundaries. Round one
agreed on the five areas and identified the structural non-symbol indexing gap
and helper-filter path. Round two explicitly agreed with all five improvements,
their acceptance criteria and delivery order, with no blocking corrections.
The agreed plan rejects unsupported numeric improvement predictions and
distinguishes observed expansion failures from proposed explanations. It uses
separate held-out tasks, safe freshness reconciliation and a distinct packaged
executable.

Validation for this assessment: eleven history-store tests and the daemon's Git
history/co-change/diff integration test passed. A suggested `buf` filter was
independently listed and selected zero tests. Local documentation links, the five
acceptance sets and the isolated PR scope were checked. These checks support the
diagnosis; they do not implement or demonstrate the proposed retrieval gains.
