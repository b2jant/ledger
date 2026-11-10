use std::collections::HashMap;
use std::error::Error;

use ledger_core::{
    ArchitecturalLayer, ByteSpan, ContentHash, FileRecord, GenerationId, InterfaceHash, Language,
    LineSpan, LogicalSymbolKey, ParseCoverage, SymbolKind, SymbolRecord, TelemetryRecord,
    VcsFileTouch, VcsRevision, WorkspaceId,
};
use ledger_parser::ResolvedWorkspaceGraph;
use ledger_store::sqlite::is_noise_commit;
use ledger_store::{SqliteStore, StoreError, StoreStats};
use rusqlite::{Connection, params};

type TestResult = Result<(), Box<dyn Error>>;

fn empty_graph() -> ResolvedWorkspaceGraph {
    ResolvedWorkspaceGraph {
        files: Vec::new(),
        symbols: Vec::new(),
        edges: Vec::new(),
        tests_by_target_file: HashMap::new(),
    }
}

fn graph_with_symbol(path: &str, name: &str) -> ResolvedWorkspaceGraph {
    let signature = format!("pub fn {name}()");
    let hash = ContentHash::of_bytes(signature.as_bytes());
    ResolvedWorkspaceGraph {
        files: vec![FileRecord {
            rel_path: path.to_string(),
            language: Language::Rust,
            layer: ArchitecturalLayer::Backend,
            content_hash: hash.clone(),
            interface_hash: InterfaceHash::of_descriptors([signature.as_str()]),
            coverage: ParseCoverage::ScopedAst,
            byte_len: signature.len(),
            line_count: 1,
            concepts: vec![name.to_string()],
        }],
        symbols: vec![SymbolRecord {
            logical_key: LogicalSymbolKey::derive(path, "fn", "", name, 0),
            rel_path: path.to_string(),
            kind: SymbolKind::Function,
            name: name.to_string(),
            scope: None,
            docstring: Some(format!("{name} reference")),
            exported: true,
            byte_span: ByteSpan {
                start: 0,
                end: signature.len(),
            },
            line_span: LineSpan {
                start_line: 1,
                end_line: 1,
            },
            signature,
            signature_hash: hash.clone(),
            body_hash: hash,
        }],
        edges: Vec::new(),
        tests_by_target_file: HashMap::new(),
    }
}

fn revision(rev_id: &str, summary: &str, paths: &[&str]) -> VcsRevision {
    VcsRevision {
        rev_id: rev_id.to_string(),
        short_id: rev_id.chars().take(8).collect(),
        timestamp_secs: 1_800_000_000,
        author: "ledger-test".to_string(),
        summary: summary.to_string(),
        touched_files: paths
            .iter()
            .map(|path| VcsFileTouch {
                rel_path: (*path).to_string(),
                old_rel_path: None,
                added_lines: 4,
                deleted_lines: 2,
                hunk_new_ranges: vec![LineSpan {
                    start_line: 12,
                    end_line: 15,
                }],
            })
            .collect(),
    }
}

fn assert_generation_mismatch<T>(
    result: &Result<T, StoreError>,
    current: GenerationId,
    expected: GenerationId,
) {
    assert!(matches!(
        result,
        Err(StoreError::GenerationMismatch { db_gen, graph_gen })
            if *db_gen == current && *graph_gen == expected
    ));
}

