//! Real-repository coverage for read-only, policy-enforced Git history.

use std::error::Error;
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use ledger_core::{DefaultScopeGuard, LineSpan, VcsBackendKind, VcsFileTouch, VcsRevision};
use ledger_vcs::{
    CommandOutput, CommandRunner, GitCliBackend, SystemCommandRunner, VcsBackend, VcsError,
};
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;

/// Fails any accidental subprocess call in filesystem-only and rejection tests.
#[derive(Debug)]
struct RejectCommandRunner;

impl CommandRunner for RejectCommandRunner {
    fn run(&self, _cwd: &Path, _program: &str, _args: &[&str]) -> Result<CommandOutput, VcsError> {
        Err(VcsError::Parse(
            "unexpected subprocess invocation".to_string(),
        ))
    }
}

/// Checks the locking invariant before forwarding to the production runner.
#[derive(Debug)]
struct CheckedCommandRunner;

impl CommandRunner for CheckedCommandRunner {
    fn run(&self, cwd: &Path, program: &str, args: &[&str]) -> Result<CommandOutput, VcsError> {
        assert_eq!(program, "git");
        assert_eq!(args.first(), Some(&"--no-optional-locks"));
        SystemCommandRunner.run(cwd, program, args)
    }
}

fn backend() -> GitCliBackend {
    GitCliBackend::new(Arc::new(SystemCommandRunner))
}

fn git(root: &Path, args: &[&str]) -> Result<String, Box<dyn Error>> {
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
        .into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn init_repo() -> Result<TempDir, Box<dyn Error>> {
    let repo = TempDir::new()?;
    git(repo.path(), &["init", "-b", "main"])?;
    git(repo.path(), &["config", "user.name", "LEDGER Test"])?;
    git(
        repo.path(),
        &["config", "user.email", "ledger@example.test"],
    )?;
    git(repo.path(), &["config", "commit.gpgsign", "false"])?;
    git(repo.path(), &["config", "core.autocrlf", "false"])?;
    let hooks = repo.path().join(".git/no-hooks");
    git(
        repo.path(),
        &["config", "core.hooksPath", &hooks.to_string_lossy()],
    )?;
    Ok(repo)
}

fn commit(root: &Path, subject: &str) -> Result<String, Box<dyn Error>> {
    git(root, &["add", "--all"])?;
    git(root, &["commit", "-m", subject])?;
    Ok(git(root, &["rev-parse", "HEAD"])?.trim().to_string())
}

fn touch<'a>(revision: &'a VcsRevision, path: &str) -> Result<&'a VcsFileTouch, Box<dyn Error>> {
    revision
        .touched_files
        .iter()
        .find(|touch| touch.rel_path == path)
        .ok_or_else(|| io::Error::other(format!("missing file touch: {path:?}")).into())
}

#[test]
fn unborn_repository_has_no_head_or_history() -> TestResult {
    let repo = init_repo()?;
    let scope = DefaultScopeGuard::new(repo.path())?;
    assert_eq!(backend().head_fingerprint(repo.path())?, None);
    assert!(
        backend()
            .recent_revisions(repo.path(), &scope, 20)?
            .is_empty()
    );
    Ok(())
}

#[test]
fn zero_revision_limit_never_invokes_git() -> TestResult {
    let repo = init_repo()?;
    let scope = DefaultScopeGuard::new(repo.path())?;
    let backend = GitCliBackend::new(Arc::new(RejectCommandRunner));
    assert!(backend.recent_revisions(repo.path(), &scope, 0)?.is_empty());
    Ok(())
}

