//! Read-only Jujutsu CLI backend (`jj --ignore-working-copy --no-pager --color=never`).

use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path};
use std::sync::Arc;

use ledger_core::scope::sanitize_untrusted_text;
use ledger_core::{LineSpan, ScopePolicy, VcsBackendKind, VcsFileTouch, VcsRevision};

use crate::backend::{CommandOutput, CommandRunner, VcsBackend, VcsError, VcsHeadFingerprint};

const READ_ONLY_ARGS: [&str; 3] = ["--ignore-working-copy", "--no-pager", "--color=never"];
const REVISION_MARKER: &str = "LEDGER_REVISION\t";
// JSON strings keep tabs/newlines in untrusted identities and subjects from
// creating field or record boundaries. Diff lines always have their own prefix.
const LOG_TEMPLATE: &str = concat!(
    "\"LEDGER_REVISION\\t\" ++ commit_id ++ \"\\t\" ++ ",
    "change_id.shortest(8) ++ \"\\t\" ++ ",
    "committer.timestamp().format(\"%s\") ++ \"\\t\" ++ ",
    "json(author.email()) ++ \"\\t\" ++ ",
    "json(description.first_line()) ++ \"\\n\""
);

/// Non-mutating Jujutsu history provider guaranteeing zero working-copy snapshots.
#[derive(Debug, Clone)]
pub struct JjCliBackend {
    kind: VcsBackendKind,
    runner: Arc<dyn CommandRunner>,
}

impl JjCliBackend {
    /// Creates a new [`JjCliBackend`] (`Jujutsu` or `JujutsuColocatedGit`).
    #[must_use]
    pub fn new(kind: VcsBackendKind, runner: Arc<dyn CommandRunner>) -> Self {
        Self { kind, runner }
    }

    /// Returns a reference to the underlying command runner.
    #[must_use]
    pub fn runner(&self) -> &dyn CommandRunner {
        self.runner.as_ref()
    }

    fn run_jj(&self, repo_root: &Path, args: &[&str]) -> Result<CommandOutput, VcsError> {
        let mut read_only_args = READ_ONLY_ARGS.to_vec();
        read_only_args.extend_from_slice(args);
        let output = self.runner.run(repo_root, "jj", &read_only_args)?;
        if output.status_code != Some(0) {
            return Err(VcsError::CommandFailed {
                program: "jj".to_string(),
                code: output.status_code,
                stderr: sanitize_untrusted_text(&String::from_utf8_lossy(&output.stderr), 240),
            });
        }
        Ok(output)
    }
}

impl VcsBackend for JjCliBackend {
    fn kind(&self) -> VcsBackendKind {
        self.kind
    }

