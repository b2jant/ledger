//! End-to-end tests for the per-workspace Owner Daemon (`fd-lock` single-owner exclusivity,
//! versioned Unix-socket handshake, RPC queries, and drain-before-unlock idle shutdown).

use std::fs;
use std::time::Duration;

use ledger_core::{
    AnchorInput, ContextRequest, ExpandDelivery, ExpandRequest, FocusMode, QueryMode, WorkspaceId,
};
use ledger_daemon::{
    DaemonError, LedgerService, SocketClient, resolve_daemon_socket_path, run_daemon_server,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_enforces_single_owner_lock_serves_rpc_and_drains_cleanly()
-> Result<(), Box<dyn std::error::Error>> {
    let tmp = tempfile::tempdir()?;
    let root = std::fs::canonicalize(tmp.path())?;
    let ws_id = WorkspaceId::from_canonical_path(&root);
    let expected_sock = resolve_daemon_socket_path(&root, &ws_id)?;

    fs::create_dir_all(root.join("src"))?;
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"ipc-demo\"\nversion = \"0.1.0\"\n",
    )?;
    fs::write(
        root.join("src/lib.rs"),
        "pub fn verify_signature(payload: &str) -> bool {\n    payload.starts_with(\"sig_\")\n}\n",
    )?;

    let root_for_owner = root.clone();
    let owner_handle = tokio::spawn(async move {
        run_daemon_server(&root_for_owner, Duration::from_millis(700)).await
    });

    // Wait until the daemon socket is bound and answering handshakes.
    let mut connected: Option<SocketClient> = None;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(30)).await;
        let r = root.clone();
        if let Ok(Ok(c)) =
            tokio::task::spawn_blocking(move || SocketClient::connect_existing(&r)).await
        {
            connected = Some(c);
            break;
        }
    }
    let client = connected.ok_or("daemon socket did not become ready in time")?;
    assert!(expected_sock.exists());

    // Second daemon on the same workspace MUST fail immediately with `DaemonError::AlreadyRunning`.
    let second_attempt = run_daemon_server(&root, Duration::from_millis(200)).await;
    assert!(
        matches!(second_attempt, Err(DaemonError::AlreadyRunning)),
        "expected AlreadyRunning, got {second_attempt:?}"
    );

    // Exercise `LedgerService` over the Unix socket inside `spawn_blocking`.
    let c_rpc = client.clone();
    let (generation, pack, stats) =
        tokio::task::spawn_blocking(move || -> Result<_, DaemonError> {
            let g = c_rpc.index_now()?;
            let pack = c_rpc.query_context(&ContextRequest {
                task: "Check signature prefix verification".to_string(),
                anchors: vec![AnchorInput::Plain("verify_signature".to_string())],
                mode: QueryMode::Implement,
                focus_paths: Vec::new(),
                focus_mode: FocusMode::Boost,
                max_approx_tokens: 2000,
            })?;
            let _exp = c_rpc.expand(&ExpandRequest {
                handle: pack.handle.to_string(),
                items: vec!["slice:src/lib.rs:1-3".to_string()],
                delivery: ExpandDelivery::Complete,
                max_approx_tokens: 1200,
            })?;
            let stats = c_rpc.status()?;
            Ok((g, pack, stats))
        })
        .await??;

    assert!(generation >= 1);
    assert_eq!(pack.edit_targets.len(), 1);
    assert_eq!(pack.edit_targets[0].rel_path, "src/lib.rs");
    assert!(stats.total_queries_served >= 2);

    // Let the daemon reach its 700ms idle timeout and verify drain-before-unlock unlinks the socket.
    owner_handle.await??;
    assert!(!expected_sock.exists());

    Ok(())
}
