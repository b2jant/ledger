//! TOCTOU-free single-buffer file reader (`SingleBufferReader`).
//!
//! Reads a file once into an immutable `Arc<[u8]>`, verifies pre/post metadata stability
//! across bounded retries to catch concurrent writes, computes `ContentHash` (`blake3`),
//! and renders 1-indexed line slices directly from that exact buffer.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use thiserror::Error;

use crate::ids::{ContentHash, LineSpan};
use crate::scope::{ScopeDecision, ScopePolicy};

/// Errors produced when loading a file into a [`FileBuffer`].
#[derive(Debug, Error)]
pub enum SingleBufferError {
    /// Path rejected by [`ScopePolicy`] (secret file, outside workspace, or internal dir).
    #[error("path '{path}' blocked by scope policy: {reason}")]
    DeniedByPolicy {
        /// Requested path string.
        path: String,
        /// Denial explanation.
        reason: String,
    },
    /// File exceeds configured byte limit.
    #[error("file '{path}' size ({bytes} B) exceeds limit ({limit} B)")]
    FileTooLarge {
        /// File path.
        path: String,
        /// Observed size.
        bytes: u64,
        /// Maximum allowed bytes.
        limit: u64,
    },
    /// File contains non-UTF-8 binary data.
    #[error("file '{path}' is binary or invalid UTF-8")]
    BinaryOrNonUtf8 {
        /// File path.
        path: String,
    },
    /// File changed concurrently during all bounded read attempts.
    #[error("file '{path}' changed concurrently during read retries (unstable file)")]
    UnstableConcurrentWrite {
        /// File path.
        path: String,
    },
    /// Underlying filesystem error.
    #[error("I/O error reading '{path}': {source}")]
    Io {
        /// File path.
        path: String,
        /// OS error.
        source: std::io::Error,
    },
}

/// Immutable in-memory snapshot of a single UTF-8 source file with precomputed line offsets.
#[derive(Debug, Clone)]
pub struct FileBuffer {
    rel_path: String,
    bytes: Arc<[u8]>,
    content_hash: ContentHash,
    line_starts: Arc<[usize]>,
}

impl FileBuffer {
    /// Constructs a `FileBuffer` from verified UTF-8 text (useful in unit tests and live buffers).
    #[must_use]
    pub fn from_string(rel_path: impl Into<String>, text: impl Into<String>) -> Self {
        let s: String = text.into();
        let bytes: Arc<[u8]> = Arc::from(s.into_bytes());
        let content_hash = ContentHash::of_bytes(&bytes);
        let line_starts = Self::compute_line_starts(&bytes);
        Self {
            rel_path: rel_path.into(),
            bytes,
            content_hash,
            line_starts: Arc::from(line_starts),
        }
    }

    fn compute_line_starts(bytes: &[u8]) -> Vec<usize> {
        let mut starts = vec![0usize];
        for (idx, &b) in bytes.iter().enumerate() {
            if b == b'\n' && idx + 1 < bytes.len() {
                starts.push(idx + 1);
            }
        }
        starts
    }

    /// Workspace-relative POSIX path.
    #[must_use]
    pub fn rel_path(&self) -> &str {
        &self.rel_path
    }

    /// Raw UTF-8 validated bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Borrowed `&str` view over the immutable buffer.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // Validated as UTF-8 on construction (`from_string` or `SingleBufferReader::read_file`).
        std::str::from_utf8(&self.bytes).unwrap_or("")
    }

    /// BLAKE3 digest of the exact buffer bytes.
    #[must_use]
    pub fn content_hash(&self) -> &ContentHash {
        &self.content_hash
    }

    /// Total 1-indexed line count.
    #[must_use]
    pub fn line_count(&self) -> u32 {
        u32::try_from(self.line_starts.len()).unwrap_or(u32::MAX)
    }

    /// Renders a numbered source slice for 1-indexed inclusive `span` (`"  14| pub fn ..."`).
    #[must_use]
    pub fn render_numbered_slice(&self, span: LineSpan) -> String {
        if self.bytes.is_empty() {
            return String::new();
        }
        let total = self.line_count().max(1);
        let start = span.start_line.clamp(1, total) as usize;
        let end = span.end_line.clamp(start as u32, total) as usize;

        let text = self.as_str();
        let mut out = String::new();
        for line_no in start..=end {
            let byte_start = self.line_starts[line_no - 1];
            let byte_end = if line_no < self.line_starts.len() {
                self.line_starts[line_no]
            } else {
                self.bytes.len()
            };
            let raw_line = text[byte_start..byte_end]
                .trim_end_matches('\n')
                .trim_end_matches('\r');
            out.push_str(&format!("{line_no:>4}| {raw_line}\n"));
        }
        out
    }

    /// Extracts unnumbered text for 1-indexed inclusive `span`.
    #[must_use]
    pub fn slice_lines(&self, span: LineSpan) -> &str {
        if self.bytes.is_empty() {
            return "";
        }
        let total = self.line_count().max(1);
        let start = span.start_line.clamp(1, total) as usize;
        let end = span.end_line.clamp(start as u32, total) as usize;
        let byte_start = self.line_starts[start - 1];
        let byte_end = if end < self.line_starts.len() {
            self.line_starts[end]
        } else {
            self.bytes.len()
        };
        &self.as_str()[byte_start..byte_end]
    }
}

