use std::error::Error;
use std::fs;

use ledger_core::DefaultScopeGuard;
use ledger_search::{LexicalSearcher, RipgrepSearcher, SearchBudget};

#[test]
fn discovery_and_search_exclude_nested_git_worktree_and_jj_source() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    fs::create_dir(root.path().join(".git"))?;
    fs::write(root.path().join("outer.rs"), "pub fn visible_needle() {}\n")?;
    for name in ["nested_git", "worktree", "nested_jj"] {
        let path = root.path().join(name);
        fs::create_dir_all(path.join("src"))?;
        if name == "worktree" {
            fs::write(path.join(".git"), "gitdir: /unused/other/worktree\n")?;
        } else {
            fs::create_dir(path.join(if name == "nested_jj" { ".jj" } else { ".git" }))?;
        }
        fs::write(path.join("src/inner.rs"), "pub fn hidden_needle() {}\n")?;
    }
    fs::write(root.path().join(".env"), "hidden_needle=secret\n")?;
    let searcher = RipgrepSearcher::new(DefaultScopeGuard::new(root.path())?);
    let (files, _) = searcher.list_allowed_files();
    assert_eq!(
        files
            .iter()
            .map(|(_, rel)| rel.as_str())
            .collect::<Vec<_>>(),
        vec!["outer.rs"]
    );
    let report = searcher.search_literal("needle", &SearchBudget::default())?;
    assert_eq!(report.matches.len(), 1);
    assert_eq!(report.matches[0].rel_path, "outer.rs");
    assert_eq!(report.files_scanned, 1);
    assert!(!report.search_incomplete);
    Ok(())
}
