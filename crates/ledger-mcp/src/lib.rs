//! Composable Model Context Protocol (MCP) JSON-RPC 2.0 `stdio` server exposing:
//! - 2 read-only tools: `ledger_context` and `ledger_expand`
//! - 2 Augment-style ✨ prompt-enhancement slash prompts: `enhance` and `plan`
//!
//! Parameterized over [`LedgerService`] and generic `BufRead` / `Write` streams so
//! protocol compliance and harness interactions are 100% unit-testable in memory.

use std::io::{BufRead, Write};

use ledger_core::{ContextRequest, ExpandRequest, PackFormatter, QueryMode};
use ledger_daemon::LedgerService;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

/// Maximum newline-delimited incoming MCP frame (including newline).
pub const MAX_MCP_FRAME_BYTES: usize = 1024 * 1024;
/// Maximum encoded outgoing MCP frame (including newline).
pub const MAX_MCP_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Errors emitted if the underlying `stdio` transport breaks.
#[derive(Debug, Error)]
pub enum McpError {
    /// Underlying stream read/write error.
    #[error("MCP stdio I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialization failure.
    #[error("MCP JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Incoming JSON-RPC 2.0 message frame.
#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

/// Outgoing JSON-RPC 2.0 response frame.
#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcErrorObj>,
}

#[derive(Debug, Serialize)]
struct JsonRpcErrorObj {
    code: i32,
    message: String,
}

/// Composable MCP server wrapping any [`LedgerService`] implementation.
#[derive(Debug)]
pub struct McpServer<S: LedgerService> {
    service: S,
}

impl<S: LedgerService> McpServer<S> {
    /// Wraps `service` in an `McpServer`.
    #[must_use]
    pub const fn new(service: S) -> Self {
        Self { service }
    }