#[test]
fn noise_is_persisted_but_only_domain_commits_are_searchable() -> TestResult {
    let directory = tempfile::tempdir()?;
    let db_path = directory.path().join("ledger.db");
    let store = SqliteStore::open(&db_path, WorkspaceId::from_raw("noise-history"))?;
    let generation = GenerationId(1);
    let mut sweep = revision("sweep", "synchronize shadow journal across workspace", &[]);
    sweep.touched_files = (0..55)
        .map(|index| VcsFileTouch {
            rel_path: format!("src/sweep_{index}.rs"),
            old_rel_path: None,
            added_lines: 1,
            deleted_lines: 1,
            hunk_new_ranges: Vec::new(),
        })
        .collect();
    let revisions = vec![
        revision("wip", "wip", &["src/wip.rs"]),
        revision("merge", "Merge pull request #42", &["src/merge.rs"]),
        revision("format", "chore: fmt", &["src/format.rs"]),
        sweep,
        revision(
            "journal-domain",
            "sync shadow journal rounding version when invoice tax tiers change",
            &["src/billing/invoice_tax.rs"],
        ),
        revision(
            "cursor-domain",
            "guard decode_cursor against negative pagination offset",
            &["src/api/decode_cursor.rs"],
        ),
    ];
    store.publish_generation_with_vcs(generation, &empty_graph(), Some("head-one"), &revisions)?;

    let inspection = Connection::open(&db_path)?;
    let noise_count: i64 = inspection.query_row(
        "SELECT COUNT(*) FROM vcs_revisions WHERE is_noise = 1",
        [],
        |row| row.get(0),
    )?;
    let fts_count: i64 =
        inspection.query_row("SELECT COUNT(*) FROM fts_lex_history", [], |row| row.get(0))?;
    assert_eq!(noise_count, 4);
    assert_eq!(fts_count, 2);
    for term in ["wip", "merge", "fmt", "sweep"] {
        assert!(
            store
                .search_history_pinned(generation, &[term.to_string()], 10)?
                .is_empty()
        );
    }
    for (term, expected_rev, expected_path) in [
        ("shad", "journal-domain", "src/billing/invoice_tax.rs"),
        (
            "invoice_tax",
            "journal-domain",
            "src/billing/invoice_tax.rs",
        ),
        ("decode_cursor", "cursor-domain", "src/api/decode_cursor.rs"),
        ("pagination", "cursor-domain", "src/api/decode_cursor.rs"),
    ] {
        let hits = store.search_history_pinned(generation, &[term.to_string()], 10)?;
        assert_eq!(hits.len(), 1, "query: {term}");
        assert_eq!(hits[0].rev_id, expected_rev);
        assert_eq!(hits[0].touched_files, [expected_path]);
        assert!(hits[0].score > 0.0 && hits[0].score <= 0.35);
    }
    let loaded = store.load_vcs_revisions_pinned(generation)?;
    assert_eq!(loaded.len(), revisions.len());
    for original in &revisions {
        assert_eq!(
            loaded
                .iter()
                .find(|loaded| loaded.rev_id == original.rev_id),
            Some(original)
        );
    }
    Ok(())
}

#[test]
fn noise_filter_strips_conventional_prefixes_without_losing_domain_summaries() {
    for summary in [
        "",
        "   ",
        "abc",
        "wip",
        "fix",
        "fixup",
        "lint",
        "fmt",
        "rustfmt",
        "prettier",
        "format",
        "formatting",
        "chore",
        "typo",
        "cleanup",
        "temp",
        "tmp",
        "update",
        "test",
        "tests",
        "chore: fmt",
        "FIX: Cleanup!",
        "style(api)!: formatting",
        "fix: typo",
        "chore: fix lint",
        "fix:",
        "Merge branch topic",
        "Merge pull request #42",
        "MERGE REMOTE-TRACKING origin/main",
        "auto-merge dependency update",
        "chore(deps): bump packages",
        "build(deps): upgrade runtime",
    ] {
        assert!(
            is_noise_commit(&revision("noise", summary, &["src/lib.rs"])),
            "expected noise: {summary}"
        );
    }
    for summary in [
        "sync shadow journal rounding version when invoice tax tiers change",
        "guard decode_cursor against negative pagination offset",
        "fix: guard negative pagination offset",
        "chore: preserve invoice journal compatibility",
        "style: align invoice tax tiers with currency precision",
        "test: cover cursor pagination bounds",
        "merge invoice tax tiers into the journal",
    ] {
        assert!(
            !is_noise_commit(&revision("domain", summary, &["src/lib.rs"])),
            "expected domain signal: {summary}"
        );
    }
    assert!(is_noise_commit(&revision(
        "empty",
        "guard cursor offset",
        &[]
    )));
    let mut boundary = revision("boundary", "guard cursor offset", &["src/lib.rs"]);
    boundary.touched_files = vec![boundary.touched_files[0].clone(); 50];
    assert!(!is_noise_commit(&boundary));
    boundary.touched_files.push(VcsFileTouch {
        rel_path: "src/extra.rs".to_string(),
        old_rel_path: None,
        added_lines: 1,
        deleted_lines: 0,
        hunk_new_ranges: Vec::new(),
    });
    assert!(is_noise_commit(&boundary));
}

