//! Engine regression coverage using isolated, explicitly initialized in-process engines.

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use ledger_core::{
    Anchor, AnchorInput, AnchorKind, ApproxTokens, ContextPack, ContextRequest, EdgeConfidence,
    ExpandDelivery, ExpandRequest, ExpandResponse, ExpandStatus, FileBuffer, FocusMode, IndexState,
    LineSpan, PackFormatter, QueryMode,
};
use ledger_daemon::WorkspaceEngine;

type TestResult = Result<(), Box<dyn Error>>;

fn request(symbol: &str, mode: QueryMode, budget: u32) -> ContextRequest {
    ContextRequest {
        task: format!("Update {symbol}"),
        anchors: vec![AnchorInput::Typed(Anchor {
            kind: AnchorKind::Symbol,
            value: symbol.into(),
        })],
        mode,
        focus_paths: Vec::new(),
        focus_mode: FocusMode::Boost,
        max_approx_tokens: budget,
    }
}

fn body(pack: &ContextPack, name: &str) -> Result<String, Box<dyn Error>> {
    let target = pack
        .edit_targets
        .iter()
        .find(|target| target.symbol_name == name)
        .ok_or_else(|| format!("missing target {name}: {:?}", pack.edit_targets))?;
    Ok(format!(
        "body:{}",
        target.symbol_key.as_ref().ok_or("target has no key")?
    ))
}

fn expand(
    engine: &WorkspaceEngine,
    pack: &ContextPack,
    id: &str,
    delivery: ExpandDelivery,
    budget: u32,
) -> Result<ExpandResponse, Box<dyn Error>> {
    Ok(engine.expand(&ExpandRequest {
        handle: pack.handle.to_string(),
        items: vec![id.into()],
        delivery,
        max_approx_tokens: budget,
    })?)
}

fn collect_pages(
    engine: &WorkspaceEngine,
    pack: &ContextPack,
    first: &str,
    budget: u32,
) -> Result<String, Box<dyn Error>> {
    let mut id = first.to_string();
    let mut content = String::new();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..500 {
        assert!(
            seen.insert(id.clone()),
            "continuation repeated without progress: {id}"
        );
        let page = expand(engine, pack, &id, ExpandDelivery::Complete, budget)?;
        let retry = expand(engine, pack, &id, ExpandDelivery::Complete, budget)?;
        assert_eq!(
            PackFormatter::render_expand_response(&page),
            PackFormatter::render_expand_response(&retry)
        );
        assert!(ApproxTokens::estimate(&PackFormatter::render_expand_response(&page)).0 <= budget);
        let item = page.results.first().ok_or("missing page item")?;
        content.push_str(&item.content);
        if let Some(next) = &item.continuation_id {
            assert_eq!(item.status, ExpandStatus::TruncatedWithContinuation);
            assert!(
                !item.content.is_empty(),
                "continuation failed to deliver any content"
            );
            assert_ne!(next, &id);
            id = next.clone();
        } else {
            assert!(
                matches!(
                    item.status,
                    ExpandStatus::Ok | ExpandStatus::UpdatedSinceHandle
                ),
                "unexpected final status: {:?}: {}",
                item.status,
                item.content
            );
            return Ok(content);
        }
    }
    Err("continuation did not terminate".into())
}

