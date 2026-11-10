//! Read-only Git CLI backend (`git --no-optional-locks`).

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use ledger_core::scope::sanitize_untrusted_text;
use ledger_core::{LineSpan, ScopePolicy, VcsBackendKind, VcsFileTouch, VcsRevision};

use crate::backend::{CommandOutput, CommandRunner, VcsBackend, VcsError, VcsHeadFingerprint};

/// Bound retained attribution metadata even for files with thousands of hunks.
const MAX_HUNK_RANGES: usize = 64;
const LOG_FORMAT: &str = "--format=%x1e%H%x00%at%x00%an%x00%s%x00";

/// Non-mutating Git history provider using `git --no-optional-locks`.
#[derive(Debug, Clone)]
pub struct GitCliBackend {
    runner: Arc<dyn CommandRunner>,
}

impl GitCliBackend {
    /// Creates a new [`GitCliBackend`] using `runner`.
    #[must_use]
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    /// Returns a reference to the underlying command runner.
    #[must_use]
    pub fn runner(&self) -> &dyn CommandRunner {
        self.runner.as_ref()
    }
}

impl VcsBackend for GitCliBackend {
    fn kind(&self) -> VcsBackendKind {
        VcsBackendKind::Git
    }

    fn head_fingerprint(&self, repo_root: &Path) -> Result<Option<VcsHeadFingerprint>, VcsError> {
        let Some(git_dir) = resolve_git_dir(repo_root)? else {
            return Ok(None);
        };
        let Some(head) = read_optional(&git_dir.join("HEAD"))? else {
            return Ok(None);
        };
        let head = head.trim();
        let sha = if let Some(reference) = head.strip_prefix("ref: ") {
            resolve_reference(&git_dir, reference)?
        } else {
            Some(validate_sha(head)?.to_string())
        };
        Ok(sha.map(|sha| VcsHeadFingerprint {
            kind: VcsBackendKind::Git,
            display_badge: format!("git:{}", &sha[..7]),
            state_digest: blake3::hash(sha.as_bytes()).to_hex().to_string(),
        }))
    }

    fn recent_revisions(
        &self,
        repo_root: &Path,
        scope: &dyn ScopePolicy,
        max_revisions: usize,
    ) -> Result<Vec<VcsRevision>, VcsError> {
        if max_revisions == 0 {
            return Ok(Vec::new());
        }
        let limit = max_revisions.to_string();
        let output = self.runner.run(
            repo_root,
            "git",
            &[
                "--no-optional-locks",
                "log",
                "-n",
                &limit,
                "--no-merges",
                "-M",
                "--numstat",
                "-z",
                "--patch",
                "--unified=0",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--no-relative",
                LOG_FORMAT,
            ],
        )?;
        if output.status_code != Some(0) {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("does not have any commits yet")
                || stderr.contains("bad default revision 'HEAD'")
            {
                return Ok(Vec::new());
            }
            return Err(command_error(&output));
        }
        parse_revisions(&output.stdout, scope, max_revisions)
    }

    fn revision_file_diff(
        &self,
        repo_root: &Path,
        scope: &dyn ScopePolicy,
        rev_id: &str,
        rel_path: &str,
        max_lines: usize,
    ) -> Result<String, VcsError> {
        if rel_path.is_empty() || !scope.is_history_path_allowed(Path::new(rel_path)) {
            return Err(VcsError::DeniedByPolicy(rel_path.to_string()));
        }
        validate_revision(rev_id)?;
        let output = self.runner.run(
            repo_root,
            "git",
            &[
                "--no-optional-locks",
                "--literal-pathspecs",
                "show",
                "--format=short",
                "--unified=3",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                rev_id,
                "--",
                rel_path,
            ],
        )?;
        if output.status_code != Some(0) {
            return Err(command_error(&output));
        }
        let max_lines = max_lines.max(1);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut lines = stdout.lines();
        let mut diff = String::new();
        for line in lines.by_ref().take(max_lines) {
            diff.push_str(&sanitize_untrusted_text(line, usize::MAX));
            diff.push('\n');
        }
        if lines.next().is_some() {
            diff.push_str(&format!("// ... [diff truncated at {max_lines} lines]\n"));
        }
        Ok(diff)
    }
}

