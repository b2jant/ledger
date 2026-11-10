//! Mathematical and behavioral checks for the bounded bipartite history index.

use std::collections::{HashMap, HashSet};
use std::error::Error;

use ledger_core::{
    ByteSpan, CoChangePeer, ContentHash, HunkConfidence, LineSpan, LogicalSymbolKey, SymbolKind,
    SymbolRecord, VcsFileTouch, VcsRevision,
};
use ledger_vcs::{
    BipartiteCoChangeIndex, CoChangeConfig, HunkSymbolAttributor, derive_cochange_test_files,
};

type TestResult = Result<(), Box<dyn Error>>;

const NOW: i64 = 1_800_000_000;
const DAY: i64 = 86_400;

fn touch(path: &str) -> VcsFileTouch {
    VcsFileTouch {
        rel_path: path.to_owned(),
        old_rel_path: None,
        added_lines: 1,
        deleted_lines: 0,
        hunk_new_ranges: Vec::new(),
    }
}

fn revision(id: u32, timestamp_secs: i64, paths: &[&str]) -> VcsRevision {
    VcsRevision {
        rev_id: format!("revision-{id}"),
        short_id: format!("r{id}"),
        timestamp_secs,
        author: "history-test".to_owned(),
        summary: format!("Change {id}"),
        touched_files: paths.iter().map(|path| touch(path)).collect(),
    }
}

fn permissive_config() -> CoChangeConfig {
    CoChangeConfig {
        min_lift: 0.0,
        min_shared_commits: 1,
        ..CoChangeConfig::default()
    }
}

fn query(index: &BipartiteCoChangeIndex, anchor: &str) -> Vec<CoChangePeer> {
    index.query_peers(&[anchor.to_owned()], &HashMap::new(), usize::MAX)
}

fn find_peer<'a>(
    peers: &'a [CoChangePeer],
    path: &str,
) -> Result<&'a CoChangePeer, Box<dyn Error>> {
    peers
        .iter()
        .find(|peer| peer.rel_path == path)
        .ok_or_else(|| format!("missing co-change peer {path}: {peers:?}").into())
}

fn assert_near(actual: f32, expected: f64) {
    assert!(
        (f64::from(actual) - expected).abs() < 1e-6,
        "actual {actual}, expected {expected}"
    );
}

fn symbol(path: &str, name: &str, kind: SymbolKind, start: u32, end: u32) -> SymbolRecord {
    SymbolRecord {
        logical_key: LogicalSymbolKey::derive(path, kind.as_str(), "", name, 0),
        rel_path: path.to_owned(),
        kind,
        name: name.to_owned(),
        scope: None,
        signature: name.to_owned(),
        docstring: None,
        exported: false,
        byte_span: ByteSpan::new(0, 100),
        line_span: LineSpan::new(start, end),
        signature_hash: ContentHash::of_bytes(name.as_bytes()),
        body_hash: ContentHash::of_bytes(name.as_bytes()),
    }
}

#[test]
fn mass_sweep_has_no_edges_or_false_peers() {
    let paths: Vec<_> = (0..60).map(|id| format!("src/file_{id}.rs")).collect();
    let path_refs: Vec<_> = paths.iter().map(String::as_str).collect();
    let revisions = [revision(0, NOW, &path_refs)];
    let index = BipartiteCoChangeIndex::build(&revisions, CoChangeConfig::default(), NOW);
    assert_eq!(index.sweeps_filtered(), 1);
    assert_eq!(index.revisions_indexed(), 0);
    assert!(query(&index, "src/file_0.rs").is_empty());
}