#[test]
fn delta_uses_only_complete_initial_rendered_bodies_and_is_retry_safe() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    fs::write(
        root.join("short.rs"),
        "pub fn short_target() -> u32 { 7 }\n",
    )?;
    let long_source = format!(
        "pub fn long_target() {{\n{}\n}}\n",
        "    // continuation payload\n".repeat(150)
    );
    fs::write(root.join("long.rs"), long_source)?;
    let engine = WorkspaceEngine::open(root)?;
    engine.index_workspace()?;
    let short_pack = engine.query_context(&request("short_target", QueryMode::Auto, 12_000))?;
    let short_id = body(&short_pack, "short_target")?;
    let delta = expand(
        &engine,
        &short_pack,
        &short_id,
        ExpandDelivery::Delta,
        12_000,
    )?;
    assert_eq!(delta.results[0].status, ExpandStatus::UnchangedDeltaOmitted);
    assert_eq!(
        delta.results[0].content,
        expand(
            &engine,
            &short_pack,
            &short_id,
            ExpandDelivery::Delta,
            12_000
        )?
        .results[0]
            .content
    );
    let long_pack = engine.query_context(&request("long_target", QueryMode::Auto, 12_000))?;
    let long_id = body(&long_pack, "long_target")?;
    assert!(long_pack.edit_targets[0].omitted_expand_id.is_some());
    let first = expand(&engine, &long_pack, &long_id, ExpandDelivery::Delta, 12_000)?;
    let retry = expand(&engine, &long_pack, &long_id, ExpandDelivery::Delta, 12_000)?;
    assert_eq!(first.results[0].status, ExpandStatus::Ok);
    assert_eq!(first.results[0].content, retry.results[0].content);
    assert!(first.results[0].content.contains(" 153| }"));
    let enhance = engine.query_context(&request("short_target", QueryMode::Enhance, 1_000))?;
    let enhance_id = body(&enhance, "short_target")?;
    assert_eq!(
        expand(&engine, &enhance, &enhance_id, ExpandDelivery::Delta, 1_000)?.results[0].status,
        ExpandStatus::Ok
    );

    // Two full-length single-line bodies compete for a small context budget. The
    // formatter's omission is observable through the baseline reported to the engine.
    fs::write(
        root.join("omitted.rs"),
        format!(
            "pub fn omitted_alpha() {{ let _ = \"{}\"; }}\npub fn omitted_beta() {{ let _ = \"{}\"; }}\n",
            "a".repeat(1_500),
            "b".repeat(1_500)
        ),
    )?;
    engine.index_workspace()?;
    let mut omitted_request = request("omitted", QueryMode::Auto, 500);
    omitted_request.anchors = vec![AnchorInput::Typed(Anchor {
        kind: AnchorKind::Path,
        value: "omitted.rs".into(),
    })];
    let omitted = engine.query_context(&omitted_request)?;
    let (_, delivered) = PackFormatter::render_context_pack_with_delivery(&omitted);
    let unseen = omitted
        .edit_targets
        .iter()
        .filter_map(|target| target.symbol_key.as_ref())
        .map(|key| format!("body:{key}"))
        .find(|id| !delivered.contains_key(id))
        .ok_or("fixture did not omit any full body")?;
    let expanded = expand(&engine, &omitted, &unseen, ExpandDelivery::Delta, 12_000)?;
    assert_eq!(expanded.results[0].status, ExpandStatus::Ok);
    assert!(!expanded.results[0].content.is_empty());
    engine.shutdown_indexing()?;
    Ok(())
}

#[test]
fn body_and_slice_continuations_are_complete_utf8_safe_and_bounded() -> TestResult {
    let directory = tempfile::tempdir()?;
    let lines = (0..350)
        .map(|index| format!("    // line {index:03}: Καλημέρα 世界 🦀\n"))
        .collect::<String>();
    let source = format!("pub fn unicode_target() {{\n{lines}}}\n");
    fs::write(directory.path().join("lib.rs"), &source)?;
    fs::write(
        directory.path().join("single.rs"),
        format!(
            "pub fn huge_line() {{ let _ = \"{}\"; }}\n",
            "🦀é中".repeat(2_000)
        ),
    )?;
    let engine = WorkspaceEngine::open(directory.path())?;
    engine.index_workspace()?;
    let pack = engine.query_context(&request("unicode_target", QueryMode::Auto, 12_000))?;
    let id = body(&pack, "unicode_target")?;
    let expected =
        FileBuffer::from_string("lib.rs", source).render_numbered_slice(LineSpan::new(1, 352));
    assert_eq!(collect_pages(&engine, &pack, &id, 300)?, expected);
    assert_eq!(
        collect_pages(&engine, &pack, "slice:lib.rs:1-352", 300)?,
        expected
    );
    let single = engine.query_context(&request("huge_line", QueryMode::Auto, 12_000))?;
    let full = expand(
        &engine,
        &single,
        "slice:single.rs:1-1",
        ExpandDelivery::Complete,
        12_000,
    )?;
    assert_eq!(full.results[0].status, ExpandStatus::Ok);
    assert_eq!(
        collect_pages(&engine, &single, "slice:single.rs:1-1", 300)?,
        full.results[0].content
    );
    let huge_id = body(&single, "huge_line")?;
    assert_eq!(
        collect_pages(&engine, &single, &huge_id, 300)?,
        full.results[0].content
    );
    engine.shutdown_indexing()?;
    Ok(())
}