/// Reads optional metadata without hiding permission or malformed UTF-8 errors.
fn read_optional(path: &Path) -> Result<Option<String>, VcsError> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Resolves ordinary repositories and relative or absolute worktree gitdir files.
fn resolve_git_dir(repo_root: &Path) -> Result<Option<PathBuf>, VcsError> {
    let dot_git = repo_root.join(".git");
    match fs::metadata(&dot_git) {
        Ok(metadata) if metadata.is_dir() => Ok(Some(dot_git)),
        Ok(_) => {
            let content = fs::read_to_string(&dot_git)?;
            let target = content
                .trim()
                .strip_prefix("gitdir:")
                .map(str::trim)
                .filter(|target| !target.is_empty())
                .ok_or_else(|| VcsError::Parse("invalid .git gitdir file".to_string()))?;
            Ok(Some(repo_root.join(target)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Accepts both SHA-1 and SHA-256 repository object IDs before slicing badges.
fn validate_sha(sha: &str) -> Result<&str, VcsError> {
    if matches!(sha.len(), 40 | 64) && sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(sha)
    } else {
        Err(VcsError::Parse("invalid Git object ID".to_string()))
    }
}

/// Reads loose refs first, then streams packed refs from the worktree common dir.
fn resolve_reference(git_dir: &Path, reference: &str) -> Result<Option<String>, VcsError> {
    if !reference.starts_with("refs/")
        || reference.contains('\0')
        || Path::new(reference)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(VcsError::Parse("invalid symbolic HEAD ref".to_string()));
    }
    let common_dir = read_optional(&git_dir.join("commondir"))?.map_or_else(
        || git_dir.to_path_buf(),
        |common| git_dir.join(common.trim()),
    );
    for directory in [git_dir, common_dir.as_path()] {
        if let Some(sha) = read_optional(&directory.join(reference))? {
            return Ok(Some(validate_sha(sha.trim())?.to_string()));
        }
    }
    for directory in [git_dir, common_dir.as_path()] {
        let packed_refs = match fs::File::open(directory.join("packed-refs")) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        for line in BufReader::new(packed_refs).lines() {
            let line = line?;
            if let Some((sha, name)) = line.split_once(' ')
                && name == reference
            {
                return Ok(Some(validate_sha(sha)?.to_string()));
            }
        }
    }
    // A valid symbolic HEAD with no loose or packed target is an unborn branch.
    Ok(None)
}

/// Restricts revision syntax to ordinary IDs, ref names, and ancestry selectors.
fn validate_revision(rev_id: &str) -> Result<(), VcsError> {
    if rev_id.is_empty()
        || rev_id.starts_with('-')
        || !rev_id.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/' | b'~' | b'^')
        })
    {
        return Err(VcsError::Parse("unsafe Git revision ID".to_string()));
    }
    Ok(())
}

fn command_error(output: &CommandOutput) -> VcsError {
    VcsError::CommandFailed {
        program: "git".to_string(),
        code: output.status_code,
        stderr: sanitize_untrusted_text(&String::from_utf8_lossy(&output.stderr), 1024),
    }
}

/// Consumes an exact NUL-delimited field, including raw filenames with newlines.
fn take_field<'a>(remaining: &mut &'a [u8]) -> Result<&'a [u8], VcsError> {
    let end = remaining
        .iter()
        .position(|&byte| byte == 0)
        .ok_or_else(|| VcsError::Parse("unterminated Git log field".to_string()))?;
    let field = &remaining[..end];
    *remaining = &remaining[end + 1..];
    Ok(field)
}

/// Parses framed metadata and raw numstat paths; diff content is never retained.
fn parse_revisions(
    output: &[u8],
    scope: &dyn ScopePolicy,
    max_revisions: usize,
) -> Result<Vec<VcsRevision>, VcsError> {
    let mut remaining = output;
    let mut revisions = Vec::new();
    while !remaining.is_empty() && revisions.len() < max_revisions {
        remaining = remaining.trim_ascii_start();
        let Some(after_marker) = remaining.strip_prefix(b"\x1e") else {
            return Err(VcsError::Parse("missing Git revision marker".to_string()));
        };
        remaining = after_marker;
        let rev_id = String::from_utf8_lossy(take_field(&mut remaining)?).into_owned();
        validate_sha(&rev_id)?;
        let timestamp_secs = String::from_utf8_lossy(take_field(&mut remaining)?)
            .parse()
            .map_err(|_| VcsError::Parse("invalid Git author timestamp".to_string()))?;
        let author =
            sanitize_untrusted_text(&String::from_utf8_lossy(take_field(&mut remaining)?), 240);
        let summary =
            sanitize_untrusted_text(&String::from_utf8_lossy(take_field(&mut remaining)?), 240);
        remaining = remaining.trim_ascii_start();
        if let Some(after_separator) = remaining.strip_prefix(b"\0") {
            remaining = after_separator.trim_ascii_start();
        }
        let mut touches = parse_numstats(&mut remaining, scope)?;
        parse_patch_hunks(&mut remaining, &mut touches);
        revisions.push(VcsRevision {
            short_id: rev_id[..7].to_string(),
            rev_id,
            timestamp_secs,
            author,
            summary,
            touched_files: touches.into_iter().flatten().collect(),
        });
    }
    Ok(revisions)
}