#[test]
fn globally_frequent_changelog_is_suppressed_below_domain_peer() -> TestResult {
    let revisions: Vec<_> = (0..20)
        .map(|id| {
            let paths = if id < 4 {
                vec!["src/invoice.rs", "src/tax_table.rs", "CHANGELOG.md"]
            } else if id < 18 {
                vec!["CHANGELOG.md", "src/unrelated.rs"]
            } else {
                vec!["src/unrelated.rs"]
            };
            revision(id, NOW, &paths)
        })
        .collect();
    let index = BipartiteCoChangeIndex::build(&revisions, CoChangeConfig::default(), NOW);
    let peers = query(&index, "src/invoice.rs");
    assert_eq!(peers.len(), 1);
    let tax = find_peer(&peers, "src/tax_table.rs")?;
    assert_eq!(tax.cochange_commits, 4);
    assert_eq!(tax.anchor_total_commits, 4);
    assert!(tax.lift >= CoChangeConfig::default().min_lift);

    let unfiltered = BipartiteCoChangeIndex::build(&revisions, permissive_config(), NOW);
    let peers = query(&unfiltered, "src/invoice.rs");
    let tax = find_peer(&peers, "src/tax_table.rs")?;
    let changelog = find_peer(&peers, "CHANGELOG.md")?;
    assert!(tax.lift > changelog.lift);
    assert!(changelog.lift < CoChangeConfig::default().min_lift);
    assert_eq!(peers[0].rel_path, "src/tax_table.rs");
    Ok(())
}

#[test]
fn recent_cochanges_outweigh_equal_count_stale_cochanges() -> TestResult {
    let mut revisions = Vec::new();
    for id in 0..3 {
        revisions.push(revision(id, NOW - DAY, &["recent.rs", "recent_test.rs"]));
        revisions.push(revision(
            id + 3,
            NOW - 270 * DAY,
            &["stale.rs", "stale_test.rs"],
        ));
    }
    let config = permissive_config();
    let index = BipartiteCoChangeIndex::build(&revisions, config, NOW);
    let recent = query(&index, "recent.rs");
    let stale = query(&index, "stale.rs");
    let recent = find_peer(&recent, "recent_test.rs")?;
    let stale = find_peer(&stale, "stale_test.rs")?;
    assert_eq!(recent.cochange_commits, 3);
    assert_eq!(stale.cochange_commits, 3);
    assert!(recent.regularized_prob > stale.regularized_prob);
    for (peer, age) in [(recent, DAY), (stale, 270 * DAY)] {
        let shared_weight = 3.0 * (-config.decay_lambda_per_sec * age as f64).exp() / 3.0_f64.ln();
        assert_near(
            peer.regularized_prob,
            shared_weight / (shared_weight + config.smoothing_alpha),
        );
    }
    Ok(())
}

#[test]
fn single_file_commits_contribute_to_marginals_and_anchor_count() -> TestResult {
    let config = CoChangeConfig {
        decay_lambda_per_sec: 0.0,
        ..permissive_config()
    };
    let revisions: Vec<_> = (0..17)
        .map(|id| {
            let paths = if id < 4 {
                vec!["A", "B"]
            } else if id < 10 {
                vec!["A"]
            } else {
                vec!["C"]
            };
            revision(id, NOW, &paths)
        })
        .collect();
    let index = BipartiteCoChangeIndex::build(&revisions, config, NOW);
    assert_eq!(index.revisions_indexed(), 17);
    let peers = query(&index, "A");
    let peer = find_peer(&peers, "B")?;
    let shared_weight = 4.0 / 3.0_f64.ln();
    let anchor_weight = shared_weight + 6.0 / 2.0_f64.ln();
    let total_weight = anchor_weight + 7.0 / 2.0_f64.ln();
    let probability = shared_weight / (anchor_weight + 3.0);
    assert_eq!(peer.anchor_total_commits, 10);
    assert_eq!(peer.cochange_commits, 4);
    assert_near(peer.regularized_prob, probability);
    assert_near(peer.lift, probability / (shared_weight / total_weight));
    Ok(())
}

