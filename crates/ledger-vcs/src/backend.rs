//! Composable VCS backend abstraction (`VcsBackend`), mockable `CommandRunner`,
//! and O(1) filesystem HEAD / op-head fingerprinting.

use std::path::Path;
use std::sync::Arc;

use ledger_core::{ScopePolicy, VcsBackendKind, VcsRevision};
use thiserror::Error;

use crate::git::GitCliBackend;
use crate::jj::JjCliBackend;

/// Errors returned by VCS backends.
#[derive(Debug, Error)]
pub enum VcsError {
    /// I/O error inspecting VCS metadata or spawning CLI process.
    #[error("vcs io error: {0}")]
    Io(#[from] std::io::Error),
    /// Subprocess exited with non-zero status.
    #[error("vcs command `{program}` failed (code {code:?}): {stderr}")]
    CommandFailed {
        /// Executable name (`git` or `jj`).
        program: String,
        /// Exit status code.
        code: Option<i32>,
        /// Sanitized stderr summary.
        stderr: String,
    },
    /// Path blocked by `ScopePolicy`.
    #[error("path `{0}` denied by ScopePolicy")]
    DeniedByPolicy(String),
    /// Malformed revision log or diff output.
    #[error("vcs parse error: {0}")]
    Parse(String),
}

/// Captured stdout/stderr from a non-mutating VCS CLI invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit status code (`0` on success).
    pub status_code: Option<i32>,
    /// Raw stdout bytes.
    pub stdout: Vec<u8>,
    /// Raw stderr bytes.
    pub stderr: Vec<u8>,
}

/// Mockable command runner so `git` and `jj` backends are deterministic and testable
/// even when `jj` is not installed on the host machine.
pub trait CommandRunner: Send + Sync + std::fmt::Debug {
    /// Executes `program` with `args` in `cwd` without mutating repository state.
    fn run(&self, cwd: &Path, program: &str, args: &[&str]) -> Result<CommandOutput, VcsError>;
}

/// Production [`CommandRunner`] backed by `std::process::Command`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, cwd: &Path, program: &str, args: &[&str]) -> Result<CommandOutput, VcsError> {
        let out = std::process::Command::new(program)
            .args(args)
            .current_dir(cwd)
            .env("LC_ALL", "C")
            .output()?;
        Ok(CommandOutput {
            status_code: out.status.code(),
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }
}

/// Fast O(1) filesystem fingerprint of the active VCS revision / operation head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcsHeadFingerprint {
    /// Detected backend kind.
    pub kind: VcsBackendKind,
    /// Human-readable short badge (`git:88f1c2a` or `jj:qpvuntsm`).
    pub display_badge: String,
    /// Deterministic BLAKE3 digest of HEAD / op_store heads for O(1) change detection.
    pub state_digest: String,
}

/// Read-only, `ScopePolicy`-enforced VCS history provider.
pub trait VcsBackend: Send + Sync + std::fmt::Debug {
    /// Returns the VCS backend kind (`git`, `jj`, `jj+git`, `none`).
    fn kind(&self) -> VcsBackendKind;

    /// Computes an O(1) HEAD / operation-head fingerprint without spawning subprocesses when possible.
    fn head_fingerprint(&self, repo_root: &Path) -> Result<Option<VcsHeadFingerprint>, VcsError>;

    /// Loads up to `max_revisions` recent revisions, filtering every touched path through `scope`.
    fn recent_revisions(
        &self,
        repo_root: &Path,
        scope: &dyn ScopePolicy,
        max_revisions: usize,
    ) -> Result<Vec<VcsRevision>, VcsError>;

    /// Loads a bounded unified diff for `(rev_id, rel_path)`, rejecting any `ScopePolicy`-denied path.
    fn revision_file_diff(
        &self,
        repo_root: &Path,
        scope: &dyn ScopePolicy,
        rev_id: &str,
        rel_path: &str,
        max_lines: usize,
    ) -> Result<String, VcsError>;
}

/// Detects the active VCS backend for `repo_root` (`.jj` + `.git`, `.jj`, `.git`, or none).
#[must_use]
pub fn detect_vcs_backend(
    repo_root: &Path,
    runner: Arc<dyn CommandRunner>,
) -> Option<Arc<dyn VcsBackend>> {
    let has_jj = repo_root.join(".jj").exists();
    let has_git = repo_root.join(".git").exists();
    if has_jj {
        let kind = if has_git {
            VcsBackendKind::JujutsuColocatedGit
        } else {
            VcsBackendKind::Jujutsu
        };
        Some(Arc::new(JjCliBackend::new(kind, runner)))
    } else if has_git {
        Some(Arc::new(GitCliBackend::new(runner)))
    } else {
        None
    }
}