#[test]
fn fingerprint_tracks_commits_packed_refs_and_detached_head_without_commands() -> TestResult {
    let repo = init_repo()?;
    fs::write(repo.path().join("lib.rs"), "pub fn first() {}\n")?;
    let first_sha = commit(repo.path(), "first")?;
    let backend = GitCliBackend::new(Arc::new(RejectCommandRunner));
    let first = backend
        .head_fingerprint(repo.path())?
        .ok_or_else(|| io::Error::other("missing first fingerprint"))?;
    assert_eq!(first.kind, VcsBackendKind::Git);
    assert_eq!(first.display_badge, format!("git:{}", &first_sha[..7]));
    assert_eq!(
        first.state_digest,
        blake3::hash(first_sha.as_bytes()).to_hex().to_string()
    );

    fs::write(repo.path().join("lib.rs"), "pub fn second() {}\n")?;
    let second_sha = commit(repo.path(), "second")?;
    let second = backend
        .head_fingerprint(repo.path())?
        .ok_or_else(|| io::Error::other("missing second fingerprint"))?;
    assert_ne!(first.state_digest, second.state_digest);
    assert_eq!(second.display_badge, format!("git:{}", &second_sha[..7]));

    git(repo.path(), &["pack-refs", "--all", "--prune"])?;
    assert!(!repo.path().join(".git/refs/heads/main").exists());
    assert_eq!(backend.head_fingerprint(repo.path())?, Some(second.clone()));
    git(repo.path(), &["checkout", "--detach", &first_sha])?;
    assert_eq!(backend.head_fingerprint(repo.path())?, Some(first));
    Ok(())
}

#[test]
fn worktree_resolves_absolute_relative_and_common_packed_refs() -> TestResult {
    let repo = init_repo()?;
    fs::write(repo.path().join("lib.rs"), "pub fn main() {}\n")?;
    let main_sha = commit(repo.path(), "main")?;
    let worktree = repo.path().join("linked");
    git(
        repo.path(),
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            &worktree.to_string_lossy(),
        ],
    )?;
    let backend = GitCliBackend::new(Arc::new(RejectCommandRunner));
    let initial = backend.head_fingerprint(&worktree)?;
    assert_eq!(initial, backend.head_fingerprint(repo.path())?);

    fs::write(worktree.join("lib.rs"), "pub fn feature() {}\n")?;
    let feature_sha = commit(&worktree, "feature")?;
    assert_ne!(feature_sha, main_sha);
    let feature = backend
        .head_fingerprint(&worktree)?
        .ok_or_else(|| io::Error::other("missing worktree fingerprint"))?;
    assert_eq!(feature.display_badge, format!("git:{}", &feature_sha[..7]));
    assert_ne!(
        Some(feature.clone()),
        backend.head_fingerprint(repo.path())?
    );

    fs::write(worktree.join(".git"), "gitdir: ../.git/worktrees/linked\n")?;
    assert_eq!(backend.head_fingerprint(&worktree)?, Some(feature.clone()));
    git(repo.path(), &["pack-refs", "--all", "--prune"])?;
    assert!(!repo.path().join(".git/refs/heads/feature").exists());
    assert_eq!(backend.head_fingerprint(&worktree)?, Some(feature));
    Ok(())
}

#[test]
fn detached_sha256_fingerprint_and_malformed_head_are_handled() -> TestResult {
    let repo = TempDir::new()?;
    fs::create_dir(repo.path().join(".git"))?;
    let sha = "a".repeat(64);
    fs::write(repo.path().join(".git/HEAD"), format!("{sha}\n"))?;
    let fingerprint = backend()
        .head_fingerprint(repo.path())?
        .ok_or_else(|| io::Error::other("missing SHA-256 fingerprint"))?;
    assert_eq!(fingerprint.display_badge, "git:aaaaaaa");
    assert_eq!(
        fingerprint.state_digest,
        blake3::hash(sha.as_bytes()).to_hex().to_string()
    );
    fs::write(repo.path().join(".git/HEAD"), "ref: refs/../../outside\n")?;
    assert!(matches!(
        backend().head_fingerprint(repo.path()),
        Err(VcsError::Parse(_))
    ));
    Ok(())
}

