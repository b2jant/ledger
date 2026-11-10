//! Strongly typed domain identifiers and spans preventing primitive obsession.

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Error returned when parsing a snapshot handle or identifier fails.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum IdParseError {
    /// Handle string did not conform to `snap:<ws>:<gen>:<qid>`.
    #[error("invalid snapshot handle format: '{0}'")]
    InvalidSnapshotHandle(String),
}

/// Deterministic identifier for a canonical workspace checkout root.
///
/// Separate Git worktrees or Jujutsu workspaces receive distinct `WorkspaceId`s
/// even when sharing an underlying `.git` or `.jj/repo` store.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkspaceId(String);

impl WorkspaceId {
    /// Derives a stable 16-hex-char workspace identifier from a canonical root path.
    #[must_use]
    pub fn from_canonical_path(canonical_root: &Path) -> Self {
        let hash = blake3::hash(canonical_root.to_string_lossy().as_bytes());
        let hex = hash.to_hex();
        Self(hex[..16].to_string())
    }

    /// Creates a `WorkspaceId` from a persisted identifier string.
    #[must_use]
    pub fn from_raw(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// Short 8-char prefix suitable for compact snapshot handles.
    #[must_use]
    pub fn short_prefix(&self) -> &str {
        let len = self.0.len().min(8);
        &self.0[..len]
    }

    /// Returns the underlying string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Monotonically increasing snapshot generation published atomically across SQLite and `petgraph`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GenerationId(pub u64);

impl GenerationId {
    /// Generation 0 represents an unindexed / warming cold-start workspace.
    pub const COLD_START: Self = Self(0);

    /// Returns the next monotonic generation identifier.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl fmt::Display for GenerationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "g{}", self.0)
    }
}

/// BLAKE3 digest of raw file or symbol slice content (16 hex chars / 64-bit prefix for compact UI + full 64 hex for storage).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentHash(String);

impl ContentHash {
    /// Computes the full BLAKE3 hex digest of `bytes`.
    #[must_use]
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
    }

    /// Wraps an already-computed hex digest string.
    #[must_use]
    pub fn from_hex(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    /// Short 12-hex-char fingerprint for compact MCP evidence headers.
    #[must_use]
    pub fn short(&self) -> &str {
        let len = self.0.len().min(12);
        &self.0[..len]
    }

    /// Full hex representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.short())
    }
}

/// BLAKE3 digest of a file's exported interface (exported symbol signatures, imports, and contracts).
///
/// When a live edit modifies a function body without changing `InterfaceHash`, incoming
/// cross-file edges remain valid; when `InterfaceHash` changes, cross-file edges are
/// re-resolved or conservatively downgraded to `StaleAfterEdit`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InterfaceHash(String);

