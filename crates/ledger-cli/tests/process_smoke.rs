//! Exercise the real binary, including spawned owner lifetime and clean MCP stdout.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use ledger_core::WorkspaceId;
use ledger_daemon::resolve_daemon_socket_path;
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn std::error::Error>>;
const BINARY: &str = env!("CARGO_BIN_EXE_ledger");

struct Process(Option<Child>);
impl Process {
    fn wait_output(mut self) -> Result<Output, std::io::Error> {
        self.0
            .take()
            .ok_or_else(|| std::io::Error::other("process already reaped"))?
            .wait_with_output()
    }
    fn child(&mut self) -> Result<&mut Child, std::io::Error> {
        self.0
            .as_mut()
            .ok_or_else(|| std::io::Error::other("process already reaped"))
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            if !matches!(child.try_wait(), Ok(Some(_))) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

struct Workspace {
    temp: tempfile::TempDir,
    children: Vec<Child>,
}
impl Workspace {
    fn new() -> Result<Self, std::io::Error> {
        let temp = tempfile::tempdir()?;
        std::fs::write(
            temp.path().join("source.rs"),
            "pub fn smoke_symbol(value: u32) -> u32 {\n    value + 1\n}\n",
        )?;
        Ok(Self {
            temp,
            children: Vec::new(),
        })
    }
    fn root(&self) -> &Path {
        self.temp.path()
    }
    fn command(&self) -> Command {
        let mut command = Command::new(BINARY);
        command.arg("--workspace").arg(self.root());
        command
    }
    fn run(&self, args: &[&str]) -> Result<Output, Box<dyn std::error::Error>> {
        let output = self.command().args(args).output()?;
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(output)
    }
    fn socket(&self) -> Result<PathBuf, std::io::Error> {
        let root = self.root().canonicalize()?;
        resolve_daemon_socket_path(&root, &WorkspaceId::from_canonical_path(&root))
    }
    fn identity(&self) -> Result<Value, Box<dyn std::error::Error>> {
        let mut stream = std::os::unix::net::UnixStream::connect(self.socket()?)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.write_all(b"{\"op\":\"handshake\"}\n")?;
        let mut frame = String::new();
        BufReader::new(stream).read_line(&mut frame)?;
        Ok(serde_json::from_str(&frame)?)
    }
    fn spawn_owner(&mut self) -> Result<usize, Box<dyn std::error::Error>> {
        let child = self
            .command()
            .args(["daemon", "--idle-seconds", "30"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let index = self.children.len();
        self.children.push(child);
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.identity().is_err() {
            if Instant::now() >= deadline {
                return Err("daemon startup timed out".into());
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        Ok(index)
    }
    fn wait_child(&mut self, index: usize) -> TestResult {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.children[index].try_wait()? {
                assert!(
                    status.success(),
                    "owner process exited unsuccessfully: {status}"
                );
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("owner process did not exit after negotiated shutdown".into());
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        // Clean up auto-spawned owners as well as explicitly owned children on assertion failure.
        let _ = self
            .command()
            .arg("stop")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        for child in &mut self.children {
            if !matches!(child.try_wait(), Ok(Some(_))) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

#[test]
fn cli_init_query_expand_mcp_and_stop_use_the_owner_process() -> TestResult {
    let workspace = Workspace::new()?;
    let init = workspace.run(&["init"])?;
    assert!(String::from_utf8(init.stdout)?.contains("Initialized LEDGER"));
    assert!(workspace.root().join(".ledger/ledger.db").exists());
    assert!(
        std::fs::read_to_string(workspace.root().join(".gitignore"))?.contains(".ledger/runtime/")
    );
    let output = workspace.run(&[
        "query",
        "smoke_symbol",
        "--anchor",
        "smoke_symbol",
        "--fresh",
    ])?;
    let text = String::from_utf8(output.stdout)?;
    assert!(text.contains("source.rs"));
    let handle = text
        .split("handle=")
        .nth(1)
        .and_then(|text| text.split_whitespace().next())
        .ok_or("query omitted handle")?;
    let expanded = workspace.run(&["expand", handle, "slice:source.rs:1-3"])?;
    assert!(String::from_utf8(expanded.stdout)?.contains("smoke_symbol"));
    let mcp = workspace
        .command()
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut mcp = Process(Some(mcp));
    let frames = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ledger_context","arguments":{"task":"smoke_symbol","anchors":["smoke_symbol"]}}}),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"ledger_expand","arguments":{"handle":handle,"items":["slice:source.rs:1-3"]}}}),
    ];
    {
        let stdin = mcp.child()?.stdin.as_mut().ok_or("missing MCP stdin")?;
        for frame in frames {
            writeln!(stdin, "{frame}")?;
        }
    }
    let output = mcp.wait_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout)?;
    let responses: Vec<Value> = text
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(
        responses.len(),
        4,
        "stdout contains exactly MCP response frames"
    );
    assert_eq!(
        responses[1]["result"]["tools"]
            .as_array()
            .ok_or("tools missing")?
            .len(),
        2
    );
    assert_eq!(responses[2]["result"]["isError"], false);
    assert_eq!(responses[3]["result"]["isError"], false);
    workspace.run(&["stop"])?;
    assert!(!workspace.socket()?.exists());
    Ok(())
}

#[test]
fn restart_drains_a_real_owner_and_old_process_exits() -> TestResult {
    let mut workspace = Workspace::new()?;
    let child = workspace.spawn_owner()?;
    workspace.run(&["index"])?;
    let before = workspace.identity()?;
    workspace.run(&["restart"])?;
    workspace.wait_child(child)?;
    let after = workspace.identity()?;
    assert_eq!(before["db_incarnation"], after["db_incarnation"]);
    assert_ne!(before["owner_id"], after["owner_id"]);
    workspace.run(&["status"])?;
    workspace.run(&["stop"])?;
    assert!(!workspace.socket()?.exists());
    Ok(())
}

#[test]
fn default_query_does_not_issue_an_unconditional_index_request() -> TestResult {
    let workspace = Workspace::new()?;
    // A scripted peer accepts precisely handshake + query. An unconditional index
    // changes the second operation and makes this test fail without relying on timing.
    std::fs::create_dir_all(workspace.root().join(".ledger/runtime"))?;
    let path = workspace.socket()?;
    let listener = std::os::unix::net::UnixListener::bind(&path)?;
    // Obtain the CLI executable fingerprint via an actual short-lived owner first.
    drop(listener);
    std::fs::remove_file(&path)?;
    workspace.run(&["status"])?;
    let identity = workspace.identity()?;
    workspace.run(&["stop"])?;
    let listener = std::os::unix::net::UnixListener::bind(&path)?;
    let peer = std::thread::spawn(move || -> Result<(), std::io::Error> {
        for operation in ["handshake", "query_context"] {
            let (mut stream, _) = listener.accept()?;
            let mut request = String::new();
            BufReader::new(stream.try_clone()?).read_line(&mut request)?;
            let request: Value = serde_json::from_str(&request)?;
            if operation == "handshake" {
                assert_eq!(request["op"], operation);
                writeln!(stream, "{identity}")?;
            } else {
                assert_eq!(request["request"]["op"], operation);
                let mut bound_identity = identity.clone();
                bound_identity
                    .as_object_mut()
                    .ok_or_else(|| std::io::Error::other("missing identity"))?
                    .remove("type");
                writeln!(
                    stream,
                    "{}",
                    json!({"type":"bound", "identity":bound_identity,"response":{"type":"error","message":"query observed without index"}})
                )?;
            }
        }
        Ok(())
    });
    let output = workspace
        .command()
        .args(["query", "smoke_symbol"])
        .output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("query observed without index"));
    peer.join().map_err(|_| "scripted owner panicked")??;
    std::fs::remove_file(path)?;
    Ok(())
}

#[test]
fn advertised_build_schema_upgrade_is_negotiated_before_new_owner_starts() -> TestResult {
    let workspace = Workspace::new()?;
    workspace.run(&["status"])?;
    let mut identity = workspace.identity()?;
    workspace.run(&["stop"])?;
    std::fs::remove_file(workspace.root().join(".ledger/ledger.db"))?;
    identity["binary_build_id"] = json!("different-build");
    identity["schema_version"] = json!(999);
    identity["protocol_version"] = json!(999);
    let path = workspace.socket()?;
    let listener = std::os::unix::net::UnixListener::bind(&path)?;
    let db_path = workspace.root().join(".ledger/ledger.db");
    let socket_for_peer = path.clone();
    let peer = std::thread::spawn(move || -> Result<(), std::io::Error> {
        for operation in ["handshake", "shutdown"] {
            let (mut stream, _) = listener.accept()?;
            let mut frame = String::new();
            BufReader::new(stream.try_clone()?).read_line(&mut frame)?;
            let request: Value = serde_json::from_str(&frame)?;
            if operation == "handshake" {
                writeln!(stream, "{identity}")?;
            } else {
                assert_eq!(request["op"], "bound");
                assert_eq!(request["request"]["op"], "shutdown");
                assert!(
                    !db_path.exists(),
                    "no new writer before negotiated shutdown"
                );
                let mut response_identity = identity.clone();
                response_identity
                    .as_object_mut()
                    .ok_or_else(|| std::io::Error::other("missing identity"))?
                    .remove("type");
                assert_eq!(request["identity"], response_identity);
                writeln!(
                    stream,
                    "{}",
                    json!({"type":"bound","identity":response_identity,"response":{"type":"shutting_down"}})
                )?;
            }
        }
        drop(listener);
        std::fs::remove_file(socket_for_peer)?;
        Ok(())
    });
    workspace.run(&["init"])?;
    peer.join().map_err(|_| "upgrade peer panicked")??;
    assert!(workspace.root().join(".ledger/ledger.db").exists());
    let current = workspace.identity()?;
    assert_ne!(current["binary_build_id"], "different-build");
    assert_ne!(current["schema_version"], 999);
    workspace.run(&["stop"])?;
    Ok(())
}

#[test]
fn mcp_stdio_remains_open_when_the_idle_owner_exits() -> TestResult {
    let mut workspace = Workspace::new()?;
    let owner = workspace
        .command()
        .args(["daemon", "--idle-seconds", "1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let owner_index = workspace.children.len();
    workspace.children.push(owner);
    let deadline = Instant::now() + Duration::from_secs(5);
    while workspace.identity().is_err() {
        if Instant::now() >= deadline {
            return Err("owner did not start".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let child = workspace
        .command()
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut proxy = Process(Some(child));
    // Wait for the owner to exit, while keeping stdin open without any frames.
    workspace.wait_child(owner_index)?;
    assert!(
        proxy.child()?.try_wait()?.is_none(),
        "idle stdio proxy was terminated"
    );
    writeln!(
        proxy.child()?.stdin.as_mut().ok_or("missing MCP stdin")?,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"ping"})
    )?;
    let output = proxy.wait_output()?;
    assert!(output.status.success());
    assert_eq!(serde_json::from_slice::<Value>(&output.stdout)?["id"], 1);
    Ok(())
}
