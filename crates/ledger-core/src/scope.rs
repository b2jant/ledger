//! Unified filesystem boundary and secret-exclusion policy (`ScopeGuard`).
//!
//! Enforced across indexing, live scans, expansion and historical diffs. This is
//! filename/path filtering, not detection of secret values within allowed files.

use std::path::{Component, Path, PathBuf};

/// Decision returned by a [`ScopePolicy`] check for a candidate path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeDecision {
    /// Path is inside the workspace and safe to index/read; carries normalized POSIX relative path.
    Allowed {
        /// Canonical absolute path verified under `workspace_root`.
        canonical_path: PathBuf,
        /// Workspace-relative forward-slash path (`src/orders/query.rs`).
        rel_path: String,
    },
    /// Denied because the file matches the secret/credential denylist.
    DeniedSecret {
        /// Reason string for audit telemetry.
        reason: &'static str,
    },
    /// Denied because the path escapes `workspace_root` (e.g., via symlink or `..`).
    DeniedOutsideWorkspace,
    /// Excluded internal VCS, LEDGER state, or build artifact directory.
    ExcludedInternal,
}

/// Composable security & scope boundary trait.
pub trait ScopePolicy: Send + Sync + std::fmt::Debug {
    /// Canonical workspace root guarded by this policy.
    fn workspace_root(&self) -> &Path;

    /// Maximum single-file byte size eligible for indexing or slicing.
    fn max_file_bytes(&self) -> u64;

    /// Evaluates whether `candidate` (absolute or workspace-relative) may be read or indexed.
    fn evaluate_path(&self, candidate: &Path) -> ScopeDecision;

    /// Pre-filter relative names and nested repository roots before live traversal.
    fn is_relative_name_allowed(&self, rel_path: &Path) -> bool;

    /// Checks an object-store path without requiring the historical leaf to exist.
    /// Object reads do not follow working-tree symlinks. Implementors may distinguish
    /// this from live canonical containment while retaining lexical scope restrictions.
    fn is_history_path_allowed(&self, rel_path: &Path) -> bool {
        self.is_relative_name_allowed(rel_path)
    }
}

/// Default production [`ScopePolicy`] enforcing secret exclusion and canonical symlink containment.
#[derive(Debug, Clone)]
pub struct DefaultScopeGuard {
    canonical_root: PathBuf,
    requested_root: PathBuf,
    max_file_bytes: u64,
}

impl DefaultScopeGuard {
    /// Default 1 MiB file ceiling preventing generated bundles or binary blobs from flooding RAM.
    pub const DEFAULT_MAX_FILE_BYTES: u64 = 1_048_576;

    /// Creates a `DefaultScopeGuard` rooted at `workspace_root` (canonicalized once at startup).
    pub fn new(workspace_root: &Path) -> std::io::Result<Self> {
        let canonical_root = std::fs::canonicalize(workspace_root)?;
        Ok(Self {
            canonical_root,
            requested_root: std::path::absolute(workspace_root)?,
            max_file_bytes: Self::DEFAULT_MAX_FILE_BYTES,
        })
    }

    /// Overrides the maximum allowed single-file byte size.
    #[must_use]
    pub const fn with_max_file_bytes(mut self, max_file_bytes: u64) -> Self {
        self.max_file_bytes = max_file_bytes;
        self
    }