#[test]
fn duplicate_touches_are_deduplicated_before_sweep_detection_and_weighting() -> TestResult {
    let config = CoChangeConfig {
        max_files_per_revision: 2,
        ..permissive_config()
    };
    let mut paths = vec!["A"; 60];
    paths.push("B");
    let revisions = [revision(0, NOW, &paths), revision(1, NOW, &[])];
    let index = BipartiteCoChangeIndex::build(&revisions, config, NOW);
    assert_eq!(index.sweeps_filtered(), 0);
    assert_eq!(index.revisions_indexed(), 1);
    let peers = query(&index, "A");
    let peer = find_peer(&peers, "B")?;
    assert_eq!(peer.cochange_commits, 1);
    assert_eq!(peer.anchor_total_commits, 1);
    let weight = 1.0 / 3.0_f64.ln();
    assert_near(peer.regularized_prob, weight / (weight + 3.0));
    Ok(())
}

#[test]
fn rename_lineage_retains_three_shared_commits_in_either_input_order() -> TestResult {
    let mut revisions = vec![
        revision(0, NOW - 3, &["src/legacy_auth.rs", "src/session.rs"]),
        revision(1, NOW - 2, &["src/legacy_auth.rs", "src/session.rs"]),
        revision(2, NOW - 1, &["src/auth.rs", "src/session.rs"]),
    ];
    revisions[2].touched_files[0].old_rel_path = Some("src/legacy_auth.rs".to_owned());
    for newest_first in [false, true] {
        if newest_first {
            revisions.reverse();
        }
        let index = BipartiteCoChangeIndex::build(&revisions, permissive_config(), NOW);
        let peers = query(&index, "src/auth.rs");
        let peer = find_peer(&peers, "src/session.rs")?;
        assert_eq!(peer.anchor_file, "src/auth.rs");
        assert_eq!(peer.cochange_commits, 3);
        assert_eq!(peer.anchor_total_commits, 3);
        assert_eq!(peer.recent_rev_short, "r2");
        assert_eq!(peer.recent_summary, "Change 2");
        assert_eq!(peer.expand_id, "diff:revision-2:src/session.rs");
        let reverse_peers = query(&index, "src/session.rs");
        assert_eq!(reverse_peers.len(), 1);
        assert_eq!(reverse_peers[0].rel_path, "src/auth.rs");
    }
    Ok(())
}

#[test]
fn rename_chains_allow_a_historical_name_to_be_reused() -> TestResult {
    let mut revisions = vec![
        revision(0, NOW - 4, &["old.rs", "session.rs"]),
        revision(1, NOW - 3, &["middle.rs", "session.rs"]),
        revision(2, NOW - 2, &["auth.rs", "session.rs"]),
        revision(3, NOW - 1, &["old.rs", "unrelated.rs"]),
    ];
    revisions[1].touched_files[0].old_rel_path = Some("old.rs".to_owned());
    revisions[2].touched_files[0].old_rel_path = Some("middle.rs".to_owned());
    let index = BipartiteCoChangeIndex::build(&revisions, permissive_config(), NOW);
    let auth = query(&index, "auth.rs");
    assert_eq!(find_peer(&auth, "session.rs")?.cochange_commits, 3);
    assert!(!auth.iter().any(|peer| peer.rel_path == "unrelated.rs"));
    let reused = query(&index, "old.rs");
    assert_eq!(reused.len(), 1);
    assert_eq!(reused[0].rel_path, "unrelated.rs");
    Ok(())
}

#[test]
fn simultaneous_name_swap_preserves_separate_lineages() -> TestResult {
    let mut revisions = vec![
        revision(0, NOW - 3, &["A", "peer_A"]),
        revision(1, NOW - 2, &["B", "peer_B"]),
        revision(2, NOW - 1, &["B", "A"]),
    ];
    revisions[2].touched_files[0].old_rel_path = Some("A".to_owned());
    revisions[2].touched_files[1].old_rel_path = Some("B".to_owned());
    let index = BipartiteCoChangeIndex::build(&revisions, permissive_config(), NOW);
    let a = query(&index, "A");
    let b = query(&index, "B");
    assert_eq!(find_peer(&a, "peer_B")?.cochange_commits, 1);
    assert_eq!(find_peer(&b, "peer_A")?.cochange_commits, 1);
    assert!(!a.iter().any(|peer| peer.rel_path == "peer_A"));
    assert!(!b.iter().any(|peer| peer.rel_path == "peer_B"));
    Ok(())
}