/// Safe single-buffer reader backed by a [`ScopePolicy`].
#[derive(Debug, Clone)]
pub struct SingleBufferReader<P: ScopePolicy> {
    policy: P,
}

impl<P: ScopePolicy> SingleBufferReader<P> {
    /// Maximum read attempts when detecting a concurrent mid-read modification.
    const MAX_STABILITY_RETRIES: usize = 3;

    /// Wraps a [`ScopePolicy`].
    #[must_use]
    pub const fn new(policy: P) -> Self {
        Self { policy }
    }

    /// Returns a reference to the underlying [`ScopePolicy`].
    #[must_use]
    pub const fn policy(&self) -> &P {
        &self.policy
    }

    /// Reads `candidate` into a verified [`FileBuffer`], enforcing policy, size limits,
    /// UTF-8 validity, and pre/post read stability checks.
    pub fn read_file(&self, candidate: &Path) -> Result<FileBuffer, SingleBufferError> {
        let (canonical_path, rel_path) = match self.policy.evaluate_path(candidate) {
            ScopeDecision::Allowed {
                canonical_path,
                rel_path,
            } => (canonical_path, rel_path),
            ScopeDecision::DeniedSecret { reason } => {
                return Err(SingleBufferError::DeniedByPolicy {
                    path: candidate.display().to_string(),
                    reason: format!("secret_denylist({reason})"),
                });
            }
            ScopeDecision::DeniedOutsideWorkspace => {
                return Err(SingleBufferError::DeniedByPolicy {
                    path: candidate.display().to_string(),
                    reason: "outside_workspace_boundary".to_string(),
                });
            }
            ScopeDecision::ExcludedInternal => {
                return Err(SingleBufferError::DeniedByPolicy {
                    path: candidate.display().to_string(),
                    reason: "excluded_internal_path".to_string(),
                });
            }
        };

        let max_bytes = self.policy.max_file_bytes();

        for _attempt in 0..Self::MAX_STABILITY_RETRIES {
            let mut file = File::open(&canonical_path).map_err(|source| SingleBufferError::Io {
                path: rel_path.clone(),
                source,
            })?;
            let meta_before = file.metadata().map_err(|source| SingleBufferError::Io {
                path: rel_path.clone(),
                source,
            })?;
            let len_before = meta_before.len();
            if len_before > max_bytes {
                return Err(SingleBufferError::FileTooLarge {
                    path: rel_path,
                    bytes: len_before,
                    limit: max_bytes,
                });
            }

            let cap = usize::try_from(len_before).unwrap_or(0);
            let mut buf = Vec::with_capacity(cap.saturating_add(1));
            file.by_ref()
                .take(max_bytes.saturating_add(1))
                .read_to_end(&mut buf)
                .map_err(|source| SingleBufferError::Io {
                    path: rel_path.clone(),
                    source,
                })?;

            if buf.len() as u64 > max_bytes {
                return Err(SingleBufferError::FileTooLarge {
                    path: rel_path,
                    bytes: buf.len() as u64,
                    limit: max_bytes,
                });
            }

            let meta_after = file.metadata().map_err(|source| SingleBufferError::Io {
                path: rel_path.clone(),
                source,
            })?;
            let mtime_before = meta_before
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok());
            let mtime_after = meta_after
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok());

            if meta_after.len() != buf.len() as u64 || mtime_before != mtime_after {
                continue;
            }

            // Reject binary / non-UTF-8 files early.
            if buf.contains(&0) || std::str::from_utf8(&buf).is_err() {
                return Err(SingleBufferError::BinaryOrNonUtf8 { path: rel_path });
            }

            let content_hash = ContentHash::of_bytes(&buf);
            let line_starts = FileBuffer::compute_line_starts(&buf);
            return Ok(FileBuffer {
                rel_path,
                bytes: Arc::from(buf),
                content_hash,
                line_starts: Arc::from(line_starts),
            });
        }

        Err(SingleBufferError::UnstableConcurrentWrite { path: rel_path })
    }
}