#[test]
fn changed_continuation_refuses_to_mix_source_versions() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("lib.rs"),
        format!("pub fn target() {{ let _ = \"{}\"; }}\n", "x".repeat(8_000)),
    )?;
    let engine = WorkspaceEngine::open(directory.path())?;
    engine.index_workspace()?;
    let pack = engine.query_context(&request("target", QueryMode::Auto, 12_000))?;
    engine.shutdown_indexing()?;
    let first = expand(
        &engine,
        &pack,
        "slice:lib.rs:1-1",
        ExpandDelivery::Complete,
        300,
    )?;
    assert_eq!(
        first.results[0].status,
        ExpandStatus::TruncatedWithContinuation
    );
    let cursor = first.results[0]
        .continuation_id
        .as_ref()
        .ok_or("missing continuation")?;
    fs::write(
        directory.path().join("lib.rs"),
        "pub fn target() { let changed = true; }\n",
    )?;
    let changed = expand(&engine, &pack, cursor, ExpandDelivery::Complete, 300)?;
    assert_eq!(changed.results[0].status, ExpandStatus::UpdatedSinceHandle);
    assert!(
        changed.results[0]
            .content
            .contains("restart with slice:lib.rs:1-1")
    );
    assert!(changed.results[0].continuation_id.is_none());
    let retry = expand(&engine, &pack, cursor, ExpandDelivery::Complete, 300)?;
    assert_eq!(changed.results[0].content, retry.results[0].content);
    Ok(())
}

#[test]
fn oversized_metadata_errors_and_multi_item_budget_includes_all_headers() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("lib.rs"),
        format!(
            "pub fn target() {{ let _ = \"{}\"; }}\n",
            "payload".repeat(1_000)
        ),
    )?;
    let engine = WorkspaceEngine::open(directory.path())?;
    engine.index_workspace()?;
    let pack = engine.query_context(&request("target", QueryMode::Auto, 12_000))?;
    let response = engine.expand(&ExpandRequest {
        handle: pack.handle.to_string(),
        items: vec!["slice:lib.rs:1-1".into(), "body:missing".into()],
        delivery: ExpandDelivery::Complete,
        max_approx_tokens: 300,
    })?;
    assert_eq!(response.results.len(), 2);
    assert_eq!(
        response.results[0].status,
        ExpandStatus::TruncatedWithContinuation
    );
    assert_eq!(response.results[1].status, ExpandStatus::SymbolDeleted);
    assert!(ApproxTokens::estimate(&PackFormatter::render_expand_response(&response)).0 <= 300);
    let oversized = engine.expand(&ExpandRequest {
        handle: pack.handle.to_string(),
        items: vec!["unsupported".repeat(2_000)],
        delivery: ExpandDelivery::Complete,
        max_approx_tokens: 300,
    });
    assert!(oversized.is_err());
    let too_many = engine.expand(&ExpandRequest {
        handle: pack.handle.to_string(),
        items: vec!["slice:lib.rs:1-1".into(); 17],
        delivery: ExpandDelivery::Complete,
        max_approx_tokens: 12_000,
    });
    assert!(
        too_many.is_err(),
        "items must never disappear through a silent take(16)"
    );
    engine.shutdown_indexing()?;
    Ok(())
}

