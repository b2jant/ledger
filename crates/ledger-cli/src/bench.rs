//! Phase 1 reproducible benchmark harness comparing `B0` (raw lexical search + file read
//! baseline) against `L1` (LEDGER literal fast-path + scoped AST + cross-boundary HTTP/SQL/Config
//! contracts + manifest test resolution).

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::Instant;

use ledger_core::tokens::extract_query_terms;
use ledger_core::{
    AnchorInput, ApproxTokens, ContextRequest, DefaultScopeGuard, FocusMode, PackFormatter,
    QueryMode,
};
use ledger_daemon::WorkspaceEngine;
use ledger_search::{LexicalSearcher, RipgrepSearcher, SearchBudget};

/// Benchmark scenario definition with ground-truth edit targets and cross-layer reference peers.
#[derive(Debug)]
pub(crate) struct BenchScenario {
    /// Human-readable task name.
    pub name: &'static str,
    /// User prompt given to the agent.
    pub task: &'static str,
    /// Optional explicit anchors.
    pub anchors: Vec<&'static str>,
    /// Ground-truth files that MUST appear in primary targets.
    pub expected_edit_files: &'static [&'static str],
    /// Ground-truth cross-file callers/contracts/tests needed for a safe, complete edit.
    pub expected_context_peers: &'static [&'static str],
}

/// Measured result row for one `(scenario, arm)` trial.
#[derive(Debug)]
pub(crate) struct TrialResult {
    /// Scenario name.
    pub scenario: &'static str,
    /// Arm (`"B0: rg+read"` or `"L1: LEDGER"`).
    pub arm: &'static str,
    /// Recall over `expected_edit_files` (`0.0..=1.0`).
    pub edit_recall: f32,
    /// Recall over `expected_context_peers` (`0.0..=1.0`).
    pub peer_recall: f32,
    /// Simulated exploration rounds required (`1` if complete on first call, `+1` per missing peer hop).
    pub estimated_rounds: usize,
    /// Approximate tokens returned to the LLM context window.
    pub approx_tokens: u32,
    /// Measured wall-clock query time in microseconds.
    pub latency_us: u64,
    /// True if `.env` secret value leaked into output (must always be `false`).
    pub leaked_secret: bool,
}

/// Runs the Phase 1 paired `B0` vs `L1` benchmark suite on an isolated synthetic polyglot
/// repository and prints a verified Markdown metrics table.
pub(crate) fn run_phase1_benchmark() -> anyhow::Result<Vec<TrialResult>> {
    let tmp = tempfile::tempdir()?;
    populate_polyglot_fixture(tmp.path())?;

    let scenarios = vec![
        BenchScenario {
            name: "1. Literal Error Bug (Cursor)",
            task: "Fix 400 error when filter cursor query parameter is empty string",
            anchors: vec!["ERR_INVALID_FILTER_CURSOR"],
            expected_edit_files: &["src/api/orders/cursor.rs"],
            expected_context_peers: &[
                "src/api/orders/handler.rs",
                "web/src/hooks/useOrdersFilter.ts",
            ],
        },
        BenchScenario {
            name: "2. Cross-Layer Backend Filtering Feature",
            task: "Update backend order filtering predicates to exclude archived orders by default",
            anchors: vec![],
            expected_edit_files: &["src/orders/query.rs"],
            expected_context_peers: &[
                "src/api/orders/handler.rs",
                "migrations/001_create_orders.sql",
                "tests/orders_filter_it.rs",
            ],
        },
        BenchScenario {
            name: "3. Shared Config Limit Refactor",
            task: "Raise default order page limit read from ORDER_PAGE_LIMIT configuration",
            anchors: vec!["ORDER_PAGE_LIMIT"],
            expected_edit_files: &["src/orders/config.rs"],
            expected_context_peers: &[".env.example", "src/orders/query.rs"],
        },
    ];

    let guard = DefaultScopeGuard::new(tmp.path())?;
    let rg = RipgrepSearcher::new(guard);

    let engine = WorkspaceEngine::open(tmp.path())?;
    let _gen = engine.index_workspace()?;

    let mut results = Vec::new();
    for sc in &scenarios {
        results.push(run_b0_baseline(tmp.path(), &rg, sc)?);
        results.push(run_l1_ledger(&engine, sc)?);
    }

    print_results_table(&results);
    Ok(results)
}

