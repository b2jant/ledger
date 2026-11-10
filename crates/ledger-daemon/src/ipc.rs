//! Single-owner Unix IPC, identity-bound operations and drain-before-unlock shutdown.

use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use fd_lock::RwLock as FileLock;
use ledger_core::{ContextPack, ContextRequest, ExpandRequest, ExpandResponse, WorkspaceId};
use ledger_store::{SqliteStore, StoreStats};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::engine::{EngineError, WorkspaceEngine};

/// Version of the identity-bound request/response protocol.
pub const PROTOCOL_VERSION: u32 = 2;
/// Maximum encoded request or response frame, including its newline.
pub const MAX_IPC_FRAME_BYTES: usize = 4 * 1024 * 1024;
const SHUTDOWN_CAPABILITY: &str = "bound_shutdown_v1";
const CLIENT_TIMEOUT: Duration = Duration::from_secs(120);

/// Errors returned by daemon transport or lifecycle management.
#[derive(Debug, Error)]
pub enum DaemonError {
    /// Underlying engine error.
    #[error("engine error: {0}")]
    Engine(#[from] EngineError),
    /// Socket / lockfile I/O error.
    #[error("daemon I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON framing error.
    #[error("daemon wire error: {0}")]
    Json(#[from] serde_json::Error),
    /// Another process holds the workspace ownership lock.
    #[error("workspace daemon lock is already held by an active owner process")]
    AlreadyRunning,
    /// Protocol, build, workspace or database identity mismatch.
    #[error(
        "daemon identity mismatch: {0}; reconnect/re-query after `ledger restart`; a legacy owner must be stopped using its original executable (never delete a live lock)"
    )]
    HandshakeMismatch(String),
    /// Remote application error.
    #[error("daemon RPC error: {0}")]
    Remote(String),
}

/// Identity captured on connection and echoed on every operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonHandshake {
    /// Wire protocol version.
    pub protocol_version: u32,
    /// SQLite schema version.
    pub schema_version: u32,
    /// Canonical workspace ID.
    pub workspace_id: String,
    /// Database incarnation ID.
    pub db_incarnation: String,
    /// Executable fingerprint, cached once per process. Empty for legacy peers.
    #[serde(default)]
    pub binary_build_id: String,
    /// Unique owner process lifetime; reconnect after a restart.
    #[serde(default)]
    pub owner_id: String,
    /// Independently versioned capabilities, including safe shutdown negotiation.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// Owner daemon requests. Only handshake is accepted without an identity envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DaemonRequest {
    /// Inspect owner identity without opening another database writer.
    Handshake,
    /// Identity-bound operation; checked on the same socket before execution.
    Bound {
        /// Exact owner identity observed by the client.
        identity: DaemonHandshake,
        /// Operation (nested envelopes are rejected).
        request: Box<DaemonRequest>,
    },
    /// Stop admission and drain all requests/indexing before releasing ownership.
    Shutdown,
    /// Commit a synchronized index generation.
    IndexNow,
    /// Compile context.
    QueryContext {
        /// Context request.
        request: ContextRequest,
    },
    /// Expand evidence.
    Expand {
        /// Expansion request.
        request: ExpandRequest,
    },
    /// Read status.
    Status,
}

/// Owner daemon responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonResponse {
    /// Handshake metadata.
    Handshake(DaemonHandshake),
    /// Response bound to the identity that executed it.
    Bound {
        /// Executing owner identity.
        identity: DaemonHandshake,
        /// Operation response.
        response: Box<DaemonResponse>,
    },
    /// Shutdown accepted (ownership is retained until draining finishes).
    ShuttingDown,
    /// Committed generation.
    Indexed {
        /// Published generation number.
        generation: u64,
    },
    /// Context pack.
    ContextPack(Box<ContextPack>),
    /// Expansion response.
    ExpandResponse(ExpandResponse),
    /// Store statistics.
    Status(StoreStats),
    /// Structured error.
    Error {
        /// Human-readable reason.
        message: String,
    },
}

/// Service shared by CLI, MCP and explicit in-process test/benchmark clients.
pub trait LedgerService: Send + Sync {
    /// Synchronize indexing.
    fn index_now(&self) -> Result<u64, DaemonError>;
    /// Compile context.
    fn query_context(&self, req: &ContextRequest) -> Result<ContextPack, DaemonError>;
    /// Expand evidence.
    fn expand(&self, req: &ExpandRequest) -> Result<ExpandResponse, DaemonError>;
    /// Read status.
    fn status(&self) -> Result<StoreStats, DaemonError>;
}