#[test]
fn history_tracks_line_stats_function_hunks_and_renames() -> TestResult {
    let repo = init_repo()?;
    fs::create_dir(repo.path().join("src"))?;
    let initial = "pub fn answer() -> u32 {\n    1\n}\n\npub fn stable() -> u32 {\n    7\n}\n";
    fs::write(repo.path().join("src/lib.rs"), initial)?;
    let first = commit(repo.path(), "initial")?;
    fs::write(
        repo.path().join("src/lib.rs"),
        initial.replace("    1", "    42"),
    )?;
    let second = commit(
        repo.path(),
        "fix: answer; punctuation | stays intact\n\nbody\nwith lines",
    )?;
    git(repo.path(), &["mv", "src/lib.rs", "src/renamed.rs"])?;
    let third = commit(repo.path(), "rename")?;
    let scope = DefaultScopeGuard::new(repo.path())?;
    let revisions = backend().recent_revisions(repo.path(), &scope, 3)?;
    assert_eq!(revisions.len(), 3);
    assert_eq!(revisions[0].rev_id, third);
    let renamed = touch(&revisions[0], "src/renamed.rs")?;
    assert_eq!(renamed.old_rel_path.as_deref(), Some("src/lib.rs"));
    assert_eq!((renamed.added_lines, renamed.deleted_lines), (0, 0));
    assert!(renamed.hunk_new_ranges.is_empty());

    assert_eq!(revisions[1].rev_id, second);
    assert_eq!(revisions[1].author, "LEDGER Test");
    assert!(revisions[1].timestamp_secs > 0);
    assert_eq!(
        revisions[1].summary,
        "fix: answer; punctuation | stays intact"
    );
    let modified = touch(&revisions[1], "src/lib.rs")?;
    assert_eq!((modified.added_lines, modified.deleted_lines), (1, 1));
    assert_eq!(modified.hunk_new_ranges, vec![LineSpan::new(2, 2)]);
    assert_eq!(revisions[2].rev_id, first);
    let added = touch(&revisions[2], "src/lib.rs")?;
    assert_eq!((added.added_lines, added.deleted_lines), (7, 0));
    assert_eq!(added.hunk_new_ranges, vec![LineSpan::new(1, 7)]);
    assert_eq!(backend().recent_revisions(repo.path(), &scope, 1)?.len(), 1);
    Ok(())
}

#[test]
fn denied_secret_paths_never_appear_in_history_or_diff() -> TestResult {
    let repo = init_repo()?;
    fs::create_dir(repo.path().join("src"))?;
    fs::create_dir(repo.path().join(".ssh"))?;
    for path in [".env", "secrets.json", "id_rsa", ".ssh/config"] {
        fs::write(repo.path().join(path), "SECRET_KEY=topsecret\n")?;
    }
    fs::write(repo.path().join("src/lib.rs"), "pub fn safe() {}\n")?;
    let revision = commit(repo.path(), "safe and sensitive files")?;
    let scope = DefaultScopeGuard::new(repo.path())?;
    let revisions = backend().recent_revisions(repo.path(), &scope, 10)?;
    assert_eq!(revisions.len(), 1);
    assert_eq!(revisions[0].touched_files.len(), 1);
    let safe = touch(&revisions[0], "src/lib.rs")?;
    assert_eq!(safe.hunk_new_ranges, vec![LineSpan::new(1, 1)]);
    assert!(!format!("{revisions:?}").contains("topsecret"));
    let backend = GitCliBackend::new(Arc::new(RejectCommandRunner));
    for path in [".env", "secrets.json", "id_rsa", ".ssh/config", ""] {
        assert!(matches!(
            backend.revision_file_diff(repo.path(), &scope, &revision, path, 80),
            Err(VcsError::DeniedByPolicy(denied)) if denied == path
        ));
    }
    Ok(())
}

