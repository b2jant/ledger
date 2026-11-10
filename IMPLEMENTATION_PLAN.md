# LEDGER core release hardening plan

Status: implemented and verified. GPT-6 Astra (extra-high) and Gemini 4 Argon (high)
agreed after two review rounds; Gemini returned `AGREE_WITH_PLAN`. Exactly three
GPT-6.1 Sol agents at extra-high effort completed the three streams. Astra reviewed,
integrated and verified the combined changes within the user-confirmed core-only scope.

## Scope and baseline

Deliver a dependable local CLI/MCP release of the existing Phase 1–2 functionality.
Dashboard/serve, annotations, embeddings and ship remain clearly identified roadmap
items. The user explicitly selected "Dependable CLI/MCP release first (recommended)".
Do not describe this work as completion of the entire architecture.

Baseline: all 78 workspace tests, `cargo fmt --all -- --check`, and
`cargo clippy --workspace --all-targets -- -D warnings` pass. This directory has no
Git metadata. Source backup:
`/var/folders/8y/57y4fxcd5f37z8tvbt447j1h0000gn/T/ledger-before-hardening-tv_7kok0/source`.
Three implementation agents will use the shared directory with exclusive ownership;
no Git initialization, worktrees, publication or deployments are required.

## Decisions from the alternating review

1. Fix indexing races, discovery/freshness, snapshot publication, live-symbol identity,
   expansion retries/continuations/budgets, and daemon ownership before feature growth.
2. Delta compares against an immutable baseline of complete content ACTUALLY rendered
   in the original context response. Enhance blueprints, truncated bodies and bodies
   omitted by the formatter do not count as delivered. Expansion never updates it.
3. A shutdown timeout must NEVER release the owner lock while background jobs can
   still write. Stop admission and refresh scheduling, drain requests, close indexing
   admission, wait for all indexing jobs, checkpoint, unlink the owned socket, then
   release the lock. Queued jobs count as jobs, not only the current index pass.
4. Production clients never fall back to an unlocked EmbeddedClient. Retain explicit
   in-process clients for isolated tests/benchmarks. `init` must use the daemon too.
5. Periodic bounded reconciliation is acceptable for automatic freshness. Cache parsed
   files by path/content hash, so unchanged content is not reparsed. Call it polling,
   not an event watcher. Polling must not reset the idle deadline. Include new, deleted,
   renamed, ignored and nested-repository files and VCS changes.
6. Use one atomic published graph/VCS snapshot plus a short publication read/write gate
   around database commit and snapshot swap / query evidence reads. Build expensive
   graph structures outside the gate. Never disguise database errors as empty matches.
7. Resolve dirty symbols by logical identity or an unambiguous match of file, kind,
   scope and name. Do not fall back to name alone or stale line spans. Deleted/ambiguous
   symbols and changed reference files get explicit coverage/status treatment.
8. Source and history policy must exclude nested repositories consistently while still
   allowing deleted historical paths. Git object reads do not dereference working-tree
   symlinks; do not blindly canonicalize historical paths and break deleted-file history.
9. Align security documentation with implemented filename/path filtering. Broad secret
   value detection/redaction is deferred; do not silently rewrite edit-ready code or
   claim arbitrary secrets cannot appear in allowed source files. Fix boundary bugs.
10. Per-query SQLite connections currently close on Drop; this is overhead, not an FD
    leak. Connection pooling is deferred unless required for correctness. No unrelated
    optimizer or prompt-telemetry overhaul is required for this release.
11. Only advertise implemented expansion IDs. Defer a new contract-ID grammar; document
    existing contract evidence expansion through advertised body/slice/callers IDs.
12. Upgrade negotiation is safe only after verifying workspace identity and an advertised
    shutdown capability. Legacy daemons without it produce actionable mismatch errors,
    never forced termination or an unlocked migration. Cache a binary fingerprint once
    per process; bind connected clients to the observed database incarnation and validate
    request/response identity so replacement cannot be silently mistaken for the old owner.

## Exactly three implementation streams

### A — Evidence contracts, formatting, scope and store metadata

Exclusive ownership: `crates/ledger-core/**`, `crates/ledger-store/**`,
`crates/ledger-search/**`, `crates/ledger-vcs/**`, and their tests/manifests.

