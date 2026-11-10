use std::error::Error;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use ledger_core::{DefaultScopeGuard, LineSpan, VcsBackendKind};
use ledger_vcs::{CommandOutput, CommandRunner, JjCliBackend, VcsBackend, VcsError};

type TestResult = Result<(), Box<dyn Error>>;

const READ_ONLY_PREFIX: [&str; 3] = ["--ignore-working-copy", "--no-pager", "--color=never"];
const LOG_FIXTURE: &str = concat!(
    "LEDGER_REVISION\t88f1c2aabbccddeeff\tqpvuntsm\t1730000000\t",
    "\"alice\\u0000@example.test\"\t\"Move parser\\u001b and extend coverage\"\n",
    "diff --git a/src/old.rs b/src/new.rs\n",
    "similarity index 80%\n",
    "rename from src/old.rs\n",
    "rename to src/new.rs\n",
    "--- a/src/old.rs\n",
    "+++ b/src/new.rs\n",
    "@@ -10,4 +12,8 @@ fn parse() {\n",
    " context one\n",
    "-old line\n",
    "+new line one\n",
    "+new line two\n",
    "+new line three\n",
    "+new line four\n",
    "+new line five\n",
    " context two\n",
    " context three\n",
    "diff --git a/.env b/.env\n",
    "--- a/.env\n",
    "+++ b/.env\n",
    "@@ -1 +1 @@\n",
    "-TOKEN=old-secret\n",
    "+TOKEN=new-secret\n",
    "diff --git a/private.key b/private.key\n",
    "@@ -1 +1 @@\n",
    "-old private key\n",
    "+new private key\n",
    "diff --git a/.jj/config.toml b/.jj/config.toml\n",
    "@@ -1 +1 @@\n",
    "-old config\n",
    "+new config\n",
    "LEDGER_REVISION\tabc123ffffffffffff\tabc123\t1729999900\t",
    "\"bob@example.test\"\t\"Add parser test\"\n",
    "diff --git a/tests/parser.rs b/tests/parser.rs\n",
    "new file mode 100644\n",
    "--- /dev/null\n",
    "+++ b/tests/parser.rs\n",
    "@@ -0,0 +1 @@\n",
    "+fn parser_test() {}\n"
);

#[derive(Debug, Clone)]
struct MockJjRunner {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
    output: CommandOutput,
}

impl MockJjRunner {
    fn new(stdout: &str) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            output: CommandOutput {
                status_code: Some(0),
                stdout: stdout.as_bytes().to_vec(),
                stderr: Vec::new(),
            },
        }
    }

    fn recorded_calls(&self) -> Result<Vec<Vec<String>>, VcsError> {
        self.calls
            .lock()
            .map(|calls| calls.clone())
            .map_err(|_| VcsError::Parse("mock runner call lock poisoned".to_string()))
    }
}

impl CommandRunner for MockJjRunner {
    fn run(&self, _cwd: &Path, program: &str, args: &[&str]) -> Result<CommandOutput, VcsError> {
        assert_eq!(program, "jj");
        self.calls
            .lock()
            .map_err(|_| VcsError::Parse("mock runner call lock poisoned".to_string()))?
            .push(args.iter().map(|arg| (*arg).to_string()).collect());
        Ok(self.output.clone())
    }
}

fn backend(kind: VcsBackendKind, runner: &Arc<MockJjRunner>) -> JjCliBackend {
    JjCliBackend::new(kind, runner.clone())
}