#[test]
fn renamed_secret_source_is_removed_and_secret_destination_is_denied() -> TestResult {
    let repo = init_repo()?;
    fs::write(repo.path().join(".env"), "old contents\n")?;
    fs::write(repo.path().join("safe.rs"), "other contents\n")?;
    commit(repo.path(), "initial")?;
    git(repo.path(), &["mv", ".env", "allowed.rs"])?;
    git(repo.path(), &["mv", "safe.rs", "secrets.json"])?;
    commit(repo.path(), "move files")?;
    let scope = DefaultScopeGuard::new(repo.path())?;
    let revisions = backend().recent_revisions(repo.path(), &scope, 1)?;
    assert_eq!(revisions[0].touched_files.len(), 1);
    assert_eq!(touch(&revisions[0], "allowed.rs")?.old_rel_path, None);
    Ok(())
}

#[test]
fn unusual_paths_and_quoted_rename_headers_keep_exact_names() -> TestResult {
    let repo = init_repo()?;
    fs::create_dir(repo.path().join("src"))?;
    let paths = [
        "src/with space.rs",
        "src/with\"quote.rs",
        "src/café.rs",
        "src/with\ttab.rs",
        "src/with\nnewline.rs",
        "src/with\\slash.rs",
    ];
    for path in paths {
        fs::write(
            repo.path().join(path),
            format!("// {path:?}\npub fn sample() {{}}\n"),
        )?;
    }
    commit(repo.path(), "unusual paths")?;
    let scope = DefaultScopeGuard::new(repo.path())?;
    let revisions = backend().recent_revisions(repo.path(), &scope, 1)?;
    assert_eq!(revisions[0].touched_files.len(), paths.len());
    for path in paths {
        assert_eq!(touch(&revisions[0], path)?.added_lines, 2);
        assert_eq!(
            touch(&revisions[0], path)?.hunk_new_ranges,
            vec![LineSpan::new(1, 2)]
        );
    }
    git(repo.path(), &["mv", paths[4], "src/new\nname.rs"])?;
    commit(repo.path(), "quoted rename")?;
    let revisions = backend().recent_revisions(repo.path(), &scope, 1)?;
    assert_eq!(
        touch(&revisions[0], "src/new\nname.rs")?
            .old_rel_path
            .as_deref(),
        Some(paths[4])
    );
    Ok(())
}

#[test]
fn unsafe_revision_ids_are_rejected_before_any_command() -> TestResult {
    let repo = init_repo()?;
    let scope = DefaultScopeGuard::new(repo.path())?;
    let backend = GitCliBackend::new(Arc::new(RejectCommandRunner));
    for revision in [
        "--output=/tmp/pwn",
        "",
        "-HEAD",
        "HEAD path",
        "HEAD\n",
        "HEAD\0",
        "HEAD;id",
        "$(id)",
    ] {
        let result = backend.revision_file_diff(repo.path(), &scope, revision, "src/lib.rs", 80);
        assert!(
            matches!(result, Err(VcsError::Parse(message)) if message == "unsafe Git revision ID")
        );
    }
    Ok(())
}

#[test]
fn diffs_are_sanitized_bounded_and_use_literal_paths_and_optional_lock_flag() -> TestResult {
    let repo = init_repo()?;
    fs::create_dir(repo.path().join("src"))?;
    fs::write(
        repo.path().join("src/lib.rs"),
        "pub fn sample() {}\n// \u{1b}[31mcolor\u{7}\n",
    )?;
    fs::write(repo.path().join(".env"), "SECRET_KEY=topsecret\n")?;
    let revision = commit(repo.path(), "diff with control chars")?;
    let scope = DefaultScopeGuard::new(repo.path())?;
    let backend = GitCliBackend::new(Arc::new(CheckedCommandRunner));
    assert_eq!(backend.recent_revisions(repo.path(), &scope, 1)?.len(), 1);
    let diff = backend.revision_file_diff(repo.path(), &scope, &revision, "src/lib.rs", 80)?;
    assert!(diff.contains("+pub fn sample() {}"));
    assert!(!diff.contains('\u{1b}'));
    assert!(!diff.contains('\u{7}'));
    assert!(!diff.contains("topsecret"));
    assert!(diff.contains('\n'));
    for limit in [0, 1, 3] {
        let diff = backend.revision_file_diff(repo.path(), &scope, "HEAD", "src/lib.rs", limit)?;
        let effective = limit.max(1);
        assert_eq!(diff.lines().count(), effective + 1);
        assert!(diff.ends_with(&format!("// ... [diff truncated at {effective} lines]\n")));
    }
    assert!(
        backend
            .revision_file_diff(repo.path(), &scope, "HEAD", "src/*.rs", 80)?
            .is_empty()
    );
    assert!(!repo.path().join(".git/index.lock").exists());
    assert!(matches!(
        backend.revision_file_diff(repo.path(), &scope, "does-not-exist", "src/lib.rs", 80),
        Err(VcsError::CommandFailed { .. })
    ));
    Ok(())
}

