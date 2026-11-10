//! Regression tests for IPC ownership, identity binding, limits and reconciliation.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ledger_core::{ContextRequest, WorkspaceId};
use ledger_daemon::ipc::{MAX_IPC_FRAME_BYTES, PROTOCOL_VERSION};
use ledger_daemon::{
    DaemonClient, DaemonError, DaemonHandshake, DaemonOptions, DaemonRequest, DaemonResponse,
    LedgerService, SocketClient, resolve_daemon_socket_path, run_daemon_server_with_options,
};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

struct Owner(tokio::task::JoinHandle<Result<(), DaemonError>>);
impl Drop for Owner {
    fn drop(&mut self) {
        self.0.abort();
    }
}
fn fixture() -> Result<tempfile::TempDir, std::io::Error> {
    let temp = tempfile::tempdir()?;
    std::fs::write(
        temp.path().join("one.rs"),
        "pub fn first_target() -> u32 { 1 }\n",
    )?;
    std::fs::write(
        temp.path().join("two.rs"),
        "pub fn other_target() -> u32 { 2 }\n",
    )?;
    Ok(temp)
}
fn options() -> DaemonOptions {
    let mut options = DaemonOptions::new(Duration::from_secs(3));
    options.refresh_interval = Duration::from_millis(80);
    options.frame_timeout = Duration::from_millis(150);
    options.write_timeout = Duration::from_millis(150);
    options
}
fn start(root: &Path, options: DaemonOptions) -> Owner {
    let root = root.to_path_buf();
    Owner(tokio::spawn(async move {
        run_daemon_server_with_options(&root, options).await
    }))
}
async fn connect(root: &Path) -> Result<SocketClient, Box<dyn std::error::Error + Send + Sync>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let root = root.to_path_buf();
        if let Ok(client) =
            tokio::task::spawn_blocking(move || SocketClient::connect_existing(&root)).await?
        {
            return Ok(client);
        }
        if Instant::now() >= deadline {
            return Err("owner did not become ready".into());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
async fn rpc<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, DaemonError> + Send + 'static,
) -> Result<T, Box<dyn std::error::Error + Send + Sync>> {
    Ok(tokio::task::spawn_blocking(f).await??)
}
fn socket(root: &Path) -> Result<PathBuf, std::io::Error> {
    let root = root.canonicalize()?;
    resolve_daemon_socket_path(&root, &WorkspaceId::from_canonical_path(&root))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn polling_discovers_new_and_non_target_edits_without_querying_them() -> TestResult {
    let temp = fixture()?;
    let mut owner = start(temp.path(), options());
    let client = connect(temp.path()).await?;
    let c = client.clone();
    rpc(move || c.index_now()).await?;
    let c = client.clone();
    let initial = rpc(move || c.status()).await?;
    std::fs::write(
        temp.path().join("two.rs"),
        "pub fn changed_other() -> u32 { 3 }\npub fn added_other() {}\n",
    )?;
    std::fs::write(
        temp.path().join("new.rs"),
        "pub fn discovered_new_file() {}\n",
    )?;
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let c = client.clone();
        let stats = rpc(move || c.status()).await?;
        if stats.file_count > initial.file_count && stats.symbol_count >= initial.symbol_count + 2 {
            break;
        }
        if Instant::now() >= deadline {
            return Err("polling failed to discover files/symbols without a target query".into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let c = client.clone();
    let pack = rpc(move || {
        c.query_context(&ContextRequest {
            task: "changed_other".into(),
            anchors: vec![ledger_core::AnchorInput::Plain("changed_other".into())],
            mode: ledger_core::QueryMode::Implement,
            focus_paths: Vec::new(),
            focus_mode: ledger_core::FocusMode::Boost,
            max_approx_tokens: 2000,
        })
    })
    .await?;
    assert!(
        pack.edit_targets
            .iter()
            .any(|target| target.rel_path == "two.rs")
    );
    rpc(move || client.shutdown()).await?;
    (&mut owner.0).await??;
    assert!(!socket(temp.path())?.exists());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn polling_ticks_do_not_extend_idle_deadline() -> TestResult {
    let temp = fixture()?;
    let mut options = options();
    options.idle_timeout = Duration::from_millis(300);
    options.refresh_interval = Duration::from_millis(50);
    let start_time = Instant::now();
    let mut owner = start(temp.path(), options);
    let _ = connect(temp.path()).await?;
    tokio::time::timeout(Duration::from_secs(2), &mut owner.0).await???;
    assert!(start_time.elapsed() < Duration::from_secs(2));
    assert!(!socket(temp.path())?.exists());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_and_oversized_frames_are_bounded_and_do_not_block_shutdown() -> TestResult {
    let temp = fixture()?;
    let mut owner = start(temp.path(), options());
    let client = connect(temp.path()).await?;
    let path = socket(temp.path())?;
    tokio::task::spawn_blocking(move || -> TestResult {
        let mut partial = std::os::unix::net::UnixStream::connect(&path)?;
        partial.set_read_timeout(Some(Duration::from_secs(2)))?;
        partial.write_all(b"{\"op\":")?;
        let mut response = Vec::new();
        partial.read_to_end(&mut response)?;
        assert!(
            response.is_empty(),
            "partial frame timed out without dispatch"
        );
        let mut huge = std::os::unix::net::UnixStream::connect(&path)?;
        huge.set_read_timeout(Some(Duration::from_secs(2)))?;
        huge.write_all(&vec![b'x'; MAX_IPC_FRAME_BYTES + 1])?;
        let mut response = String::new();
        BufReader::new(huge).read_line(&mut response)?;
        assert!(response.contains("oversized"));
        Ok(())
    })
    .await?
    .map_err(|error| error.to_string())?;
    rpc(move || client.shutdown()).await?;
    tokio::time::timeout(Duration::from_secs(3), &mut owner.0).await???;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_client_cannot_use_a_replacement_owner_or_database() -> TestResult {
    let temp = fixture()?;
    let mut owner = start(temp.path(), options());
    let stale = connect(temp.path()).await?;
    let c = stale.clone();
    rpc(move || c.index_now()).await?;
    let old_incarnation = stale.verify_handshake()?.db_incarnation;
    let c = stale.clone();
    rpc(move || c.shutdown()).await?;
    (&mut owner.0).await??;
    std::fs::rename(
        temp.path().join(".ledger/ledger.db"),
        temp.path().join(".ledger/old.db"),
    )?;
    let mut replacement = start(temp.path(), options());
    let current = connect(temp.path()).await?;
    assert_ne!(current.verify_handshake()?.db_incarnation, old_incarnation);
    let rejected = tokio::task::spawn_blocking(move || stale.status()).await?;
    assert!(matches!(rejected, Err(DaemonError::HandshakeMismatch(_))));
    rpc(move || current.shutdown()).await?;
    (&mut replacement.0).await??;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn database_replacement_while_running_is_rejected_before_execution() -> TestResult {
    let temp = fixture()?;
    let mut owner = start(temp.path(), options());
    let client = connect(temp.path()).await?;
    let c = client.clone();
    rpc(move || c.index_now()).await?;
    let db = temp.path().join(".ledger/ledger.db");
    std::fs::rename(&db, temp.path().join(".ledger/removed.db"))?;
    std::fs::write(db, b"replacement is deliberately invalid")?;
    let result = tokio::task::spawn_blocking(move || client.status()).await?;
    assert!(matches!(
        result,
        Err(DaemonError::HandshakeMismatch(_)) | Err(DaemonError::Io(_))
    ));
    // A checkpoint can report a replaced file error, but shutdown must still drain/unlink.
    let _ = tokio::time::timeout(Duration::from_secs(3), &mut owner.0).await??;
    assert!(!socket(temp.path())?.exists());
    Ok(())
}

fn fake_handshake(root: &Path, workspace: String, capable: bool) -> TestResult {
    std::fs::create_dir_all(root.join(".ledger/runtime"))?;
    let path = socket(root)?;
    let listener = std::os::unix::net::UnixListener::bind(&path)?;
    let removed_path = path.clone();
    let peer = std::thread::spawn(move || -> Result<(), std::io::Error> {
        let (mut stream, _) = listener.accept()?;
        let mut request = String::new();
        BufReader::new(stream.try_clone()?).read_line(&mut request)?;
        let response = serde_json::json!({"type":"handshake", "protocol_version":1,"schema_version":0,"workspace_id":workspace,"db_incarnation":"legacy", "capabilities": if capable {vec!["bound_shutdown_v1"]} else {vec![]}});
        writeln!(stream, "{response}")?;
        Ok(())
    });
    let result = DaemonClient::connect(root);
    assert!(matches!(result, Err(DaemonError::HandshakeMismatch(_))));
    assert!(
        !root.join(".ledger/ledger.db").exists(),
        "mismatch must never initialize a second writer"
    );
    peer.join().map_err(|_| "fake owner panicked")??;
    std::fs::remove_file(removed_path)?;
    Ok(())
}

#[test]
fn unrelated_and_legacy_owners_fail_without_opening_a_database() -> TestResult {
    let temp = fixture()?;
    fake_handshake(temp.path(), "unrelated_workspace".into(), true)?;
    let root = temp.path().canonicalize()?;
    fake_handshake(
        temp.path(),
        WorkspaceId::from_canonical_path(&root).to_string(),
        false,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_and_response_identities_are_both_required() -> TestResult {
    let temp = fixture()?;
    let mut owner = start(temp.path(), options());
    let client = connect(temp.path()).await?;
    let identity = client.verify_handshake()?;
    let path = socket(temp.path())?;
    let identity_for_fake = identity.clone();
    tokio::task::spawn_blocking(move || -> Result<(), std::io::Error> {
        let mut altered = identity;
        altered.db_incarnation = "stale".into();
        let mut stream = std::os::unix::net::UnixStream::connect(path)?;
        writeln!(
            stream,
            "{}",
            serde_json::to_string(&DaemonRequest::Bound {
                identity: altered,
                request: Box::new(DaemonRequest::IndexNow)
            })?
        )?;
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response)?;
        assert!(response.contains("mismatched owner identity"));
        Ok(())
    })
    .await??;
    rpc(move || client.shutdown()).await?;
    (&mut owner.0).await??;
    // A peer that accepts the request but lies about the response identity is rejected.
    let path = socket(temp.path())?;
    let listener = std::os::unix::net::UnixListener::bind(&path)?;
    let fake = std::thread::spawn(move || -> Result<(), std::io::Error> {
        for request_number in 0..2 {
            let (mut stream, _) = listener.accept()?;
            let mut request = String::new();
            BufReader::new(stream.try_clone()?).read_line(&mut request)?;
            let response = if request_number == 0 {
                DaemonResponse::Handshake(identity_for_fake.clone())
            } else {
                let mut changed = identity_for_fake.clone();
                changed.db_incarnation = "wrong".into();
                DaemonResponse::Bound {
                    identity: changed,
                    response: Box::new(DaemonResponse::Indexed { generation: 999 }),
                }
            };
            writeln!(stream, "{}", serde_json::to_string(&response)?)?;
        }
        Ok(())
    });
    let root = temp.path().to_path_buf();
    let result =
        tokio::task::spawn_blocking(move || SocketClient::connect_existing(&root)?.index_now())
            .await?;
    assert!(matches!(result, Err(DaemonError::HandshakeMismatch(_))));
    fake.join().map_err(|_| "fake response owner panicked")??;
    std::fs::remove_file(path)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_server_still_drains_indexing_before_unlocking() -> TestResult {
    let temp = fixture()?;
    for i in 0..100 {
        std::fs::write(
            temp.path().join(format!("file{i}.rs")),
            format!("pub fn symbol_{i}() {{}}\n"),
        )?;
    }
    let mut slow_options = options();
    slow_options.idle_timeout = Duration::from_secs(10);
    let mut owner = start(temp.path(), slow_options);
    let _client = connect(temp.path()).await?;
    owner.0.abort();
    let _ = (&mut owner.0).await;
    assert!(!socket(temp.path())?.exists());
    // A replacement immediately acquires the lock, indexes and shuts down cleanly.
    let mut replacement = start(temp.path(), options());
    let client = connect(temp.path()).await?;
    let c = client.clone();
    rpc(move || c.index_now()).await?;
    rpc(move || client.shutdown()).await?;
    (&mut replacement.0).await??;
    Ok(())
}

// Keep public wire schema constructible independently of a live owner.
#[test]
fn legacy_handshake_defaults_do_not_advertise_shutdown() -> TestResult {
    let identity: DaemonHandshake = serde_json::from_value(serde_json::json!({
        "protocol_version": PROTOCOL_VERSION - 1, "schema_version": 1,
        "workspace_id": "workspace", "db_incarnation": "database"
    }))?;
    assert!(identity.binary_build_id.is_empty());
    assert!(identity.capabilities.is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_lock_owner_prevents_database_open_and_stop_claims() -> TestResult {
    let temp = fixture()?;
    let runtime = temp.path().join(".ledger/runtime");
    std::fs::create_dir_all(&runtime)?;
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(runtime.join("daemon.lock"))?;
    let mut lock = fd_lock::RwLock::new(lock_file);
    let _owner = lock.try_write()?;
    let database = temp.path().join(".ledger/ledger.db");
    std::fs::write(&database, b"do not open/migrate this sentinel")?;
    let result = run_daemon_server_with_options(temp.path(), options()).await;
    assert!(matches!(result, Err(DaemonError::AlreadyRunning)));
    assert_eq!(
        std::fs::read(database)?,
        b"do not open/migrate this sentinel"
    );
    let root = temp.path().to_path_buf();
    let result = tokio::task::spawn_blocking(move || SocketClient::stop_workspace(&root)).await?;
    assert!(
        matches!(result, Err(DaemonError::Remote(_))),
        "stop must report an unreachable lock owner rather than success"
    );
    Ok(())
}
