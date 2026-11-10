//! Dual-VCS history engine (`git` + `jj`), bounded bipartite file <-> revision CSR co-change
//! graph, mass-sweep suppression, time-decayed lift scoring, and hunk-verified symbol history.
//!
//! Strictly enforces `unsafe_code = "forbid"` and routes every historical path and diff read
//! through [`ledger_core::scope::ScopePolicy`].

pub mod backend;
pub mod cochange;
pub mod git;
pub mod history;
pub mod jj;

pub use backend::{
    CommandOutput, CommandRunner, SystemCommandRunner, VcsBackend, VcsError, VcsHeadFingerprint,
    detect_vcs_backend,
};
pub use cochange::{BipartiteCoChangeIndex, CoChangeConfig};
pub use git::GitCliBackend;
pub use history::{HunkSymbolAttributor, derive_cochange_test_files};
pub use jj::JjCliBackend;
