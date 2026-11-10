//! Phase 2 multi-commit time-travel ablation benchmark (`B0` vs `L1` vs `L2` vs `L3`).

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use ledger_core::{
    ApproxTokens, CoChangePeer, DefaultScopeGuard, RelationKind, SingleBufferReader, SymbolRecord,
    TestCandidate, TestProvenance, TypedEdge, VcsRevision,
};
// The file-extractor registry is exported as `ParserRegistry` in this workspace.
use ledger_parser::{ParserRegistry, ResolvedWorkspaceGraph, ScopedResolver, detect_manifest};
use ledger_search::{LexicalSearcher, RipgrepSearcher, SearchBudget};
use ledger_vcs::{
    BipartiteCoChangeIndex, CoChangeConfig, GitCliBackend, SystemCommandRunner, VcsBackend,
    VcsError, derive_cochange_test_files,
};
use tempfile::TempDir;

const INVOICE: &str = "src/billing/invoice_engine.rs";
const SHADOW: &str = "src/billing/shadow_journal_rules.rs";
const SHADOW_TEST: &str = "tests/shadow_reconcile_integration.rs";
const SCHEMA: &str = "schema/billing.graphql";
const VOID_HANDLER: &str = "src/billing/void_handler.rs";
const CLIENT: &str = "web/src/billingClient.ts";
const VOID_TEST: &str = "tests/void_invoice_contract.test.ts";
const SECRET_MARKER: &str = "sk_live_NEVER_LEAK_777";
const COMMIT_TIMES: [i64; 7] = [
    1_760_000_000,
    1_760_300_000,
    1_760_600_000,
    1_760_900_000,
    1_761_200_000,
    1_761_600_000,
    1_762_000_000,
];
const REFERENCE_NOW_SECS: i64 = 1_762_000_000;
const MAX_PEERS: usize = 256;

#[derive(Debug)]
struct GitFixture {
    directory: TempDir,
    revision_ids: Vec<String>,
}

#[derive(Debug)]
struct TaskCase {
    name: &'static str,
    task: &'static str,
    anchors: &'static [&'static str],
    required_sources: &'static [&'static str],
    required_tests: &'static [&'static str],
}

const TASKS: [TaskCase; 2] = [
    TaskCase {
        name: "P2-A",
        task: "Update compute_tax_tier_cents (ERR_TAX_TIER_DRIFT) tax tier rounding without breaking shadow journal audit reconciliation",
        anchors: &["compute_tax_tier_cents", "ERR_TAX_TIER_DRIFT"],
        required_sources: &[INVOICE, SHADOW],
        required_tests: &[SHADOW_TEST],
    },
    TaskCase {
        name: "P2-B",
        task: "Change voidInvoice GraphQL mutation and billing:invoice_voided event payload",
        anchors: &["voidInvoice", "billing:invoice_voided"],
        required_sources: &[SCHEMA, VOID_HANDLER, CLIENT],
        required_tests: &[VOID_TEST],
    },
];

#[derive(Debug, Clone, Copy)]
enum Tier {
    B0,
    L1,
    L2,
    L3,
}

