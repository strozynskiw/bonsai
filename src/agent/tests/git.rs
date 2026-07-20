//! Git scaffolding for the review/diff tests: init a throwaway repo and commit
//! files into it.

use std::path::Path;

pub(super) fn init_repo(root: &Path) {
    run_git(root, &["init", "--quiet"]);
    run_git(root, &["config", "user.email", "test@example.com"]);
    run_git(root, &["config", "user.name", "Test"]);
}

pub(super) fn commit_file(root: &Path, path: &str, content: &str, message: &str) {
    std::fs::write(root.join(path), content).unwrap();
    run_git(root, &["add", path]);
    run_git(root, &["commit", "--quiet", "-m", message]);
}

pub(super) fn run_git(root: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed with {status}");
}