#[test]
fn stale_generation_is_rejected_across_store_connections() -> TestResult {
    let directory = tempfile::tempdir()?;
    let db_path = directory.path().join("ledger.db");
    let workspace = WorkspaceId::from_raw("cross-store-history");
    let first = SqliteStore::open(&db_path, workspace.clone())?;
    let old_generation = GenerationId(1);
    let new_generation = GenerationId(2);
    first.publish_generation_with_vcs(
        old_generation,
        &empty_graph(),
        Some("old-head"),
        &[revision("old", "guard cursor offset", &["src/cursor.rs"])],
    )?;
    let second = SqliteStore::open(&db_path, workspace)?;
    second.publish_generation_with_vcs(
        new_generation,
        &empty_graph(),
        Some("new-head"),
        &[revision("new", "sync shadow journal", &["src/journal.rs"])],
    )?;
    for (terms, limit) in [(vec!["cursor".to_string()], 10), (vec![], 10), (vec![], 0)] {
        assert_generation_mismatch(
            &first.search_history_pinned(old_generation, &terms, limit),
            new_generation,
            old_generation,
        );
    }
    assert_generation_mismatch(
        &first.load_vcs_revisions_pinned(old_generation),
        new_generation,
        old_generation,
    );
    assert_generation_mismatch(
        &first.publish_vcs_history(old_generation, Some("stale-head"), &[]),
        new_generation,
        old_generation,
    );
    assert_eq!(first.current_generation()?, new_generation);
    assert_eq!(first.stats()?.vcs_head.as_deref(), Some("new-head"));
    assert_eq!(first.load_vcs_revisions_pinned(new_generation)?.len(), 1);
    Ok(())
}

#[test]
fn generation_publication_prunes_old_history_and_persists_stats() -> TestResult {
    let directory = tempfile::tempdir()?;
    let db_path = directory.path().join("ledger.db");
    let workspace = WorkspaceId::from_raw("prune-history");
    let store = SqliteStore::open(&db_path, workspace.clone())?;
    let incarnation = store.db_incarnation().to_string();
    store.publish_generation_with_vcs(
        GenerationId(1),
        &empty_graph(),
        Some("head-one"),
        &[
            revision("removed", "guard obsolete cursor", &["src/cursor.rs"]),
            revision("shared", "validate archived invoices", &["src/invoice.rs"]),
        ],
    )?;
    store.publish_generation_with_vcs(
        GenerationId(2),
        &empty_graph(),
        Some("head-two"),
        &[
            revision("shared", "sync shadow journal", &["src/journal.rs"]),
            revision("noise", "chore: fmt", &["src/journal.rs"]),
        ],
    )?;
    let inspection = Connection::open(&db_path)?;
    for query in [
        "SELECT COUNT(*) FROM vcs_revisions WHERE gen_id = 1",
        "SELECT COUNT(*) FROM fts_lex_history WHERE rev_id = 'removed'",
        "SELECT COUNT(*) FROM generations WHERE gen_id = 1",
    ] {
        let count: i64 = inspection.query_row(query, [], |row| row.get(0))?;
        assert_eq!(count, 0, "{query}");
    }
    let fts_count: i64 =
        inspection.query_row("SELECT COUNT(*) FROM fts_lex_history", [], |row| row.get(0))?;
    assert_eq!(fts_count, 1);
    assert!(
        store
            .search_history_pinned(GenerationId(2), &["archived".to_string()], 10)?
            .is_empty()
    );
    drop(store);
    let reopened = SqliteStore::open(&db_path, workspace)?;
    assert_eq!(reopened.db_incarnation(), incarnation);
    let stats = reopened.stats()?;
    assert_eq!(stats.schema_version, 2);
    assert_eq!(SqliteStore::SCHEMA_VERSION, 2);
    assert_eq!(stats.current_generation, 2);
    assert_eq!(stats.vcs_head.as_deref(), Some("head-two"));
    assert_eq!(stats.indexed_revisions, 1);
    let loaded = reopened.load_vcs_revisions_pinned(GenerationId(2))?;
    assert_eq!(loaded.len(), 2);
    assert!(loaded.iter().all(|revision| revision.rev_id != "removed"));
    Ok(())
}