impl Tier {
    const fn label(self) -> &'static str {
        match self {
            Self::B0 => "B0: rg + read",
            Self::L1 => "L1: Phase 1 graph",
            Self::L2 => "L2: Event/GraphQL",
            Self::L3 => "L3: VCS history",
        }
    }

    const fn includes(self, relation: RelationKind) -> bool {
        match relation {
            RelationKind::Calls
            | RelationKind::Imports
            | RelationKind::ImplementsOrRefsType
            | RelationKind::HttpEndpoint
            | RelationKind::SqlTableAccess
            | RelationKind::ConfigEnvKey => !matches!(self, Self::B0),
            RelationKind::EventChannel | RelationKind::GraphqlSchemaOp => {
                matches!(self, Self::L2 | Self::L3)
            }
            RelationKind::Tests => false,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Evidence {
    files: BTreeSet<String>,
    edges: Vec<TypedEdge>,
    test_candidates: Vec<TestCandidate>,
    historical_tests: BTreeSet<String>,
    peers: Vec<CoChangePeer>,
    search_incomplete: bool,
}

#[derive(Debug)]
struct HistoricalDiff {
    revision_id: String,
    rel_path: String,
    text: String,
}

#[derive(Debug, Default)]
struct HistoryAudit {
    diffs: Vec<HistoricalDiff>,
    secret_diff_denials: usize,
}

/// Runs the Phase 2 multi-commit ablation benchmark and returns a formatted Markdown report.
pub(crate) fn run_phase2_ablation_suite() -> anyhow::Result<String> {
    let fixture = build_git_fixture()?;
    let root = fixture.directory.path();
    let guard = DefaultScopeGuard::new(root)?;
    let searcher = RipgrepSearcher::new(guard.clone());
    let graph = resolve_fixture(&searcher)?;
    let backend = GitCliBackend::new(Arc::new(SystemCommandRunner));
    let revisions = backend.recent_revisions(root, &guard, 250)?;
    // A seven-commit fixture cannot establish production-strength Lift. Keep the default
    // alpha and half-life, but expose all one-commit peers so neither Lift nor the default
    // two-commit minimum can conceal a failed sweep filter.
    let config = CoChangeConfig {
        min_shared_commits: 1,
        min_lift: 0.0,
        ..CoChangeConfig::default()
    };
    let cochange = BipartiteCoChangeIndex::build(&revisions, config, REFERENCE_NOW_SECS);
    let symbols_by_file = symbols_by_file(&graph);
    let audit = audit_history(&fixture, &backend, &guard, &revisions)?;
    let mut all_peers = cochange.query_peers(&[INVOICE.to_string()], &symbols_by_file, MAX_PEERS);

    let mut report = String::from("### LEDGER Phase 2: real Git time-travel ablation\n\n");
    writeln!(
        report,
        "Seven commits use fixed author/committer timestamps from `{}` to `{}`; co-change reference time is `{REFERENCE_NOW_SECS}`. No wall-clock time enters the scores.\n",
        COMMIT_TIMES[0], COMMIT_TIMES[6]
    )?;
    for task in &TASKS {
        writeln!(report, "- **{}:** {}", task.name, task.task)?;
    }
    report.push_str(
        "\nAll tiers share the same literal anchor searches and first four unique raw-file reads. \
         L1 adds one-hop Phase 1 relations (Calls, Imports, TypeRef, HTTP, SQL, Env) and supported test candidates; \
         L2 additionally admits EventChannel/GraphqlSchemaOp and their ContractRouteRef tests; \
         L3 adds real revisions, one-hop co-change peers, historical diffs, and derive_cochange_test_files.\n\n\
         Source recall covers the engine + hidden shadow peer for P2-A, and schema + handler + client for P2-B. \
         Test-file recall measures discovery separately from provenance-backed test selection. \
         P2-B deliberately allows lexical search to find all contracts; names alone can achieve full file recall.\n\n\
         | Task | Tier | Source recall | Test-file recall | P1 / P2 edges | Contract tests | History tests | Co-change peers | Approx tokens |\n\
         | :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n",
    );
    let mut incomplete_searches = 0;
    for task in &TASKS {
        let baseline = lexical_evidence(&searcher, task)?;
        incomplete_searches += usize::from(baseline.search_incomplete);
        for tier in [Tier::B0, Tier::L1, Tier::L2, Tier::L3] {
            let mut evidence = baseline.clone();
            if !matches!(tier, Tier::B0) {
                add_graph_evidence(&mut evidence, &graph, tier);
            }
            if matches!(tier, Tier::L3) {
                add_history_evidence(&mut evidence, &cochange, &symbols_by_file);
                all_peers.extend_from_slice(&evidence.peers);
            }
            let rendered = render_evidence(root, &guard, &evidence, &revisions, &audit, tier)?;
            let phase1_edges = evidence
                .edges
                .iter()
                .filter(|edge| Tier::L1.includes(edge.relation))
                .count();
            let contract_tests = evidence
                .test_candidates
                .iter()
                .filter(|test| test.provenance == TestProvenance::ContractRouteRef)
                .map(|test| test.test_file.as_str())
                .collect::<BTreeSet<_>>()
                .len();
            writeln!(
                report,
                "| {} | {} | {:.0}% | {:.0}% | {} / {} | {} | {} | {} | {} |",
                task.name,
                tier.label(),
                recall(&evidence.files, task.required_sources),
                recall(&evidence.files, task.required_tests),
                phase1_edges,
                evidence.edges.len() - phase1_edges,
                contract_tests,
                evidence.historical_tests.len(),
                evidence.peers.len(),
                ApproxTokens::estimate(&rendered).0,
            )?;
        }
    }

    let sweep_false_peers = all_peers
        .iter()
        .filter(|peer| {
            peer.rel_path.starts_with("src/generated_pad/mod_") && peer.rel_path.ends_with(".rs")
        })
        .map(|peer| peer.rel_path.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    let secret_env_leaked = contains_secret(&serde_json::to_string(&revisions)?)
        || contains_secret(&serde_json::to_string(&all_peers)?)
        || audit.diffs.iter().any(|diff| contains_secret(&diff.text));
    writeln!(
        report,
        "\nSafety and coverage invariants (measured, not substituted):\n\n\
         - `fixture_commits == {}`; `recent_revisions == {}` (expected 7).\n\
         - `revisions_indexed == {}`; `sweeps_filtered == {}` (expected one 56-path sweep).\n\
         - `sweep_false_peers == {sweep_false_peers}` (target 0; queried up to {MAX_PEERS} peers).\n\
         - `secret_env_leaked == {secret_env_leaked}` (target false; checks revision records, peers, and returned diffs).\n\
         - Secret diff requests denied: `{}/7`; successful allowed-path diff requests: `{}`.\n\
         - Incomplete lexical searches: `{incomplete_searches}`.\n\n\
         Co-change config: max_files_per_revision={}, min_shared_commits={}, min_lift={}, \
         smoothing_alpha={}, decay_lambda_per_sec={:.12}. The permissive peer thresholds are specific to this small ablation fixture.\n\n\
         Approximate tokens include raw source plus the actual relation/test evidence and, for L3, \
         relevant revisions and diffs. Index construction is shared setup; no query-latency or task-success claim is made. \
         Missing API outputs lower measured recall/coverage, including while parallel implementations are incomplete.\n",
        fixture.revision_ids.len(),
        revisions.len(),
        cochange.revisions_indexed(),
        cochange.sweeps_filtered(),
        audit.secret_diff_denials,
        audit.diffs.len(),
        config.max_files_per_revision,
        config.min_shared_commits,
        config.min_lift,
        config.smoothing_alpha,
        config.decay_lambda_per_sec,
    )?;
    Ok(report)
}

fn resolve_fixture(
    searcher: &RipgrepSearcher<DefaultScopeGuard>,
) -> anyhow::Result<ResolvedWorkspaceGraph> {
    let parser = ParserRegistry::new();
    let reader = SingleBufferReader::new(searcher.policy().clone());
    let (files, _) = searcher.list_allowed_files();
    let mut extracted = Vec::new();
    let mut manifests = Vec::new();
    for (path, rel_path) in files {
        let buffer = reader.read_file(&path)?;
        if let Some(manifest) = detect_manifest(&rel_path, buffer.as_str()) {
            manifests.push(manifest);
        }
        extracted.push(parser.extract_file(&buffer));
    }
    Ok(ScopedResolver::new().resolve(&extracted, &manifests))
}

fn symbols_by_file(graph: &ResolvedWorkspaceGraph) -> HashMap<String, Vec<SymbolRecord>> {
    let mut by_file: HashMap<String, Vec<SymbolRecord>> = HashMap::new();
    for symbol in &graph.symbols {
        by_file
            .entry(symbol.rel_path.clone())
            .or_default()
            .push(symbol.clone());
    }
    by_file
}

fn lexical_evidence(searcher: &impl LexicalSearcher, task: &TaskCase) -> anyhow::Result<Evidence> {
    let budget = SearchBudget {
        max_matches: 4096,
        max_files: 1000,
        timeout: Duration::from_secs(5),
        ..SearchBudget::default()
    };
    let mut files = BTreeSet::new();
    let mut search_incomplete = false;
    for anchor in task.anchors {
        let result = searcher.search_literal(anchor, &budget)?;
        search_incomplete |= result.search_incomplete;
        files.extend(result.matches.into_iter().map(|hit| hit.rel_path));
    }
    Ok(Evidence {
        files: files.into_iter().take(4).collect(),
        search_incomplete,
        ..Evidence::default()
    })
}

fn add_graph_evidence(evidence: &mut Evidence, graph: &ResolvedWorkspaceGraph, tier: Tier) {
    let anchors = evidence.files.clone();
    for edge in &graph.edges {
        if tier.includes(edge.relation)
            && (anchors.contains(&edge.src_file) || anchors.contains(&edge.dst_file))
        {
            evidence.files.insert(edge.src_file.clone());
            evidence.files.insert(edge.dst_file.clone());
            evidence.edges.push(edge.clone());
        }
    }
    for target in evidence.files.clone() {
        if let Some(candidates) = graph.tests_by_target_file.get(&target) {
            for candidate in candidates {
                let admitted = match candidate.provenance {
                    TestProvenance::DirectSymbolRef | TestProvenance::ModuleOrNamingProximity => {
                        true
                    }
                    TestProvenance::ContractRouteRef => evidence.edges.iter().any(|edge| {
                        edge.src_file == candidate.test_file
                            && edge.dst_file == target
                            && matches!(
                                edge.relation,
                                RelationKind::HttpEndpoint
                                    | RelationKind::EventChannel
                                    | RelationKind::GraphqlSchemaOp
                            )
                    }),
                    TestProvenance::HistoricalCochange => false,
                };
                if admitted && !evidence.test_candidates.contains(candidate) {
                    evidence.files.insert(candidate.test_file.clone());
                    evidence.test_candidates.push(candidate.clone());
                }
            }
        }
    }
    evidence.edges.sort_by(|left, right| {
        (
            &left.src_file,
            &left.dst_file,
            left.relation.to_string(),
            left.evidence_line,
        )
            .cmp(&(
                &right.src_file,
                &right.dst_file,
                right.relation.to_string(),
                right.evidence_line,
            ))
    });
    evidence.test_candidates.sort_by(|left, right| {
        (&left.test_file, &left.command, left.provenance.to_string()).cmp(&(
            &right.test_file,
            &right.command,
            right.provenance.to_string(),
        ))
    });
}

fn add_history_evidence(
    evidence: &mut Evidence,
    cochange: &BipartiteCoChangeIndex,
    symbols: &HashMap<String, Vec<SymbolRecord>>,
) {
    let anchors: Vec<String> = evidence.files.iter().cloned().collect();
    evidence.peers = cochange.query_peers(&anchors, symbols, MAX_PEERS);
    for peer in &evidence.peers {
        evidence.files.insert(peer.rel_path.clone());
    }
    for test in derive_cochange_test_files(&evidence.peers) {
        evidence.historical_tests.insert(test.rel_path.clone());
        evidence.files.insert(test.rel_path.clone());
    }
}

fn render_evidence(
    root: &Path,
    guard: &DefaultScopeGuard,
    evidence: &Evidence,
    revisions: &[VcsRevision],
    audit: &HistoryAudit,
    tier: Tier,
) -> anyhow::Result<String> {
    let mut rendered = String::new();
    for rel_path in &evidence.files {
        // These are deliberately raw reads for the common rg+read baseline, after the
        // same scope policy used by parsing and history has approved the path.
        if let ledger_core::ScopeDecision::Allowed { canonical_path, .. } =
            ledger_core::ScopePolicy::evaluate_path(guard, &root.join(rel_path))
        {
            writeln!(
                rendered,
                "--- {rel_path} ---\n{}",
                fs::read_to_string(canonical_path)?
            )?;
        }
    }
    rendered.push_str(&serde_json::to_string(&evidence.edges)?);
    rendered.push_str(&serde_json::to_string(&evidence.test_candidates)?);
    if matches!(tier, Tier::L3) {
        rendered.push_str(&serde_json::to_string(&evidence.peers)?);
        rendered.push_str(&serde_json::to_string(&evidence.historical_tests)?);
        for revision in revisions {
            if revision
                .touched_files
                .iter()
                .any(|touch| evidence.files.contains(&touch.rel_path))
            {
                rendered.push_str(&serde_json::to_string(revision)?);
            }
        }
        for diff in &audit.diffs {
            if evidence.files.contains(&diff.rel_path) {
                writeln!(
                    rendered,
                    "diff:{}:{}\n{}",
                    diff.revision_id, diff.rel_path, diff.text
                )?;
            }
        }
    }
    Ok(rendered)
}

fn recall(found: &BTreeSet<String>, required: &[&str]) -> f64 {
    let hits = required
        .iter()
        .filter(|path| found.contains(**path))
        .count();
    100.0 * hits as f64 / required.len().max(1) as f64
}

fn contains_secret(text: &str) -> bool {
    text.contains(".env") || text.contains(SECRET_MARKER)
}

fn audit_history(
    fixture: &GitFixture,
    backend: &GitCliBackend,
    guard: &DefaultScopeGuard,
    revisions: &[VcsRevision],
) -> anyhow::Result<HistoryAudit> {
    let mut requests = BTreeSet::new();
    for revision in revisions {
        for touch in &revision.touched_files {
            requests.insert((revision.rev_id.clone(), touch.rel_path.clone()));
        }
    }
    // Probe known real revisions even if a backend is temporarily returning no records.
    for revision_id in &fixture.revision_ids {
        requests.insert((revision_id.clone(), INVOICE.to_string()));
        requests.insert((revision_id.clone(), ".env".to_string()));
    }
    let mut audit = HistoryAudit::default();
    for (revision_id, rel_path) in requests {
        match backend.revision_file_diff(
            fixture.directory.path(),
            guard,
            &revision_id,
            &rel_path,
            120,
        ) {
            Ok(text) => audit.diffs.push(HistoricalDiff {
                revision_id,
                rel_path,
                text,
            }),
            Err(VcsError::DeniedByPolicy(_)) if rel_path == ".env" => {
                audit.secret_diff_denials += 1;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(audit)
}

fn build_git_fixture() -> anyhow::Result<GitFixture> {
    let directory = tempfile::Builder::new()
        .prefix("ledger-phase2-")
        .tempdir()?;
    let root = directory.path();
    git(
        root,
        &[
            "init",
            "--initial-branch=main",
            "--object-format=sha1",
            "--template=",
        ],
        None,
    )?;
    for (key, value) in [
        ("user.name", "LEDGER Phase 2 Fixture"),
        ("user.email", "phase2@example.invalid"),
        ("commit.gpgsign", "false"),
        ("core.autocrlf", "false"),
        ("core.excludesFile", "/dev/null"),
    ] {
        git(root, &["config", "--local", key, value], None)?;
    }
    write_fixture_file(
        root,
        "Cargo.toml",
        "[package]\nname = \"billing-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    write_fixture_file(
        root,
        "package.json",
        "{\"name\":\"billing-contract-fixture\",\"scripts\":{\"test\":\"vitest run\"}}\n",
    )?;
    write_fixture_file(
        root,
        "src/lib.rs",
        "mod billing {\n    pub(crate) mod invoice_engine;\n    pub(crate) mod shadow_journal_rules;\n}\n",
    )?;
    let mut revision_ids = Vec::with_capacity(COMMIT_TIMES.len());
    for (index, timestamp) in COMMIT_TIMES.iter().take(4).copied().enumerate() {
        let version = index + 1;
        write_fixture_file(root, INVOICE, &invoice_source(version))?;
        write_fixture_file(
            root,
            SHADOW,
            &format!(
                "pub(crate) const SHADOW_ROUNDING_SCHEMA_VERSION: u32 = {version};\n\n\
             pub(crate) fn reconcile_shadow_entry(audit_payload: &str) -> bool {{\n\
             \x20   let version = format!(\"\\\"rounding_version\\\":{{SHADOW_ROUNDING_SCHEMA_VERSION}}\");\n\
             \x20   audit_payload.contains(&version) && audit_payload.contains(\"\\\"tax_cents\\\":\")\n\
             }}\n"
            ),
        )?;
        // The test consumes an independently recorded serialized payload, with no
        // direct symbol references to either side of the implicit invariant.
        write_fixture_file(
            root,
            SHADOW_TEST,
            &format!(
                "#[test]\nfn serialized_audit_fixture_roundtrips() {{\n\
             \x20   let audit_payload = r#\"{{\"rounding_version\":{version},\"tax_cents\":12}}\"#;\n\
             \x20   assert!(audit_payload.contains(\"\\\"rounding_version\\\":{version}\"));\n\
             }}\n"
            ),
        )?;
        revision_ids.push(commit_fixture(root, timestamp, &format!(
            "fix: sync shadow_journal_rules rounding version whenever invoice_engine tax tiers change (round {version})"
        ))?);
    }
    write_fixture_file(
        root,
        SCHEMA,
        "type Mutation {\n  voidInvoice(invoiceId: ID!): VoidInvoicePayload!\n}\n\ntype VoidInvoicePayload {\n  ok: Boolean!\n}\n",
    )?;
    write_fixture_file(
        root,
        VOID_HANDLER,
        r#"pub(crate) const MUTATION_SDL: &str = r"type Mutation { voidInvoice(invoiceId: ID!): VoidInvoicePayload! }";

pub(crate) struct EventBus;
impl EventBus {
    fn emit(&self, _channel: &str, _payload: &str) {}
}

#[allow(non_snake_case)]
pub(crate) fn voidInvoice(event_bus: &EventBus, invoice_id: &str) -> bool {
    let payload = format!("{{\"invoiceId\":\"{invoice_id}\",\"ok\":true}}");
    event_bus.emit("billing:invoice_voided", &payload);
    true
}
"#,
    )?;
    write_fixture_file(
        root,
        CLIENT,
        r#"import { gql } from "@apollo/client";
declare const eventBus: { on(channel: string, callback: (payload: unknown) => void): void };

export const voidInvoiceMutation = gql`mutation VoidInvoice($invoiceId: ID!) { voidInvoice(invoiceId: $invoiceId) { ok } }`;
export function observeVoidedInvoice(onPayload: (payload: unknown) => void) {
  eventBus.on("billing:invoice_voided", onPayload);
}
"#,
    )?;
    write_fixture_file(
        root,
        VOID_TEST,
        r#"import { gql } from "@apollo/client";
import { test, expect } from "vitest";
declare const eventBus: { on(channel: string, callback: (payload: unknown) => void): void };

test("void invoice contract", () => {
  const operation = gql`mutation VoidInvoice($invoiceId: ID!) { voidInvoice(invoiceId: $invoiceId) { ok } }`;
  eventBus.on("billing:invoice_voided", payload => { expect(payload).toBeDefined(); });
  expect(operation).toBeDefined();
});
"#,
    )?;
    revision_ids.push(commit_fixture(
        root,
        COMMIT_TIMES[4],
        "feat: pair voidInvoice mutation and invoice voided event contracts",
    )?);

    // Generated padding is introduced in the sweep itself. Earlier commits therefore
    // cannot create legitimate invoice/padding co-change evidence and mask this trap.
    for index in 1..=55 {
        write_fixture_file(
            root,
            &format!("src/generated_pad/mod_{index:02}.rs"),
            &format!("pub(crate) fn padding_{index:02}() -> usize {{\n    {index}\n}}\n"),
        )?;
    }
    write_fixture_file(
        root,
        INVOICE,
        &format!(
            "{}\n// Formatting sweep: normalized trailing whitespace.\n",
            invoice_source(4)
        ),
    )?;
    revision_ids.push(commit_fixture(
        root,
        COMMIT_TIMES[5],
        "chore: treewide rustfmt sweep",
    )?);

    write_fixture_file(
        root,
        ".env",
        &format!("STRIPE_LIVE_SECRET={SECRET_MARKER}\n"),
    )?;
    write_fixture_file(
        root,
        INVOICE,
        &format!(
            "{}\n// Preserve serialized audit metadata during deployment.\n",
            invoice_source(4)
        ),
    )?;
    revision_ids.push(commit_fixture(
        root,
        COMMIT_TIMES[6],
        "fix: preserve tax audit metadata with deployment configuration",
    )?);
    Ok(GitFixture {
        directory,
        revision_ids,
    })
}

fn invoice_source(version: usize) -> String {
    format!(
        r#"pub(crate) const ERR_TAX_TIER_DRIFT: &str = "ERR_TAX_TIER_DRIFT";

pub(crate) fn compute_tax_tier_cents(amount_cents: i64) -> Result<(i64, String), &'static str> {{
    if amount_cents < 0 {{
        return Err(ERR_TAX_TIER_DRIFT);
    }}
    let tax_cents = (amount_cents + {version}) / 100;
    let audit_payload = format!("{{{{\"rounding_version\":{version},\"tax_cents\":{{tax_cents}}}}}}");
    Ok((tax_cents, audit_payload))
}}
"#
    )
}

fn write_fixture_file(root: &Path, rel_path: &str, contents: &str) -> anyhow::Result<()> {
    let path = root.join(rel_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents).with_context(|| format!("writing Phase 2 fixture {rel_path}"))
}

fn commit_fixture(root: &Path, timestamp: i64, summary: &str) -> anyhow::Result<String> {
    git(root, &["add", "--all", "--force"], None)?;
    git(
        root,
        &["commit", "--quiet", "--no-gpg-sign", "-m", summary],
        Some(timestamp),
    )?;
    Ok(git(root, &["rev-parse", "HEAD"], None)?.trim().to_string())
}

fn git(root: &Path, args: &[&str], timestamp: Option<i64>) -> anyhow::Result<String> {
    let mut command = Command::new("git");
    command
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .current_dir(root)
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_COUNT", "0")
        .env("GIT_TERMINAL_PROMPT", "0");
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
    ] {
        command.env_remove(variable);
    }
    if let Some(timestamp) = timestamp {
        let date = format!("@{timestamp} +0000");
        command
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date);
    }
    let output = command
        .output()
        .context("running git for Phase 2 fixture")?;
    ensure!(
        output.status.success(),
        "fixture git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).context("fixture git emitted non-UTF-8 output")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase2_bench_fixture_has_seven_reproducible_commits() -> anyhow::Result<()> {
        let fixture = build_git_fixture()?;
        let repeated = build_git_fixture()?;
        let root = fixture.directory.path();
        assert_eq!(fixture.revision_ids, repeated.revision_ids);
        assert_eq!(
            git(root, &["rev-list", "--count", "HEAD"], None)?.trim(),
            "7"
        );
        assert!(git(root, &["status", "--porcelain"], None)?.is_empty());
        let times = git(root, &["log", "--reverse", "--format=%at:%ct"], None)?;
        let expected: Vec<String> = COMMIT_TIMES
            .iter()
            .map(|time| format!("{time}:{time}"))
            .collect();
        assert_eq!(times.lines().collect::<Vec<_>>(), expected);
        for revision in fixture.revision_ids.iter().take(4) {
            let paths = touched_paths(root, revision)?;
            for path in [INVOICE, SHADOW, SHADOW_TEST] {
                assert!(paths.contains(path));
            }
        }
        let contract_paths = touched_paths(root, &fixture.revision_ids[4])?;
        assert_eq!(
            contract_paths,
            [SCHEMA, VOID_HANDLER, CLIENT, VOID_TEST]
                .map(String::from)
                .into()
        );
        let sweep_paths = touched_paths(root, &fixture.revision_ids[5])?;
        assert_eq!(sweep_paths.len(), 56);
        assert!(sweep_paths.contains(INVOICE));
        assert_eq!(
            sweep_paths
                .iter()
                .filter(|path| path.starts_with("src/generated_pad/mod_"))
                .count(),
            55
        );
        let secret_paths = touched_paths(root, &fixture.revision_ids[6])?;
        assert_eq!(secret_paths, [INVOICE, ".env"].map(String::from).into());
        let secret_blob = format!("{}:.env", fixture.revision_ids[6]);
        assert!(git(root, &["show", &secret_blob], None)?.contains(SECRET_MARKER));
        let engine_source = fs::read_to_string(root.join(INVOICE))?;
        let shadow_source = fs::read_to_string(root.join(SHADOW))?;
        assert!(!engine_source.contains("shadow_journal_rules"));
        assert!(!engine_source.contains("reconcile_shadow_entry"));
        assert!(!shadow_source.contains("invoice_engine"));
        assert!(!shadow_source.contains("compute_tax_tier_cents"));
        Ok(())
    }

    #[test]
    fn phase2_bench_suite_reports_safety_invariants() -> anyhow::Result<()> {
        let report = run_phase2_ablation_suite()?;
        assert!(report.contains("fixture_commits == 7"));
        assert!(report.contains("secret_env_leaked == false"));
        assert!(report.contains("sweep_false_peers == 0"));
        assert!(!report.contains(SECRET_MARKER));
        for task in &TASKS {
            for tier in [Tier::B0, Tier::L1, Tier::L2, Tier::L3] {
                assert!(report.contains(&format!("| {} | {} |", task.name, tier.label())));
            }
        }
        println!("{report}");
        Ok(())
    }

    fn touched_paths(root: &Path, revision: &str) -> anyhow::Result<BTreeSet<String>> {
        Ok(git(
            root,
            &[
                "diff-tree",
                "--root",
                "--no-commit-id",
                "--name-only",
                "-r",
                revision,
            ],
            None,
        )?
        .lines()
        .map(String::from)
        .collect())
    }
}