/// Keeps denied entries as empty slots so patch order still matches numstat order.
fn parse_numstats(
    remaining: &mut &[u8],
    scope: &dyn ScopePolicy,
) -> Result<Vec<Option<VcsFileTouch>>, VcsError> {
    let mut touches = Vec::new();
    while !remaining.is_empty()
        && !remaining.starts_with(b"\x1e")
        && !remaining.starts_with(b"diff --git ")
    {
        let record = take_field(remaining)?;
        if record.is_empty() {
            break;
        }
        let mut fields = record.splitn(3, |&byte| byte == b'\t');
        let added_lines = parse_line_count(fields.next())?;
        let deleted_lines = parse_line_count(fields.next())?;
        let path = fields
            .next()
            .ok_or_else(|| VcsError::Parse("missing Git numstat path".to_string()))?;
        let (old_path, new_path) = if path.is_empty() {
            (Some(take_field(remaining)?), take_field(remaining)?)
        } else {
            (None, path)
        };
        // -z emits decoded bytes, so C-style quoting and brace rename syntax
        // cannot obscure policy checks or corrupt paths containing tabs/spaces.
        let rel_path = std::str::from_utf8(new_path).ok();
        let touch = rel_path
            .filter(|path| !path.is_empty() && scope.is_history_path_allowed(Path::new(path)))
            .map(|path| VcsFileTouch {
                rel_path: path.to_string(),
                old_rel_path: old_path
                    .and_then(|old| std::str::from_utf8(old).ok())
                    .filter(|old| scope.is_history_path_allowed(Path::new(old)))
                    .map(str::to_string),
                added_lines,
                deleted_lines,
                hunk_new_ranges: Vec::new(),
            });
        touches.push(touch);
    }
    Ok(touches)
}

/// Binary file stats use `-` instead of line counts.
fn parse_line_count(field: Option<&[u8]>) -> Result<u32, VcsError> {
    match field {
        Some(b"-") => Ok(0),
        Some(bytes) => String::from_utf8_lossy(bytes)
            .parse()
            .map_err(|_| VcsError::Parse("invalid Git numstat line count".to_string())),
        None => Err(VcsError::Parse(
            "missing Git numstat line count".to_string(),
        )),
    }
}

/// Only unprefixed diff/hunk headers are structural; added/deleted body lines
/// always have a leading `+`/`-`, even when their content resembles a header.
fn parse_patch_hunks(remaining: &mut &[u8], touches: &mut [Option<VcsFileTouch>]) {
    let mut file_index = None;
    while !remaining.is_empty() && !remaining.starts_with(b"\x1e") {
        let end = remaining
            .iter()
            .position(|&byte| byte == b'\n')
            .map_or(remaining.len(), |index| index + 1);
        let line = &remaining[..end];
        *remaining = &remaining[end..];
        if line.starts_with(b"diff --git ") {
            file_index = Some(file_index.map_or(0, |index| index + 1));
        } else if let Some(touch) = file_index
            .and_then(|index| touches.get_mut(index))
            .and_then(Option::as_mut)
            && touch.hunk_new_ranges.len() < MAX_HUNK_RANGES
            && let Some(span) = parse_hunk_span(line)
        {
            touch.hunk_new_ranges.push(span);
        }
    }
}

/// Parses the new-side inclusive span, omitting deletion-only zero-length ranges.
fn parse_hunk_span(line: &[u8]) -> Option<LineSpan> {
    let line = std::str::from_utf8(line).ok()?.strip_prefix("@@ -")?;
    let (_, new_side) = line.split_once(" +")?;
    let (range, _) = new_side.split_once(" @@")?;
    let (start, count) = range.split_once(',').unwrap_or((range, "1"));
    let start: u32 = start.parse().ok()?;
    let count: u32 = count.parse().ok()?;
    if count == 0 {
        return None;
    }
    Some(LineSpan::new(start, start.saturating_add(count - 1)))
}