#[test]
fn every_jj_invocation_starts_with_read_only_flags() -> TestResult {
    let root = tempfile::tempdir()?;
    let scope = DefaultScopeGuard::new(root.path())?;
    let runner = Arc::new(MockJjRunner::new(LOG_FIXTURE));
    let backend = backend(VcsBackendKind::Jujutsu, &runner);
    assert_eq!(backend.kind(), VcsBackendKind::Jujutsu);

    backend.recent_revisions(root.path(), &scope, 12)?;
    backend.revision_file_diff(root.path(), &scope, "qpvuntsm", "src/new.rs", 4)?;
    let calls = runner.recorded_calls()?;
    assert_eq!(calls.len(), 2);
    for call in &calls {
        assert!(call.starts_with(&READ_ONLY_PREFIX.map(str::to_string)));
    }
    assert_eq!(
        &calls[0][3..10],
        [
            "log",
            "--no-graph",
            "-r",
            "::@ & ~empty()",
            "-n",
            "12",
            "-T"
        ]
    );
    assert!(calls[0][10].contains("change_id.shortest(8)"));
    assert!(calls[0][10].contains("committer.timestamp().format(\"%s\")"));
    assert!(calls[0][10].contains("json(author.email())"));
    assert!(calls[0][10].contains("json(description.first_line())"));
    assert_eq!(calls[0].last().map(String::as_str), Some("--git"));
    assert_eq!(
        &calls[1][3..],
        ["diff", "-r", "qpvuntsm", "--git", "--", "src/new.rs"]
    );
    Ok(())
}

#[test]
fn operation_head_fingerprint_tracks_sorted_files_without_subprocesses() -> TestResult {
    let root = tempfile::tempdir()?;
    let runner = Arc::new(MockJjRunner::new(""));
    let backend = backend(VcsBackendKind::Jujutsu, &runner);
    assert!(backend.head_fingerprint(root.path())?.is_none());
    let heads = root.path().join(".jj/repo/op_store/heads");
    fs::create_dir_all(&heads)?;
    assert!(backend.head_fingerprint(root.path())?.is_none());
    fs::create_dir(heads.join("not-a-head-directory"))?;
    fs::write(heads.join("ffffffff99999999"), "")?;
    fs::write(heads.join("12345678aaaaaaaa"), "")?;
    let initial = backend
        .head_fingerprint(root.path())?
        .ok_or("missing operation fingerprint")?;
    assert_eq!(initial.kind, VcsBackendKind::Jujutsu);
    assert_eq!(initial.display_badge, "jj:12345678");
    assert_eq!(initial.state_digest.len(), 64);
    assert_eq!(
        backend.head_fingerprint(root.path())?,
        Some(initial.clone())
    );

    // Insertion order and file contents are irrelevant; all head names matter.
    fs::remove_file(heads.join("12345678aaaaaaaa"))?;
    fs::write(heads.join("12345678aaaaaaaa"), "ignored head contents")?;
    assert_eq!(
        backend.head_fingerprint(root.path())?,
        Some(initial.clone())
    );
    fs::write(heads.join("eeeeeeee11111111"), "")?;
    let added = backend
        .head_fingerprint(root.path())?
        .ok_or("missing fingerprint after adding head")?;
    assert_eq!(added.display_badge, initial.display_badge);
    assert_ne!(added.state_digest, initial.state_digest);
    fs::remove_file(heads.join("12345678aaaaaaaa"))?;
    let removed = backend
        .head_fingerprint(root.path())?
        .ok_or("missing fingerprint after removing head")?;
    assert_eq!(removed.display_badge, "jj:eeeeeeee");
    assert_ne!(removed.state_digest, added.state_digest);
    assert!(runner.recorded_calls()?.is_empty());
    Ok(())
}