#[test]
fn deletion_only_hunks_have_no_new_ranges_and_binary_stats_are_zero() -> TestResult {
    let repo = init_repo()?;
    fs::write(repo.path().join("file.rs"), "one\ntwo\nthree\n")?;
    commit(repo.path(), "initial")?;
    fs::write(repo.path().join("file.rs"), "one\nthree\n")?;
    fs::write(repo.path().join("binary.bin"), [0, 1, 2, 3])?;
    commit(repo.path(), "delete and add binary")?;
    let scope = DefaultScopeGuard::new(repo.path())?;
    let revisions = backend().recent_revisions(repo.path(), &scope, 1)?;
    let deleted = touch(&revisions[0], "file.rs")?;
    assert_eq!((deleted.added_lines, deleted.deleted_lines), (0, 1));
    assert!(deleted.hunk_new_ranges.is_empty());
    let binary = touch(&revisions[0], "binary.bin")?;
    assert_eq!((binary.added_lines, binary.deleted_lines), (0, 0));
    assert!(binary.hunk_new_ranges.is_empty());
    Ok(())
}

#[test]
fn hunk_metadata_is_capped_and_diff_body_cannot_forge_headers() -> TestResult {
    let repo = init_repo()?;
    let initial: String = (0..260).map(|line| format!("line {line}\n")).collect();
    fs::write(repo.path().join("generated.rs"), initial)?;
    commit(repo.path(), "initial")?;
    let modified: String = (0..260)
        .map(|line| {
            if line % 2 == 0 {
                format!("changed {line}\n")
            } else {
                format!("line {line}\n")
            }
        })
        .collect();
    fs::write(repo.path().join("generated.rs"), modified)?;
    fs::write(
        repo.path().join("headers.rs"),
        "diff --git a/fake b/fake\n@@ -1 +999 @@\nrename to .env\n\u{1e}fake revision\n",
    )?;
    commit(repo.path(), "many hunks")?;
    let scope = DefaultScopeGuard::new(repo.path())?;
    let revisions = backend().recent_revisions(repo.path(), &scope, 1)?;
    let generated = touch(&revisions[0], "generated.rs")?;
    assert_eq!((generated.added_lines, generated.deleted_lines), (130, 130));
    assert_eq!(generated.hunk_new_ranges.len(), 64);
    assert_eq!(generated.hunk_new_ranges[0], LineSpan::new(1, 1));
    assert_eq!(generated.hunk_new_ranges[63], LineSpan::new(127, 127));
    assert_eq!(
        touch(&revisions[0], "headers.rs")?.hunk_new_ranges,
        vec![LineSpan::new(1, 4)]
    );
    Ok(())
}

