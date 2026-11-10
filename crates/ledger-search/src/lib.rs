//! Embedded `ripgrep` (`grep-searcher` + `grep-regex` + `ignore`) live lexical engine.
//!
//! Composes directly with [`ledger_core::ScopePolicy`] so cold-start queries, exact
//! error-string fast paths, and mid-edit dirty files return sub-20ms verified matches
//! while strictly enforcing secret and workspace symlink boundaries.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use ignore::WalkBuilder;
use ledger_core::{ScopeDecision, ScopePolicy};
use thiserror::Error;

/// Errors returned by [`LexicalSearcher`].
#[derive(Debug, Error)]
pub enum SearchError {
    /// Pattern could not be compiled into a valid finite regex.
    #[error("invalid search pattern: {0}")]
    InvalidPattern(String),
}

/// One matched line returned by [`LexicalSearcher`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexicalMatch {
    /// Workspace-relative POSIX path (`src/api/orders/cursor.rs`).
    pub rel_path: String,
    /// Canonical filesystem path.
    pub canonical_path: PathBuf,
    /// 1-indexed line number of the match.
    pub line_number: u32,
    /// Trimmed matched line preview.
    pub line_text: String,
}

/// Outcome of a bounded live search pass, explicitly distinguishing complete zero-match
/// scans from deadline/limit-capped partial scans.
#[derive(Debug, Clone)]
pub struct LexicalSearchReport {
    /// Ranked/ordered matches collected within budget.
    pub matches: Vec<LexicalMatch>,
    /// Number of allowed workspace files scanned.
    pub files_scanned: usize,
    /// Number of secret files skipped by `ScopePolicy`.
    pub secret_files_excluded: usize,
    /// True if the scan terminated early due to `max_matches` or `deadline`.
    pub search_incomplete: bool,
}

/// Bounded scan configuration for [`RipgrepSearcher`].
#[derive(Debug, Clone)]
pub struct SearchBudget {
    /// Maximum matches to collect before stopping.
    pub max_matches: usize,
    /// Maximum files to inspect.
    pub max_files: usize,
    /// Wall-clock deadline for cold-start responsiveness.
    pub timeout: Duration,
    /// Case-insensitive matching flag.
    pub case_insensitive: bool,
}

impl Default for SearchBudget {
    fn default() -> Self {
        Self {
            max_matches: 64,
            max_files: 10_000,
            timeout: Duration::from_millis(350),
            case_insensitive: false,
        }
    }
}

/// Composable trait for live workspace lexical searchers.
pub trait LexicalSearcher: Send + Sync {
    /// Searches for an exact literal substring across allowed workspace files.
    fn search_literal(
        &self,
        literal: &str,
        budget: &SearchBudget,
    ) -> Result<LexicalSearchReport, SearchError>;

    /// Searches for any of `terms` (disjunction) across allowed workspace files.
    fn search_terms(
        &self,
        terms: &[String],
        budget: &SearchBudget,
    ) -> Result<LexicalSearchReport, SearchError>;

    /// Enumerates all allowed workspace files (plus count of excluded secret files).
    fn list_allowed_files(&self) -> (Vec<(PathBuf, String)>, usize);
}

/// Production [`LexicalSearcher`] powered by `ripgrep`'s core Rust crates.
#[derive(Debug, Clone)]
pub struct RipgrepSearcher<P: ScopePolicy> {
    policy: P,
}

impl<P: ScopePolicy> RipgrepSearcher<P> {
    /// Creates a new `RipgrepSearcher` guarded by `policy`.
    #[must_use]
    pub const fn new(policy: P) -> Self {
        Self { policy }
    }

    /// Reference to the underlying [`ScopePolicy`].
    #[must_use]
    pub const fn policy(&self) -> &P {
        &self.policy
    }

    fn execute_pattern(
        &self,
        pattern: &str,
        budget: &SearchBudget,
    ) -> Result<LexicalSearchReport, SearchError> {
        let matcher = RegexMatcherBuilder::new()
            .case_insensitive(budget.case_insensitive)
            .line_terminator(Some(b'\n'))
            .build(pattern)
            .map_err(|e| SearchError::InvalidPattern(e.to_string()))?;

        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(b'\x00'))
            .line_number(true)
            .build();

        let started = Instant::now();
        let (allowed_files, secret_files_excluded) = self.list_allowed_files();

        let mut matches = Vec::new();
        let mut files_scanned = 0usize;
        let mut search_incomplete = false;