#[test]
fn secondary_workspace_pointer_and_colocated_git_are_read_from_disk() -> TestResult {
    let root = tempfile::tempdir()?;
    let primary = root.path().join("primary/.jj/repo");
    let secondary = root.path().join("secondary");
    fs::create_dir_all(primary.join("op_store/heads"))?;
    fs::create_dir_all(secondary.join(".jj"))?;
    fs::write(primary.join("op_store/heads/abcdef0123456789"), "")?;
    fs::write(secondary.join(".jj/repo"), "../../primary/.jj/repo\n")?;
    let runner = Arc::new(MockJjRunner::new(""));
    let backend = backend(VcsBackendKind::JujutsuColocatedGit, &runner);
    let without_git = backend
        .head_fingerprint(&secondary)?
        .ok_or("missing secondary workspace fingerprint")?;
    assert_eq!(without_git.display_badge, "jj+git:abcdef01");

    fs::create_dir_all(secondary.join(".git/refs/heads"))?;
    fs::write(secondary.join(".git/HEAD"), "ref: refs/heads/main\n")?;
    fs::write(secondary.join(".git/refs/heads/main"), "88f1c2affffff\n")?;
    let first = backend
        .head_fingerprint(&secondary)?
        .ok_or("missing colocated fingerprint")?;
    assert_eq!(first.display_badge, "jj+git:abcdef01(88f1c2a)");
    assert_ne!(first.state_digest, without_git.state_digest);
    fs::write(secondary.join(".git/refs/heads/main"), "1234567aaaaa\n")?;
    let advanced = backend
        .head_fingerprint(&secondary)?
        .ok_or("missing updated git fingerprint")?;
    assert_eq!(advanced.display_badge, "jj+git:abcdef01(1234567)");
    assert_ne!(advanced.state_digest, first.state_digest);

    fs::write(secondary.join(".git/HEAD"), "fedcba988888\n")?;
    let detached = backend
        .head_fingerprint(&secondary)?
        .ok_or("missing detached HEAD fingerprint")?;
    assert_eq!(detached.display_badge, "jj+git:abcdef01(fedcba9)");
    fs::write(
        secondary.join(".jj/repo"),
        primary.to_str().ok_or("non-UTF-8 temporary path")?,
    )?;
    assert_eq!(backend.head_fingerprint(&secondary)?, Some(detached));
    fs::write(primary.join("op_store/heads/00000000aaaaaaaa"), "")?;
    let op_changed = backend
        .head_fingerprint(&secondary)?
        .ok_or("missing changed op fingerprint")?;
    assert_eq!(op_changed.display_badge, "jj+git:00000000(fedcba9)");
    assert_ne!(op_changed.state_digest, advanced.state_digest);
    assert!(runner.recorded_calls()?.is_empty());
    Ok(())
}

#[test]
fn recent_revisions_parse_renames_hunks_counts_and_filter_secrets() -> TestResult {
    let root = tempfile::tempdir()?;
    let scope = DefaultScopeGuard::new(root.path())?;
    let runner = Arc::new(MockJjRunner::new(LOG_FIXTURE));
    let revisions =
        backend(VcsBackendKind::Jujutsu, &runner).recent_revisions(root.path(), &scope, 8)?;
    assert_eq!(revisions.len(), 2);
    let first = &revisions[0];
    assert_eq!(first.rev_id, "88f1c2aabbccddeeff");
    assert_eq!(first.short_id, "qpvuntsm(88f1c2)");
    assert_eq!(first.timestamp_secs, 1_730_000_000);
    assert_eq!(first.author, "alice@example.test");
    assert_eq!(first.summary, "Move parser and extend coverage");
    assert_eq!(first.touched_files.len(), 1);
    let touch = &first.touched_files[0];
    assert_eq!(touch.rel_path, "src/new.rs");
    assert_eq!(touch.old_rel_path.as_deref(), Some("src/old.rs"));
    assert_eq!(touch.hunk_new_ranges, [LineSpan::new(12, 19)]);
    assert_eq!(touch.added_lines, 5);
    assert_eq!(touch.deleted_lines, 1);
    let serialized = serde_json::to_string(&revisions)?;
    for denied in [".env", "private.key", ".jj", "old-secret", "new-secret"] {
        assert!(!serialized.contains(denied));
    }
    let second = &revisions[1];
    assert_eq!(second.short_id, "abc123");
    assert_eq!(second.touched_files.len(), 1);
    assert_eq!(second.touched_files[0].rel_path, "tests/parser.rs");
    assert_eq!(second.touched_files[0].added_lines, 1);
    assert_eq!(second.touched_files[0].deleted_lines, 0);
    assert_eq!(
        second.touched_files[0].hunk_new_ranges,
        [LineSpan::new(1, 1)]
    );
    Ok(())
}

