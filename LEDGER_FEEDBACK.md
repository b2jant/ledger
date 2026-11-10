# LEDGER usage feedback

This log records real upgrade tasks, including unsuccessful retrieval and necessary
native exploration. It is qualitative evidence, not a benchmark or proof of completeness.
Follow the standing workflow in [AGENTS.md](AGENTS.md).

## 2026-11-09 — Establish the standing workflow

**Task:** make future upgrades use LEDGER first and record where its retrieval helps
or misses relevant context. This was an instruction/documentation change.

**Access and conditions:** connected LEDGER MCP tools were unavailable; used the local
`target/release/ledger` CLI. The broad initial query used `--mode enhance --fresh`.
Subsequent responses included both a warming snapshot and fresh ready snapshots;
these are reported separately rather than treated as identical conditions.

**Queries and observed results:**

1. Initial task: `Make future LEDGER upgrades use repository evidence first and record
   where retrieval is useful or misses relevant files and tests`, without anchors.
   On a ready snapshot, all four edit targets were unrelated to the intended workflow
   change: an unborn-repository test, a daemon-upgrade test, `first_line_signature`,
   and a future-timestamp test. Existing workflow instructions and their installer
   were not returned as edit targets. Coverage notes did not identify this relevance gap.
2. Refined task: `Update ledger-enhance workflow instructions and record retrieval
   successes and misses during future upgrades`, anchored to `install_harness_skills`.
   Without `--fresh`, a warming response returned the caller `cmd_init` through a
   literal call-site match, rather than the anchored definition. It correctly disclosed
   incomplete cross-file coverage.
3. The same refined task/anchor with `--fresh` found `install_harness_skills`, but ranked
   it fourth behind formatter/telemetry symbols. Expanding the first-ranked formatter
   returned valid source, although that source was not what the workflow change needed.
4. A narrow query and anchor both set to `install_harness_skills`, with `--fresh`,
   ranked the installer first. Its advertised body expansion returned the complete
   function and exposed how optional instructions are generated. This was useful for
   understanding how those instructions could conflict with independent verification.

**Native verification and fallback:** inspected the existing skill and command files,
the CLI installer and repository documentation. Symbol knowledge from native source
inspection was needed to refine the query; the successful narrow result was not first-attempt task
understanding. The installed helper prompts restrict native searches, so the standing
repository instructions explicitly require independent verification and allow
documented fallback when evidence is insufficient.

**Confirmed verification-candidate miss:** the narrow query suggested
`cargo test -p ledger-cli --test process_smoke identity`. The source identifies
`identity` as a helper method, not a test. Running
`cargo test --locked -p ledger-cli --test process_smoke identity -- --list`
completed successfully but reported **0 tests, 0 benchmarks**. The suggested filter
therefore selects no actual tests. No test execution or validation is claimed for it.

**Actual change:** added `AGENTS.md` and this log, and linked the workflow from README.
The new files did not exist when queries ran; their absence was expected. No runtime
retrieval code or installer-generated skill was changed. Documentation links and the
standing instruction were checked; the full software test suite is not needed for
this documentation-only change.

**Follow-up candidates:** task-level relevance for instruction/documentation work;
weight of explicit symbol anchors relative to broad FTS matches; cold symbol-definition
selection; and generated test commands that select helper names instead of real tests.
These are observations from one task, not general quality scores. No retrieval fix or
before/after improvement is claimed in this entry.

## 2026-11-09 — Publish with Jujutsu and require it for future work

**Task and initial query (no separate anchors):** `Publish the existing LEDGER code
to https://github.com/b2jant/ledger.git using Jujutsu (jj), and require all future
branch work in this folder to use jj; inspect AGENTS.md, README.md and .gitignore
for setup and workflow instructions`.

**Access/freshness:** connected LEDGER MCP tools were unavailable. Used the existing
`target/release/ledger` CLI with `--mode enhance --fresh`; the binary was newer than
the Rust sources. Response: ready generation `g7`, handle
`snap:076e18eb:g7:7d89a81b`.

**Results and fallback:** all edit targets were unrelated daemon connection or
parser/GraphQL extraction symbols; references were daemon callers. The named
workflow files were not returned, and coverage notes did not flag the relevance
gap. No returned source body was needed. Direct reads and targeted `rg` confirmed
there was no existing contributor Jujutsu workflow. Repository/remote inspection
found no local VCS history and an empty GitHub repository. The suggested
`process_smoke identity` and `process_smoke socket` filters name helper methods,
not tests (verified in source); neither was used as validation. Official Jujutsu
documentation and installed CLI help supplied the setup/push syntax.