#[test]
fn dirty_method_identity_and_deleted_symbol_never_fall_back_to_names_or_spans() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("lib.rs"),
        "struct First;\nimpl First { fn same_name() -> u32 { 111 } }\nstruct Second;\nimpl Second { fn same_name() -> u32 { 222 } }\n",
    )?;
    let engine = WorkspaceEngine::open(directory.path())?;
    engine.index_workspace()?;
    let pack = engine.query_context(&request("same_name", QueryMode::Auto, 12_000))?;
    let first_id = body(&pack, "First::same_name")?;
    let second_id = body(&pack, "Second::same_name")?;
    engine.shutdown_indexing()?;
    fs::write(
        directory.path().join("lib.rs"),
        "struct First;\nstruct Second;\nimpl Second { fn same_name() -> u32 { 333 } }\n",
    )?;
    let deleted = expand(&engine, &pack, &first_id, ExpandDelivery::Complete, 12_000)?;
    assert_eq!(deleted.results[0].status, ExpandStatus::SymbolDeleted);
    assert!(deleted.results[0].content.is_empty());
    let updated = expand(&engine, &pack, &second_id, ExpandDelivery::Complete, 12_000)?;
    assert_eq!(updated.results[0].status, ExpandStatus::UpdatedSinceHandle);
    assert!(updated.results[0].content.contains("333"));
    let live = engine.query_context(&request("same_name", QueryMode::Auto, 12_000))?;
    assert!(matches!(
        live.index_state,
        IndexState::ReadyWithLiveOverlay { .. }
    ));
    assert!(
        !live
            .edit_targets
            .iter()
            .any(|target| target.symbol_name == "First::same_name")
    );
    assert!(
        live.unknowns
            .iter()
            .any(|note| note.contains("Deleted or ambiguous"))
    );
    Ok(())
}

#[test]
fn duplicate_declaration_removal_cannot_reassign_an_old_handle() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("lib.rs"),
        "fn duplicate() -> u32 { 111 }\nfn duplicate() -> u32 { 222 }\n",
    )?;
    let engine = WorkspaceEngine::open(directory.path())?;
    engine.index_workspace()?;
    let pack = engine.query_context(&request("duplicate", QueryMode::Auto, 12_000))?;
    let id = body(&pack, "duplicate")?;
    assert_eq!(
        expand(&engine, &pack, &id, ExpandDelivery::Complete, 12_000)?.results[0].status,
        ExpandStatus::Ok
    );
    engine.shutdown_indexing()?;
    fs::write(
        directory.path().join("lib.rs"),
        "fn duplicate() -> u32 { 222 }\n",
    )?;
    assert_eq!(
        expand(&engine, &pack, &id, ExpandDelivery::Complete, 12_000)?.results[0].status,
        ExpandStatus::SymbolDeleted
    );
    let live = engine.query_context(&request("duplicate", QueryMode::Auto, 12_000))?;
    assert!(
        live.edit_targets.is_empty(),
        "ambiguous old descriptors must not choose the remaining same-name declaration"
    );
    Ok(())
}

#[test]
fn dirty_unselected_callers_are_downgraded_and_stale_snippets_are_suppressed() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("target.rs"),
        "pub fn actual_target() -> u32 { 5 }\n",
    )?;
    fs::write(
        directory.path().join("caller.rs"),
        "use crate::target::actual_target;\npub fn caller() -> u32 { actual_target() }\n",
    )?;
    let engine = WorkspaceEngine::open(directory.path())?;
    engine.index_workspace()?;
    let mut query = request("actual_target", QueryMode::Auto, 12_000);
    query.focus_paths = vec!["target.rs".into()];
    query.focus_mode = FocusMode::Restrict;
    let initial = engine.query_context(&query)?;
    assert!(
        initial
            .reference_context
            .iter()
            .any(|reference| reference.rel_path == "caller.rs")
    );
    let caller_id = initial
        .reference_context
        .iter()
        .find(|reference| reference.expand_id.starts_with("body:"))
        .ok_or("caller body ID missing")?
        .expand_id
        .clone();
    let target_id = body(&initial, "actual_target")?;
    let callers_id = target_id.replacen("body:", "callers:", 1);
    engine.shutdown_indexing()?;
    fs::write(
        directory.path().join("caller.rs"),
        "use crate::target::actual_target;\npub fn caller() -> u32 { 99 }\n",
    )?;
    let changed = engine.query_context(&query)?;
    let references: Vec<_> = changed
        .reference_context
        .iter()
        .filter(|reference| reference.rel_path == "caller.rs")
        .collect();
    assert!(!references.is_empty());
    assert!(
        references
            .iter()
            .all(|reference| reference.confidence == EdgeConfidence::StaleAfterEdit)
    );
    assert!(
        references
            .iter()
            .all(|reference| !reference.summary.contains("actual_target()"))
    );
    assert!(
        matches!(changed.index_state, IndexState::ReadyWithLiveOverlay { ref dirty_files, .. } if dirty_files.contains(&"caller.rs".to_string()))
    );
    let caller_body = expand(
        &engine,
        &initial,
        &caller_id,
        ExpandDelivery::Complete,
        12_000,
    )?;
    assert_eq!(
        caller_body.results[0].status,
        ExpandStatus::UpdatedSinceHandle
    );
    assert!(caller_body.results[0].content.contains("99"));
    let callers = expand(
        &engine,
        &initial,
        &callers_id,
        ExpandDelivery::Complete,
        12_000,
    )?;
    assert_eq!(callers.results[0].status, ExpandStatus::UpdatedSinceHandle);
    assert!(callers.results[0].content.contains("STALE_AFTER_EDIT"));
    assert!(!callers.results[0].content.contains("actual_target()"));
    Ok(())
}