        for (canonical_path, rel_path) in allowed_files {
            if files_scanned >= budget.max_files || started.elapsed() >= budget.timeout {
                search_incomplete = true;
                break;
            }
            files_scanned += 1;

            let mut file_hit_limit = false;
            let _ = searcher.search_path(
                &matcher,
                &canonical_path,
                UTF8(|line_num, line_str| {
                    let trimmed = line_str.trim().chars().take(240).collect::<String>();
                    matches.push(LexicalMatch {
                        rel_path: rel_path.clone(),
                        canonical_path: canonical_path.clone(),
                        line_number: u32::try_from(line_num).unwrap_or(u32::MAX),
                        line_text: trimmed,
                    });
                    if matches.len() >= budget.max_matches {
                        file_hit_limit = true;
                        Ok(false)
                    } else {
                        Ok(true)
                    }
                }),
            );

            if file_hit_limit {
                search_incomplete = true;
                break;
            }
        }

        Ok(LexicalSearchReport {
            matches,
            files_scanned,
            secret_files_excluded,
            search_incomplete,
        })
    }
}

impl<P: ScopePolicy> LexicalSearcher for RipgrepSearcher<P> {
    fn search_literal(
        &self,
        literal: &str,
        budget: &SearchBudget,
    ) -> Result<LexicalSearchReport, SearchError> {
        let trimmed = literal.trim();
        if trimmed.is_empty() {
            return Ok(LexicalSearchReport {
                matches: Vec::new(),
                files_scanned: 0,
                secret_files_excluded: 0,
                search_incomplete: false,
            });
        }
        let escaped = regex::escape(trimmed);
        self.execute_pattern(&escaped, budget)
    }

    fn search_terms(
        &self,
        terms: &[String],
        budget: &SearchBudget,
    ) -> Result<LexicalSearchReport, SearchError> {
        let valid: Vec<String> = terms
            .iter()
            .map(|t| t.trim())
            .filter(|t| t.len() >= 2)
            .take(12)
            .map(regex::escape)
            .collect();
        if valid.is_empty() {
            return Ok(LexicalSearchReport {
                matches: Vec::new(),
                files_scanned: 0,
                secret_files_excluded: 0,
                search_incomplete: false,
            });
        }
        let pattern = valid.join("|");
        let mut ci_budget = budget.clone();
        ci_budget.case_insensitive = true;
        self.execute_pattern(&pattern, &ci_budget)
    }

    fn list_allowed_files(&self) -> (Vec<(PathBuf, String)>, usize) {
        let root = self.policy.workspace_root().to_path_buf();
        let max_bytes = self.policy.max_file_bytes();

        let walker = WalkBuilder::new(&root)
            .hidden(false) // Inspect dotfiles like `.env.example` so `ScopePolicy` makes explicit decisions.
            .git_ignore(true)
            .git_global(false)
            .git_exclude(true)
            .follow_links(false) // Never follow symlinks blindly during traversal.
            .max_filesize(Some(max_bytes))
            .filter_entry({
                let root_clone = root.clone();
                move |entry| {
                    if entry.path() == root_clone {
                        return true;
                    }
                    // Prune the repository root itself, including Git worktrees whose
                    // `.git` is a file. Skipping only metadata directories still leaks
                    // nested source into the outer workspace's traversal.
                    if entry.file_type().is_some_and(|kind| kind.is_dir())
                        && [".git", ".jj"].iter().any(|marker| {
                            std::fs::symlink_metadata(entry.path().join(marker)).is_ok()
                        })
                    {
                        return false;
                    }
                    if let Ok(rel) = entry.path().strip_prefix(&root_clone) {
                        for comp in rel.components() {
                            let s = comp.as_os_str().to_string_lossy();
                            if matches!(
                                s.as_ref(),
                                ".git" | ".jj" | ".ledger" | "node_modules" | "target"
                            ) {
                                return false;
                            }
                        }
                    }
                    true
                }
            })
            .build();

        let mut allowed = Vec::new();
        let mut secret_excluded = 0usize;

        for dent in walker.flatten() {
            let Some(ft) = dent.file_type() else {
                continue;
            };
            if ft.is_dir() {
                continue;
            }
            match self.policy.evaluate_path(dent.path()) {
                ScopeDecision::Allowed {
                    canonical_path,
                    rel_path,
                } => allowed.push((canonical_path, rel_path)),
                ScopeDecision::DeniedSecret { .. } => {
                    secret_excluded += 1;
                }
                ScopeDecision::DeniedOutsideWorkspace | ScopeDecision::ExcludedInternal => {}
            }
        }

        allowed.sort_by(|a, b| a.1.cmp(&b.1));
        (allowed, secret_excluded)
    }
}