**Changes/checks:** installed `jj 0.47.1`, initialized a colocated repository and
configured its identity and `origin`. Updated `AGENTS.md`, `README.md`, `.gitignore`
and this log; application source was unchanged. Reviewed all 66 files selected for
the initial import and verified VCS/build/database/runtime exclusions. A limited
credential-pattern scan found only a dummy private-key header in a test fixture.
`cargo fmt --all -- --check` passed. `cargo test --locked --workspace`, with SDKROOT
set to the active Xcode SDK as documented in README, passed all 141 tests. The
standing instruction and README push commands were checked against the installed
CLI. No retrieval behavior changed, so no before/after retrieval claim is made.

## 2026-11-09 — Assess retrieval and production readiness with Gemini

**Task/initial query (verbatim, no anchors):** `Assess how useful LEDGER has been
at selecting the right commits and pull requests and understanding the semantics
of user tasks; identify five evidence-based improvements to retrieval, intent
understanding, production reliability and distribution including Homebrew, and
prepare a separate production-readiness roadmap PR.`

**Access/freshness:** connected MCP tools were unavailable. Used the existing
`target/debug/ledger` CLI, newer than Rust sources, in the original checkout with
`--mode enhance --fresh`. Ready `g12`, handle `snap:076e18eb:g12:7e103907`, effective
1200-token ceiling (default maximum 2800). Targets were a phase2 fixture test, a
Git framing test, a GraphQL extractor and a history-noise test. The history test
and benchmark references were useful leads, but ranking/distribution targets were
missing and coverage notes did not identify the relevance gap. Expanding the
advertised benchmark body failed as expired/wrong-workspace; cause was not proved.

**Refinement:** `Assess query_warm_snapshot task understanding, extract_query_terms,
is_noise_commit history selection, and run_index_pass scaling for production
readiness`, anchored to the known `query_warm_snapshot`, `is_noise_commit` and
`run_index_pass`, same mode/freshness/budget. Ready `g13`, handle
`snap:076e18eb:g13:28921aa3`; `query_warm_snapshot` ranked third behind propagated
helpers. Its advertised body expansion succeeded with a continuation. Native
`rg`/reads verified symbol-only FTS population, path-anchor gaps, first-twelve-term
selection, history gating/noise filters, helper-derived test commands, full file
reads despite parse caching, subprocess limits and release/packaging gaps.

**Gemini round one:** Gemini 4 Argon with `thinking_level=high`, read-only, used
fresh enhance CLI query `Assess production-readiness gaps in LEDGER retrieval,
commit/PR history provenance, evaluation/test discovery, daemon handle lifetimes,
and release/Homebrew distribution across query_warm_snapshot, extract_query_terms,
symbols_in_file, is_noise_commit, and run_index_pass`, anchored to those five
known symbols. Ready `g13`, handle `snap:076e18eb:g13:4edc3a71`; three anchored
symbols returned, but `content_hash` ranked first. Engine expansion succeeded;
docs/workflows were absent. Reviewer reported zero tests for suggested `buf` and
`extract` filters. Parent independently confirmed `parser_and_contracts buf`
selects **0 tests, 0 benchmarks**; it is a helper and is not validation.

**Checks/limits:** `cargo test --locked -p ledger-store --test history_fts_tests
-p ledger-daemon --test phase2_vcs_daemon` passed **12 tests**. Initial test and
discovery attempts failed linking against the newer CommandLineTools SDK;
retrying with `SDKROOT=/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX26.5.sdk`
passed. Same unchanged Rust sources as main; no new runtime test was added.
GitHub initially returned no PRs/releases; actual PR-selection effectiveness is
unmeasured. Official Homebrew, Cargo and GitHub docs verified distribution/API
constraints. Source confirmed missing license texts and registry dependency
metadata. Parent rejected unsupported handle-cause certainty, predicted relevance
gains, a false claim that Rust 1.97.1 was unavailable (installed locally), and the
claim that bare Cargo substring filters necessarily fail.

**Changes:** independent JJ workspace from main preserves existing VCS-plan and
runner changes. Added `PRODUCTION_READINESS.md`, linked it from README and appended
this entry. No retrieval/indexing/output behavior changed, so no before/after
improvement is claimed. Local Markdown links and whitespace passed validation;
exactly five improvements each have a first deliverable and acceptance set.

**Gemini round two:** same requested model/high setting; explicit consensus on
all five areas, corrections and sequencing, with no blocking findings. Fresh
enhance query `Review production readiness roadmap and 5 improvements: FTS lexical
search and path/doc indexing, graph hub ranking and git history filtering,
candidate test filter generation, incremental watcher and fresh snapshot handle
validation, and release packaging`, anchors `query_context`, `run_index_pass`,
`WorkspaceEngine`. Ready `g13`, handle `snap:076e18eb:g13:ec518e54`; FTS methods and
engine references were useful, `run_git` was noise, and resolver/runner/packaging
targets were missed. Expansion succeeded with continuation. Reviewer read the
separate-workspace draft through JJ's shared revision store after direct path
access was unavailable; the roadmap's absence from the original queried workspace
was expected. No reviewer modified files.
