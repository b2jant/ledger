use ledger_core::{
    ApproxTokens, CoChangePeer, ContentHash, ContextPack, EdgeConfidence, GenerationId,
    HunkConfidence, IndexState, LineSpan, LogicalSymbolKey, PackFormatter, QueryMode,
    ReferenceItem, RelationKind, SnapshotHandle, SymbolKind, TargetSlice, TestCandidate,
    TestProvenance, WorkspaceId,
};

fn target(name: &str, lines: usize) -> TargetSlice {
    TargetSlice {
        rel_path: format!("src/{name}.rs"),
        symbol_key: Some(LogicalSymbolKey::from_raw(format!("sym_{name}"))),
        symbol_kind: Some(SymbolKind::Function),
        symbol_name: name.to_string(),
        signature: format!("pub fn {name}()"),
        line_span: LineSpan::new(1, lines as u32),
        file_hash: ContentHash::of_bytes(name.as_bytes()),
        match_reason: "EXACT_SYMBOL".to_string(),
        numbered_source: (1..=lines)
            .map(|line| format!("{line:>4}| // 完整 🦀 {name} line {line}\n"))
            .collect(),
        omitted_expand_id: None,
    }
}

fn pack(budget: u32, mode: QueryMode) -> ContextPack {
    ContextPack {
        handle: SnapshotHandle::new(&WorkspaceId::from_raw("formatter"), GenerationId(4), "q"),
        workspace_root: "/workspace".to_string(),
        index_state: IndexState::Ready {
            generation: GenerationId(4),
            indexed_files: 1,
        },
        vcs_revision: Some("git:abc123".to_string()),
        route: "hybrid_lexical_ast_contracts".to_string(),
        mode,
        edit_targets: vec![target("small", 3)],
        reference_context: Vec::new(),
        cochange_peers: Vec::new(),
        test_candidates: Vec::new(),
        language_coverage: "rust(scoped_ast)".to_string(),
        unknowns: Vec::new(),
        budget_ceiling: ApproxTokens(budget),
    }
}

#[test]
fn complete_rendered_bodies_alone_define_delivery() {
    let mut p = pack(800, QueryMode::Auto);
    let mut partial = target("partial", 2);
    partial.omitted_expand_id = Some("body:sym_partial".to_string());
    let omitted = target("oversized", 500);
    p.edit_targets.extend([partial, omitted]);
    let (text, delivered) = PackFormatter::render_context_pack_with_delivery(&p);
    assert!(text.contains("small line 3"));
    assert!(text.contains("partial source; remaining lines omitted"));
    assert!(text.contains("body omitted for token ceiling; expand: \"body:sym_oversized\""));
    assert!(!text.contains("oversized line 1"));
    assert_eq!(delivered.len(), 2);
    assert_eq!(
        delivered.get("body:sym_small"),
        Some(&p.edit_targets[0].file_hash)
    );
    assert_eq!(
        delivered.get("slice:src/small.rs:1-3"),
        Some(&p.edit_targets[0].file_hash)
    );
    assert!(!delivered.contains_key("body:sym_partial"));
    assert!(!delivered.contains_key("body:sym_oversized"));
    assert_eq!(PackFormatter::render_context_pack(&p), text);
    assert!(ApproxTokens::estimate(&text).0 <= 800);
}

#[test]
fn formatter_omission_never_delivers_a_source_prefix() {
    let mut p = pack(400, QueryMode::Auto);
    p.edit_targets = vec![target("huge", 1000)];
    let (text, delivered) = PackFormatter::render_context_pack_with_delivery(&p);
    assert!(ApproxTokens::estimate(&text).0 <= 400);
    assert!(text.contains("body:sym_huge"));
    assert!(text.contains("body omitted"));
    assert!(!text.contains("huge line 1"));
    assert!(delivered.is_empty());
    assert!(text.contains("COVERAGE_AND_UNKNOWNS:"));
}

