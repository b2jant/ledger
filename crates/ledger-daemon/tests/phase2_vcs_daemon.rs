//! End-to-end Phase 2 integration tests for `WorkspaceEngine` over real multi-commit Git history:
//! - `ContextPack.vcs_revision` badge (`git:<sha7>`) & O(1) live HEAD drift detection
//! - `ContextPack.cochange_peers` (`BLAST_RADIUS_AND_COCHANGE`) + `TestProvenance::HistoricalCochange`
//! - `QueryMode::History` (`fts_lex_history` provenance + `history_hit` summaries)
//! - `ledger_expand` `diff:<rev>:<path>` (`Ok`, `UnchangedDeltaOmitted`, and `DeniedByPolicy` on `.env`)

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

use ledger_core::{
    Anchor, AnchorInput, AnchorKind, ContextRequest, ExpandDelivery, ExpandRequest, ExpandStatus,
    FocusMode, HunkConfidence, PackFormatter, QueryMode, TestProvenance,
};
use ledger_daemon::WorkspaceEngine;

type TestResult = Result<(), Box<dyn Error>>;

fn run_git(repo: &Path, args: &[&str], date_iso: &str) -> TestResult {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "Ledger Test")
        .env("GIT_AUTHOR_EMAIL", "ledger@example.test")
        .env("GIT_COMMITTER_NAME", "Ledger Test")
        .env("GIT_COMMITTER_EMAIL", "ledger@example.test")
        .env("GIT_AUTHOR_DATE", date_iso)
        .env("GIT_COMMITTER_DATE", date_iso)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(())
}