#[test]
fn rename_in_suppressed_sweep_still_canonicalizes_retained_history() -> TestResult {
    let mut revisions = vec![
        revision(0, NOW - 1, &["old", "peer"]),
        revision(1, NOW, &["new", "unrelated_1", "unrelated_2"]),
    ];
    revisions[1].touched_files[0].old_rel_path = Some("old".to_owned());
    let config = CoChangeConfig {
        max_files_per_revision: 2,
        ..permissive_config()
    };
    let index = BipartiteCoChangeIndex::build(&revisions, config, NOW);
    assert_eq!(index.sweeps_filtered(), 1);
    assert_eq!(index.revisions_indexed(), 1);
    let peers = query(&index, "new");
    assert_eq!(peers.len(), 1);
    assert_eq!(find_peer(&peers, "peer")?.cochange_commits, 1);
    Ok(())
}

#[test]
fn peers_are_strictly_one_hop_without_transitive_expansion() {
    let revisions: Vec<_> = (0..8)
        .map(|id| revision(id, NOW, if id < 4 { &["A", "B"] } else { &["B", "C"] }))
        .collect();
    let index = BipartiteCoChangeIndex::build(&revisions, permissive_config(), NOW);
    let peers = query(&index, "A");
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].rel_path, "B");
    assert_eq!(peers[0].cochange_commits, 4);
    assert_eq!(peers[0].anchor_total_commits, 4);
}

#[test]
fn multiple_anchors_keep_the_highest_ranked_link_and_exclude_all_anchors() -> TestResult {
    let revisions: Vec<_> = (0..17)
        .map(|id| {
            let paths = if id < 4 {
                vec!["A", "peer"]
            } else if id < 7 {
                vec!["B", "peer"]
            } else {
                vec!["A"]
            };
            revision(id, NOW, &paths)
        })
        .collect();
    let index = BipartiteCoChangeIndex::build(&revisions, permissive_config(), NOW);
    let peers = index.query_peers(
        &["A".to_owned(), "B".to_owned(), "A".to_owned()],
        &HashMap::new(),
        10,
    );
    assert_eq!(peers.len(), 1);
    let peer = find_peer(&peers, "peer")?;
    assert_eq!(peer.anchor_file, "B");
    assert_eq!(peer.cochange_commits, 3);
    let reversed = index.query_peers(&["B".to_owned(), "A".to_owned()], &HashMap::new(), 10);
    assert_eq!(peers, reversed);

    let linked = BipartiteCoChangeIndex::build(
        &[revision(99, NOW, &["A", "B", "peer"])],
        permissive_config(),
        NOW,
    );
    let peers = linked.query_peers(&["A".to_owned(), "B".to_owned()], &HashMap::new(), 10);
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].rel_path, "peer");
    assert_eq!(peers[0].anchor_file, "A");
    Ok(())
}

#[test]
fn equal_scores_sort_by_path_before_truncation_and_honor_zero_limit() {
    let revisions = [revision(0, NOW, &["A", "z_peer", "a_peer"])];
    let index = BipartiteCoChangeIndex::build(&revisions, permissive_config(), NOW);
    let peers = query(&index, "A");
    assert_eq!(peers.len(), 2);
    assert_eq!(peers[0].rel_path, "a_peer");
    assert_eq!(peers[1].rel_path, "z_peer");
    let truncated = index.query_peers(&["A".to_owned()], &HashMap::new(), 1);
    assert_eq!(truncated, peers[..1]);
    assert!(
        index
            .query_peers(&["A".to_owned()], &HashMap::new(), 0)
            .is_empty()
    );
    assert!(query(&index, "missing").is_empty());
    assert!(index.query_peers(&[], &HashMap::new(), 10).is_empty());
}