#[test]
fn history_replacement_preserves_code_and_legacy_publication_clears_history() -> TestResult {
    let store = SqliteStore::open_in_memory(WorkspaceId::from_raw("legacy-compatibility"))?;
    let resolved = graph_with_symbol("src/cursor.rs", "decode_cursor");
    let generation = store.publish_generation(&resolved)?;
    assert_eq!(generation, GenerationId(1));
    assert_eq!(store.current_generation()?, generation);
    let initial = revision("initial", "guard negative offset", &["src/cursor.rs"]);
    assert_eq!(
        store.publish_vcs_history(generation, Some("initial-head"), &[initial])?,
        1
    );
    let replacement = revision("replacement", "sync shadow journal", &["src/journal.rs"]);
    let noise = revision("noise", "chore: fmt", &["src/lib.rs"]);
    assert_eq!(
        store.publish_vcs_history(generation, Some("replacement-head"), &[replacement, noise])?,
        1
    );
    assert_eq!(store.stats()?.indexed_revisions, 1);
    assert_eq!(store.stats()?.vcs_head.as_deref(), Some("replacement-head"));
    assert!(
        store
            .search_history_pinned(generation, &["negative".to_string()], 10)?
            .is_empty()
    );
    let lexical = store.search_lexical_pinned(generation, &["decode".to_string()], 10)?;
    assert!(lexical.iter().any(|hit| hit.channel == "lex_code"));
    assert!(lexical.iter().any(|hit| hit.channel == "lex_docs"));
    assert_eq!(store.stats()?.file_count, 1);
    assert_eq!(store.stats()?.symbol_count, 1);
    assert_eq!(store.publish_vcs_history(generation, None, &[])?, 0);
    assert_eq!(store.stats()?.vcs_head, None);
    assert!(store.load_vcs_revisions_pinned(generation)?.is_empty());
    store.publish_vcs_history(
        generation,
        Some("restored-head"),
        &[revision(
            "restored",
            "guard cursor offset",
            &["src/cursor.rs"],
        )],
    )?;
    let next = store.publish_generation(&resolved)?;
    assert_eq!(next, GenerationId(2));
    assert_eq!(store.stats()?.vcs_head, None);
    assert_eq!(store.stats()?.indexed_revisions, 0);
    assert!(store.load_vcs_revisions_pinned(next)?.is_empty());
    assert!(
        !store
            .search_fts_pinned(next, &["decode".to_string()], 10)?
            .is_empty()
    );
    Ok(())
}

#[test]
fn failed_publication_rolls_back_code_history_head_and_generation() -> TestResult {
    let store = SqliteStore::open_in_memory(WorkspaceId::from_raw("atomic-publication"))?;
    let initial_graph = graph_with_symbol("src/cursor.rs", "decode_cursor");
    let initial_revision = revision("initial", "guard negative offset", &["src/cursor.rs"]);
    store.publish_generation_with_vcs(
        GenerationId(1),
        &initial_graph,
        Some("initial-head"),
        std::slice::from_ref(&initial_revision),
    )?;
    let duplicate = revision("duplicate", "sync shadow journal", &["src/journal.rs"]);
    let duplicates = [duplicate.clone(), duplicate];
    assert!(
        store
            .publish_generation_with_vcs(
                GenerationId(2),
                &graph_with_symbol("src/journal.rs", "sync_journal"),
                Some("failed-head"),
                &duplicates,
            )
            .is_err()
    );
    assert_eq!(store.current_generation()?, GenerationId(1));
    assert_eq!(store.stats()?.vcs_head.as_deref(), Some("initial-head"));
    assert_eq!(
        store.load_vcs_revisions_pinned(GenerationId(1))?.as_slice(),
        std::slice::from_ref(&initial_revision)
    );
    assert!(
        !store
            .search_fts_pinned(GenerationId(1), &["decode".to_string()], 10)?
            .is_empty()
    );
    assert!(
        store
            .search_fts_pinned(GenerationId(1), &["sync".to_string()], 10)?
            .is_empty()
    );
    assert!(
        store
            .publish_vcs_history(GenerationId(1), Some("failed-history-head"), &duplicates)
            .is_err()
    );
    assert_eq!(store.stats()?.vcs_head.as_deref(), Some("initial-head"));
    assert_eq!(store.stats()?.indexed_revisions, 1);
    assert_eq!(
        store.load_vcs_revisions_pinned(GenerationId(1))?,
        [initial_revision]
    );
    assert_eq!(
        store
            .search_history_pinned(GenerationId(1), &["negative".to_string()], 10)?
            .len(),
        1
    );
    Ok(())
}

