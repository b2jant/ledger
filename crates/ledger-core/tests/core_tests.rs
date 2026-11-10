use std::fs;
use std::str::FromStr;

use ledger_core::{
    ArchitecturalLayer, DefaultScopeGuard, GenerationId, Language, LineSpan, ScopeDecision,
    ScopePolicy, SingleBufferReader, SnapshotHandle, WorkspaceId,
    tokens::{extract_query_terms, infer_architectural_layer, split_identifier_tokens},
};

#[test]
fn scope_guard_blocks_secrets_and_symlinks_but_allows_env_example()
-> Result<(), Box<dyn std::error::Error>> {
    let outside = tempfile::tempdir()?;
    let secret_outside = outside.path().join("outside.rs");
    fs::write(&secret_outside, "pub fn stolen() {}\n")?;

    let ws = tempfile::tempdir()?;
    fs::create_dir_all(ws.path().join("src"))?;
    fs::write(ws.path().join("src/lib.rs"), "pub fn ok() {}\n")?;
    fs::write(ws.path().join(".env"), "DATABASE_URL=postgres://secret\n")?;
    fs::write(ws.path().join(".env.production"), "API_KEY=topsecret\n")?;
    fs::write(
        ws.path().join(".env.example"),
        "DATABASE_URL=postgres://localhost/dev\n",
    )?;
    fs::write(
        ws.path().join("server.pem"),
        "-----BEGIN PRIVATE KEY-----\n",
    )?;

    #[cfg(unix)]
    std::os::unix::fs::symlink(&secret_outside, ws.path().join("src/escape.rs"))?;

    let guard = DefaultScopeGuard::new(ws.path())?;

    assert!(matches!(
        guard.evaluate_path(&ws.path().join("src/lib.rs")),
        ScopeDecision::Allowed { ref rel_path, .. } if rel_path == "src/lib.rs"
    ));
    assert!(matches!(
        guard.evaluate_path(&ws.path().join(".env.example")),
        ScopeDecision::Allowed { ref rel_path, .. } if rel_path == ".env.example"
    ));
    assert!(matches!(
        guard.evaluate_path(&ws.path().join(".env")),
        ScopeDecision::DeniedSecret { .. }
    ));
    assert!(matches!(
        guard.evaluate_path(&ws.path().join(".env.production")),
        ScopeDecision::DeniedSecret { .. }
    ));
    assert!(matches!(
        guard.evaluate_path(&ws.path().join("server.pem")),
        ScopeDecision::DeniedSecret { .. }
    ));

    #[cfg(unix)]
    assert_eq!(
        guard.evaluate_path(&ws.path().join("src/escape.rs")),
        ScopeDecision::DeniedOutsideWorkspace
    );

    Ok(())
}

#[test]
fn single_buffer_reader_slices_lines_accurately() -> Result<(), Box<dyn std::error::Error>> {
    let ws = tempfile::tempdir()?;
    let file_path = ws.path().join("cursor.rs");
    fs::write(
        &file_path,
        "use crate::ApiError;\n\npub fn decode(s: &str) -> bool {\n    !s.is_empty()\n}\n",
    )?;

    let guard = DefaultScopeGuard::new(ws.path())?;
    let reader = SingleBufferReader::new(guard);
    let buf = reader.read_file(&file_path)?;

    assert_eq!(buf.rel_path(), "cursor.rs");
    assert_eq!(buf.line_count(), 5);
    let rendered = buf.render_numbered_slice(LineSpan::new(3, 5));
    assert!(rendered.contains("   3| pub fn decode(s: &str) -> bool {"));
    assert!(rendered.contains("   4|     !s.is_empty()"));
    assert!(rendered.contains("   5| }"));
    Ok(())
}

#[test]
fn identifier_splitting_stemming_and_layer_classification() {
    let parts = split_identifier_tokens("applyOrderFilterPredicates");
    assert_eq!(parts, vec!["apply", "filter", "order", "predicates"]);

    let query_terms = extract_query_terms("Update backend filtering on archived orders");
    assert!(query_terms.contains(&"backend".to_string()));
    assert!(query_terms.contains(&"filter".to_string()));
    assert!(query_terms.contains(&"order".to_string()));
    assert!(query_terms.contains(&"archive".to_string()));

    assert_eq!(
        infer_architectural_layer("src/api/orders/handler.rs", Language::Rust),
        ArchitecturalLayer::Backend
    );
    assert_eq!(
        infer_architectural_layer("web/src/hooks/useOrdersFilter.ts", Language::TypeScript),
        ArchitecturalLayer::Frontend
    );
    assert_eq!(
        infer_architectural_layer("migrations/001_orders.sql", Language::Sql),
        ArchitecturalLayer::Database
    );
    assert_eq!(
        infer_architectural_layer("tests/orders_it.rs", Language::Rust),
        ArchitecturalLayer::Tests
    );
}

