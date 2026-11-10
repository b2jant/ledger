//! Per-workspace Owner Daemon, snapshot-consistent query & expansion engine,
//! progressive cold-start serving, live dirty-buffer overlays, and safe Unix socket IPC.

pub mod engine;
pub mod ipc;

pub use engine::{EngineError, WorkspaceEngine};
pub use ipc::{
    DaemonClient, DaemonError, DaemonHandshake, DaemonOptions, DaemonRequest, DaemonResponse,
    EmbeddedClient, LedgerService, SocketClient, resolve_daemon_socket_path, run_daemon_server,
    run_daemon_server_with_options,
};