#[test]
fn query_window_uses_the_most_recent_250_revisions_with_complete_denominator() -> TestResult {
    let revisions: Vec<_> = (0..251)
        .map(|id| {
            revision(
                id,
                NOW - 251 + i64::from(id),
                if id == 0 {
                    &["A", "old"]
                } else {
                    &["A", "recent"]
                },
            )
        })
        .collect();
    let config = CoChangeConfig {
        decay_lambda_per_sec: 0.0,
        smoothing_alpha: 0.0,
        ..permissive_config()
    };
    let index = BipartiteCoChangeIndex::build(&revisions, config, NOW);
    let peers = query(&index, "A");
    assert_eq!(peers.len(), 1);
    let peer = find_peer(&peers, "recent")?;
    assert_eq!(peer.cochange_commits, 250);
    assert_eq!(peer.anchor_total_commits, 251);
    assert_near(peer.regularized_prob, 250.0 / 251.0);
    assert_eq!(peer.recent_rev_short, "r250");
    Ok(())
}

#[test]
fn future_timestamps_are_clamped_and_extreme_stale_weights_remain_finite() -> TestResult {
    let config = permissive_config();
    let future =
        BipartiteCoChangeIndex::build(&[revision(0, i64::MAX, &["A", "B"])], config, i64::MIN);
    let peers = query(&future, "A");
    let peer = find_peer(&peers, "B")?;
    let weight = 1.0 / 3.0_f64.ln();
    assert_near(peer.regularized_prob, weight / (weight + 3.0));
    assert!(peer.lift.is_finite());
    let stale =
        BipartiteCoChangeIndex::build(&[revision(0, i64::MIN, &["A", "B"])], config, i64::MAX);
    assert_eq!(stale.revisions_indexed(), 1);
    assert!(query(&stale, "A").is_empty());
    assert!(query(&BipartiteCoChangeIndex::default(), "A").is_empty());
    Ok(())
}

#[test]
fn minimum_shared_count_filters_single_observations() {
    let config = CoChangeConfig {
        min_shared_commits: 2,
        ..permissive_config()
    };
    let index = BipartiteCoChangeIndex::build(
        &[
            revision(0, NOW, &["A", "frequent", "rare"]),
            revision(1, NOW, &["A", "frequent"]),
        ],
        config,
        NOW,
    );
    let peers = query(&index, "A");
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].rel_path, "frequent");
}

#[test]
fn hunk_attribution_ignores_modules_and_returns_unique_overlapping_symbols() {
    let symbols = vec![
        symbol("a.rs", "a", SymbolKind::Module, 1, 100),
        symbol("a.rs", "work", SymbolKind::Function, 10, 20),
        symbol("a.rs", "LIMIT", SymbolKind::Constant, 30, 30),
    ];
    for hunks in [
        vec![],
        vec![LineSpan::new(2, 9)],
        vec![LineSpan::new(21, 29)],
    ] {
        assert_eq!(
            HunkSymbolAttributor::classify_touch(&hunks, &symbols),
            HunkConfidence::FileLevel
        );
        assert!(HunkSymbolAttributor::overlapping_symbols(&hunks, &symbols).is_empty());
    }
    for span in [
        LineSpan::new(9, 10),
        LineSpan::new(20, 21),
        LineSpan::new(30, 30),
    ] {
        assert_eq!(
            HunkSymbolAttributor::classify_touch(&[span], &symbols),
            HunkConfidence::HunkVerifiedSymbol
        );
    }
    let overlapping = HunkSymbolAttributor::overlapping_symbols(
        &[
            LineSpan::new(10, 11),
            LineSpan::new(12, 15),
            LineSpan::new(30, 30),
        ],
        &symbols,
    );
    assert_eq!(overlapping, vec![&symbols[1], &symbols[2]]);
    assert_eq!(
        HunkSymbolAttributor::classify_touch(&[LineSpan::new(1, 100)], &symbols[..1]),
        HunkConfidence::FileLevel
    );
    assert_eq!(
        HunkSymbolAttributor::classify_touch(&[LineSpan::new(1, 100)], &[]),
        HunkConfidence::FileLevel
    );
}