#[test]
fn headers_huge_metadata_notes_and_lists_share_the_final_budget() {
    for mode in [QueryMode::Auto, QueryMode::Enhance] {
        for budget in [400, 401, 800, 1200, 2800, 12_000] {
            let mut p = pack(budget, mode);
            let huge = "巨大🦀".repeat(2000);
            p.workspace_root = huge.clone();
            p.vcs_revision = Some(huge.clone());
            p.route = huge.clone();
            p.language_coverage = huge.clone();
            p.unknowns = vec![huge.clone(); 100];
            p.edit_targets[0].signature = huge.clone();
            p.edit_targets[0].symbol_name = huge.clone();
            p.edit_targets[0].match_reason = huge.clone();
            p.reference_context = (0..100)
                .map(|i| ReferenceItem {
                    relation: RelationKind::Calls,
                    confidence: EdgeConfidence::Exact,
                    rel_path: format!("src/caller_{i}.rs"),
                    line: 8,
                    peer_symbol_name: None,
                    summary: huge.clone(),
                    expand_id: format!("slice:src/caller_{i}.rs:1-20"),
                })
                .collect();
            p.cochange_peers = (0..100)
                .map(|i| CoChangePeer {
                    rel_path: format!("tests/test_{i}.rs"),
                    anchor_file: "src/small.rs".to_string(),
                    cochange_commits: 4,
                    anchor_total_commits: 5,
                    regularized_prob: 0.8,
                    lift: 1.5,
                    confidence: HunkConfidence::FileLevel,
                    recent_rev_short: huge.clone(),
                    recent_summary: huge.clone(),
                    expand_id: format!("diff:abc123:tests/test_{i}.rs"),
                })
                .collect();
            p.test_candidates = (0..100)
                .map(|i| TestCandidate {
                    command: format!("cargo test -p tests test_{i}"),
                    cwd: ".".to_string(),
                    manifest_path: "Cargo.toml".to_string(),
                    test_file: format!("tests/test_{i}.rs"),
                    test_symbol: None,
                    provenance: TestProvenance::DirectSymbolRef,
                })
                .collect();
            let (text, delivered) = PackFormatter::render_context_pack_with_delivery(&p);
            assert!(
                ApproxTokens::estimate(&text).0 <= budget,
                "{mode:?} {budget}: {}",
                text.len()
            );
            assert!(text.contains("handle=snap:"));
            assert!(text.contains("COVERAGE_"));
            assert!(text.contains("omitted"));
            assert!(text.contains("body:sym_small"));
            if mode == QueryMode::Enhance {
                assert!(delivered.is_empty());
                assert!(!text.contains("small line 1"));
            }
        }
    }
}

#[test]
fn test_commands_are_omitted_whole_and_enhance_never_delivers_source() {
    let mut p = pack(400, QueryMode::Enhance);
    p.test_candidates.push(TestCandidate {
        command: format!("cargo test {}", "enormous_test_name".repeat(1000)),
        cwd: ".".to_string(),
        manifest_path: "Cargo.toml".to_string(),
        test_file: "tests/all.rs".to_string(),
        test_symbol: None,
        provenance: TestProvenance::DirectSymbolRef,
    });
    let (text, delivered) = PackFormatter::render_context_pack_with_delivery(&p);
    assert!(ApproxTokens::estimate(&text).0 <= 400);
    assert!(text.contains("VERIFICATION_COMMANDS:"));
    assert!(text.contains("omitted"));
    assert!(!text.contains("cargo test enormous"));
    assert!(delivered.is_empty());
    assert_eq!(PackFormatter::render_enhance_blueprint(&p), text);
}

#[test]
fn pathological_sub_protocol_budgets_still_bound_output() {
    for budget in 0..50 {
        for mode in [QueryMode::Auto, QueryMode::Enhance] {
            let p = pack(budget, mode);
            let (text, delivered) = PackFormatter::render_context_pack_with_delivery(&p);
            assert!(ApproxTokens::estimate(&text).0 <= budget);
            assert!(delivered.is_empty());
        }
    }
}

#[test]
fn oversized_location_preserves_a_working_symbol_expansion_instead_of_partial_source() {
    for mode in [QueryMode::Auto, QueryMode::Enhance] {
        let mut p = pack(400, mode);
        p.edit_targets[0].rel_path = format!("{}/lib.rs", "超長路徑".repeat(2000));
        let (text, delivered) = PackFormatter::render_context_pack_with_delivery(&p);
        assert!(ApproxTokens::estimate(&text).0 <= 400);
        assert!(text.contains("expand: \"body:sym_small\""));
        assert!(text.contains("omitted"));
        assert!(!text.contains("small line 1"));
        assert!(delivered.is_empty());
    }
}