#[test]
fn empty_commits_and_untrusted_metadata_preserve_record_framing() -> TestResult {
    let repo = init_repo()?;
    let long_author = format!("Author\u{1f}{}", "a".repeat(260));
    git(repo.path(), &["config", "user.name", &long_author])?;
    let subject = format!("subject\u{1e}\u{1f}{}", "s".repeat(260));
    git(repo.path(), &["commit", "--allow-empty", "-m", &subject])?;
    fs::write(repo.path().join("file.rs"), "pub fn sample() {}\n")?;
    commit(repo.path(), "normal")?;
    let scope = DefaultScopeGuard::new(repo.path())?;
    let revisions = backend().recent_revisions(repo.path(), &scope, 10)?;
    assert_eq!(revisions.len(), 2);
    assert!(revisions[1].touched_files.is_empty());
    assert_eq!(revisions[1].author.chars().count(), 243);
    assert_eq!(revisions[1].summary.chars().count(), 243);
    assert!(!revisions[1].author.chars().any(char::is_control));
    assert!(!revisions[1].summary.chars().any(char::is_control));
    Ok(())
}

#[test]
fn history_filters_nested_roots_but_retains_deleted_outer_source() -> TestResult {
    let repo = init_repo()?;
    for directory in ["nested_git/src", "worktree/src", "nested_jj/src", "old"] {
        fs::create_dir_all(repo.path().join(directory))?;
    }
    for path in [
        "nested_git/src/inner.rs",
        "worktree/src/inner.rs",
        "nested_jj/src/inner.rs",
        "old/deleted.rs",
    ] {
        fs::write(repo.path().join(path), "pub fn historic() {}\n")?;
    }
    let initial = commit(repo.path(), "outer tracked source")?;
    fs::remove_file(repo.path().join("old/deleted.rs"))?;
    commit(repo.path(), "delete outer file")?;
    // These roots became separate repositories after the old outer revision.
    fs::create_dir(repo.path().join("nested_git/.git"))?;
    fs::write(
        repo.path().join("worktree/.git"),
        "gitdir: /unrelated/worktrees/nested\n",
    )?;
    fs::create_dir(repo.path().join("nested_jj/.jj"))?;
    let scope = DefaultScopeGuard::new(repo.path())?;
    let revisions = backend().recent_revisions(repo.path(), &scope, 10)?;
    assert_eq!(revisions.len(), 2);
    assert!(revisions.iter().all(|revision| {
        revision
            .touched_files
            .iter()
            .all(|file| file.rel_path == "old/deleted.rs")
    }));
    assert!(
        backend()
            .revision_file_diff(repo.path(), &scope, &initial, "old/deleted.rs", 40)?
            .contains("pub fn historic()")
    );
    let rejecting = GitCliBackend::new(Arc::new(RejectCommandRunner));
    for path in [
        "nested_git/src/inner.rs",
        "worktree/src/inner.rs",
        "nested_jj/src/inner.rs",
        "old/../old/deleted.rs",
    ] {
        assert!(
            matches!(
                rejecting.revision_file_diff(repo.path(), &scope, &initial, path, 40),
                Err(VcsError::DeniedByPolicy(_))
            ),
            "{path}"
        );
    }
    Ok(())
}

#[test]
#[cfg(unix)]
fn object_history_does_not_dereference_external_working_tree_symlink_ancestors() -> TestResult {
    let repo = init_repo()?;
    fs::create_dir(repo.path().join("former_directory"))?;
    fs::write(
        repo.path().join("former_directory/source.rs"),
        "pub fn from_object_store() {}\n",
    )?;
    let revision = commit(repo.path(), "track ordinary directory")?;
    fs::remove_dir_all(repo.path().join("former_directory"))?;
    let external = tempfile::tempdir()?;
    fs::create_dir(external.path().join(".git"))?;
    fs::write(external.path().join("source.rs"), "external_secret_value\n")?;
    std::os::unix::fs::symlink(external.path(), repo.path().join("former_directory"))?;
    let scope = DefaultScopeGuard::new(repo.path())?;
    let diff = backend().revision_file_diff(
        repo.path(),
        &scope,
        &revision,
        "former_directory/source.rs",
        80,
    )?;
    assert!(diff.contains("pub fn from_object_store()"));
    assert!(!diff.contains("external_secret_value"));
    assert!(
        ledger_core::SingleBufferReader::new(scope)
            .read_file(&repo.path().join("former_directory/source.rs"))
            .is_err()
    );
    Ok(())
}