#[test]
fn query_confidence_requires_shared_hunk_overlap_on_either_side() -> TestResult {
    let scenarios = [
        (true, false, true, HunkConfidence::HunkVerifiedSymbol),
        (false, true, true, HunkConfidence::HunkVerifiedSymbol),
        (false, false, true, HunkConfidence::FileLevel),
        (true, true, false, HunkConfidence::FileLevel),
    ];
    for (anchor_overlap, peer_overlap, functions, expected) in scenarios {
        let mut shared = revision(0, NOW - 2, &["anchor.rs", "schema.sql"]);
        if anchor_overlap {
            shared.touched_files[0].hunk_new_ranges = vec![LineSpan::new(10, 10)];
        }
        if peer_overlap {
            shared.touched_files[1].hunk_new_ranges = vec![LineSpan::new(10, 10)];
        }
        let recent = revision(1, NOW - 1, &["anchor.rs", "schema.sql"]);
        let mut unrelated = revision(2, NOW, &["schema.sql", "other.rs"]);
        unrelated.touched_files[0].hunk_new_ranges = vec![LineSpan::new(10, 10)];
        let kind = if functions {
            SymbolKind::Function
        } else {
            SymbolKind::Module
        };
        let mut symbols = HashMap::new();
        symbols.insert(
            "anchor.rs".to_owned(),
            vec![symbol("anchor.rs", "anchor", kind, 10, 20)],
        );
        if peer_overlap || !anchor_overlap {
            symbols.insert(
                "schema.sql".to_owned(),
                vec![symbol("schema.sql", "peer", kind, 10, 20)],
            );
        }
        let index =
            BipartiteCoChangeIndex::build(&[shared, recent, unrelated], permissive_config(), NOW);
        let peers = index.query_peers(&["anchor.rs".to_owned()], &symbols, 10);
        let peer = find_peer(&peers, "schema.sql")?;
        assert_eq!(peer.confidence, expected);
        assert_eq!(peer.recent_rev_short, "r1");
        assert_eq!(peer.cochange_commits, 2);
    }
    Ok(())
}

#[test]
fn historical_test_discovery_covers_language_conventions_without_substring_false_positives() {
    let positive = [
        "tests/integration.rs",
        "crates/domain/tests/invoice.rs",
        "web/__tests__/invoice.tsx",
        "__tests__/service.js",
        "src/auth_test.rs",
        "pkg/auth_test.go",
        "service/auth_test.py",
        "test_auth.py",
        "service/test_auth.py",
        "web/auth.test.ts",
        "web/auth.test.tsx",
        "web/auth.spec.ts",
        "web/auth.spec.tsx",
        "web/auth.test.js",
        "web/auth.spec.js",
        "web/auth.test.jsx",
        "web/auth.spec.jsx",
    ];
    let negative = [
        "src/contests/invoice.rs",
        "src/__tests__helpers/invoice.ts",
        "src/test_helpers.rs",
        "src/test_configuration.ts",
        "src/auth.testing.ts",
        "src/auth.spec.ts.backup",
        "src/latest.py",
        "src/tests",
    ];
    let peers: Vec<_> = positive
        .iter()
        .chain(&negative)
        .map(|path| CoChangePeer {
            rel_path: (*path).to_owned(),
            anchor_file: "src/invoice.rs".to_owned(),
            cochange_commits: 2,
            anchor_total_commits: 3,
            regularized_prob: 0.4,
            lift: 2.0,
            confidence: HunkConfidence::FileLevel,
            recent_rev_short: "abc123".to_owned(),
            recent_summary: "Invoice change".to_owned(),
            expand_id: format!("diff:abc123:{path}"),
        })
        .collect();
    let tests = derive_cochange_test_files(&peers);
    let discovered: Vec<_> = tests.iter().map(|peer| peer.rel_path.as_str()).collect();
    assert_eq!(discovered, positive);
    assert!(std::ptr::eq(tests[0], &peers[0]));
    assert!(derive_cochange_test_files(&[]).is_empty());
}

