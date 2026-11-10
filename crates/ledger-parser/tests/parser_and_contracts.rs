//! Integration tests for polyglot Tree-sitter AST extraction, cross-boundary contracts,
//! `InterfaceHash` stability, syntax-error recovery, and `ScopedResolver` ambiguity tracking.

use ledger_core::{EdgeConfidence, FileBuffer, ParseCoverage, RelationKind};
use ledger_parser::{ParserRegistry, ScopedResolver, detect_manifest};

fn buf(rel_path: &str, source: &str) -> FileBuffer {
    FileBuffer::from_string(rel_path, source)
}

#[test]
fn polyglot_ast_and_cross_boundary_contracts_resolve_cleanly() {
    let registry = ParserRegistry::new();
    let resolver = ScopedResolver::new();

    let rust_api = buf(
        "src/api/orders.rs",
        r#"
use crate::db::orders_repo::load_order_row;

#[get("/api/orders/:id")]
pub async fn get_order(order_id: String) -> String {
    let webhook = std::env::var("ORDER_WEBHOOK_URL").ok();
    let _ = webhook;
    load_order_row(&order_id)
}
"#,
    );

    let rust_repo = buf(
        "src/db/orders_repo.rs",
        r#"
pub fn load_order_row(id: &str) -> String {
    let sql = "SELECT id, total_cents FROM ledger_orders WHERE id = $1";
    format!("{sql}:{id}")
}
"#,
    );

    let ts_client = buf(
        "web/src/ordersClient.ts",
        r#"
export async function fetchOrderDetails(id: string): Promise<Response> {
    return fetch(`/api/orders/${id}`);
}
"#,
    );

    let sql_migration = buf(
        "migrations/001_orders.sql",
        "CREATE TABLE IF NOT EXISTS ledger_orders (id TEXT PRIMARY KEY, total_cents BIGINT NOT NULL);\n",
    );

    let env_template = buf(
        ".env.example",
        "ORDER_WEBHOOK_URL=https://hooks.internal.invalid/placeholder\n",
    );

    let py_worker = buf(
        "workers/reconcile.py",
        r#"
import os

class OrderReconciler:
    """Reconciles pending order webhooks."""
    def run_batch(self) -> str:
        return os.getenv("ORDER_WEBHOOK_URL") or ""
"#,
    );

    let go_cli = buf(
        "tools/verify.go",
        r#"
package main

import "os"

func ReadWebhookConfig() string {
    return os.Getenv("ORDER_WEBHOOK_URL")
}
"#,
    );

    let extracted = vec![
        registry.extract_file(&rust_api),
        registry.extract_file(&rust_repo),
        registry.extract_file(&ts_client),
        registry.extract_file(&sql_migration),
        registry.extract_file(&env_template),
        registry.extract_file(&py_worker),
        registry.extract_file(&go_cli),
    ];

    // Verify `.env.example` never captures raw placeholder values in contract snippets.
    assert!(
        !extracted
            .iter()
            .flat_map(|f| &f.contracts)
            .any(|c| c.snippet.contains("hooks.internal.invalid"))
    );

    let manifest = detect_manifest(
        "Cargo.toml",
        "[package]\nname = \"demo-orders\"\nversion = \"0.1.0\"\n",
    );
    let manifests: Vec<_> = manifest.into_iter().collect();

    let graph = resolver.resolve(&extracted, &manifests);

    // 1. Rust -> Rust import-scoped call (`get_order` -> `load_order_row`).
    assert!(graph.edges.iter().any(|e| {
        e.relation == RelationKind::Calls
            && e.src_file == "src/api/orders.rs"
            && e.dst_file == "src/db/orders_repo.rs"
            && matches!(
                e.confidence,
                EdgeConfidence::ScopedHeuristic | EdgeConfidence::Exact
            )
    }));

    // 2. TypeScript `fetch` -> Rust `#[get("/api/orders/:id")]` cross-language HTTP contract.
    assert!(graph.edges.iter().any(|e| {
        e.relation == RelationKind::HttpEndpoint
            && e.src_file == "web/src/ordersClient.ts"
            && e.dst_file == "src/api/orders.rs"
    }));

    // 3. Rust SQL DML -> SQL DDL migration table contract (`ledger_orders`).
    assert!(graph.edges.iter().any(|e| {
        e.relation == RelationKind::SqlTableAccess
            && e.src_file == "src/db/orders_repo.rs"
            && e.dst_file == "migrations/001_orders.sql"
            && e.confidence == EdgeConfidence::VerifiedPair
    }));

    // 4. `.env.example` -> Rust, Python, and Go `ORDER_WEBHOOK_URL` config contracts.
    for consumer in [
        "src/api/orders.rs",
        "workers/reconcile.py",
        "tools/verify.go",
    ] {
        assert!(
            graph.edges.iter().any(|e| {
                e.relation == RelationKind::ConfigEnvKey
                    && e.src_file == consumer
                    && e.dst_file == ".env.example"
            }),
            "missing ConfigEnvKey edge for {consumer}"
        );
    }
}

#[test]
fn interface_hash_ignores_private_body_edits_and_changes_on_pub_signature_edit() {
    let registry = ParserRegistry::new();

    let v1 = registry.extract_file(&buf(
        "src/math.rs",
        "pub fn compute_fee(cents: i64) -> i64 {\n    cents * 2\n}\n",
    ));
    let v2_body_only = registry.extract_file(&buf(
        "src/math.rs",
        "pub fn compute_fee(cents: i64) -> i64 {\n    let fee = cents * 3;\n    fee + 1\n}\n",
    ));
    let v3_sig_changed = registry.extract_file(&buf(
        "src/math.rs",
        "pub fn compute_fee(cents: i64, bps: i64) -> i64 {\n    (cents * bps) / 10_000\n}\n",
    ));

    assert_ne!(v1.file.content_hash, v2_body_only.file.content_hash);
    assert_eq!(v1.file.interface_hash, v2_body_only.file.interface_hash);
    assert_ne!(v1.file.interface_hash, v3_sig_changed.file.interface_hash);
}

#[test]
fn broken_syntax_recovers_partial_ast_and_unimported_duplicates_mark_ambiguous() {
    let registry = ParserRegistry::new();
    let resolver = ScopedResolver::new();

    let broken_caller = registry.extract_file(&buf(
        "src/caller.rs",
        "pub fn run() {\n    validate_token(\"abc\");\n}\nfn incomplete( {\n",
    ));
    assert_eq!(
        broken_caller.file.coverage,
        ParseCoverage::PartialAstWithSyntaxErrors
    );

    let mod_a = registry.extract_file(&buf(
        "src/auth_a.rs",
        "pub fn validate_token(tok: &str) -> bool { !tok.is_empty() }\n",
    ));
    let mod_b = registry.extract_file(&buf(
        "src/auth_b.rs",
        "pub fn validate_token(tok: &str) -> bool { tok.len() > 3 }\n",
    ));

    let graph = resolver.resolve(&[broken_caller, mod_a, mod_b], &[]);
    assert!(graph.edges.iter().any(|e| {
        e.relation == RelationKind::Calls
            && e.src_file == "src/caller.rs"
            && e.confidence == EdgeConfidence::Ambiguous { candidate_count: 2 }
    }));
}