impl InterfaceHash {
    /// Computes an `InterfaceHash` over canonicalized interface descriptors.
    #[must_use]
    pub fn of_descriptors<'a>(items: impl IntoIterator<Item = &'a str>) -> Self {
        let mut hasher = blake3::Hasher::new();
        for item in items {
            hasher.update(item.as_bytes());
            hasher.update(&[0]);
        }
        Self(hasher.finalize().to_hex().to_string())
    }

    /// Wraps a persisted hex string.
    #[must_use]
    pub fn from_hex(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    /// Returns the hex string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Stable logical identity of a symbol slot within a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LogicalSymbolKey(String);

impl LogicalSymbolKey {
    /// Derives a deterministic symbol key from path, kind, scope chain, name, and disambiguator.
    #[must_use]
    pub fn derive(
        rel_path: &str,
        kind: &str,
        scope_chain: &str,
        name: &str,
        disambiguator: u32,
    ) -> Self {
        let mut hasher = blake3::Hasher::new();
        for part in [rel_path, kind, scope_chain, name] {
            hasher.update(part.as_bytes());
            hasher.update(&[0x1f]);
        }
        hasher.update(&disambiguator.to_le_bytes());
        let hex = hasher.finalize().to_hex();
        Self(format!("sym_{}", &hex[..16]))
    }

    /// Wraps a persisted key string.
    #[must_use]
    pub fn from_raw(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// Underlying string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LogicalSymbolKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Half-open byte range `[start, end)` within a file buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ByteSpan {
    /// Inclusive starting byte offset.
    pub start: usize,
    /// Exclusive ending byte offset.
    pub end: usize,
}

impl ByteSpan {
    /// Constructs a validated byte span where `end >= start`.
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Self {
        if end < start {
            Self { start, end: start }
        } else {
            Self { start, end }
        }
    }

    /// Byte length of the span.
    #[must_use]
    pub const fn len(self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// Returns true if the span has zero bytes.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len() == 0
    }
}

/// 1-indexed inclusive line range `[start_line, end_line]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LineSpan {
    /// 1-indexed starting line.
    pub start_line: u32,
    /// 1-indexed inclusive ending line.
    pub end_line: u32,
}

impl LineSpan {
    /// Constructs a 1-indexed inclusive line span.
    #[must_use]
    pub const fn new(start_line: u32, end_line: u32) -> Self {
        let s = if start_line == 0 { 1 } else { start_line };
        let e = if end_line < s { s } else { end_line };
        Self {
            start_line: s,
            end_line: e,
        }
    }

    /// Total number of lines covered.
    #[must_use]
    pub const fn line_count(self) -> u32 {
        self.end_line.saturating_sub(self.start_line) + 1
    }
}

impl fmt::Display for LineSpan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.start_line, self.end_line)
    }
}

/// Approximate token count using `o200k` / `cl100k` code-calibrated byte ratio (~3.6 bytes/token).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ApproxTokens(pub u32);

impl ApproxTokens {
    /// Estimates token count for UTF-8 code/text bytes conservatively.
    #[must_use]
    pub fn estimate(text: &str) -> Self {
        // 10 / 36 (~3.6 bytes per token) rounded up conservatively for punctuation-dense code.
        let bytes = text.len() as u64;
        let est = bytes.saturating_mul(10).div_ceil(36);
        Self(u32::try_from(est).unwrap_or(u32::MAX))
    }
}

impl fmt::Display for ApproxTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Opaque snapshot-bound handle (`snap:<ws_short>:g<gen>:<query_id>`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SnapshotHandle {
    /// Short workspace prefix verifying workspace binding.
    pub workspace_short: String,
    /// Generation identifier pinned when `ledger_context` executed.
    pub generation: GenerationId,
    /// Short deterministic query hash.
    pub query_id: String,
}

impl SnapshotHandle {
    /// Builds a new `SnapshotHandle`.
    #[must_use]
    pub fn new(workspace_id: &WorkspaceId, generation: GenerationId, seed: &str) -> Self {
        let hash = blake3::hash(seed.as_bytes());
        let hex = hash.to_hex();
        Self {
            workspace_short: workspace_id.short_prefix().to_string(),
            generation,
            query_id: hex[..8].to_string(),
        }
    }
}

impl fmt::Display for SnapshotHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "snap:{}:{}:{}",
            self.workspace_short, self.generation, self.query_id
        )
    }
}

impl FromStr for SnapshotHandle {
    type Err = IdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 4 || parts[0] != "snap" {
            return Err(IdParseError::InvalidSnapshotHandle(s.to_string()));
        }
        let gen_str = parts[2]
            .strip_prefix('g')
            .ok_or_else(|| IdParseError::InvalidSnapshotHandle(s.to_string()))?;
        let gen_num = gen_str
            .parse::<u64>()
            .map_err(|_| IdParseError::InvalidSnapshotHandle(s.to_string()))?;
        if parts[1].is_empty() || parts[3].is_empty() {
            return Err(IdParseError::InvalidSnapshotHandle(s.to_string()));
        }
        Ok(Self {
            workspace_short: parts[1].to_string(),
            generation: GenerationId(gen_num),
            query_id: parts[3].to_string(),
        })
    }
}