#[test]
fn snapshot_handle_roundtrips() -> Result<(), Box<dyn std::error::Error>> {
    let ws = WorkspaceId::from_raw("abcdef0123456789");
    let handle = SnapshotHandle::new(&ws, GenerationId(42), "query-seed");
    let text = handle.to_string();
    let parsed = SnapshotHandle::from_str(&text)?;
    assert_eq!(parsed, handle);
    assert_eq!(parsed.generation, GenerationId(42));
    Ok(())
}

#[test]
fn nested_repository_boundaries_apply_to_live_and_deleted_paths()
-> Result<(), Box<dyn std::error::Error>> {
    let ws = tempfile::tempdir()?;
    fs::create_dir_all(ws.path().join(".git"))?;
    fs::write(ws.path().join("normal.rs"), "pub fn outer() {}\n")?;
    for (name, marker, directory) in [
        ("nested_git", ".git", true),
        ("worktree", ".git", false),
        ("nested_jj", ".jj", true),
    ] {
        let nested = ws.path().join(name);
        fs::create_dir_all(nested.join("src"))?;
        if directory {
            fs::create_dir(nested.join(marker))?;
        } else {
            fs::write(
                nested.join(marker),
                "gitdir: /unrelated/git/worktrees/nested\n",
            )?;
        }
        fs::write(nested.join("src/inner.rs"), "pub fn inner() {}\n")?;
    }
    let guard = DefaultScopeGuard::new(ws.path())?;
    let reader = SingleBufferReader::new(guard.clone());
    assert!(reader.read_file(&ws.path().join("normal.rs")).is_ok());
    for root in ["nested_git", "worktree", "nested_jj"] {
        assert!(!guard.is_relative_name_allowed(std::path::Path::new(root)));
        for suffix in ["src/inner.rs", "src/deleted.rs"] {
            let relative = format!("{root}/{suffix}");
            assert!(!guard.is_history_path_allowed(std::path::Path::new(&relative)));
            assert!(reader.read_file(&ws.path().join(&relative)).is_err());
        }
    }
    assert!(guard.is_history_path_allowed(std::path::Path::new("deleted/old.rs")));
    assert!(!guard.is_history_path_allowed(std::path::Path::new("deleted/.env")));
    for unsafe_name in [
        "",
        "../outside.rs",
        "src/../../normal.rs",
        "./normal.rs",
        "/absolute.rs",
        "src/../normal.rs",
        "bad\0name.rs",
    ] {
        let path = std::path::Path::new(unsafe_name);
        assert!(!guard.is_history_path_allowed(path), "{unsafe_name:?}");
        assert!(
            !matches!(guard.evaluate_path(path), ScopeDecision::Allowed { .. }),
            "{unsafe_name:?}"
        );
    }
    Ok(())
}

#[test]
#[cfg(unix)]
fn symlink_ancestors_cannot_launder_live_scope_but_history_is_lexical()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::symlink;
    use std::path::Path;
    let ws = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    fs::create_dir_all(ws.path().join("nested/.git"))?;
    fs::create_dir_all(ws.path().join(".ledger"))?;
    fs::write(ws.path().join("nested/inner.rs"), "nested repository\n")?;
    fs::write(ws.path().join(".ledger/internal.rs"), "internal state\n")?;
    fs::write(outside.path().join("outside.rs"), "outside workspace\n")?;
    symlink(ws.path().join("nested"), ws.path().join("alias_nested"))?;
    symlink(ws.path().join(".ledger"), ws.path().join("alias_internal"))?;
    symlink(outside.path(), ws.path().join("alias_outside"))?;
    let guard = DefaultScopeGuard::new(ws.path())?;
    let reader = SingleBufferReader::new(guard.clone());
    for file in [
        "alias_nested/inner.rs",
        "alias_internal/internal.rs",
        "alias_outside/outside.rs",
    ] {
        assert!(reader.read_file(Path::new(file)).is_err(), "{file}");
    }
    // A Git object lookup of this name does not read the symlink's external target.
    assert!(guard.is_history_path_allowed(Path::new("alias_outside/deleted.rs")));
    assert!(guard.is_history_path_allowed(Path::new("alias_nested/old.rs")));
    assert!(!guard.is_history_path_allowed(Path::new("alias_outside/.env")));
    Ok(())
}