#[test]
fn denied_paths_and_invalid_revision_ids_never_reach_runner() -> TestResult {
    let root = tempfile::tempdir()?;
    let scope = DefaultScopeGuard::new(root.path())?;
    let runner = Arc::new(MockJjRunner::new(""));
    let backend = backend(VcsBackendKind::Jujutsu, &runner);
    for path in [
        "",
        ".env",
        "config/.env.local",
        "id_rsa",
        "private.key",
        ".jj/repo",
        "../outside",
    ] {
        assert!(matches!(
            backend.revision_file_diff(root.path(), &scope, "88f1c2", path, 10),
            Err(VcsError::DeniedByPolicy(denied)) if denied == path
        ));
    }
    // Policy rejection takes precedence over even an invalid revision ID.
    assert!(matches!(
        backend.revision_file_diff(root.path(), &scope, "--config=evil", ".env", 10),
        Err(VcsError::DeniedByPolicy(_))
    ));
    for rev_id in [
        "--config=evil",
        "",
        "abc def",
        "abc\0def",
        "abc\ndef",
        "@",
        "a|b",
        "é",
    ] {
        assert!(matches!(
            backend.revision_file_diff(root.path(), &scope, rev_id, "src/lib.rs", 10),
            Err(VcsError::Parse(_))
        ));
    }
    assert!(runner.recorded_calls()?.is_empty());
    Ok(())
}

#[test]
fn file_diff_sanitizes_and_truncates_at_complete_lines() -> TestResult {
    let root = tempfile::tempdir()?;
    let scope = DefaultScopeGuard::new(root.path())?;
    let runner = Arc::new(MockJjRunner::new(
        "diff --git a/src/lib.rs b/src/lib.rs\r\n\
         --- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\0\u{1b}\n",
    ));
    let backend = backend(VcsBackendKind::Jujutsu, &runner);
    let truncated = backend.revision_file_diff(root.path(), &scope, "88f1c2", "src/lib.rs", 3)?;
    assert_eq!(
        truncated,
        "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs"
    );
    assert_eq!(truncated.lines().count(), 3);
    let minimum = backend.revision_file_diff(root.path(), &scope, "88f1c2", "src/lib.rs", 0)?;
    assert_eq!(minimum, "diff --git a/src/lib.rs b/src/lib.rs");
    let full = backend.revision_file_diff(root.path(), &scope, "88f1c2", "src/lib.rs", 100)?;
    assert_eq!(full.lines().count(), 6);
    assert!(full.ends_with("+new"));
    assert!(!full.chars().any(|ch| ch.is_control() && ch != '\n'));
    Ok(())
}

#[test]
fn zero_revision_limit_skips_runner_and_positive_limit_is_enforced() -> TestResult {
    let root = tempfile::tempdir()?;
    let scope = DefaultScopeGuard::new(root.path())?;
    let runner = Arc::new(MockJjRunner::new(LOG_FIXTURE));
    let backend = backend(VcsBackendKind::Jujutsu, &runner);
    assert!(backend.recent_revisions(root.path(), &scope, 0)?.is_empty());
    assert!(runner.recorded_calls()?.is_empty());
    assert_eq!(backend.recent_revisions(root.path(), &scope, 1)?.len(), 1);
    Ok(())
}

#[test]
fn rename_from_denied_path_clears_old_name_and_decodes_quoted_paths() -> TestResult {
    let root = tempfile::tempdir()?;
    let scope = DefaultScopeGuard::new(root.path())?;
    let output = concat!(
        "LEDGER_REVISION\t88f1c2abcdef\tqpvuntsm\t1730000000\t\"a\"\t\"Rename\"\n",
        "diff --git a/.env b/.env.example\n",
        "rename from .env\nrename to .env.example\n",
        "diff --git \"a/src/old name.rs\" \"b/src/new name.rs\"\n",
        "rename from \"src/old name.rs\"\nrename to \"src/new name.rs\"\n",
        "@@ -1 +1,2 @@\n-old\n+new\n++content starting with plus\n",
        "diff --git \"a/\\056env\" \"b/\\056env\"\n",
        "@@ -1 +1 @@\n-secret\n+secret\n",
        "diff --git \"a/src/\\303\\251.rs\" \"b/src/\\303\\251.rs\"\n",
        "@@ -1 +0,0 @@\n-deleted\n",
        "diff --git a/src/space name.rs b/src/space name.rs\n",
        "@@ -1 +1 @@\n-old\n+new\n"
    );
    let runner = Arc::new(MockJjRunner::new(output));
    let revisions =
        backend(VcsBackendKind::Jujutsu, &runner).recent_revisions(root.path(), &scope, 1)?;
    let touches = &revisions[0].touched_files;
    assert_eq!(touches.len(), 4);
    assert_eq!(touches[0].rel_path, ".env.example");
    assert!(touches[0].old_rel_path.is_none());
    assert_eq!(touches[1].rel_path, "src/new name.rs");
    assert_eq!(touches[1].old_rel_path.as_deref(), Some("src/old name.rs"));
    assert_eq!(touches[1].added_lines, 2);
    assert_eq!(touches[1].deleted_lines, 1);
    assert_eq!(touches[2].rel_path, "src/é.rs");
    assert!(touches[2].hunk_new_ranges.is_empty());
    assert_eq!(touches[2].deleted_lines, 1);
    assert_eq!(touches[3].rel_path, "src/space name.rs");
    Ok(())
}

