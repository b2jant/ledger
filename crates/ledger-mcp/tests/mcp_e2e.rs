//! End-to-end JSON-RPC 2.0 MCP `stdio` protocol, Augment-style `enhance` / `plan` prompts,
//! live dirty-buffer overlay (`STALE_AFTER_EDIT`), idempotent `ledger_expand`, and `.env` denial tests.

use std::fs;
use std::io::Cursor;

use ledger_daemon::{EmbeddedClient, WorkspaceEngine};
use ledger_mcp::McpServer;
use serde_json::{Value, json};

fn run_mcp_frames(
    server: &McpServer<EmbeddedClient>,
    frames: &[Value],
) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
    let mut input_buf = String::new();
    for f in frames {
        input_buf.push_str(&serde_json::to_string(f)?);
        input_buf.push('\n');
    }
    let mut out = Vec::new();
    server.serve_stdio(Cursor::new(input_buf.into_bytes()), &mut out)?;
    let text = String::from_utf8(out)?;
    let mut parsed = Vec::new();
    for line in text.lines() {
        if !line.trim().is_empty() {
            parsed.push(serde_json::from_str::<Value>(line)?);
        }
    }
    Ok(parsed)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_protocol_prompts_live_overlay_and_secret_denial_work_end_to_end()
-> Result<(), Box<dyn std::error::Error>> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();

    fs::create_dir_all(root.join("src"))?;
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"mcp-e2e\"\nversion = \"0.1.0\"\n",
    )?;
    fs::write(
        root.join(".env"),
        "STRIPE_SECRET_KEY=sk_live_NEVER_LEAK_999999\n",
    )?;
    fs::write(
        root.join("src/pricing.rs"),
        "pub fn compute_discount(subtotal: i64) -> i64 {\n    subtotal / 10\n}\n",
    )?;
    fs::write(
        root.join("src/checkout.rs"),
        "use crate::pricing::compute_discount;\n\npub fn finalize_total(subtotal: i64) -> i64 {\n    subtotal - compute_discount(subtotal)\n}\n",
    )?;

    let engine = WorkspaceEngine::open(root)?;
    let _ = engine.index_workspace()?;
    let server = McpServer::new(EmbeddedClient::new(engine));

    // 1. Handshake + silent notification + tools/list + prompts/list + prompts/get (`enhance`)
    let responses = run_mcp_frames(
        &server,
        &[
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": "2025-06-18" }
            }),
            // Notification (no `id`) MUST NOT produce a response frame!
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list"
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "prompts/get",
                "params": {
                    "name": "enhance",
                    "arguments": { "task": "Cap compute_discount at 5000 cents" }
                }
            }),
        ],
    )?;

    assert_eq!(
        responses.len(),
        3,
        "notification frame must not emit a response"
    );
    let tools = responses[1]["result"]["tools"]
        .as_array()
        .ok_or("missing tools array")?;
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["name"], "ledger_context");
    assert_eq!(tools[1]["name"], "ledger_expand");

    let enhance_msg = responses[2]["result"]["messages"][0]["content"]["text"]
        .as_str()
        .ok_or("missing enhance prompt text")?;
    assert!(enhance_msg.contains("[LEDGER_ENHANCE_BLUEPRINT"));
    assert!(enhance_msg.contains("src/pricing.rs"));
    assert!(enhance_msg.contains("src/checkout.rs"));
    assert!(!enhance_msg.contains("sk_live_NEVER_LEAK_999999"));

    // 2. Simulate an uncommitted dirty edit on disk changing `compute_discount`'s exported signature!
    fs::write(
        root.join("src/pricing.rs"),
        "pub fn compute_discount(subtotal: i64, max_cap: i64) -> i64 {\n    (subtotal / 10).min(max_cap)\n}\n",
    )?;

    let ctx_resps = run_mcp_frames(
        &server,
        &[json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "ledger_context",
                "arguments": {
                    "task": "Update compute_discount cap handling",
                    "anchors": ["compute_discount"]
                }
            }
        })],
    )?;

    let pack_text = ctx_resps[0]["result"]["content"][0]["text"]
        .as_str()
        .ok_or("missing ledger_context text")?;
    // Must serve fresh live dirty buffer immediately AND mark cross-file caller edge STALE_AFTER_EDIT!
    assert!(pack_text.contains("max_cap: i64"));
    assert!(
        pack_text.contains("READY_WITH_LIVE_OVERLAY") || pack_text.contains("STALE_AFTER_EDIT")
    );

    // Extract `handle=snap:...` from `[LEDGER handle=snap:... | ...]` header to test `ledger_expand`.
    let handle = pack_text
        .split("handle=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .ok_or("failed to parse handle from pack header")?
        .to_string();

    let exp_resps = run_mcp_frames(
        &server,
        &[json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": {
                "name": "ledger_expand",
                "arguments": {
                    "handle": handle,
                    "items": [
                        "slice:.env:1-5",
                        "slice:../etc/passwd:1-5",
                        "slice:src/checkout.rs:1-6"
                    ]
                }
            }
        })],
    )?;

    let exp_text = exp_resps[0]["result"]["content"][0]["text"]
        .as_str()
        .ok_or("missing ledger_expand text")?;
    assert!(exp_text.contains("status=DeniedByPolicy"));
    assert!(exp_text.contains("finalize_total"));
    assert!(!exp_text.contains("sk_live_NEVER_LEAK_999999"));

    Ok(())
}