fn git(root: &Path, args: &[&str]) -> TestResult {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Engine Test")
        .env("GIT_AUTHOR_EMAIL", "engine@example.test")
        .env("GIT_COMMITTER_NAME", "Engine Test")
        .env("GIT_COMMITTER_EMAIL", "engine@example.test")
        .output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
    }
    Ok(())
}

#[test]
fn historical_diff_and_callers_continuations_are_complete_and_retry_safe() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    git(root, &["init", "-q"])?;
    fs::write(root.join(".gitignore"), ".ledger/\n")?;
    fs::write(root.join("target.rs"), "pub fn target() {}\n")?;
    let callers = (0..80)
        .map(|index| format!("pub fn caller_{index:03}() {{ target(); }}\n"))
        .collect::<String>();
    fs::write(
        root.join("callers.rs"),
        format!("use crate::target::target;\n{callers}"),
    )?;
    git(root, &["add", "."])?;
    git(root, &["commit", "-qm", "initial complete caller fixture"])?;
    fs::write(
        root.join("history.rs"),
        (0..400)
            .map(|index| format!("// historical line {index:03}: original complete evidence\n"))
            .collect::<String>(),
    )?;
    git(root, &["add", "."])?;
    git(root, &["commit", "-qm", "add large historical evidence"])?;
    let engine = WorkspaceEngine::open(root)?;
    engine.index_workspace()?;
    let pack = engine.query_context(&request("target", QueryMode::Auto, 12_000))?;
    let diff = "diff:HEAD:history.rs";
    let complete = expand(&engine, &pack, diff, ExpandDelivery::Delta, 12_000)?;
    assert_eq!(complete.results[0].status, ExpandStatus::Ok);
    assert!(complete.results[0].content.contains("historical line 399"));
    assert_eq!(
        collect_pages(&engine, &pack, diff, 400)?,
        complete.results[0].content
    );
    assert_eq!(
        expand(&engine, &pack, diff, ExpandDelivery::Delta, 12_000)?.results[0].content,
        complete.results[0].content
    );
    let callers_id = body(&pack, "target")?.replacen("body:", "callers:", 1);
    let complete = expand(
        &engine,
        &pack,
        &callers_id,
        ExpandDelivery::Complete,
        12_000,
    )?;
    assert_eq!(complete.results[0].status, ExpandStatus::Ok);
    assert!(complete.results[0].content.contains("callers.rs:81"));
    assert_eq!(
        collect_pages(&engine, &pack, &callers_id, 400)?,
        complete.results[0].content
    );
    engine.shutdown_indexing()?;
    Ok(())
}

#[test]
fn public_cold_query_registers_working_bodies_while_indexing_is_closed() -> TestResult {
    let directory = tempfile::tempdir()?;
    let source = format!(
        "pub fn cold_target() {{\n{}\n}}\n",
        "    // cold_target remains available\n".repeat(130)
    );
    fs::write(directory.path().join("lib.rs"), source)?;
    let engine: Arc<WorkspaceEngine> = WorkspaceEngine::open(directory.path())?;
    engine.shutdown_indexing()?;
    let pack = engine.query_context(&request("cold_target", QueryMode::Auto, 12_000))?;
    assert!(matches!(pack.index_state, IndexState::Warming { .. }));
    let id = body(&pack, "cold_target")?;
    assert!(pack.edit_targets[0].omitted_expand_id.is_some());
    let expanded = expand(&engine, &pack, &id, ExpandDelivery::Delta, 12_000)?;
    assert_eq!(expanded.results[0].status, ExpandStatus::Ok);
    assert!(expanded.results[0].content.contains(" 133| }"));
    Ok(())
}