    /// Processes newline-delimited JSON-RPC 2.0 MCP frames from `reader` and writes
    /// responses to `writer` until EOF.
    pub fn serve_stdio<R: BufRead, W: Write>(
        &self,
        mut reader: R,
        mut writer: W,
    ) -> Result<(), McpError> {
        while let Some(frame) = read_frame(&mut reader)? {
            let raw = std::str::from_utf8(&frame).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "MCP frame must be UTF-8")
            })?;
            if raw.trim().is_empty() {
                continue;
            }
            if let Some(response) = self.handle_raw_frame(raw.trim()) {
                let mut encoded = BoundedResponse(Vec::new());
                serde_json::to_writer(&mut encoded, &response)?;
                encoded.0.push(b'\n');
                writer.write_all(&encoded.0)?;
                writer.flush()?;
            }
        }
        Ok(())
    }

    fn handle_raw_frame(&self, raw: &str) -> Option<JsonRpcResponse> {
        let req: JsonRpcRequest = match serde_json::from_str(raw) {
            Ok(r) => r,
            Err(e) => {
                return Some(JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: Value::Null,
                    result: None,
                    error: Some(JsonRpcErrorObj {
                        code: -32700,
                        message: format!("parse error: {e}"),
                    }),
                });
            }
        };

        // JSON-RPC notifications (no `id`, e.g., `notifications/initialized`, `notifications/cancelled`)
        // must never receive a response frame.
        let id = req.id?;

        let outcome = self.dispatch_method(&req.method, &req.params);
        Some(match outcome {
            Ok(val) => JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: Some(val),
                error: None,
            },
            Err((code, message)) => JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: None,
                error: Some(JsonRpcErrorObj { code, message }),
            },
        })
    }

    fn dispatch_method(&self, method: &str, params: &Value) -> Result<Value, (i32, String)> {
        match method {
            "initialize" => {
                let client_proto = "2025-06-18";
                Ok(json!({
                    "protocolVersion": client_proto,
                    "capabilities": {
                        "tools": { "listChanged": false },
                        "prompts": { "listChanged": false }
                    },
                    "serverInfo": {
                        "name": "ledger",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "instructions": "Use `ledger_context` first for unfamiliar feature, bugfix, or blast-radius work to obtain complete `EDIT_TARGETS`, cross-file `REFERENCE_CONTEXT` (callers & HTTP/SQL/Config contracts), and `TEST_CANDIDATES` in one call. Use batched `ledger_expand` with the returned `handle` only for explicitly omitted items."
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(Self::tools_list_payload()),
            "tools/call" => self.handle_tool_call(params),
            "prompts/list" => Ok(Self::prompts_list_payload()),
            "prompts/get" => self.handle_prompt_get(params),
            _ => Err((-32601, format!("method not found: {method}"))),
        }
    }

    fn tools_list_payload() -> Value {
        json!({
            "tools": [
                {
                    "name": "ledger_context",
                    "description": "Compile a fresh, token-budgeted evidence pack separating `EDIT_TARGETS` (complete functions to modify), `REFERENCE_CONTEXT` (callers & cross-boundary HTTP/SQL/Config contracts), and `TEST_CANDIDATES`. Pass `mode: \"enhance\"` for a compact prompt-enhancement blueprint.",
                    "annotations": {
                        "title": "LEDGER Context Compiler",
                        "readOnlyHint": true,
                        "destructiveHint": false,
                        "idempotentHint": true,
                        "openWorldHint": false
                    },
                    "inputSchema": {
                        "type": "object",
                        "required": ["task"],
                        "properties": {
                            "task": {
                                "type": "string",
                                "description": "Concrete coding goal, bug description, or rough user prompt."
                            },
                            "anchors": {
                                "type": "array",
                                "description": "Known file paths, symbol names, HTTP routes, or exact error literals.",
                                "items": {
                                    "anyOf": [
                                        { "type": "string" },
                                        {
                                            "type": "object",
                                            "required": ["value"],
                                            "properties": {
                                                "kind": {
                                                    "type": "string",
                                                    "enum": ["auto", "literal", "path", "symbol", "route"]
                                                },
                                                "value": { "type": "string" }
                                            }
                                        }
                                    ]
                                }
                            },
                            "mode": {
                                "type": "string",
                                "enum": ["auto", "enhance", "implement", "debug", "blast_radius", "history"],
                                "default": "auto"
                            },
                            "focus_paths": {
                                "type": "array",
                                "items": { "type": "string" }
                            },
                            "focus_mode": {
                                "type": "string",
                                "enum": ["boost", "restrict"],
                                "default": "boost"
                            },
                            "max_approx_tokens": {
                                "type": "integer",
                                "default": 2800,
                                "minimum": 400,
                                "maximum": 12000
                            }
                        }
                    }
                },
                {
                    "name": "ledger_expand",
                    "description": "Batch-fetch complete function bodies (`body:sym_...`), additional callers (`callers:sym_...`), or file slices (`slice:path:start-end`) advertised by a `ledger_context` snapshot handle.",
                    "annotations": {
                        "title": "LEDGER Snapshot Expansion",
                        "readOnlyHint": true,
                        "destructiveHint": false,
                        "idempotentHint": true,
                        "openWorldHint": false
                    },
                    "inputSchema": {
                        "type": "object",
                        "required": ["handle", "items"],
                        "properties": {
                            "handle": {
                                "type": "string",
                                "description": "Snapshot handle from `ledger_context` (`snap:...`)."
                            },
                            "items": {
                                "type": "array",
                                "maxItems": 16,
                                "items": { "type": "string" }
                            },
                            "delivery": {
                                "type": "string",
                                "enum": ["complete", "delta"],
                                "default": "complete"
                            },
                            "max_approx_tokens": {
                                "type": "integer",
                                "default": 2400,
                                "minimum": 300,
                                "maximum": 12000
                            }
                        }
                    }
                }
            ]
        })
    }

    fn handle_tool_call(&self, params: &Value) -> Result<Value, (i32, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| (-32602, "missing tool `name`".to_string()))?;
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));

        match name {
            "ledger_context" => {
                let req: ContextRequest = serde_json::from_value(args)
                    .map_err(|e| (-32602, format!("invalid ledger_context arguments: {e}")))?;
                match self.service.query_context(&req) {
                    Ok(pack) => {
                        let text = PackFormatter::render_context_pack(&pack);
                        Ok(json!({
                            "content": [{ "type": "text", "text": text }],
                            "isError": false
                        }))
                    }
                    Err(e) => Ok(json!({
                        "content": [{ "type": "text", "text": format!("LEDGER_ERROR: {e}") }],
                        "isError": true
                    })),
                }
            }
            "ledger_expand" => {
                let req: ExpandRequest = serde_json::from_value(args)
                    .map_err(|e| (-32602, format!("invalid ledger_expand arguments: {e}")))?;
                match self.service.expand(&req) {
                    Ok(resp) => {
                        let text = PackFormatter::render_expand_response(&resp);
                        Ok(json!({
                            "content": [{ "type": "text", "text": text }],
                            "isError": false
                        }))
                    }
                    Err(e) => Ok(json!({
                        "content": [{ "type": "text", "text": format!("LEDGER_EXPAND_ERROR: {e}") }],
                        "isError": true
                    })),
                }
            }
            _ => Err((-32602, format!("unknown tool: {name}"))),
        }
    }

    fn prompts_list_payload() -> Value {
        json!({
            "prompts": [
                {
                    "name": "enhance",
                    "description": "✨ Ground a rough user task in exact repository files to edit, files/contracts to reference, and verification tests before executing.",
                    "arguments": [
                        {
                            "name": "task",
                            "description": "Rough task or feature/bug description.",
                            "required": true
                        }
                    ]
                },
                {
                    "name": "plan",
                    "description": "✨ Generate a reviewable, file-anchored implementation plan grounded in LEDGER's graph without modifying files yet.",
                    "arguments": [
                        {
                            "name": "task",
                            "description": "Rough task or feature/bug description.",
                            "required": true
                        }
                    ]
                }
            ]
        })
    }

    fn handle_prompt_get(&self, params: &Value) -> Result<Value, (i32, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| (-32602, "missing prompt `name`".to_string()))?;
        let task = params
            .get("arguments")
            .and_then(|a| a.get("task"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                (
                    -32602,
                    "missing required prompt argument `task`".to_string(),
                )
            })?;

        if !matches!(name, "enhance" | "plan") {
            return Err((-32602, format!("unknown prompt: {name}")));
        }

        let req = ContextRequest {
            task: task.to_string(),
            anchors: Vec::new(),
            mode: QueryMode::Enhance,
            focus_paths: Vec::new(),
            focus_mode: ledger_core::FocusMode::Boost,
            max_approx_tokens: 1100,
        };

        let blueprint = match self.service.query_context(&req) {
            Ok(pack) => PackFormatter::render_enhance_blueprint(&pack),
            Err(e) => format!("[LEDGER_ENHANCE_FALLBACK: {e}]"),
        };

        let directive = if name == "plan" {
            "First output a concise, reviewable Enhanced Task Brief & Plan anchored to the exact `FILES_TO_EDIT`, `FILES_TO_REFERENCE_AND_RESPECT`, and `VERIFICATION_COMMANDS` below, and wait for confirmation before editing."
        } else {
            "First present a 4-6 line Enhanced Task Summary anchored to the exact `FILES_TO_EDIT` and `FILES_TO_REFERENCE_AND_RESPECT` below, call `ledger_expand` on the handle if you need full bodies, and carry out the task without redundant `grep` scans."
        };

        let prompt_text = format!(
            "User Request: {task}\n\n{directive}\n\n--- Repository Grounding (untrusted repo evidence) ---\n{blueprint}"
        );

        Ok(json!({
            "description": format!("LEDGER grounded prompt enhancement for: {task}"),
            "messages": [
                {
                    "role": "user",
                    "content": {
                        "type": "text",
                        "text": prompt_text
                    }
                }
            ]
        }))
    }
}

// No idle timeout: an MCP session may remain open between requests indefinitely.
fn read_frame(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, std::io::Error> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "MCP frame requires a terminating newline",
                ))
            };
        }
        let count = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |i| i + 1);
        if frame.len().saturating_add(count) > MAX_MCP_FRAME_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "MCP request exceeds 1 MiB frame limit",
            ));
        }
        let complete = available[count - 1] == b'\n';
        frame.extend_from_slice(&available[..count]);
        reader.consume(count);
        if complete {
            return Ok(Some(frame));
        }
    }
}
struct BoundedResponse(Vec<u8>);
impl Write for BoundedResponse {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) >= MAX_MCP_RESPONSE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "MCP response exceeds 4 MiB frame limit; reduce requested output",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
