//! MCP frame allocation is bounded without imposing a session idle timeout.

use std::cell::Cell;
use std::io::{BufRead, Cursor, Read};
use std::rc::Rc;

use ledger_daemon::{EmbeddedClient, WorkspaceEngine};
use ledger_mcp::{MAX_MCP_FRAME_BYTES, McpServer};
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct EndlessFrame {
    consumed: Rc<Cell<usize>>,
    chunk: [u8; 8192],
}
impl Read for EndlessFrame {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = buffer.len().min(self.chunk.len());
        buffer[..count].copy_from_slice(&self.chunk[..count]);
        self.consume(count);
        Ok(count)
    }
}
impl BufRead for EndlessFrame {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        Ok(&self.chunk)
    }
    fn consume(&mut self, count: usize) {
        self.consumed.set(self.consumed.get() + count);
    }
}

#[test]
fn endless_unterminated_frame_stops_at_limit_without_unbounded_read() -> TestResult {
    let temp = tempfile::tempdir()?;
    let engine = WorkspaceEngine::open(temp.path())?;
    let server = McpServer::new(EmbeddedClient::new(engine));
    let consumed = Rc::new(Cell::new(0));
    let input = EndlessFrame {
        consumed: Rc::clone(&consumed),
        chunk: [b'x'; 8192],
    };
    let mut output = Vec::new();
    let error = server
        .serve_stdio(input, &mut output)
        .err()
        .ok_or("oversized frame accepted")?;
    assert!(error.to_string().contains("1 MiB"));
    assert!(consumed.get() <= MAX_MCP_FRAME_BYTES);
    assert!(output.is_empty());
    Ok(())
}

#[test]
fn incomplete_eof_is_rejected_and_large_valid_frame_is_accepted() -> TestResult {
    let temp = tempfile::tempdir()?;
    let engine = WorkspaceEngine::open(temp.path())?;
    let server = McpServer::new(EmbeddedClient::new(engine));
    let partial = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}";
    assert!(
        server
            .serve_stdio(Cursor::new(partial), Vec::new())
            .is_err()
    );
    let base = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}";
    let mut frame = base.to_string();
    frame.push_str(&" ".repeat(MAX_MCP_FRAME_BYTES - base.len() - 1));
    frame.push('\n');
    let mut output = Vec::new();
    server.serve_stdio(Cursor::new(frame), &mut output)?;
    assert_eq!(serde_json::from_slice::<Value>(&output)?["id"], 1);
    Ok(())
}

#[test]
fn initialize_does_not_echo_an_unsupported_protocol_and_notifications_stay_silent() -> TestResult {
    let temp = tempfile::tempdir()?;
    let server = McpServer::new(EmbeddedClient::new(WorkspaceEngine::open(temp.path())?));
    let frames = format!(
        "{}\n{}\n",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"unsupported"}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    );
    let mut output = Vec::new();
    server.serve_stdio(Cursor::new(frames), &mut output)?;
    let text = String::from_utf8(output)?;
    assert_eq!(text.lines().count(), 1);
    let response: Value = serde_json::from_str(text.trim())?;
    assert_eq!(response["result"]["protocolVersion"], "2025-06-18");
    Ok(())
}