#[test]
fn metadata_json_preserves_record_boundaries_and_bounds_untrusted_text() -> TestResult {
    let root = tempfile::tempdir()?;
    let scope = DefaultScopeGuard::new(root.path())?;
    let author = serde_json::to_string(&"a".repeat(300))?;
    let summary = serde_json::to_string("Subject\twith tab\0 and \"quotes\"")?;
    let output = format!("LEDGER_REVISION\t88f1c2abcdef\t\t1730000000\t{author}\t{summary}\n");
    let runner = Arc::new(MockJjRunner::new(&output));
    let revisions =
        backend(VcsBackendKind::Jujutsu, &runner).recent_revisions(root.path(), &scope, 2)?;
    assert_eq!(revisions.len(), 1);
    assert_eq!(revisions[0].short_id, "88f1c2");
    assert_eq!(revisions[0].author, format!("{}...", "a".repeat(240)));
    assert_eq!(revisions[0].summary, "Subject\twith tab and \"quotes\"");
    Ok(())
}

#[test]
fn mixed_quoting_and_spaces_before_b_prefix_cannot_bypass_scope() -> TestResult {
    let root = tempfile::tempdir()?;
    let scope = DefaultScopeGuard::new(root.path())?;
    let output = concat!(
        "LEDGER_REVISION\t88f1c2abcdef\tqpvuntsm\t1730000000\t\"a\"\t\"Paths\"\n",
        "diff --git a/src/plain.rs \"b/src/line\\tname.rs\"\n",
        "rename from src/plain.rs\nrename to \"src/line\\tname.rs\"\n",
        "diff --git \"a/src/line\\tname.rs\" b/src/plain.rs\n",
        "rename from \"src/line\\tname.rs\"\nrename to src/plain.rs\n",
        "diff --git a/src/a b/file.rs b/src/a b/file.rs\n",
        "@@ -1 +1 @@\n-old\n+new\n",
        "diff --git a/.jj/secret b/innocent.rs b/.jj/secret b/innocent.rs\n",
        "@@ -1 +1 @@\n-old\n+new\n"
    );
    let runner = Arc::new(MockJjRunner::new(output));
    let revisions =
        backend(VcsBackendKind::Jujutsu, &runner).recent_revisions(root.path(), &scope, 1)?;
    let touches = &revisions[0].touched_files;
    assert_eq!(touches.len(), 3);
    assert_eq!(touches[0].rel_path, "src/line\tname.rs");
    assert_eq!(touches[0].old_rel_path.as_deref(), Some("src/plain.rs"));
    assert_eq!(touches[1].rel_path, "src/plain.rs");
    assert_eq!(
        touches[1].old_rel_path.as_deref(),
        Some("src/line\tname.rs")
    );
    assert_eq!(touches[2].rel_path, "src/a b/file.rs");
    assert!(!serde_json::to_string(&revisions)?.contains(".jj"));
    Ok(())
}