- Add nested Git/Jujutsu repository boundary checks for live traversal, direct expansion
  and historical lexical paths without requiring deleted historical files to exist.
- Make context/enhance formatting honor the complete rendered approximate-token budget,
  including headers, tests, notes and long metadata. Preserve useful expansion paths and
  explicit omission markers. No truncation may falsely present a partial function as full.
- Add `PackFormatter::render_context_pack_with_delivery(&ContextPack)` returning
  `(String, HashMap<String, ContentHash>)`; keep `render_context_pack` as its text wrapper.
  The map contains only full bodies actually rendered. Enhance returns an empty map.
- Extend immutable handle metadata with bounded symbol descriptors for cold/old-handle
  expansion. Concrete contract: `symbols: HashMap<LogicalSymbolKey, SymbolRecord>`;
  add `HandleRegistry::insert_with_symbols(handle, generation, delivered_digests, symbols)`.
  Keep the existing `insert` wrapper for compatibility. Remove mutable delivery tracking
  once engine call sites are migrated; coordinate through the parent if necessary.
- Add `SqliteStore::checkpoint(&self) -> Result<(), StoreError>` for orderly shutdown.
- Tests: budget ceilings in normal/enhance modes, actual-rendered baseline accounting,
  nested roots including `.git` files, safe deleted history, scope enforcement on every
  read path, immutable handle metadata. Preserve all current VCS/store tests.

### B — Indexing, coherent queries and expansion

Exclusive ownership: `crates/ledger-daemon/src/engine.rs`, new engine helper modules,
`crates/ledger-daemon/tests/engine_*.rs`. Do not edit IPC or shared manifests unannounced.

- Coordinate all explicit and background index passes. Explicit requests wait for a pass
  covering their request, including a follow-up if the in-flight scan started earlier.
  No double cold-start publication, stale early success, or lost pending refresh. Ensure
  failed/panicking workers cannot permanently wedge the coordinator.
- Cache extraction/manifest data by path/content hash; handle additions/deletions/renames
  deterministically. Retain existing polling-independent explicit index behavior.
- Publish graph/VCS together; short publication gate keeps pinned FTS/history reads
  coherent. On failures propagate errors or explicitly use live degraded retrieval with
  coverage notes; never silently claim a ready/complete indexed search.
- Check dirty target and emitted reference evidence; never emit stale snippets with
  undowngraded confidence. Resolve duplicate names by scope/kind/logical identity and
  handle deleted symbols honestly in context and expansion.
- Register immutable handles using stream A's rendered-delivery report and symbol map.
  Cold long functions must advertise working expansion IDs before the graph is ready.
- Enforce expansion budgets against the final rendered response, including the first
  item and metadata. Paginate bodies/slices/callers/diffs with deterministic continuations
  that make progress, work on retries, and detect changed content. Large single lines and
  Unicode must terminate safely. If request metadata alone cannot fit, return a concise
  actionable error rather than exceeding the budget or silently dropping requested items.
- Repeated delta calls against unchanged content return the same content/status. Unseen,
  truncated or formatter-omitted bodies must remain retrievable. No mutable record_delivery.
- Required lifecycle API for stream C:
  `pub fn shutdown_indexing(&self) -> Result<(), EngineError>` closes new-job admission,
  drains ALL queued/running jobs, then checkpoints using stream A's store method. It is
  idempotent and does not claim success with active writers. Stream C calls it off-runtime.
  Keep `trigger_background_index(self: &Arc<Self>)` nonblocking and no-op after closure.
- Tests: concurrent cold/explicit indexing, a follow-up scan after mid-pass edits,
  unchanged parse reuse, generation consistency under publication, duplicate/deleted
  symbols, changed references, cold expansion, repeated delta, oversized first item,
  continued slice/body/diff delivery, Unicode, and shutdown with queued work.

### C — Daemon lifecycle, MCP/CLI delivery and documentation

Exclusive ownership: `crates/ledger-daemon/src/ipc.rs`, `crates/ledger-daemon/src/lib.rs`,
`crates/ledger-daemon/tests/daemon_ipc.rs`, new `ipc_*.rs` tests, `crates/ledger-mcp/**`,
`crates/ledger-cli/**`, `README.md`, `ARCHITECTURE.md`, `.github/**`.