    /// Checks whether a filename or path component indicates secret/credential material.
    #[must_use]
    pub fn classify_secret_filename(file_name_lower: &str) -> Option<&'static str> {
        // Explicit safe templates allowed so config keys remain discoverable.
        const SAFE_ENV_TEMPLATES: &[&str] = &[
            ".env.example",
            ".env.sample",
            ".env.template",
            ".env.dist",
            ".env.schema",
        ];
        if SAFE_ENV_TEMPLATES.contains(&file_name_lower) {
            return None;
        }
        if file_name_lower == ".env" || file_name_lower.starts_with(".env.") {
            return Some("dotenv_secret_file");
        }
        for ext in [".pem", ".key", ".p12", ".pfx", ".keystore", ".jks"] {
            if file_name_lower.ends_with(ext) {
                return Some("private_key_or_cert");
            }
        }
        const SECRET_NAMES: &[&str] = &[
            "id_rsa",
            "id_ed25519",
            "id_ecdsa",
            "credentials.json",
            "service-account.json",
            "secrets.yaml",
            "secrets.yml",
            "secrets.toml",
            "secrets.json",
            ".npmrc",
            ".pypirc",
            ".netrc",
        ];
        if SECRET_NAMES.contains(&file_name_lower) {
            return Some("credential_file");
        }
        None
    }

    /// Checks whether any directory segment is an excluded internal or sensitive folder.
    fn has_excluded_dir_component(rel_path: &Path) -> Result<bool, &'static str> {
        if rel_path.as_os_str().is_empty() {
            return Err("empty_path");
        }
        for comp in rel_path.components() {
            match comp {
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err("non_relative_or_parent_traversal");
                }
                Component::CurDir => return Err("non_normal_relative_path"),
                Component::Normal(os) => {
                    let Some(s) = os.to_str() else {
                        return Err("non_utf8_path");
                    };
                    // Tabs/newlines and backslashes are legal literal Unix filenames;
                    // VCS parsers preserve them using NUL framing or quoted paths.
                    if s.contains('\0') {
                        return Err("unsafe_path_character");
                    }
                    let lower = s.to_ascii_lowercase();
                    if let Some(reason) = Self::classify_secret_filename(&lower) {
                        return Err(reason);
                    }
                    if matches!(lower.as_str(), ".aws" | ".ssh" | ".gnupg") {
                        return Err("sensitive_dotdir");
                    }
                    if matches!(
                        lower.as_str(),
                        ".git"
                            | ".jj"
                            | ".ledger"
                            | "node_modules"
                            | "target"
                            | "__pycache__"
                            | ".venv"
                            | "venv"
                            | "dist"
                            | ".next"
                            | ".turbo"
                    ) {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    /// Examines lexical ancestors without canonicalizing missing historical leaves.
    /// Stop at symlinks: historical object names below them do not access their targets;
    /// live reads additionally validate the fully canonical path in `evaluate_path`.
    fn crosses_nested_repository(&self, rel_path: &Path) -> bool {
        let mut ancestor = self.canonical_root.clone();
        for component in rel_path.components() {
            ancestor.push(component.as_os_str());
            match std::fs::symlink_metadata(&ancestor) {
                Ok(meta) if meta.file_type().is_symlink() => break,
                Ok(meta) if meta.is_dir() => {
                    if [".git", ".jj"]
                        .iter()
                        .any(|marker| std::fs::symlink_metadata(ancestor.join(marker)).is_ok())
                    {
                        return true;
                    }
                }
                // Descendants of a missing path cannot have a live repository marker.
                _ => break,
            }
        }
        false
    }
}

impl ScopePolicy for DefaultScopeGuard {
    fn workspace_root(&self) -> &Path {
        &self.canonical_root
    }

    fn max_file_bytes(&self) -> u64 {
        self.max_file_bytes
    }

    fn is_relative_name_allowed(&self, rel_path: &Path) -> bool {
        match Self::has_excluded_dir_component(rel_path) {
            Ok(true) | Err(_) => return false,
            Ok(false) => {}
        }
        !self.crosses_nested_repository(rel_path)
    }

    fn evaluate_path(&self, candidate: &Path) -> ScopeDecision {
        if candidate
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        {
            return ScopeDecision::DeniedOutsideWorkspace;
        }
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.canonical_root.join(candidate)
        };

        let Ok(requested_rel) = joined
            .strip_prefix(&self.canonical_root)
            .or_else(|_| joined.strip_prefix(&self.requested_root))
        else {
            return ScopeDecision::DeniedOutsideWorkspace;
        };
        // Check the requested spelling before resolving links or `..`; safe targets
        // cannot launder excluded paths such as `.env -> safe.txt` or `nested/../file`.
        match Self::has_excluded_dir_component(requested_rel) {
            Err(
                "non_relative_or_parent_traversal"
                | "non_normal_relative_path"
                | "empty_path"
                | "non_utf8_path"
                | "unsafe_path_character",
            ) => {
                return ScopeDecision::DeniedOutsideWorkspace;
            }
            Err(reason) => return ScopeDecision::DeniedSecret { reason },
            Ok(true) => return ScopeDecision::ExcludedInternal,
            Ok(false) => {}
        }
        if self.crosses_nested_repository(requested_rel) {
            return ScopeDecision::ExcludedInternal;
        }

        let Ok(canonical) = std::fs::canonicalize(&joined) else {
            return ScopeDecision::DeniedOutsideWorkspace;
        };

        let Ok(rel) = canonical.strip_prefix(&self.canonical_root) else {
            return ScopeDecision::DeniedOutsideWorkspace;
        };

        if rel.as_os_str().is_empty() {
            return ScopeDecision::ExcludedInternal;
        }

        match Self::has_excluded_dir_component(rel) {
            Err(reason) => return ScopeDecision::DeniedSecret { reason },
            Ok(true) => return ScopeDecision::ExcludedInternal,
            Ok(false) => {}
        }
        if self.crosses_nested_repository(rel) {
            return ScopeDecision::ExcludedInternal;
        }

        let rel_posix = rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/");

        ScopeDecision::Allowed {
            canonical_path: canonical,
            rel_path: rel_posix,
        }
    }
}

/// Sanitizes untrusted repository text (docstrings, comments, commit subjects) by stripping
/// ASCII control characters (except `\n` and `\t`) and bounding length.
#[must_use]
pub fn sanitize_untrusted_text(raw: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(raw.len().min(max_chars));
    for (count, ch) in raw
        .chars()
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .enumerate()
    {
        if count >= max_chars {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}