#[test]
fn malformed_log_output_returns_parse_errors() -> TestResult {
    let root = tempfile::tempdir()?;
    let scope = DefaultScopeGuard::new(root.path())?;
    for output in [
        "unexpected output\n",
        "LEDGER_REVISION\tbad-header\n",
        "LEDGER_REVISION\t--evil\tqpvuntsm\t1730000000\t\"a\"\t\"b\"\n",
        "LEDGER_REVISION\t88f1c2\tqpvuntsm\tbad-time\t\"a\"\t\"b\"\n",
        "LEDGER_REVISION\t88f1c2\tqpvuntsm\t1730000000\tbad-json\t\"b\"\n",
        "diff --git a/src/lib.rs b/src/lib.rs\n",
        concat!(
            "LEDGER_REVISION\t88f1c2\tqpvuntsm\t1730000000\t\"a\"\t\"b\"\n",
            "diff --git a/src/lib.rs b/src/lib.rs\n@@ -1 +4294967295,2 @@\n"
        ),
    ] {
        let runner = Arc::new(MockJjRunner::new(output));
        assert!(matches!(
            backend(VcsBackendKind::Jujutsu, &runner).recent_revisions(root.path(), &scope, 1),
            Err(VcsError::Parse(_))
        ));
    }
    Ok(())
}

#[test]
fn command_failures_preserve_status_and_sanitize_stderr() -> TestResult {
    let root = tempfile::tempdir()?;
    let scope = DefaultScopeGuard::new(root.path())?;
    let mut mock = MockJjRunner::new("");
    mock.output.status_code = Some(2);
    mock.output.stderr = b"failed\0\x1b\r".to_vec();
    let runner = Arc::new(mock);
    let backend = backend(VcsBackendKind::Jujutsu, &runner);
    for result in [
        backend.recent_revisions(root.path(), &scope, 1).map(|_| ()),
        backend
            .revision_file_diff(root.path(), &scope, "88f1c2", "src/lib.rs", 10)
            .map(|_| ()),
    ] {
        assert!(matches!(
            result,
            Err(VcsError::CommandFailed { program, code: Some(2), stderr })
                if program == "jj" && stderr == "failed"
        ));
    }
    for call in runner.recorded_calls()? {
        assert!(call.starts_with(&READ_ONLY_PREFIX.map(str::to_string)));
    }
    Ok(())
}

#[test]
fn jj_history_and_direct_diffs_enforce_nested_boundaries_without_requiring_old_leaves() -> TestResult
{
    let root = tempfile::tempdir()?;
    fs::create_dir_all(root.path().join("nested_git/.git"))?;
    fs::create_dir_all(root.path().join("worktree"))?;
    fs::write(
        root.path().join("worktree/.git"),
        "gitdir: /unused/worktree\n",
    )?;
    fs::create_dir_all(root.path().join("nested_jj/.jj"))?;
    let scope = DefaultScopeGuard::new(root.path())?;
    let log = concat!(
        "LEDGER_REVISION\t88f1c2abcdef\tqpvuntsm\t1730000000\t\"a\"\t\"History\"\n",
        "diff --git a/nested_git/deleted.rs b/nested_git/deleted.rs\n@@ -1 +1 @@\n-old\n+new\n",
        "diff --git a/worktree/deleted.rs b/worktree/deleted.rs\n@@ -1 +1 @@\n-old\n+new\n",
        "diff --git a/nested_jj/deleted.rs b/nested_jj/deleted.rs\n@@ -1 +1 @@\n-old\n+new\n",
        "diff --git a/old/deleted.rs b/old/deleted.rs\n@@ -1 +1 @@\n-old\n+new\n",
    );
    let runner = Arc::new(MockJjRunner::new(log));
    let backend = backend(VcsBackendKind::Jujutsu, &runner);
    let revisions = backend.recent_revisions(root.path(), &scope, 10)?;
    assert_eq!(revisions[0].touched_files.len(), 1);
    assert_eq!(revisions[0].touched_files[0].rel_path, "old/deleted.rs");
    for path in [
        "nested_git/deleted.rs",
        "worktree/deleted.rs",
        "nested_jj/deleted.rs",
        "../outside.rs",
    ] {
        assert!(
            matches!(
                backend.revision_file_diff(root.path(), &scope, "88f1c2abcdef", path, 80),
                Err(VcsError::DeniedByPolicy(_))
            ),
            "{path}"
        );
    }
    assert_eq!(runner.recorded_calls()?.len(), 1);
    assert!(
        !backend
            .revision_file_diff(root.path(), &scope, "88f1c2abcdef", "old/deleted.rs", 80)?
            .is_empty()
    );
    assert_eq!(runner.recorded_calls()?.len(), 2);
    Ok(())
}