/// Explicit in-process client for isolated library tests and benchmarks.
/// Production callers must use [`DaemonClient`] to preserve single-writer ownership.
#[derive(Debug, Clone)]
pub struct EmbeddedClient {
    engine: Arc<WorkspaceEngine>,
}

impl EmbeddedClient {
    /// Wraps an explicitly opened engine.
    #[must_use]
    pub const fn new(engine: Arc<WorkspaceEngine>) -> Self {
        Self { engine }
    }
}

impl LedgerService for EmbeddedClient {
    fn index_now(&self) -> Result<u64, DaemonError> {
        Ok(self.engine.index_workspace()?.0)
    }
    fn query_context(&self, req: &ContextRequest) -> Result<ContextPack, DaemonError> {
        Ok(self.engine.query_context(req)?)
    }
    fn expand(&self, req: &ExpandRequest) -> Result<ExpandResponse, DaemonError> {
        Ok(self.engine.expand(req)?)
    }
    fn status(&self) -> Result<StoreStats, DaemonError> {
        Ok(self.engine.stats()?)
    }
}

/// Thin Unix-socket proxy pinned to an observed database incarnation and binary build.
#[derive(Debug, Clone)]
pub struct SocketClient {
    socket_path: PathBuf,
    lock_path: PathBuf,
    identity: DaemonHandshake,
}

/// Resolve a workspace socket, using a private short directory for long Unix paths.
pub fn resolve_daemon_socket_path(
    canonical_workspace: &Path,
    workspace_id: &WorkspaceId,
) -> std::io::Result<PathBuf> {
    let in_workspace = canonical_workspace.join(".ledger/runtime/daemon.sock");
    if in_workspace.as_os_str().len() <= 96 {
        return Ok(in_workspace);
    }
    let user_seed = format!(
        "{}:{}",
        std::env::var("USER").unwrap_or_default(),
        std::env::var("HOME").unwrap_or_default()
    );
    let user_tag = &blake3::hash(user_seed.as_bytes()).to_hex()[..10];
    let fallback_dir = PathBuf::from(format!("/tmp/ledger-rt-{user_tag}"));
    private_directory(&fallback_dir)?;
    Ok(fallback_dir.join(format!("{}.sock", workspace_id.as_str())))
}

fn private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing symlinked or non-directory socket runtime path",
        ));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn binary_build_id() -> Result<&'static str, DaemonError> {
    static BUILD: OnceLock<Result<String, String>> = OnceLock::new();
    match BUILD.get_or_init(|| {
        let result = (|| -> std::io::Result<String> {
            let mut file = std::fs::File::open(std::env::current_exe()?)?;
            let mut hash = blake3::Hasher::new();
            std::io::copy(&mut file, &mut hash)?;
            Ok(format!(
                "{}:{}",
                env!("CARGO_PKG_VERSION"),
                hash.finalize().to_hex()
            ))
        })();
        result.map_err(|e| e.to_string())
    }) {
        Ok(build) => Ok(build),
        Err(message) => Err(DaemonError::Remote(format!(
            "cannot fingerprint executable: {message}"
        ))),
    }
}

fn mismatch(message: impl Into<String>) -> DaemonError {
    DaemonError::HandshakeMismatch(message.into())
}

fn validate_workspace(
    identity: &DaemonHandshake,
    workspace: &WorkspaceId,
) -> Result<(), DaemonError> {
    if identity.workspace_id != workspace.as_str() {
        return Err(mismatch("owner belongs to a different workspace"));
    }
    Ok(())
}

fn validate_compatible(
    identity: &DaemonHandshake,
    workspace: &WorkspaceId,
) -> Result<(), DaemonError> {
    validate_workspace(identity, workspace)?;
    if identity.protocol_version != PROTOCOL_VERSION
        || identity.schema_version != SqliteStore::SCHEMA_VERSION
        || identity.binary_build_id != binary_build_id()?
        || identity.db_incarnation.is_empty()
        || identity.owner_id.is_empty()
    {
        return Err(mismatch(format!(
            "owner protocol/schema/build = {}/{}/{}; client = {}/{}/{}",
            identity.protocol_version,
            identity.schema_version,
            identity.binary_build_id,
            PROTOCOL_VERSION,
            SqliteStore::SCHEMA_VERSION,
            binary_build_id()?
        )));
    }
    Ok(())
}