#[test]
fn original_test_descriptors_and_file_baselines_survive_reindexing() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"engine-fixture\"\nversion = \"0.1.0\"\n",
    )?;
    fs::write(root.join("lib.rs"), "pub fn target() -> u32 { 1 }\n")?;
    fs::create_dir(root.join("tests"))?;
    fs::write(
        root.join("tests/target_test.rs"),
        "use crate::target;\n#[test]\nfn target_test() { target(); }\n",
    )?;
    let engine = WorkspaceEngine::open(root)?;
    engine.index_workspace()?;
    let mut query = request("target", QueryMode::Auto, 12_000);
    query.focus_paths = vec!["lib.rs".into()];
    query.focus_mode = FocusMode::Restrict;
    let pack = engine.query_context(&query)?;
    let target_id = body(&pack, "target")?;
    let test_id = format!(
        "body:{}",
        pack.test_candidates
            .iter()
            .find_map(|candidate| candidate.test_symbol.as_ref())
            .ok_or("missing test candidate body")?
    );
    let initial = expand(&engine, &pack, &test_id, ExpandDelivery::Delta, 12_000)?;
    assert_eq!(initial.results[0].status, ExpandStatus::Ok);
    assert!(initial.results[0].content.contains("target_test"));
    fs::write(
        root.join("lib.rs"),
        "// moved source\npub fn target() -> u32 { 1 }\n",
    )?;
    fs::write(
        root.join("tests/target_test.rs"),
        "// moved test\nuse crate::target;\n#[test]\nfn target_test() { assert_eq!(target(), 2); }\n",
    )?;
    engine.index_workspace()?;
    let updated = expand(&engine, &pack, &test_id, ExpandDelivery::Delta, 12_000)?;
    assert_eq!(updated.results[0].status, ExpandStatus::UpdatedSinceHandle);
    assert!(updated.results[0].content.contains("target(), 2"));
    let target = expand(&engine, &pack, &target_id, ExpandDelivery::Delta, 12_000)?;
    assert_eq!(target.results[0].status, ExpandStatus::UpdatedSinceHandle);
    assert!(!target.results[0].content.is_empty());
    assert_eq!(
        PackFormatter::render_expand_response(&target),
        PackFormatter::render_expand_response(&expand(
            &engine,
            &pack,
            &target_id,
            ExpandDelivery::Delta,
            12_000
        )?)
    );
    engine.shutdown_indexing()?;
    Ok(())
}

#[test]
fn cold_path_anchor_exposes_expandable_symbols_without_index_admission() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("lib.rs"),
        "// header without a declaration\n\npub fn path_target() {}\n",
    )?;
    let engine = WorkspaceEngine::open(directory.path())?;
    engine.shutdown_indexing()?;
    let mut query = request("path_target", QueryMode::Auto, 1_000);
    query.anchors = vec![AnchorInput::Typed(Anchor {
        kind: AnchorKind::Path,
        value: "lib.rs".into(),
    })];
    let pack = engine.query_context(&query)?;
    let id = body(&pack, "path_target")?;
    let expanded = expand(&engine, &pack, &id, ExpandDelivery::Complete, 1_000)?;
    assert_eq!(expanded.results[0].status, ExpandStatus::Ok);
    assert!(expanded.results[0].content.contains("path_target"));
    Ok(())
}