#[test]
fn generated_histories_match_an_independent_weighted_set_oracle() -> TestResult {
    let paths = ["A", "B", "C", "D", "E"];
    for seed in 0..8_u32 {
        let config = CoChangeConfig {
            max_files_per_revision: 3,
            ..permissive_config()
        };
        let revisions: Vec<_> = (0..24_u32)
            .map(|id| {
                let mask = ((id * 17 + seed * 11) ^ (id >> 1)) % 32;
                let mut selected: Vec<_> = paths
                    .iter()
                    .enumerate()
                    .filter_map(|(bit, &path)| (mask & (1 << bit) != 0).then_some(path))
                    .collect();
                if id % 3 == 0
                    && let Some(&first) = selected.first()
                {
                    selected.push(first);
                }
                revision(id, NOW - i64::from(24 - id) * DAY, &selected)
            })
            .collect();
        let index = BipartiteCoChangeIndex::build(&revisions, config, NOW);
        let mut reversed = revisions.clone();
        reversed.reverse();
        let reversed_index = BipartiteCoChangeIndex::build(&reversed, config, NOW);
        let mut retained = Vec::new();
        let mut sweeps = 0;
        for revision in &revisions {
            let files: HashSet<_> = revision
                .touched_files
                .iter()
                .map(|t| t.rel_path.as_str())
                .collect();
            if files.len() > config.max_files_per_revision {
                sweeps += 1;
            } else if !files.is_empty() {
                let age = (NOW - revision.timestamp_secs) as f64;
                let weight =
                    (-config.decay_lambda_per_sec * age).exp() / (files.len() as f64).ln_1p();
                retained.push((revision, files, weight));
            }
        }
        assert_eq!(index.sweeps_filtered(), sweeps);
        assert_eq!(index.revisions_indexed(), retained.len());
        let total_weight: f64 = retained.iter().map(|(_, _, weight)| weight).sum();
        for anchor in paths {
            let peers = query(&index, anchor);
            assert_eq!(peers, query(&reversed_index, anchor));
            let anchor_revisions: Vec<_> = retained
                .iter()
                .filter(|(_, files, _)| files.contains(anchor))
                .collect();
            let anchor_weight: f64 = anchor_revisions.iter().map(|(_, _, weight)| weight).sum();
            for path in paths {
                if path == anchor {
                    assert!(!peers.iter().any(|peer| peer.rel_path == anchor));
                    continue;
                }
                let shared: Vec<_> = anchor_revisions
                    .iter()
                    .filter(|(_, files, _)| files.contains(path))
                    .collect();
                if shared.is_empty() {
                    assert!(!peers.iter().any(|peer| peer.rel_path == path));
                    continue;
                }
                let peer = find_peer(&peers, path)?;
                let shared_weight: f64 = shared.iter().map(|(_, _, weight)| weight).sum();
                let peer_weight: f64 = retained
                    .iter()
                    .filter(|(_, files, _)| files.contains(path))
                    .map(|(_, _, weight)| weight)
                    .sum();
                let probability = shared_weight / (anchor_weight + config.smoothing_alpha);
                assert_eq!(peer.cochange_commits as usize, shared.len());
                assert_eq!(peer.anchor_total_commits as usize, anchor_revisions.len());
                assert_near(peer.regularized_prob, probability);
                assert_near(peer.lift, probability / (peer_weight / total_weight));
                let latest = shared
                    .iter()
                    .map(|(revision, _, _)| revision)
                    .max_by_key(|revision| revision.timestamp_secs)
                    .ok_or("shared evidence unexpectedly empty")?;
                assert_eq!(peer.expand_id, format!("diff:{}:{path}", latest.rev_id));
                assert_eq!(peer.recent_summary, latest.summary);
            }
        }
    }
    Ok(())
}