impl SocketClient {
    fn inspect(canonical: &Path) -> Result<Self, DaemonError> {
        let workspace = WorkspaceId::from_canonical_path(canonical);
        let socket_path = resolve_daemon_socket_path(canonical, &workspace)?;
        match exchange(&socket_path, &DaemonRequest::Handshake)? {
            DaemonResponse::Handshake(identity) => {
                validate_workspace(&identity, &workspace)?;
                Ok(Self {
                    socket_path,
                    lock_path: canonical.join(".ledger/runtime/daemon.lock"),
                    identity,
                })
            }
            other => Err(mismatch(format!(
                "unexpected handshake response: {other:?}"
            ))),
        }
    }

    /// Connect without spawning or changing an incompatible owner.
    pub fn connect_existing(workspace_root: &Path) -> Result<Self, DaemonError> {
        let canonical = std::fs::canonicalize(workspace_root)?;
        let client = Self::inspect(&canonical)?;
        validate_compatible(
            &client.identity,
            &WorkspaceId::from_canonical_path(&canonical),
        )?;
        Ok(client)
    }

    /// Connect, safely negotiate an advertised upgrade, or spawn the owner executable.
    /// Never opens an embedded engine or bypasses the workspace owner lock.
    pub fn connect_or_spawn(workspace_root: &Path) -> Result<Self, DaemonError> {
        let canonical = std::fs::canonicalize(workspace_root)?;
        match Self::inspect(&canonical) {
            Ok(client) => {
                match validate_compatible(
                    &client.identity,
                    &WorkspaceId::from_canonical_path(&canonical),
                ) {
                    Ok(()) => return Ok(client),
                    Err(DaemonError::HandshakeMismatch(_)) => {}
                    Err(error) => return Err(error),
                }
                client.shutdown()?;
                client.wait_stopped(Duration::from_secs(30))?;
            }
            Err(DaemonError::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) => {}
            Err(e) => return Err(e),
        }
        let log_dir = canonical.join(".ledger/runtime");
        private_directory(&log_dir)?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_dir.join("daemon.log"))?;
        let mut child = Command::new(std::env::current_exe()?)
            .arg("daemon")
            .arg("--workspace")
            .arg(&canonical)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()?;
        // Reap a daemon that exits while its spawning client is still alive.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Self::connect_existing(&canonical) {
                Ok(client) => return Ok(client),
                Err(DaemonError::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) => {}
                Err(e) => return Err(e),
            }
            if Instant::now() >= deadline {
                return Err(DaemonError::Remote(format!(
                    "owner did not start; inspect {} (an existing lock owner is never bypassed)",
                    log_dir.join("daemon.log").display()
                )));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn send_sync(&self, request: &DaemonRequest) -> Result<DaemonResponse, DaemonError> {
        let response = exchange(
            &self.socket_path,
            &DaemonRequest::Bound {
                identity: self.identity.clone(),
                request: Box::new(request.clone()),
            },
        )?;
        match response {
            DaemonResponse::Bound { identity, response } if identity == self.identity => {
                Ok(*response)
            }
            DaemonResponse::Error { message } => Err(mismatch(message)),
            _ => Err(mismatch(
                "response identity differs from the connected owner/database",
            )),
        }
    }

    /// Revalidate the original identity; a replacement never silently rebinds this client.
    pub fn verify_handshake(&self) -> Result<DaemonHandshake, DaemonError> {
        match exchange(&self.socket_path, &DaemonRequest::Handshake)? {
            DaemonResponse::Handshake(identity) if identity == self.identity => Ok(identity),
            _ => Err(mismatch("owner/database changed since connection")),
        }
    }

    /// Request shutdown only from a same-workspace owner advertising the supported protocol.
    /// Works for build/schema upgrades; legacy daemons require their original stop mechanism.
    pub fn shutdown(&self) -> Result<(), DaemonError> {
        if self.identity.binary_build_id.is_empty()
            || self.identity.db_incarnation.is_empty()
            || !self
                .identity
                .capabilities
                .iter()
                .any(|c| c == SHUTDOWN_CAPABILITY)
        {
            return Err(mismatch("owner does not advertise supported safe shutdown"));
        }
        match self.send_sync(&DaemonRequest::Shutdown)? {
            DaemonResponse::ShuttingDown => Ok(()),
            DaemonResponse::Error { message } => Err(DaemonError::Remote(message)),
            other => Err(DaemonError::Remote(format!(
                "unexpected shutdown response: {other:?}"
            ))),
        }
    }

    fn wait_stopped(&self, timeout: Duration) -> Result<(), DaemonError> {
        let deadline = Instant::now() + timeout;
        loop {
            if !self.socket_path.exists() {
                match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&self.lock_path)
                {
                    Ok(file) => {
                        let mut lock = FileLock::new(file);
                        if lock.try_write().is_ok() {
                            return Ok(());
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(error) => return Err(error.into()),
                }
            }
            if Instant::now() >= deadline {
                return Err(DaemonError::Remote(
                    "owner is still draining; ownership remains held; retry later".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Stop an existing, validated owner and wait for socket admission to end.
    /// A slow drain is reported without killing it or releasing its ownership.
    pub fn stop_workspace(workspace_root: &Path) -> Result<(), DaemonError> {
        let canonical = std::fs::canonicalize(workspace_root)?;
        let client = match Self::inspect(&canonical) {
            Ok(client) => client,
            Err(DaemonError::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                let lock_path = canonical.join(".ledger/runtime/daemon.lock");
                if let Ok(file) = OpenOptions::new().read(true).write(true).open(lock_path) {
                    let mut lock = FileLock::new(file);
                    if lock.try_write().is_err() {
                        return Err(DaemonError::Remote("owner is starting or still draining; retry stop after it becomes reachable".into()));
                    }
                }
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        client.shutdown()?;
        client.wait_stopped(Duration::from_secs(30))
    }
}

impl LedgerService for SocketClient {
    fn index_now(&self) -> Result<u64, DaemonError> {
        match self.send_sync(&DaemonRequest::IndexNow)? {
            DaemonResponse::Indexed { generation } => Ok(generation),
            DaemonResponse::Error { message } => Err(DaemonError::Remote(message)),
            other => Err(DaemonError::Remote(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }
    fn query_context(&self, req: &ContextRequest) -> Result<ContextPack, DaemonError> {
        match self.send_sync(&DaemonRequest::QueryContext {
            request: req.clone(),
        })? {
            DaemonResponse::ContextPack(pack) => Ok(*pack),
            DaemonResponse::Error { message } => Err(DaemonError::Remote(message)),
            other => Err(DaemonError::Remote(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }
    fn expand(&self, req: &ExpandRequest) -> Result<ExpandResponse, DaemonError> {
        match self.send_sync(&DaemonRequest::Expand {
            request: req.clone(),
        })? {
            DaemonResponse::ExpandResponse(response) => Ok(response),
            DaemonResponse::Error { message } => Err(DaemonError::Remote(message)),
            other => Err(DaemonError::Remote(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }
    fn status(&self) -> Result<StoreStats, DaemonError> {
        match self.send_sync(&DaemonRequest::Status)? {
            DaemonResponse::Status(stats) => Ok(stats),
            DaemonResponse::Error { message } => Err(DaemonError::Remote(message)),
            other => Err(DaemonError::Remote(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }
}

/// Production client. Connection failures never open an unlocked embedded engine.
#[derive(Debug)]
pub struct DaemonClient(SocketClient);
impl DaemonClient {
    /// Connect or safely spawn the single owner.
    pub fn connect(root: &Path) -> Result<Self, DaemonError> {
        Ok(Self(SocketClient::connect_or_spawn(root)?))
    }
}
impl LedgerService for DaemonClient {
    fn index_now(&self) -> Result<u64, DaemonError> {
        self.0.index_now()
    }
    fn query_context(&self, req: &ContextRequest) -> Result<ContextPack, DaemonError> {
        self.0.query_context(req)
    }
    fn expand(&self, req: &ExpandRequest) -> Result<ExpandResponse, DaemonError> {
        self.0.expand(req)
    }
    fn status(&self) -> Result<StoreStats, DaemonError> {
        self.0.status()
    }
}

struct FrameWriter(Vec<u8>);
impl Write for FrameWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(buf.len()) >= MAX_IPC_FRAME_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "IPC frame exceeds 4 MiB limit",
            ));
        }
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encode_frame(value: &impl Serialize) -> Result<Vec<u8>, DaemonError> {
    let mut writer = FrameWriter(Vec::new());
    serde_json::to_writer(&mut writer, value)?;
    writer.0.push(b'\n');
    Ok(writer.0)
}
fn exchange(socket: &Path, request: &DaemonRequest) -> Result<DaemonResponse, DaemonError> {
    let payload = encode_frame(request)?;
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    // Nonblocking I/O enforces a total deadline without repeatedly changing socket
    // options (which can fail on macOS after the peer closes a buffered response).
    stream.set_nonblocking(true)?;
    let write_deadline = Instant::now() + Duration::from_secs(5);
    let mut pending = payload.as_slice();
    while !pending.is_empty() {
        let remaining = remaining_time(write_deadline, "IPC request write deadline elapsed")?;
        match stream.write(pending) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "IPC request write failed",
                )
                .into());
            }
            Ok(written) => pending = &pending[written..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(remaining.min(Duration::from_millis(2)))
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut reader = std::io::BufReader::new(stream);
    let response_timeout = match request {
        DaemonRequest::Handshake => Duration::from_secs(5),
        DaemonRequest::Bound { request, .. } if matches!(**request, DaemonRequest::Shutdown) => {
            Duration::from_secs(5)
        }
        _ => CLIENT_TIMEOUT,
    };
    let deadline = Instant::now() + response_timeout;
    let mut frame = Vec::new();
    loop {
        let remaining = remaining_time(deadline, "IPC response deadline elapsed")?;
        let available = match reader.fill_buf() {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(remaining.min(Duration::from_millis(2)));
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if available.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "incomplete IPC response",
            )
            .into());
        }
        let count = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |i| i + 1);
        if frame.len().saturating_add(count) > MAX_IPC_FRAME_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "IPC response exceeds 4 MiB limit",
            )
            .into());
        }
        let complete = available[count - 1] == b'\n';
        frame.extend_from_slice(&available[..count]);
        reader.consume(count);
        if complete {
            return Ok(serde_json::from_slice(&frame)?);
        }
    }
}

fn remaining_time(deadline: Instant, message: &'static str) -> std::io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::TimedOut, message))
}

/// Lifecycle limits. Polling is periodic reconciliation, not an event watcher.
#[derive(Debug, Clone)]
pub struct DaemonOptions {
    /// Time without an active connection before shutdown.
    pub idle_timeout: Duration,
    /// Reconciliation frequency (minimum 50 ms; default two seconds).
    pub refresh_interval: Duration,
    /// Total time to receive a complete request, including silent/partial connections.
    pub frame_timeout: Duration,
    /// Total time allowed to send a response.
    pub write_timeout: Duration,
}
impl DaemonOptions {
    /// Default transport limits and polling interval for an idle timeout.
    #[must_use]
    pub const fn new(idle_timeout: Duration) -> Self {
        Self {
            idle_timeout,
            refresh_interval: Duration::from_secs(2),
            frame_timeout: Duration::from_secs(5),
            write_timeout: Duration::from_secs(5),
        }
    }
}

// Registered before spawn_blocking so queued requests also prevent owner release.
#[derive(Default)]
struct RequestGate {
    state: Mutex<(bool, usize)>,
    drained: Condvar,
}
impl RequestGate {
    fn admit(self: &Arc<Self>) -> Option<RequestPermit> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.0 {
            return None;
        }
        state.1 += 1;
        Some(RequestPermit(Arc::clone(self)))
    }
    fn close_and_drain(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.0 = true;
        while state.1 != 0 {
            state = self
                .drained
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}
struct RequestPermit(Arc<RequestGate>);
impl Drop for RequestPermit {
    fn drop(&mut self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.1 -= 1;
        if state.1 == 0 {
            self.0.drained.notify_all();
        }
    }
}

// Declared after the OS lock guard. On cancellation/unwind it drains blocking work
// synchronously before the lock guard can drop; normal shutdown uses spawn_blocking.
struct OwnerLifetime {
    engine: Arc<WorkspaceEngine>,
    gate: Arc<RequestGate>,
    socket: PathBuf,
    drained: bool,
}
impl Drop for OwnerLifetime {
    fn drop(&mut self) {
        self.gate.close_and_drain();
        if !self.drained
            && let Err(error) = self.engine.shutdown_indexing()
        {
            tracing::error!(%error, "index shutdown/checkpoint failed after drain");
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct DatabaseFile {
    device: u64,
    inode: u64,
}
fn database_file(path: &Path) -> std::io::Result<DatabaseFile> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path)?;
    Ok(DatabaseFile {
        device: meta.dev(),
        inode: meta.ino(),
    })
}

/// Run the owner with default polling/transport limits. Ownership lasts through
/// request drain, indexing closure/drain, checkpoint and socket removal.
pub async fn run_daemon_server(root: &Path, idle_timeout: Duration) -> Result<(), DaemonError> {
    run_daemon_server_with_options(root, DaemonOptions::new(idle_timeout)).await
}

/// Run with configurable lifecycle limits (also useful for integration tests).
pub async fn run_daemon_server_with_options(
    root: &Path,
    options: DaemonOptions,
) -> Result<(), DaemonError> {
    if options.refresh_interval < Duration::from_millis(50)
        || options.frame_timeout.is_zero()
        || options.write_timeout.is_zero()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refresh must be >=50ms and transport deadlines must be nonzero",
        )
        .into());
    }
    let canonical = std::fs::canonicalize(root)?;
    let build = binary_build_id()?.to_string();
    let runtime = canonical.join(".ledger/runtime");
    private_directory(&runtime)?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(runtime.join("daemon.lock"))?;
    let mut lock = FileLock::new(file);
    let _exclusive_guard = lock.try_write().map_err(|_| DaemonError::AlreadyRunning)?;
    let workspace = WorkspaceId::from_canonical_path(&canonical);
    let socket = resolve_daemon_socket_path(&canonical, &workspace)?;
    if socket.exists() {
        std::fs::remove_file(&socket)?;
    }
    // Open/migrate only while the exclusive owner lock is held.
    let engine = WorkspaceEngine::open(&canonical)?;
    let gate = Arc::new(RequestGate::default());
    let mut lifetime = OwnerLifetime {
        engine: Arc::clone(&engine),
        gate: Arc::clone(&gate),
        socket: socket.clone(),
        drained: false,
    };
    let identity = DaemonHandshake {
        protocol_version: PROTOCOL_VERSION,
        schema_version: SqliteStore::SCHEMA_VERSION,
        workspace_id: workspace.to_string(),
        db_incarnation: engine.db_incarnation().to_string(),
        binary_build_id: build,
        owner_id: format!("{}:{:?}", std::process::id(), std::time::SystemTime::now()),
        capabilities: vec![SHUTDOWN_CAPABILITY.to_string()],
    };
    let db_path = canonical.join(".ledger/ledger.db");
    let db_file = database_file(&db_path)?;
    let listener = UnixListener::bind(&socket)?;
    // Protect short and workspace sockets, independent of the process umask.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    engine.trigger_background_index();
    let (stop_tx, mut stop_rx) = watch::channel(false);
    let mut tasks = JoinSet::new();
    let mut refresh = tokio::time::interval_at(
        tokio::time::Instant::now() + options.refresh_interval,
        options.refresh_interval,
    );
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut idle_deadline = tokio::time::Instant::now() + options.idle_timeout;
    let mut server_error = None;
    loop {
        tokio::select! {
            biased;
            _ = stop_rx.changed() => break,
            () = tokio::time::sleep_until(idle_deadline), if tasks.is_empty() => break,
            result = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = result { tracing::warn!(%error, "daemon connection task failed"); }
                if tasks.is_empty() { idle_deadline = tokio::time::Instant::now() + options.idle_timeout; }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        if tasks.len() >= 64 { drop(stream); continue; }
                        tasks.spawn(handle_connection(stream, Arc::clone(&engine), Arc::clone(&gate), identity.clone(), db_path.clone(), db_file, stop_tx.clone(), options.clone()));
                    }
                    Err(error) => { server_error = Some(DaemonError::Io(error)); break; }
                }
            }
            _ = refresh.tick() => {
                if database_file(&db_path).ok() != Some(db_file) {
                    server_error = Some(mismatch("database file was replaced while owner was running"));
                    break;
                }
                engine.trigger_background_index();
            }
        }
    }
    drop(listener); // Stop request admission AND polling before draining.
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result {
            tracing::warn!(%error, "daemon request failed during drain");
        }
    }
    let eng = Arc::clone(&engine);
    let request_gate = Arc::clone(&gate);
    let result = tokio::task::spawn_blocking(move || {
        request_gate.close_and_drain();
        eng.shutdown_indexing()
    })
    .await
    .map_err(|e| DaemonError::Remote(format!("shutdown worker failed: {e}")))?;
    // shutdown_indexing drains even when checkpoint returns an error.
    result?;
    lifetime.drained = true;
    drop(lifetime); // Socket is unlinked before `_exclusive_guard` is released.
    if let Some(error) = server_error {
        return Err(error);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    stream: UnixStream,
    engine: Arc<WorkspaceEngine>,
    gate: Arc<RequestGate>,
    identity: DaemonHandshake,
    db_path: PathBuf,
    db_file: DatabaseFile,
    stop: watch::Sender<bool>,
    options: DaemonOptions,
) -> Result<(), DaemonError> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read).take(MAX_IPC_FRAME_BYTES as u64 + 1);
    let mut frame = Vec::new();
    let n = tokio::time::timeout(options.frame_timeout, reader.read_until(b'\n', &mut frame))
        .await
        .map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "IPC request deadline elapsed")
        })??;
    if n == 0 {
        return Ok(());
    }
    let response = if n > MAX_IPC_FRAME_BYTES || frame.last() != Some(&b'\n') {
        error_response("IPC request is oversized or incomplete")
    } else {
        match serde_json::from_slice::<DaemonRequest>(&frame) {
            Ok(DaemonRequest::Handshake) if database_file(&db_path).ok() == Some(db_file) => {
                DaemonResponse::Handshake(identity.clone())
            }
            Ok(DaemonRequest::Bound {
                identity: expected,
                request,
            }) if expected == identity => {
                if database_file(&db_path).ok() != Some(db_file) {
                    let _ = stop.send(true);
                    error_response("database replaced; reconnect and obtain a new handle")
                } else {
                    let response = if matches!(*request, DaemonRequest::Shutdown) {
                        let _ = stop.send(true);
                        DaemonResponse::ShuttingDown
                    } else {
                        dispatch_request(engine, gate, *request).await
                    };
                    if database_file(&db_path).ok() != Some(db_file) {
                        let _ = stop.send(true);
                        error_response("database replaced during request; discard response")
                    } else {
                        DaemonResponse::Bound {
                            identity,
                            response: Box::new(response),
                        }
                    }
                }
            }
            Ok(_) => {
                error_response("missing or mismatched owner identity; handshake/reconnect required")
            }
            Err(error) => error_response(format!("invalid request JSON: {error}")),
        }
    };
    let encoded = encode_frame(&response).or_else(|_| {
        let error = error_response("IPC response exceeds 4 MiB limit; reduce query/expansion size");
        let error = match &response {
            DaemonResponse::Bound { identity, .. } => DaemonResponse::Bound {
                identity: identity.clone(),
                response: Box::new(error),
            },
            _ => error,
        };
        encode_frame(&error)
    })?;
    tokio::time::timeout(options.write_timeout, write.write_all(&encoded))
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "IPC response write deadline elapsed",
            )
        })??;
    Ok(())
}
fn error_response(message: impl Into<String>) -> DaemonResponse {
    DaemonResponse::Error {
        message: message.into(),
    }
}
async fn dispatch_request(
    engine: Arc<WorkspaceEngine>,
    gate: Arc<RequestGate>,
    request: DaemonRequest,
) -> DaemonResponse {
    let Some(permit) = gate.admit() else {
        return error_response("owner is shutting down");
    };
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        match request {
            DaemonRequest::IndexNow => engine
                .index_workspace()
                .map(|g| DaemonResponse::Indexed { generation: g.0 }),
            DaemonRequest::QueryContext { request } => engine
                .query_context(&request)
                .map(|pack| DaemonResponse::ContextPack(Box::new(pack))),
            DaemonRequest::Expand { request } => {
                engine.expand(&request).map(DaemonResponse::ExpandResponse)
            }
            DaemonRequest::Status => engine.stats().map(DaemonResponse::Status),
            _ => return error_response("invalid nested or unbound operation"),
        }
        .unwrap_or_else(|e| error_response(e.to_string()))
    })
    .await;
    result.unwrap_or_else(|e| error_response(format!("worker task failed: {e}")))
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn owner_guard_keeps_lock_until_registered_writes_and_indexing_drain()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        std::fs::create_dir_all(root.join(".ledger/runtime"))?;
        std::fs::write(root.join("source.rs"), "pub fn queued_write() {}\n")?;
        let lock_path = root.join(".ledger/runtime/daemon.lock");
        let socket = root.join(".ledger/runtime/test.sock");
        std::fs::write(&socket, b"owned socket marker")?;
        let engine = WorkspaceEngine::open(&root)?;
        let gate = Arc::new(RequestGate::default());
        let permit = gate
            .admit()
            .ok_or("request admission unexpectedly closed")?;
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (drop_tx, drop_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let path = lock_path.clone();
        let gate_for_owner = Arc::clone(&gate);
        let engine_for_owner = Arc::clone(&engine);
        let owner = std::thread::spawn(move || -> Result<(), std::io::Error> {
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(path)?;
            let mut lock = FileLock::new(file);
            let _guard = lock.try_write()?;
            let lifetime = OwnerLifetime {
                engine: engine_for_owner,
                gate: gate_for_owner,
                socket,
                drained: false,
            };
            locked_tx.send(()).map_err(std::io::Error::other)?;
            drop_rx.recv().map_err(std::io::Error::other)?;
            drop(lifetime); // Must block while the registered request owns its permit.
            done_tx.send(()).map_err(std::io::Error::other)?;
            Ok(())
        });
        locked_rx.recv()?;
        drop_tx.send(())?;
        assert!(done_rx.recv_timeout(Duration::from_millis(30)).is_err());
        let file = OpenOptions::new().read(true).write(true).open(&lock_path)?;
        let mut contender = FileLock::new(file);
        assert!(
            contender.try_write().is_err(),
            "ownership released while a queued writer remained"
        );
        // The accepted request is allowed to finish even though owner teardown began.
        engine.index_workspace()?;
        drop(permit);
        done_rx.recv_timeout(Duration::from_secs(3))?;
        owner.join().map_err(|_| "owner guard worker panicked")??;
        let _replacement = contender.try_write()?;
        assert!(
            engine.index_workspace().is_err(),
            "index admission must be closed after shutdown"
        );
        engine.trigger_background_index(); // No-op after closure.
        assert!(!root.join(".ledger/runtime/test.sock").exists());
        Ok(())
    }

    #[test]
    fn serialization_caps_request_and_response_allocations()
    -> Result<(), Box<dyn std::error::Error>> {
        assert!(
            encode_frame(&DaemonResponse::Error {
                message: "x".repeat(MAX_IPC_FRAME_BYTES)
            })
            .is_err()
        );
        assert!(
            encode_frame(&DaemonRequest::Expand {
                request: ExpandRequest {
                    handle: "x".repeat(MAX_IPC_FRAME_BYTES),
                    items: vec![],
                    delivery: ledger_core::ExpandDelivery::Complete,
                    max_approx_tokens: 2400,
                }
            })
            .is_err()
        );
        Ok(())
    }
    #[test]
    fn client_rejects_oversized_response_before_parsing() -> Result<(), Box<dyn std::error::Error>>
    {
        let temp = tempfile::tempdir()?;
        let socket = temp.path().join("peer.sock");
        let peer = std::os::unix::net::UnixListener::bind(&socket)?;
        let worker = std::thread::spawn(move || -> Result<(), std::io::Error> {
            let (mut stream, _) = peer.accept()?;
            stream.set_write_timeout(Some(Duration::from_secs(2)))?;
            let mut request = String::new();
            std::io::BufReader::new(stream.try_clone()?).read_line(&mut request)?;
            stream.write_all(&vec![b'x'; MAX_IPC_FRAME_BYTES + 1])?;
            Ok(())
        });
        let result = exchange(&socket, &DaemonRequest::Handshake);
        assert!(
            matches!(result, Err(DaemonError::Io(ref error)) if error.kind() == std::io::ErrorKind::InvalidData),
            "unexpected response-limit result: {result:?}"
        );
        worker.join().map_err(|_| "fake peer panicked")??;
        Ok(())
    }
}