#[test]
fn vcs_badge_history_and_graph_are_pinned_until_the_next_publication() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    git(root, &["init", "-q"])?;
    fs::write(root.join(".gitignore"), ".ledger/\n")?;
    fs::write(root.join("lib.rs"), "pub fn target() -> u32 { 1 }\n")?;
    git(root, &["add", "."])?;
    git(root, &["commit", "-qm", "quasar incident recovery"])?;
    let engine = WorkspaceEngine::open(root)?;
    engine.index_workspace()?;
    let mut history = request("quasar incident recovery", QueryMode::History, 12_000);
    history.anchors.clear();
    let original = engine.query_context(&history)?;
    assert!(
        original
            .unknowns
            .iter()
            .any(|note| note.contains("history_hit") && note.contains("quasar"))
    );
    assert!(
        original
            .edit_targets
            .iter()
            .any(|target| target.rel_path == "lib.rs")
    );
    // Closing admission lets the test observe drift without an asynchronous next
    // generation racing the assertion; production reconciliation is owned by IPC.
    engine.shutdown_indexing()?;
    fs::write(root.join("lib.rs"), "pub fn target() -> u32 { 2 }\n")?;
    git(root, &["add", "."])?;
    git(root, &["commit", "-qm", "advance to a new history head"])?;
    let drift = engine.query_context(&request("target", QueryMode::Auto, 12_000))?;
    assert_eq!(drift.handle.generation, original.handle.generation);
    assert_eq!(
        drift.vcs_revision, original.vcs_revision,
        "the displayed badge must belong to the published history/graph"
    );
    assert!(
        drift
            .unknowns
            .iter()
            .any(|note| note.contains("VCS HEAD / operation head advanced"))
    );
    assert!(
        drift
            .edit_targets
            .iter()
            .any(|target| target.numbered_source.contains("{ 2 }"))
    );
    assert!(matches!(
        drift.index_state,
        IndexState::ReadyWithLiveOverlay { .. }
    ));
    Ok(())
}

#[test]
fn oversized_symbol_identity_uses_a_working_slice_instead_of_a_lost_body_id() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("lib.rs"),
        format!("pub fn name_{}() {{}}\n", "x".repeat(70_000)),
    )?;
    let engine = WorkspaceEngine::open(directory.path())?;
    engine.shutdown_indexing()?;
    let mut query = request("oversized symbol identity", QueryMode::Auto, 12_000);
    query.anchors = vec![AnchorInput::Typed(Anchor {
        kind: AnchorKind::Path,
        value: "lib.rs".into(),
    })];
    let pack = engine.query_context(&query)?;
    let rendered = PackFormatter::render_context_pack(&pack);
    assert!(rendered.contains("slice:lib.rs:1-1"));
    assert!(!rendered.contains("body:sym_"));
    assert!(
        pack.unknowns
            .iter()
            .any(|note| note.contains("handle metadata limits"))
    );
    let response = expand(
        &engine,
        &pack,
        "slice:lib.rs:1-1",
        ExpandDelivery::Complete,
        300,
    )?;
    assert_eq!(
        response.results[0].status,
        ExpandStatus::TruncatedWithContinuation
    );
    assert!(response.results[0].continuation_id.is_some());
    assert!(ApproxTokens::estimate(&PackFormatter::render_expand_response(&response)).0 <= 300);
    Ok(())
}

#[test]
fn symbolless_contract_reference_uses_the_peer_files_actual_line_bounds() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    fs::write(
        root.join("schema.sql"),
        "CREATE TABLE orders(id INTEGER);\n",
    )?;
    fs::write(
        root.join("reader.rs"),
        format!(
            "pub fn read_orders() {{\n{}    let query = \"SELECT id FROM orders\";\n}}\n",
            "    // query preparation\n".repeat(150)
        ),
    )?;
    let engine = WorkspaceEngine::open(root)?;
    engine.index_workspace()?;
    let mut query = request("read_orders", QueryMode::Auto, 12_000);
    query.focus_paths = vec!["reader.rs".into()];
    query.focus_mode = FocusMode::Restrict;
    let pack = engine.query_context(&query)?;
    let reference = pack
        .reference_context
        .iter()
        .find(|reference| reference.rel_path == "schema.sql")
        .ok_or("schema contract reference missing")?;
    assert_eq!(reference.expand_id, "slice:schema.sql:1-1");
    assert_eq!(reference.line, 1);
    let response = expand(
        &engine,
        &pack,
        &reference.expand_id,
        ExpandDelivery::Complete,
        300,
    )?;
    assert_eq!(response.results[0].status, ExpandStatus::Ok);
    assert!(response.results[0].content.contains("CREATE TABLE orders"));
    engine.shutdown_indexing()?;
    Ok(())
}