#[test]
fn history_queries_sanitize_syntax_match_renames_and_obey_limits() -> TestResult {
    let store = SqliteStore::open_in_memory(WorkspaceId::from_raw("query-sanitization"))?;
    let mut renamed = revision(
        "renamed",
        "preserve invoice journal precision",
        &["src/new_name.rs"],
    );
    renamed.touched_files[0].old_rel_path = Some("legacy/ShadowWidget.rs".to_string());
    store.publish_generation_with_vcs(
        GenerationId(1),
        &empty_graph(),
        None,
        &[
            renamed.clone(),
            revision("cursor", "guard decode_cursor offset", &["src/cursor.rs"]),
        ],
    )?;
    let renamed_hits =
        store.search_history_pinned(GenerationId(1), &["shadowWidget".to_string()], 10)?;
    assert_eq!(renamed_hits.len(), 1);
    assert_eq!(renamed_hits[0].rev_id, "renamed");
    assert_eq!(renamed_hits[0].touched_files, ["src/new_name.rs"]);
    let terms = [
        "\"invoice\"* OR NOT (".to_string(),
        "decode_cursor".to_string(),
    ];
    let hits = store.search_history_pinned(GenerationId(1), &terms, 10)?;
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|hit| hit.score > 0.0 && hit.score <= 0.35));
    assert_eq!(
        store
            .search_history_pinned(GenerationId(1), &terms, 1)?
            .len(),
        1
    );
    assert!(
        store
            .search_history_pinned(GenerationId(1), &terms, 0)?
            .is_empty()
    );
    assert!(
        store
            .search_history_pinned(GenerationId(1), &["\"* () : !".to_string()], 10)?
            .is_empty()
    );
    assert!(
        store
            .search_history_pinned(GenerationId(1), &[], 10)?
            .is_empty()
    );
    assert!(
        store
            .load_vcs_revisions_pinned(GenerationId(1))?
            .contains(&renamed)
    );
    Ok(())
}

#[test]
fn corrupt_touch_payloads_return_serialization_errors() -> TestResult {
    let directory = tempfile::tempdir()?;
    let db_path = directory.path().join("ledger.db");
    let store = SqliteStore::open(&db_path, WorkspaceId::from_raw("corrupt-history"))?;
    store.publish_generation_with_vcs(
        GenerationId(1),
        &empty_graph(),
        None,
        &[revision(
            "cursor",
            "guard negative offset",
            &["src/cursor.rs"],
        )],
    )?;
    let inspection = Connection::open(&db_path)?;
    inspection.execute(
        "UPDATE vcs_revisions SET touched_files_json = 'invalid-json'",
        [],
    )?;
    assert!(matches!(
        store.load_vcs_revisions_pinned(GenerationId(1)),
        Err(StoreError::Serialization(_))
    ));
    assert!(matches!(
        store.search_history_pinned(GenerationId(1), &["negative".to_string()], 10),
        Err(StoreError::Serialization(_))
    ));
    Ok(())
}