fn run_b0_baseline(
    root: &Path,
    rg: &RipgrepSearcher<DefaultScopeGuard>,
    sc: &BenchScenario,
) -> anyhow::Result<TrialResult> {
    let started = Instant::now();
    let budget = SearchBudget::default();

    let report = if let Some(first_anchor) = sc.anchors.first() {
        rg.search_literal(first_anchor, &budget)?
    } else {
        let terms = extract_query_terms(sc.task);
        rg.search_terms(&terms, &budget)?
    };

    // Simulate standard native harness Turn 1: `rg` match list + reading matched unique files (up to 4 files).
    let mut matched_files = Vec::new();
    for m in &report.matches {
        if !matched_files.contains(&m.rel_path) && matched_files.len() < 4 {
            matched_files.push(m.rel_path.clone());
        }
    }

    let mut combined_output = String::new();
    for rel in &matched_files {
        if let Ok(content) = fs::read_to_string(root.join(rel)) {
            combined_output.push_str(&format!("--- {rel} ---\n{content}\n"));
        }
    }
    let latency_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(0);
    let approx_tokens = ApproxTokens::estimate(&combined_output).0;
    let leaked_secret = combined_output.contains("SUPER_SECRET_PROD_PASSWORD");

    let found_set: HashSet<&str> = matched_files.iter().map(String::as_str).collect();
    let edit_hits = sc
        .expected_edit_files
        .iter()
        .filter(|&&f| found_set.contains(f))
        .count();
    let peer_hits = sc
        .expected_context_peers
        .iter()
        .filter(|&&f| found_set.contains(f))
        .count();

    let edit_recall = edit_hits as f32 / (sc.expected_edit_files.len().max(1) as f32);
    let peer_recall = peer_hits as f32 / (sc.expected_context_peers.len().max(1) as f32);
    let missing_items =
        (sc.expected_edit_files.len() - edit_hits) + (sc.expected_context_peers.len() - peer_hits);

    Ok(TrialResult {
        scenario: sc.name,
        arm: "B0: rg + read",
        edit_recall,
        peer_recall,
        estimated_rounds: 1 + missing_items,
        approx_tokens,
        latency_us,
        leaked_secret,
    })
}

fn run_l1_ledger(
    engine: &std::sync::Arc<WorkspaceEngine>,
    sc: &BenchScenario,
) -> anyhow::Result<TrialResult> {
    let started = Instant::now();
    let req = ContextRequest {
        task: sc.task.to_string(),
        anchors: sc
            .anchors
            .iter()
            .map(|s| AnchorInput::Plain((*s).to_string()))
            .collect(),
        mode: QueryMode::Auto,
        focus_paths: Vec::new(),
        focus_mode: FocusMode::Boost,
        max_approx_tokens: 2800,
    };

    let pack = engine.query_context(&req)?;
    let rendered = PackFormatter::render_context_pack(&pack);
    let latency_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(0);
    let approx_tokens = ApproxTokens::estimate(&rendered).0;
    let leaked_secret = rendered.contains("SUPER_SECRET_PROD_PASSWORD");

    let edit_set: HashSet<&str> = pack
        .edit_targets
        .iter()
        .map(|t| t.rel_path.as_str())
        .collect();
    let mut all_surfaced: HashSet<&str> = edit_set.clone();
    for r in &pack.reference_context {
        all_surfaced.insert(r.rel_path.as_str());
    }
    for tc in &pack.test_candidates {
        all_surfaced.insert(tc.test_file.as_str());
    }

    let edit_hits = sc
        .expected_edit_files
        .iter()
        .filter(|&&f| edit_set.contains(f))
        .count();
    let peer_hits = sc
        .expected_context_peers
        .iter()
        .filter(|&&f| all_surfaced.contains(f))
        .count();

    let edit_recall = edit_hits as f32 / (sc.expected_edit_files.len().max(1) as f32);
    let peer_recall = peer_hits as f32 / (sc.expected_context_peers.len().max(1) as f32);
    let missing_items =
        (sc.expected_edit_files.len() - edit_hits) + (sc.expected_context_peers.len() - peer_hits);

    Ok(TrialResult {
        scenario: sc.name,
        arm: "L1: LEDGER",
        edit_recall,
        peer_recall,
        estimated_rounds: 1 + missing_items,
        approx_tokens,
        latency_us,
        leaked_secret,
    })
}