- Remove production EmbeddedClient fallback and route init through locked daemon ownership.
- Add periodic reconciliation via `trigger_background_index` with fixed idle deadlines.
  Verify new-file and non-target edits are discovered without querying that file first.
- Bound IPC request/response and MCP frame reads; apply IPC partial-frame/idle timeouts
  and write limits. MCP stdio may wait idle indefinitely but must bound frame allocation.
- Add binary-build/workspace/database identity checks and safe negotiated shutdown/restart.
  Prevent stale clients from using handles against a replacement database. Legacy or
  unrelated daemons must fail safely with actionable diagnostics. Preserve the owner
  lock through request drain, `shutdown_indexing`, checkpoint and socket cleanup.
- Keep normal `query` responsive and add an explicit `--fresh` synchronization option.
  Add a user-visible stop/restart path if needed to make lifecycle errors actionable.
- Add README installation, generic stdio MCP configuration, CLI examples, security limits,
  troubleshooting and benchmark reproduction. Describe Unix platform support honestly.
  Update architecture with implemented/deferred status, actual polling, budgets and delta
  semantics; remove unsupported hard memory/latency and secret-scanning guarantees.
- Add CI for formatting, warning-free Clippy and tests on supported Unix runners; add a
  reproducible binary build/release workflow if practical without publishing anything.
- Tests: locked-owner/mismatch never opens a second writer, upgrade negotiation, database
  replacement rejection, oversized/partial frames, refresh does not defeat idle shutdown,
  shutdown drains engine, and actual CLI/MCP process smoke tests in temporary workspaces.

## Integration and acceptance

Astra owns this plan, root workspace manifest/lockfile coordination and final integration.
Agents must not touch each other's files or run workspace-wide formatting while peers edit.
Use existing dependencies where practical; request root dependency changes via the parent.
No subdelegation. No commits/PRs. Each agent reports files, tests, limitations and interfaces.

Final gates: all original and new meaningful tests pass, formatting passes, Clippy has no
warnings; process-level init/query/expand/MCP/stop works in a temporary workspace; an edit
to a previously unselected file and a new file become discoverable automatically; repeated
expansion is retry-safe and bounded; daemon ownership survives shutdown/restart tests.
Benchmark improvements must be measured and labeled as fixture/local measurements, not
general performance guarantees. Documentation must reflect the implementation that exists.

## Completion record

- All three implementation streams completed. No dependency or root manifest/lockfile
  changes were required. No repository initialization, commits, PRs or publication occurred.
- Parent integration updated the older historical-diff test to immutable delta semantics.
  Parent review also reproduced and fixed explicit indexing waiting for unrelated later
  refresh passes, and added lifetime-specific handle namespaces to prevent old cold
  handles from aliasing new responses after restart. Both have regression coverage.
- Final verification passed: **141 workspace tests**, workspace formatting, Clippy across
  all targets with warnings denied, and an optimized release build.
- The inherited macOS `SDKROOT` selected the Command Line Tools 27.2 SDK while the active
  Xcode linker used its 26.5 toolchain. Final verification selected the matching installed
  Xcode SDK per command; no system toolchain configuration was changed. README documents
  the temporary override for this local mismatch.
- Release-process acceptance passed against a temporary copy of the current codebase
  (20,606 Rust source lines at the time of verification, 64 indexed files): init,
  scoped context retrieval, repeated delta equality, MCP protocol and token ceiling,
  polling discovery of new/unselected files without a triggering query, restart,
  stale-handle rejection and owner shutdown. The temporary workspace was removed.
- Final logs: `/tmp/ledger-a37059c0-final-{format,clippy,tests,release}.log`.
  Acceptance details: `/tmp/ledger-a37059c0-release-smoke.json`.
- GitHub workflows are configured and locally inspected; remote CI was not run. The
  workspace declares Rust 1.89 but verification used Rust 1.97.1, as the CI configuration
  does. MCP proxies remain bound to one owner lifetime and must be restarted after owner
  exit/replacement. Historical diffs are materialized again per expansion page; output
  budgets are enforced, without a hard total-memory guarantee.
- Dashboard, annotations, embeddings and shipping tools remain deferred as agreed.
