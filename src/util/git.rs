//! Shared configuration for Git subprocesses used for inspection.

use std::path::Path;
use std::process::Command;

/// Construct an inspection command without optional index writes or common
/// repository-configured helpers. Git clean filters are not disabled here.
pub(crate) fn inspection_command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .args([
            "--no-pager",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "log.showSignature=false",
        ])
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_LAZY_FETCH", "1");
    command
}

pub(crate) fn async_inspection_command(root: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::from(inspection_command(root));
    command.kill_on_drop(true);
    command
}

#[cfg(test)]
pub(crate) fn setup_test_fsmonitor(root: &Path) {
    std::fs::write(root.join("probe.sh"), "touch fsmonitor-ran\n").unwrap();
    std::fs::write(root.join("tracked.txt"), "tracked\n").unwrap();
    for args in [
        vec!["init"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Test"],
        vec!["add", "tracked.txt"],
        vec!["commit", "-m", "initial"],
        vec!["config", "core.fsmonitor", "sh probe.sh"],
        vec!["status", "--porcelain"],
    ] {
        let output = Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
    }
    let marker = root.join("fsmonitor-ran");
    assert!(
        marker.exists(),
        "raw git status did not run the configured fsmonitor"
    );
    std::fs::remove_file(marker).unwrap();
}