#[test]
fn v1_upgrade_rebuilds_disposable_tables_and_preserves_telemetry() -> TestResult {
    let directory = tempfile::tempdir()?;
    let db_path = directory.path().join("ledger.db");
    let workspace = WorkspaceId::from_raw("schema-upgrade");
    let initial = SqliteStore::open(&db_path, workspace.clone())?;
    let incarnation = initial.db_incarnation().to_string();
    initial.publish_generation_with_vcs(
        GenerationId(7),
        &graph_with_symbol("src/cursor.rs", "decode_cursor"),
        Some("old-head"),
        &[revision("old", "guard negative offset", &["src/cursor.rs"])],
    )?;
    initial.record_telemetry(&TelemetryRecord {
        timestamp_ms: 1_800_000_000_000,
        tool_name: "ledger_context".to_string(),
        mode: "history".to_string(),
        query_summary: "guard cursor offset".to_string(),
        route: "history".to_string(),
        generation: 7,
        edit_target_files: vec!["src/cursor.rs".to_string()],
        reference_files: Vec::new(),
        latency_us: 100,
        approx_tokens: 80,
    })?;
    drop(initial);
    let inspection = Connection::open(&db_path)?;
    inspection.execute_batch(
        "UPDATE meta SET schema_version = 1;
         ALTER TABLE meta DROP COLUMN vcs_head;
         DROP TABLE files;
         CREATE TABLE files (obsolete TEXT NOT NULL);
         INSERT INTO files VALUES ('stale incompatible index');",
    )?;
    drop(inspection);
    let upgraded = SqliteStore::open(&db_path, workspace)?;
    assert_eq!(upgraded.db_incarnation(), incarnation);
    let stats = upgraded.stats()?;
    assert_eq!(stats.schema_version, 2);
    assert_eq!(stats.current_generation, 0);
    assert_eq!(stats.vcs_head, None);
    assert_eq!(stats.indexed_revisions, 0);
    assert_eq!(stats.file_count, 0);
    assert_eq!(stats.symbol_count, 0);
    assert_eq!(stats.edge_count, 0);
    assert_eq!(stats.total_queries_served, 1);
    assert_eq!(stats.recent_telemetry.len(), 1);
    assert_eq!(stats.recent_telemetry[0].generation, 7);
    assert_eq!(
        stats.recent_telemetry[0].edit_target_files,
        ["src/cursor.rs"]
    );
    assert!(
        upgraded
            .load_vcs_revisions_pinned(GenerationId::COLD_START)?
            .is_empty()
    );
    let inspection = Connection::open(&db_path)?;
    for table in [
        "generations",
        "fts_lex_code",
        "fts_lex_docs",
        "fts_lex_history",
    ] {
        let count: i64 =
            inspection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })?;
        assert_eq!(count, 0, "{table}");
    }
    let next = upgraded.publish_generation(&graph_with_symbol("src/new.rs", "invoice_journal"))?;
    assert_eq!(next, GenerationId(1));
    assert_eq!(upgraded.stats()?.file_count, 1);
    assert_eq!(upgraded.stats()?.total_queries_served, 1);
    Ok(())
}

#[test]
fn stats_deserialize_without_phase_two_fields() -> TestResult {
    let store = SqliteStore::open_in_memory(WorkspaceId::from_raw("old-stats-json"))?;
    let mut value = serde_json::to_value(store.stats()?)?;
    let object = value
        .as_object_mut()
        .ok_or("expected a stats JSON object")?;
    object.remove("vcs_head");
    object.remove("indexed_revisions");
    let decoded: StoreStats = serde_json::from_value(value)?;
    assert_eq!(decoded.vcs_head, None);
    assert_eq!(decoded.indexed_revisions, 0);
    Ok(())
}

#[test]
fn unrepresentable_generations_do_not_replace_the_current_snapshot() -> TestResult {
    let directory = tempfile::tempdir()?;
    let db_path = directory.path().join("ledger.db");
    let store = SqliteStore::open(&db_path, WorkspaceId::from_raw("generation-overflow"))?;
    store.publish_generation_with_vcs(
        GenerationId(1),
        &empty_graph(),
        Some("initial-head"),
        &[],
    )?;
    assert!(
        store
            .publish_generation_with_vcs(
                GenerationId(u64::MAX),
                &empty_graph(),
                Some("invalid"),
                &[]
            )
            .is_err()
    );
    assert_eq!(store.current_generation()?, GenerationId(1));
    assert_eq!(store.stats()?.vcs_head.as_deref(), Some("initial-head"));
    let inspection = Connection::open(&db_path)?;
    let current: i64 = inspection.query_row(
        "SELECT COUNT(*) FROM generations WHERE gen_id = ?1",
        params![1],
        |row| row.get(0),
    )?;
    assert_eq!(current, 1);
    Ok(())
}