    fn head_fingerprint(&self, repo_root: &Path) -> Result<Option<VcsHeadFingerprint>, VcsError> {
        let jj_dir = repo_root.join(".jj");
        let repo_path = jj_dir.join("repo");
        let metadata = match fs::metadata(&repo_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let repo_dir = if metadata.is_dir() {
            repo_path
        } else {
            let pointer = fs::read_to_string(repo_path)?;
            if pointer.trim().is_empty() {
                return Ok(None);
            }
            // Secondary workspaces resolve relative pointers against .jj/.
            jj_dir.join(pointer.trim())
        };
        let entries = match fs::read_dir(repo_dir.join("op_store/heads")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut heads = Vec::new();
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                let name = entry.file_name().into_string().map_err(|_| {
                    VcsError::Parse("non-UTF-8 jj operation head filename".to_string())
                })?;
                heads.push(name);
            }
        }
        heads.sort_unstable();
        let Some(first_head) = heads.first() else {
            return Ok(None);
        };
        let short_op: String = first_head.chars().take(8).collect();
        let mut digest = blake3::Hasher::new();
        for head in &heads {
            digest.update(head.as_bytes());
            digest.update(b"\0");
        }
        let display_badge = if self.kind == VcsBackendKind::JujutsuColocatedGit {
            match read_git_head(repo_root) {
                Some((head, loose_ref)) => {
                    digest.update(b"git-head\0");
                    digest.update(head.as_bytes());
                    if let Some(ref_contents) = &loose_ref {
                        digest.update(b"\0git-ref\0");
                        digest.update(ref_contents.as_bytes());
                    }
                    let git_id = loose_ref.as_deref().unwrap_or(&head).trim();
                    if !git_id.is_empty() && git_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                        let short_git: String = git_id.chars().take(7).collect();
                        format!("jj+git:{short_op}({short_git})")
                    } else {
                        format!("jj+git:{short_op}")
                    }
                }
                None => format!("jj+git:{short_op}"),
            }
        } else {
            format!("jj:{short_op}")
        };
        Ok(Some(VcsHeadFingerprint {
            kind: self.kind,
            display_badge,
            state_digest: digest.finalize().to_hex().to_string(),
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
        let output = self.run_jj(
            repo_root,
            &[
                "log",
                "--no-graph",
                "-r",
                "::@ & ~empty()",
                "-n",
                &limit,
                "-T",
                LOG_TEMPLATE,
                "--git",
            ],
        )?;
        let text = std::str::from_utf8(&output.stdout)
            .map_err(|_| VcsError::Parse("jj log output is not UTF-8".to_string()))?;
        let mut revisions = parse_log(text, scope)?;
        revisions.truncate(max_revisions);
        Ok(revisions)
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
        validate_revision_id(rev_id)?;
        let output = self.run_jj(repo_root, &["diff", "-r", rev_id, "--git", "--", rel_path])?;
        let text = sanitize_untrusted_text(&String::from_utf8_lossy(&output.stdout), usize::MAX);
        Ok(text
            .lines()
            .take(max_lines.max(1))
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

// Only fixed metadata paths are read: no subprocess, object traversal, or history scan.
fn read_git_head(repo_root: &Path) -> Option<(String, Option<String>)> {
    let mut git_dir = repo_root.join(".git");
    if git_dir.is_file() {
        let pointer = fs::read_to_string(&git_dir).ok()?;
        let target = pointer.trim().strip_prefix("gitdir:")?.trim();
        if target.is_empty() {
            return None;
        }
        git_dir = repo_root.join(target);
    }
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let loose_ref = head.trim().strip_prefix("ref: ").and_then(|name| {
        let path = Path::new(name);
        if !name.starts_with("refs/")
            || !path
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
        {
            return None;
        }
        fs::read_to_string(git_dir.join(path)).ok()
    });
    Some((head, loose_ref))
}

fn validate_revision_id(rev_id: &str) -> Result<(), VcsError> {
    if rev_id.is_empty() || !rev_id.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err(VcsError::Parse(
            "invalid jj revision identifier".to_string(),
        ));
    }
    Ok(())
}

fn parse_revision(header: &str) -> Result<VcsRevision, VcsError> {
    let fields: Vec<_> = header.splitn(5, '\t').collect();
    let [commit_id, change_id, timestamp, author, summary] = fields.as_slice() else {
        return Err(VcsError::Parse("malformed jj revision header".to_string()));
    };
    validate_revision_id(commit_id)?;
    if !change_id.is_empty() {
        validate_revision_id(change_id)?;
    }
    let commit_short: String = commit_id.chars().take(6).collect();
    let short_id = if !change_id.is_empty() && *change_id != commit_short {
        format!("{change_id}({commit_short})")
    } else {
        commit_short
    };
    let decode_text = |text: &str| -> Result<String, VcsError> {
        let decoded: String = serde_json::from_str(text)
            .map_err(|_| VcsError::Parse("invalid JSON text in jj revision header".to_string()))?;
        Ok(sanitize_untrusted_text(&decoded, 240))
    };
    Ok(VcsRevision {
        rev_id: (*commit_id).to_string(),
        short_id,
        timestamp_secs: timestamp
            .parse()
            .map_err(|_| VcsError::Parse("invalid jj revision timestamp".to_string()))?,
        author: decode_text(author)?,
        summary: decode_text(summary)?,
        touched_files: Vec::new(),
    })
}

fn finish_touch(
    revision: &mut Option<VcsRevision>,
    touch: &mut Option<VcsFileTouch>,
    scope: &dyn ScopePolicy,
) {
    if let (Some(revision), Some(mut touch)) = (revision.as_mut(), touch.take()) {
        if touch.rel_path.is_empty() || !scope.is_history_path_allowed(Path::new(&touch.rel_path)) {
            return;
        }
        if touch
            .old_rel_path
            .as_ref()
            .is_some_and(|old| !scope.is_history_path_allowed(Path::new(old)))
        {
            touch.old_rel_path = None;
        }
        revision.touched_files.push(touch);
    }
}

fn parse_log(text: &str, scope: &dyn ScopePolicy) -> Result<Vec<VcsRevision>, VcsError> {
    let mut revisions = Vec::new();
    let mut revision = None;
    let mut touch: Option<VcsFileTouch> = None;
    let mut in_hunk = false;
    for line in text.lines() {
        if let Some(header) = line.strip_prefix(REVISION_MARKER) {
            finish_touch(&mut revision, &mut touch, scope);
            if let Some(previous) = revision.take() {
                revisions.push(previous);
            }
            revision = Some(parse_revision(header)?);
            in_hunk = false;
        } else if let Some(header) = line.strip_prefix("diff --git ") {
            if revision.is_none() {
                return Err(VcsError::Parse(
                    "jj diff has no revision header".to_string(),
                ));
            }
            finish_touch(&mut revision, &mut touch, scope);
            let (old_path, rel_path) = parse_diff_paths(header)?;
            touch = Some(VcsFileTouch {
                old_rel_path: (old_path != rel_path).then_some(old_path),
                rel_path,
                added_lines: 0,
                deleted_lines: 0,
                hunk_new_ranges: Vec::new(),
            });
            in_hunk = false;
        } else if let Some(touch) = &mut touch {
            if line.starts_with("@@ ") {
                if touch.hunk_new_ranges.len() < 64
                    && let Some(range) = parse_hunk_range(line)?
                {
                    touch.hunk_new_ranges.push(range);
                }
                in_hunk = true;
            } else if in_hunk {
                if line.starts_with('+') {
                    touch.added_lines = touch.added_lines.saturating_add(1);
                } else if line.starts_with('-') {
                    touch.deleted_lines = touch.deleted_lines.saturating_add(1);
                }
            } else if let Some(path) = line.strip_prefix("rename from ") {
                touch.old_rel_path = Some(decode_git_path(path)?);
            } else if let Some(path) = line.strip_prefix("rename to ") {
                touch.rel_path = decode_git_path(path)?;
            }
        } else if revision.is_none() && !line.trim().is_empty() {
            return Err(VcsError::Parse("missing jj revision header".to_string()));
        }
    }
    finish_touch(&mut revision, &mut touch, scope);
    if let Some(last) = revision {
        revisions.push(last);
    }
    Ok(revisions)
}

fn parse_hunk_range(line: &str) -> Result<Option<LineSpan>, VcsError> {
    let invalid = || VcsError::Parse("invalid jj unified diff hunk".to_string());
    let mut fields = line.split_whitespace();
    if fields.next() != Some("@@") || !fields.next().is_some_and(|old| old.starts_with('-')) {
        return Err(invalid());
    }
    let new_range = fields
        .next()
        .and_then(|field| field.strip_prefix('+'))
        .ok_or_else(invalid)?;
    if fields.next() != Some("@@") {
        return Err(invalid());
    }
    let (start, count) = new_range.split_once(',').unwrap_or((new_range, "1"));
    let start: u32 = start.parse().map_err(|_| invalid())?;
    let count: u32 = count.parse().map_err(|_| invalid())?;
    if count == 0 {
        return Ok(None);
    }
    if start == 0 {
        return Err(invalid());
    }
    let end = start.checked_add(count - 1).ok_or_else(invalid)?;
    Ok(Some(LineSpan::new(start, end)))
}

fn parse_diff_paths(header: &str) -> Result<(String, String), VcsError> {
    let invalid = || VcsError::Parse("invalid jj git-style diff paths".to_string());
    let (old, new) = if header.starts_with('"') {
        let end = quoted_path_end(header).ok_or_else(invalid)?;
        let (old, rest) = header.split_at(end);
        (decode_git_path(old)?, decode_git_path(rest.trim_start())?)
    } else if let Some((old, new)) = header.split_once(" \"b/") {
        // Git quotes each path independently; a rename can quote only its target.
        (old.to_string(), decode_git_path(&format!("\"b/{new}"))?)
    } else {
        let candidates: Vec<_> = header
            .match_indices(" b/")
            .map(|(index, _)| (&header[..index], &header[index + 1..]))
            .collect();
        // Unquoted spaces are legal in git headers. Prefer the exact same-path
        // split for modifications; reject ambiguous renames rather than apply
        // scope policy to an incorrectly shortened path.
        let (old, new) = if let Some(same_path) = candidates
            .iter()
            .find(|(old, new)| old.strip_prefix("a/") == new.strip_prefix("b/"))
        {
            *same_path
        } else if let [(old, new)] = candidates.as_slice() {
            (*old, *new)
        } else {
            return Err(invalid());
        };
        (old.to_string(), new.to_string())
    };
    let old = old.strip_prefix("a/").ok_or_else(invalid)?.to_string();
    let new = new.strip_prefix("b/").ok_or_else(invalid)?.to_string();
    Ok((old, new))
}

fn quoted_path_end(path: &str) -> Option<usize> {
    let mut escaped = false;
    for (index, byte) in path.bytes().enumerate().skip(1) {
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            return Some(index + 1);
        }
    }
    None
}

// Git's quoted paths use C escapes, including octal UTF-8 bytes (not JSON escapes).
fn decode_git_path(path: &str) -> Result<String, VcsError> {
    if !path.starts_with('"') {
        return Ok(path.to_string());
    }
    let invalid = || VcsError::Parse("invalid quoted jj diff path".to_string());
    let end = quoted_path_end(path).ok_or_else(invalid)?;
    if end != path.len() {
        return Err(invalid());
    }
    let mut bytes = path.as_bytes()[1..end - 1].iter().copied().peekable();
    let mut decoded = Vec::new();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            decoded.push(byte);
            continue;
        }
        let escape = bytes.next().ok_or_else(invalid)?;
        let byte = match escape {
            b'a' => 7,
            b'b' => 8,
            b't' => b'\t',
            b'n' => b'\n',
            b'v' => 11,
            b'f' => 12,
            b'r' => b'\r',
            b'"' | b'\\' => escape,
            b'0'..=b'7' => {
                let mut value = u16::from(escape - b'0');
                for _ in 0..2 {
                    if let Some(next @ b'0'..=b'7') = bytes.peek().copied() {
                        bytes.next();
                        value = value * 8 + u16::from(next - b'0');
                    } else {
                        break;
                    }
                }
                u8::try_from(value).map_err(|_| invalid())?
            }
            _ => return Err(invalid()),
        };
        decoded.push(byte);
    }
    String::from_utf8(decoded).map_err(|_| invalid())
}
