use std::collections::HashMap;
use std::error::Error;

use ledger_core::{
    ByteSpan, ContentHash, GenerationId, LineSpan, LogicalSymbolKey, SymbolKind, SymbolRecord,
    WorkspaceId,
};
use ledger_parser::ResolvedWorkspaceGraph;
use ledger_store::{HandleRegistry, SqliteStore, StoreError};
use rusqlite::Connection;

type TestResult = Result<(), Box<dyn Error>>;

fn symbol(index: usize) -> SymbolRecord {
    let key = LogicalSymbolKey::from_raw(format!("sym_{index:04}"));
    SymbolRecord {
        logical_key: key,
        rel_path: "src/lib.rs".to_string(),
        kind: SymbolKind::Function,
        name: format!("function_{index}"),
        scope: Some("module".to_string()),
        signature: format!("pub fn function_{index}()"),
        docstring: Some("large documentation".repeat(1000)),
        exported: true,
        byte_span: ByteSpan::new(0, 30),
        line_span: LineSpan::new(1, 3),
        signature_hash: ContentHash::of_bytes(b"signature"),
        body_hash: ContentHash::of_bytes(b"body"),
    }
}

fn graph() -> ResolvedWorkspaceGraph {
    ResolvedWorkspaceGraph {
        files: Vec::new(),
        symbols: Vec::new(),
        edges: Vec::new(),
        tests_by_target_file: HashMap::new(),
    }
}

#[test]
fn delivered_baseline_is_immutable_across_expansion_and_reregistration() -> TestResult {
    let registry = HandleRegistry::new(2);
    let original_hash = ContentHash::of_bytes(b"original");
    let key = LogicalSymbolKey::from_raw("sym_0000");
    registry.insert_with_symbols(
        "first".to_string(),
        GenerationId(7),
        HashMap::from([("body:sym_0000".to_string(), original_hash.clone())]),
        HashMap::from([(key.clone(), symbol(0))]),
    );
    registry.insert("first".to_string(), GenerationId(8), HashMap::new());
    let mut metadata = registry.get("first").ok_or("missing handle")?;
    assert_eq!(metadata.generation, GenerationId(7));
    assert_eq!(metadata.delivered_digests.len(), 1);
    assert_eq!(
        metadata.delivered_digests.get("body:sym_0000"),
        Some(&original_hash)
    );
    assert_eq!(
        metadata.symbols.get(&key).map(|s| s.name.as_str()),
        Some("function_0")
    );
    metadata.delivered_digests.insert(
        "body:sym_unseen".to_string(),
        ContentHash::of_bytes(b"expansion"),
    );
    metadata.symbols.clear();
    assert_eq!(
        registry.get("first").ok_or("missing handle")?.symbols.len(),
        1
    );
    assert_eq!(
        registry
            .get("first")
            .ok_or("missing handle")?
            .delivered_digests
            .len(),
        1
    );
    registry.insert("second".to_string(), GenerationId(8), HashMap::new());
    registry.insert("third".to_string(), GenerationId(9), HashMap::new());
    assert!(registry.get("first").is_none());
    assert!(registry.get("third").is_some());
    Ok(())
}