#[test]
fn workspace_engine_integrates_vcs_cochange_history_fts_and_diff_expansion() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();

    run_git(root, &["init", "-q"], "2026-10-01T10:00:00Z")?;
    fs::write(root.join(".gitignore"), ".ledger/\ntarget/\n")?;
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"acme-billing\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    fs::create_dir_all(root.join("src"))?;
    fs::create_dir_all(root.join("python"))?;
    fs::create_dir_all(root.join("tests"))?;

    // Commit 1: initial invoice engine, implicit Python shadow escrow ledger (zero static import or lexical overlap!), historical test, and a secret .env trap.
    fs::write(
        root.join("src/lib.rs"),
        "pub mod invoice;\npub mod health;\n",
    )?;
    fs::write(
        root.join("src/invoice.rs"),
        "pub fn settle_invoice_cents(subtotal: i64) -> i64 {\n    subtotal + (subtotal * 7) / 100\n}\n",
    )?;
    fs::write(
        root.join("python/vat_escrow.py"),
        "def sync_vat_escrow_ledger(raw: int) -> int:\n    return raw + (raw * 7) // 100\n",
    )?;
    fs::write(
        root.join("src/health.rs"),
        "pub fn liveness_probe() -> &'static str {\n    \"ok\"\n}\n",
    )?;
    fs::write(
        root.join("tests/escrow_parity.rs"),
        "#[test]\nfn verify_escrow_parity() {\n    assert_eq!(107, 100 + 7);\n}\n",
    )?;
    fs::write(root.join(".env"), "STRIPE_LIVE_SECRET=sk_live_never_leak\n")?;

    run_git(root, &["add", "."], "2026-10-01T10:00:00Z")?;
    run_git(
        root,
        &[
            "commit",
            "-q",
            "-m",
            "Initialize billing settlement and shadow jurisdiction ledger",
        ],
        "2026-10-01T10:00:00Z",
    )?;

    // Commit 2 & Commit 3: `src/invoice.rs`, `python/vat_escrow.py`, and `tests/escrow_parity.rs` co-change
    // without any static import, direct symbol reference, or shared identifier tokens!
    fs::write(
        root.join("src/invoice.rs"),
        "pub fn settle_invoice_cents(subtotal: i64) -> i64 {\n    subtotal + ((subtotal * 7 + 50) / 100)\n}\n",
    )?;
    fs::write(
        root.join("python/vat_escrow.py"),
        "def sync_vat_escrow_ledger(raw: int) -> int:\n    return raw + ((raw * 7 + 50) // 100)\n",
    )?;
    fs::write(
        root.join("tests/escrow_parity.rs"),
        "#[test]\nfn verify_escrow_parity() {\n    assert_eq!(107, 100 + ((100 * 7 + 50) / 100));\n}\n",
    )?;
    run_git(root, &["add", "."], "2026-10-05T10:00:00Z")?;
    run_git(
        root,
        &[
            "commit",
            "-q",
            "-m",
            "Align half-up basis point math across settlement and escrow",
        ],
        "2026-10-05T10:00:00Z",
    )?;

    fs::write(
        root.join("src/invoice.rs"),
        "pub fn settle_invoice_cents(subtotal: i64) -> i64 {\n    subtotal.saturating_add((subtotal * 7 + 50) / 100)\n}\n",
    )?;
    fs::write(
        root.join("python/vat_escrow.py"),
        "def sync_vat_escrow_ledger(raw: int) -> int:\n    return max(0, raw + ((raw * 7 + 50) // 100))\n",
    )?;
    fs::write(
        root.join("tests/escrow_parity.rs"),
        "#[test]\nfn verify_escrow_parity() {\n    assert_eq!(214, 200_i64.saturating_add((200 * 7 + 50) / 100));\n}\n",
    )?;
    run_git(root, &["add", "."], "2026-10-10T10:00:00Z")?;
    run_git(
        root,
        &[
            "commit",
            "-q",
            "-m",
            "Harden overflow saturation for Vatican Eurozone anomaly",
        ],
        "2026-10-10T10:00:00Z",
    )?;

    // Commits 4..7: Unrelated health probe commits so total revision weight gives Lift > 1.15.
    fs::write(
        root.join("src/health.rs"),
        "pub fn liveness_probe() -> &'static str {\n    \"ready\"\n}\n",
    )?;
    run_git(root, &["add", "."], "2026-10-12T10:00:00Z")?;
    run_git(
        root,
        &[
            "commit",
            "-q",
            "-m",
            "Update readiness probe response payload",
        ],
        "2026-10-12T10:00:00Z",
    )?;

    fs::write(
        root.join("src/health.rs"),
        "pub fn liveness_probe() -> &'static str {\n    \"healthy\"\n}\n",
    )?;
    run_git(root, &["add", "."], "2026-10-15T10:00:00Z")?;
    run_git(
        root,
        &["commit", "-q", "-m", "Switch probe label to healthy"],
        "2026-10-15T10:00:00Z",
    )?;

    fs::write(
        root.join("src/health.rs"),
        "pub fn liveness_probe() -> &'static str {\n    \"serving\"\n}\n",
    )?;
    run_git(root, &["add", "."], "2026-10-16T10:00:00Z")?;
    run_git(
        root,
        &["commit", "-q", "-m", "Report serving status from probe"],
        "2026-10-16T10:00:00Z",
    )?;

    fs::write(
        root.join("src/health.rs"),
        "pub fn liveness_probe() -> &'static str {\n    \"green\"\n}\n",
    )?;
    run_git(root, &["add", "."], "2026-10-17T10:00:00Z")?;
    run_git(
        root,
        &[
            "commit",
            "-q",
            "-m",
            "Report green status from health endpoint",
        ],
        "2026-10-17T10:00:00Z",
    )?;

    // Open WorkspaceEngine and perform synchronous warm index pass.
    let engine = WorkspaceEngine::open(root)?;
    let gen_id = engine.index_workspace()?;
    assert!(gen_id.0 >= 1);

    let stats = engine.stats()?;
    assert!(
        stats
            .vcs_head
            .as_deref()
            .is_some_and(|h| h.starts_with("git:"))
    );
    assert!(stats.indexed_revisions >= 7);

    // 1. Query `settle_invoice_cents` -> must surface `python/vat_escrow.py` in `cochange_peers`
    // and `tests/escrow_parity.rs` with `TestProvenance::HistoricalCochange`!
    let pack = engine.query_context(&ContextRequest {
        task: "Change settle_invoice_cents fee calculation".to_string(),
        mode: QueryMode::BlastRadius,
        anchors: vec![AnchorInput::Typed(Anchor {
            kind: AnchorKind::Symbol,
            value: "settle_invoice_cents".to_string(),
        })],
        focus_paths: Vec::new(),
        focus_mode: FocusMode::Boost,
        max_approx_tokens: 3_200,
    })?;

    assert!(
        pack.vcs_revision
            .as_deref()
            .is_some_and(|v| v.starts_with("git:"))
    );
    assert!(
        !pack.cochange_peers.iter().any(|p| p.rel_path == ".env"),
        "secret .env must never appear in co-change peers"
    );
    let shadow_peer = pack
        .cochange_peers
        .iter()
        .find(|p| p.rel_path == "python/vat_escrow.py")
        .ok_or_else(|| {
            format!(
                "expected python/vat_escrow.py in cochange_peers; edit_targets={:?}, cochange_peers={:?}",
                pack.edit_targets.iter().map(|t| &t.rel_path).collect::<Vec<_>>(),
                pack.cochange_peers
            )
        })?;
    assert_eq!(shadow_peer.cochange_commits, 3);
    assert_eq!(shadow_peer.confidence, HunkConfidence::HunkVerifiedSymbol);
    assert!(
        pack.test_candidates.iter().any(|tc| {
            tc.test_file == "tests/escrow_parity.rs"
                && tc.provenance == TestProvenance::HistoricalCochange
                && tc.command.contains("escrow_parity")
        }),
        "expected HistoricalCochange test candidate for tests/escrow_parity.rs, got: {:?}",
        pack.test_candidates
    );

    let rendered = PackFormatter::render_context_pack(&pack);
    assert!(rendered.contains("BLAST_RADIUS_AND_COCHANGE:"));
    assert!(rendered.contains("HISTORICAL_COCHANGE"));

    // 2. Historical diffs were not rendered in the initial context, so delta retries
    // must return the same diff. Expansion never changes the immutable baseline.
    // Secret historical paths remain denied.
    let rev_prefix = shadow_peer
        .expand_id
        .strip_prefix("diff:")
        .and_then(|s| s.split_once(':'))
        .map(|(rev, _)| rev)
        .ok_or("invalid diff expand_id")?;
    let secret_diff_id = format!("diff:{rev_prefix}:.env");

    let exp1 = engine.expand(&ExpandRequest {
        handle: pack.handle.to_string(),
        items: vec![shadow_peer.expand_id.clone(), secret_diff_id],
        delivery: ExpandDelivery::Delta,
        max_approx_tokens: 2_000,
    })?;
    assert_eq!(exp1.results.len(), 2);
    assert_eq!(exp1.results[0].status, ExpandStatus::Ok);
    assert!(exp1.results[0].content.contains("max(0, raw"));
    assert_eq!(exp1.results[1].status, ExpandStatus::DeniedByPolicy);
    assert!(exp1.results[1].content.is_empty());

    let exp2 = engine.expand(&ExpandRequest {
        handle: pack.handle.to_string(),
        items: vec![shadow_peer.expand_id.clone()],
        delivery: ExpandDelivery::Delta,
        max_approx_tokens: 2_000,
    })?;
    assert_eq!(exp2.results[0].status, ExpandStatus::Ok);
    assert_eq!(exp2.results[0].content, exp1.results[0].content);
    assert_eq!(exp2.results[0].file_hash, exp1.results[0].file_hash);
    assert_eq!(
        exp2.results[0].continuation_id,
        exp1.results[0].continuation_id
    );

    // 3. QueryMode::History searches `fts_lex_history` for commit-only vocabulary ("Vatican Eurozone anomaly")!
    let history_pack = engine.query_context(&ContextRequest {
        task: "Investigate Vatican Eurozone anomaly".to_string(),
        mode: QueryMode::History,
        anchors: Vec::new(),
        focus_paths: Vec::new(),
        focus_mode: FocusMode::Boost,
        max_approx_tokens: 2_800,
    })?;
    assert!(
        history_pack
            .edit_targets
            .iter()
            .any(|t| t.rel_path == "src/invoice.rs" || t.rel_path == "python/vat_escrow.py"),
        "expected commit-message FTS5 match to route to touched files, got: {:?}",
        history_pack.edit_targets
    );
    assert!(
        history_pack
            .unknowns
            .iter()
            .any(|u| u.contains("history_hit") && u.contains("Vatican Eurozone")),
        "expected history_hit summary in unknowns, got: {:?}",
        history_pack.unknowns
    );

    // 4. Advance Git HEAD on disk and verify O(1) live HEAD drift detection reports movement in unknowns.
    fs::write(
        root.join("src/health.rs"),
        "pub fn liveness_probe() -> &'static str {\n    \"live-v2\"\n}\n",
    )?;
    run_git(root, &["add", "."], "2026-10-20T10:00:00Z")?;
    run_git(
        root,
        &["commit", "-q", "-m", "Advance HEAD after snapshot"],
        "2026-10-20T10:00:00Z",
    )?;

    let drift_pack = engine.query_context(&ContextRequest {
        task: "Check liveness_probe".to_string(),
        mode: QueryMode::Auto,
        anchors: vec![AnchorInput::Typed(Anchor {
            kind: AnchorKind::Symbol,
            value: "liveness_probe".to_string(),
        })],
        focus_paths: Vec::new(),
        focus_mode: FocusMode::Boost,
        max_approx_tokens: 2_400,
    })?;
    assert!(
        drift_pack
            .unknowns
            .iter()
            .any(|u| u.contains("VCS HEAD / operation head advanced on disk")),
        "expected live VCS HEAD drift diagnostic, got: {:?}",
        drift_pack.unknowns
    );

    Ok(())
}