fn print_results_table(results: &[TrialResult]) {
    println!("\n### LEDGER Phase 1 Reproducible Ablation Benchmark (`B0` vs `L1`)\n");
    println!(
        "| Scenario | Arm | Edit Recall | Cross-File Peer Recall | Est. Rounds | Approx Tokens | Latency | Secret Leak |"
    );
    println!("| :--- | :--- | :---: | :---: | :---: | :---: | :---: | :---: |");
    for r in results {
        println!(
            "| {} | {} | {:.0}% | {:.0}% | {} | {} | {:.2} ms | {} |",
            r.scenario,
            r.arm,
            r.edit_recall * 100.0,
            r.peer_recall * 100.0,
            r.estimated_rounds,
            r.approx_tokens,
            (r.latency_us as f64) / 1000.0,
            if r.leaked_secret { "LEAK!" } else { "0 (safe)" }
        );
    }
    println!();
}

fn populate_polyglot_fixture(root: &Path) -> std::io::Result<()> {
    fs::create_dir_all(root.join("src/api/orders"))?;
    fs::create_dir_all(root.join("src/orders"))?;
    fs::create_dir_all(root.join("migrations"))?;
    fs::create_dir_all(root.join("web/src/hooks"))?;
    fs::create_dir_all(root.join("tests"))?;

    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"acme-orders\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;

    // Secret trap (must NEVER be indexed or leaked!) + safe template
    fs::write(
        root.join(".env"),
        "DATABASE_URL=postgres://admin:SUPER_SECRET_PROD_PASSWORD@prod/db\nORDER_PAGE_LIMIT=999\n",
    )?;
    fs::write(
        root.join(".env.example"),
        "DATABASE_URL=postgres://localhost/acme_dev\nORDER_PAGE_LIMIT=50\n",
    )?;

    fs::write(
        root.join("migrations/001_create_orders.sql"),
        "CREATE TABLE orders (\n    id UUID PRIMARY KEY,\n    status TEXT NOT NULL,\n    archived_at TIMESTAMPTZ NULL,\n    deleted_at TIMESTAMPTZ NULL\n);\n",
    )?;

    fs::write(
        root.join("src/orders/config.rs"),
        "use std::env;\n\n/// Reads default pagination limit for order queries.\npub fn default_order_page_limit() -> usize {\n    env::var(\"ORDER_PAGE_LIMIT\")\n        .ok()\n        .and_then(|v| v.parse().ok())\n        .unwrap_or(50)\n}\n",
    )?;

    fs::write(
        root.join("src/orders/query.rs"),
        "use crate::orders::config::default_order_page_limit;\n\n/// Filter parameters for backend order search.\npub struct OrderFilter {\n    pub status: Option<String>,\n    pub include_archived: bool,\n}\n\n/// Applies SQL WHERE predicates for order filtering.\npub fn apply_order_filter_predicates(filter: &OrderFilter) -> String {\n    let limit = default_order_page_limit();\n    let mut sql = String::from(\"SELECT id, status FROM orders WHERE deleted_at IS NULL\");\n    if let Some(st) = &filter.status {\n        sql.push_str(&format!(\" AND status = '{st}'\"));\n    }\n    sql.push_str(&format!(\" LIMIT {limit}\"));\n    sql\n}\n",
    )?;

    fs::write(
        root.join("src/api/orders/cursor.rs"),
        "/// Decodes an optional pagination cursor from query params.\npub fn decode_cursor(raw: Option<&str>) -> Result<Option<String>, &'static str> {\n    let Some(s) = raw else { return Ok(None); };\n    if s.is_empty() {\n        return Err(\"ERR_INVALID_FILTER_CURSOR\");\n    }\n    Ok(Some(s.to_string()))\n}\n",
    )?;

    fs::write(
        root.join("src/api/orders/handler.rs"),
        "use crate::api::orders::cursor::decode_cursor;\nuse crate::orders::query::{OrderFilter, apply_order_filter_predicates};\n\n#[get(\"/api/orders\")]\npub fn list_orders(cursor_param: Option<&str>, filter: &OrderFilter) -> Result<String, &'static str> {\n    let _cursor = decode_cursor(cursor_param)?;\n    Ok(apply_order_filter_predicates(filter))\n}\n",
    )?;

    fs::write(
        root.join("web/src/hooks/useOrdersFilter.ts"),
        "export async function fetchFilteredOrders(status: string, cursor: string) {\n  const res = await fetch(\"/api/orders\");\n  return res.json();\n}\n",
    )?;

    fs::write(
        root.join("tests/orders_filter_it.rs"),
        "use acme_orders::orders::query::{OrderFilter, apply_order_filter_predicates};\n\n#[test]\nfn test_filtering_excludes_deleted_orders() {\n    let f = OrderFilter { status: None, include_archived: false };\n    let sql = apply_order_filter_predicates(&f);\n    assert!(sql.contains(\"deleted_at IS NULL\"));\n}\n",
    )?;

    Ok(())
}