#[test]
fn old_handle_descriptors_are_deterministically_bounded_without_changing_identity() -> TestResult {
    let registry = HandleRegistry::new(2);
    let count = HandleRegistry::MAX_SYMBOLS_PER_HANDLE + 40;
    let symbols: HashMap<_, _> = (0..count)
        .map(|i| {
            let s = symbol(i);
            (s.logical_key.clone(), s)
        })
        .collect();
    let reversed: HashMap<_, _> = (0..count)
        .rev()
        .map(|i| {
            let s = symbol(i);
            (s.logical_key.clone(), s)
        })
        .collect();
    registry.insert_with_symbols("one".to_string(), GenerationId(1), HashMap::new(), symbols);
    registry.insert_with_symbols("two".to_string(), GenerationId(1), HashMap::new(), reversed);
    let a = registry.get("one").ok_or("missing first handle")?;
    let b = registry.get("two").ok_or("missing second handle")?;
    assert_eq!(a.symbols, b.symbols);
    assert_eq!(a.symbols.len(), HandleRegistry::MAX_SYMBOLS_PER_HANDLE);
    let first = a
        .symbols
        .get(&LogicalSymbolKey::from_raw("sym_0000"))
        .ok_or("missing useful descriptor")?;
    assert_eq!(first.scope.as_deref(), Some("module"));
    assert_eq!(first.signature, "pub fn function_0()");
    assert!(first.docstring.is_none());
    assert!(
        !a.symbols
            .contains_key(&LogicalSymbolKey::from_raw(format!("sym_{:04}", count - 1)))
    );
    Ok(())
}

#[test]
fn enormous_descriptors_do_not_bypass_the_metadata_byte_bound() -> TestResult {
    let registry = HandleRegistry::new(1);
    let mut oversized = symbol(0);
    oversized.name = "n".repeat(HandleRegistry::MAX_SYMBOL_METADATA_BYTES + 1);
    let mut useful = symbol(1);
    useful.signature = "signature".repeat(1000);
    registry.insert_with_symbols(
        "bounded".to_string(),
        GenerationId(1),
        HashMap::new(),
        HashMap::from([
            (oversized.logical_key.clone(), oversized),
            (useful.logical_key.clone(), useful),
        ]),
    );
    let metadata = registry.get("bounded").ok_or("missing handle")?;
    assert_eq!(metadata.symbols.len(), 1);
    let descriptor = metadata
        .symbols
        .get(&LogicalSymbolKey::from_raw("sym_0001"))
        .ok_or("missing small identity")?;
    assert_eq!(descriptor.name, "function_1");
    assert!(descriptor.signature.is_empty());
    Ok(())
}

#[test]
fn checkpoint_truncates_wal_and_preserves_committed_generation() -> TestResult {
    let root = tempfile::tempdir()?;
    let path = root.path().join("ledger.db");
    let workspace = WorkspaceId::from_raw("checkpoint");
    let store = SqliteStore::open(&path, workspace.clone())?;
    let generation = store.publish_generation(&graph())?;
    assert!(std::fs::metadata(root.path().join("ledger.db-wal"))?.len() > 0);
    store.checkpoint()?;
    assert_eq!(
        std::fs::metadata(root.path().join("ledger.db-wal"))?.len(),
        0
    );
    drop(store);
    let reopened = SqliteStore::open(&path, workspace)?;
    assert_eq!(reopened.current_generation()?, generation);
    reopened.checkpoint()?;
    let memory = SqliteStore::open_in_memory(WorkspaceId::from_raw("memory-checkpoint"))?;
    memory.publish_generation(&graph())?;
    memory.checkpoint()?;
    Ok(())
}

#[test]
fn a_pinned_reader_makes_checkpoint_report_busy_until_released() -> TestResult {
    let root = tempfile::tempdir()?;
    let path = root.path().join("ledger.db");
    let store = SqliteStore::open(&path, WorkspaceId::from_raw("busy-checkpoint"))?;
    store.publish_generation(&graph())?;
    store.checkpoint()?;
    let reader = Connection::open(&path)?;
    reader.execute_batch("BEGIN")?;
    let old_generation: i64 =
        reader.query_row("SELECT current_gen_id FROM meta", [], |row| row.get(0))?;
    let next = store.publish_generation(&graph())?;
    assert!(next.0 > old_generation as u64);
    assert!(matches!(
        store.checkpoint(),
        Err(StoreError::CheckpointBusy { .. })
    ));
    reader.execute_batch("ROLLBACK")?;
    store.checkpoint()?;
    assert_eq!(
        std::fs::metadata(root.path().join("ledger.db-wal"))?.len(),
        0
    );
    Ok(())
}
