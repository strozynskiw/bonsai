use std::path::Path;

use super::*;

#[test]
fn disabled_sandbox_is_never_active() {
    let sb = CommandSandbox::disabled();
    assert!(!sb.is_active());
    assert_eq!(sb.backend(), SandboxBackend::Unavailable);
}

#[test]
fn disabled_command_runs_plain_shell() {
    let sb = CommandSandbox::disabled();
    let (cmd, decision) = sb.command("/bin/sh", "echo hi", Path::new("/"));
    assert_eq!(cmd.as_std().get_program(), "/bin/sh");
    assert!(!decision.confined);
}

#[test]
fn unavailable_backend_reports_no_capabilities() {
    assert!(!SandboxBackend::Unavailable.is_available());
    assert!(!SandboxBackend::Unavailable.supports_network_deny());
    assert_eq!(SandboxBackend::Unavailable.label(), "none");
}

#[cfg(target_os = "linux")]
#[test]
fn bubblewrap_backend_reports_capabilities() {
    let full = SandboxBackend::test_bubblewrap(true);
    let shared_net_only = SandboxBackend::test_bubblewrap(false);

    assert!(full.is_available());
    assert!(full.supports_network_deny());
    assert!(!shared_net_only.supports_network_deny());
    assert_eq!(full.label(), "bubblewrap");
    assert_eq!(shared_net_only.label(), "bubblewrap");
}

#[test]
fn policy_includes_project_root() {
    let proj = tempfile::tempdir().unwrap();
    let sb = CommandSandbox::new(SandboxBackend::Unavailable, proj.path());
    let root = proj.path().canonicalize().unwrap();
    assert!(sb.policy().writable_roots.contains(&root));
}

#[tokio::test]
async fn command_exports_private_session_temp_and_allows_the_shared_parent() {
    let proj = tempfile::tempdir().unwrap();
    let private_temp;
    {
        let sb = CommandSandbox::new(SandboxBackend::Unavailable, proj.path());
        private_temp = sb
            .temp_dir()
            .expect("configured sandboxes own a private temp directory")
            .to_path_buf();
        let canonical_temp = private_temp.canonicalize().unwrap();
        assert!(sb.policy().writable_roots.contains(&canonical_temp));

        // Contract change (deliberate): the shared OS temp parent IS writable —
        // macOS BSD tools (mktemp in git hooks) resolve temp via confstr and
        // ignore $TMPDIR, so confining them to the private dir is impossible.
        // The private TMPDIR export below still steers well-behaved tools.
        let shared_temp = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        assert!(
            sb.policy().writable_roots.contains(&shared_temp),
            "the OS temp dir must be writable for confstr-based tools"
        );

        let (mut command, _decision) = sb.command(
            "/bin/sh",
            "printf '%s\\n%s\\n%s' \"$TMPDIR\" \"$TMP\" \"$TEMP\"; touch \"$TMPDIR/probe\"",
            proj.path(),
        );
        let output = command.output().await.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let exported = String::from_utf8(output.stdout).unwrap();
        assert!(
            exported
                .lines()
                .all(|line| line == private_temp.to_string_lossy())
        );
        assert!(private_temp.join("probe").exists());
    }
    assert!(
        !private_temp.exists(),
        "the private temp directory should be removed with the sandbox session"
    );
}

#[tokio::test]
async fn command_isolates_ephemeral_language_caches_per_session() {
    let project = tempfile::tempdir().unwrap();
    let sandbox = CommandSandbox::new(SandboxBackend::Unavailable, project.path());
    let private_temp = sandbox.temp_dir().unwrap().to_path_buf();
    let (mut command, _) = sandbox.command(
        "/bin/sh",
        "printf '%s\n%s\n%s\n%s' \"$npm_config_cache\" \"$XDG_CACHE_HOME\" \"$PIP_CACHE_DIR\" \"$GOCACHE\"",
        project.path(),
    );
    let output = command.output().await.unwrap();
    assert!(output.status.success());
    let exported = String::from_utf8(output.stdout).unwrap();
    let expected = [
        private_temp.join("cache/npm"),
        private_temp.join("cache/xdg"),
        private_temp.join("cache/pip"),
        private_temp.join("cache/go-build"),
    ];
    assert_eq!(
        exported.lines().collect::<Vec<_>>(),
        expected
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    );
}

#[test]
fn activation_is_independent_of_autonomy() {
    // The sandbox is an enforcement floor: activation depends only on the toggle
    // and backend availability, never on the autonomy level — so it is NOT
    // bypassed at `yolo`. (Backend availability is platform-independent for
    // SeatbeltExec, so this is portable.)
    let proj = tempfile::tempdir().unwrap();
    let sb = CommandSandbox::new(SandboxBackend::test_seatbelt(), proj.path());
    assert!(
        sb.is_active(),
        "available backends are active by default, regardless of autonomy"
    );
    sb.set_enabled(false);
    assert!(!sb.is_active());
}

#[cfg(target_os = "linux")]
mod bubblewrap {
    use std::process::Stdio;

    use super::super::linux;
    use super::*;

    fn linux_backend(network_deny: bool) -> SandboxBackend {
        SandboxBackend::test_bubblewrap(network_deny)
    }

    fn linux_sandbox(root: &Path) -> CommandSandbox {
        let sb = CommandSandbox::new(linux_backend(true), root);
        sb.set_enabled(true);
        sb
    }

    fn args(cmd: &tokio::process::Command) -> Vec<String> {
        cmd.as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn arg_index(args: &[String], needle: &str) -> usize {
        args.iter()
            .position(|arg| arg == needle)
            .unwrap_or_else(|| panic!("missing argument {needle:?} in {args:?}"))
    }

    #[test]
    fn detect_backend_returns_bubblewrap_when_probe_succeeds() {
        if let Some(backend) = linux::detect_backend() {
            assert_eq!(detect_backend(), backend);
        } else {
            assert_eq!(detect_backend(), SandboxBackend::Unavailable);
        }
    }

    #[test]
    fn enabled_command_rewrites_to_bwrap() {
        let proj = tempfile::tempdir().unwrap();
        let sb = linux_sandbox(proj.path());
        let (cmd, decision) = sb.command("/bin/sh", "echo hi", proj.path());
        assert_eq!(
            cmd.as_std().get_program(),
            std::path::Path::new("/usr/bin/bwrap")
        );
        assert!(decision.confined);
        assert_eq!(decision.backend, linux_backend(true));
    }

    #[test]
    fn disabled_command_is_not_wrapped() {
        let proj = tempfile::tempdir().unwrap();
        let sb = CommandSandbox::new(linux_backend(true), proj.path());
        // An available backend is enabled by default, so disabling is explicit.
        sb.set_enabled(false);
        let (cmd, decision) = sb.command("/bin/sh", "echo hi", proj.path());
        assert_eq!(cmd.as_std().get_program(), "/bin/sh");
        assert!(!decision.confined);
    }

    #[test]
    fn command_binds_root_readonly_before_writable_roots() {
        let policy = SandboxPolicy {
            writable_roots: vec!["/proj".into(), "/tmp/work".into()],
            deny_network: true,
        };
        let (cmd, decision) = linux::wrap(
            "/bin/sh",
            "echo hi",
            Path::new("/proj"),
            linux_backend(true),
            &policy,
        );
        let args = args(&cmd);

        assert_eq!(decision.backend, linux_backend(true));
        assert!(decision.network_denied);
        assert!(
            args.windows(3)
                .any(|window| window == ["--ro-bind", "/", "/"])
        );
        assert!(
            args.windows(3)
                .any(|window| window == ["--bind-try", "/proj", "/proj"])
        );
        assert!(
            args.windows(3)
                .any(|window| window == ["--bind-try", "/tmp/work", "/tmp/work"])
        );
        assert!(
            arg_index(&args, "--ro-bind") < arg_index(&args, "--bind-try"),
            "read-only root bind must precede writable overrides: {args:?}"
        );
    }

    #[test]
    fn command_shares_network_only_when_allowed() {
        let allowed = SandboxPolicy {
            writable_roots: vec!["/proj".into()],
            deny_network: false,
        };
        let denied = SandboxPolicy {
            writable_roots: vec!["/proj".into()],
            deny_network: true,
        };

        let (cmd, allowed_decision) = linux::wrap(
            "/bin/sh",
            "echo hi",
            Path::new("/proj"),
            linux_backend(true),
            &allowed,
        );
        assert!(!allowed_decision.network_denied);
        assert!(args(&cmd).iter().any(|arg| arg == "--share-net"));

        let (cmd, denied_decision) = linux::wrap(
            "/bin/sh",
            "echo hi",
            Path::new("/proj"),
            linux_backend(true),
            &denied,
        );
        assert!(denied_decision.network_denied);
        assert!(!args(&cmd).iter().any(|arg| arg == "--share-net"));
    }

    #[test]
    fn command_degrades_network_deny_when_bubblewrap_cannot_isolate_network() {
        let policy = SandboxPolicy {
            writable_roots: vec!["/proj".into()],
            deny_network: true,
        };

        let (cmd, decision) = linux::wrap(
            "/bin/sh",
            "echo hi",
            Path::new("/proj"),
            linux_backend(false),
            &policy,
        );

        assert!(!decision.network_denied);
        assert!(
            decision
                .degraded
                .as_deref()
                .is_some_and(|reason| reason.contains("cannot isolate networking")),
            "missing network degradation reason: {decision:?}"
        );
        assert!(args(&cmd).iter().any(|arg| arg == "--share-net"));
    }

    #[test]
    fn production_policy_command_grants_project_root() {
        let proj = tempfile::tempdir().unwrap();
        let proj_root = proj.path().canonicalize().unwrap();
        let sb = CommandSandbox::new(linux_backend(true), proj.path());
        let (cmd, _) = linux::wrap(
            "/bin/sh",
            "echo hi",
            &proj_root,
            linux_backend(true),
            &sb.policy(),
        );
        let args = args(&cmd);
        let root = proj_root.to_string_lossy();
        assert!(
            args.windows(3)
                .any(|window| window[0] == "--bind-try" && window[1] == root && window[2] == root),
            "project root should be writable in production policy: {args:?}"
        );
    }

    #[tokio::test]
    async fn blocks_write_outside_project_root() {
        let Some(backend) = linux::detect_backend() else {
            eprintln!("skipping Bubblewrap integration test: bwrap unavailable");
            return;
        };

        let proj = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let proj_root = proj.path().canonicalize().unwrap();
        let policy = SandboxPolicy {
            writable_roots: vec![proj_root.clone()],
            deny_network: false,
        };

        let in_path = proj_root.join("in.txt");
        let status = run(
            backend.clone(),
            &policy,
            &proj_root,
            &format!("echo ok > '{}'", in_path.display()),
        )
        .await;
        assert!(status.success(), "in-root write should succeed");
        assert!(in_path.exists());

        let escape = outside.path().canonicalize().unwrap().join("escape.txt");
        let status = run(
            backend.clone(),
            &policy,
            &proj_root,
            &format!("echo pwned > '{}'", escape.display()),
        )
        .await;
        assert!(!status.success(), "out-of-root write must be blocked");
        assert!(!escape.exists(), "write escaped the sandbox");

        let symlink = proj_root.join("outside-link");
        std::os::unix::fs::symlink(outside.path(), &symlink).unwrap();
        let through_symlink = outside.path().join("through-symlink.txt");
        let status = run(
            backend.clone(),
            &policy,
            &proj_root,
            &format!("echo pwned > '{}/through-symlink.txt'", symlink.display()),
        )
        .await;
        assert!(!status.success(), "symlink write escape must be blocked");
        assert!(!through_symlink.exists());

        let child_escape = outside.path().join("child-escape.txt");
        let status = run(
            backend,
            &policy,
            &proj_root,
            &format!("sh -c \"echo pwned > '{}'\"", child_escape.display()),
        )
        .await;
        assert!(
            !status.success(),
            "child-process write escape must be blocked"
        );
        assert!(!child_escape.exists());
    }

    #[tokio::test]
    async fn allows_configured_writable_root() {
        let Some(backend) = linux::detect_backend() else {
            eprintln!("skipping Bubblewrap integration test: bwrap unavailable");
            return;
        };

        let proj = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        let proj_root = proj.path().canonicalize().unwrap();
        let extra_root = extra.path().canonicalize().unwrap();
        let policy = SandboxPolicy {
            writable_roots: vec![proj_root.clone(), extra_root.clone()],
            deny_network: false,
        };

        let extra_file = extra_root.join("allowed.txt");
        let status = run(
            backend,
            &policy,
            &proj_root,
            &format!("echo ok > '{}'", extra_file.display()),
        )
        .await;
        assert!(status.success(), "configured writable root should work");
        assert!(extra_file.exists());
    }

    #[tokio::test]
    #[ignore = "requires network egress; run manually"]
    async fn denies_network_when_configured() {
        let Some(backend) =
            linux::detect_backend().filter(|backend| backend.supports_network_deny())
        else {
            eprintln!("skipping Bubblewrap integration test: network isolation unavailable");
            return;
        };

        let proj = tempfile::tempdir().unwrap();
        let root = proj.path().canonicalize().unwrap();
        let policy = SandboxPolicy {
            writable_roots: vec![root.clone()],
            deny_network: true,
        };
        let status = run(backend, &policy, &root, "curl -m 2 -s https://example.com").await;
        assert!(!status.success(), "network egress must be denied");
    }

    async fn run(
        backend: SandboxBackend,
        policy: &SandboxPolicy,
        cwd: &Path,
        script: &str,
    ) -> std::process::ExitStatus {
        let (mut cmd, _) = linux::wrap("/bin/sh", script, cwd, backend, policy);
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        cmd.status().await.unwrap()
    }
}

#[cfg(target_os = "macos")]
mod seatbelt {
    use std::process::Stdio;

    use super::super::macos;
    use super::*;

    fn macos_sandbox(root: &Path) -> CommandSandbox {
        let sb = CommandSandbox::new(SandboxBackend::test_seatbelt(), root);
        sb.set_enabled(true);
        sb
    }

    #[test]
    fn enabled_command_rewrites_to_sandbox_exec() {
        let proj = tempfile::tempdir().unwrap();
        let sb = macos_sandbox(proj.path());
        let (cmd, decision) = sb.command("/bin/sh", "echo hi", proj.path());
        assert_eq!(
            cmd.as_std().get_program(),
            std::path::Path::new("/usr/bin/sandbox-exec")
        );
        assert!(decision.confined);
        assert_eq!(decision.backend, SandboxBackend::test_seatbelt());
    }

    #[test]
    fn disabled_command_is_not_wrapped() {
        let proj = tempfile::tempdir().unwrap();
        let sb = CommandSandbox::new(SandboxBackend::test_seatbelt(), proj.path());
        sb.set_enabled(false);
        let (cmd, decision) = sb.command("/bin/sh", "echo hi", proj.path());
        assert_eq!(cmd.as_std().get_program(), "/bin/sh");
        assert!(!decision.confined);
    }

    #[test]
    fn profile_lists_roots_devices_and_network_rule() {
        let policy = SandboxPolicy {
            writable_roots: vec!["/proj".into(), "/tmp/work".into()],
            deny_network: true,
        };
        let p = macos::generate_profile(&policy);
        assert!(p.contains("(subpath \"/proj\")"));
        assert!(p.contains("(subpath \"/tmp/work\")"));
        assert!(p.contains("(literal \"/dev/null\")"));
        assert!(p.contains("(deny network*)"));
        // Last-match-wins ordering is load-bearing.
        let allow_default = p.find("(allow default)").unwrap();
        let deny_writes = p.find("(deny file-write*)").unwrap();
        let allow_writes = p.find("(allow file-write*").unwrap();
        assert!(allow_default < deny_writes && deny_writes < allow_writes);
    }

    #[test]
    fn profile_omits_network_rule_when_allowed() {
        let policy = SandboxPolicy {
            writable_roots: vec!["/proj".into()],
            deny_network: false,
        };
        let p = macos::generate_profile(&policy);
        assert!(!p.contains("network"));
    }

    #[test]
    fn profile_escapes_quotes_in_paths() {
        let policy = SandboxPolicy {
            writable_roots: vec!["/weird\"path".into()],
            deny_network: false,
        };
        let p = macos::generate_profile(&policy);
        assert!(p.contains("/weird\\\"path"));
    }

    /// The production policy (`CommandSandbox::policy()`) must round-trip through
    /// `generate_profile` to a profile that grants the project root — guarding the
    /// path the live spawn actually takes, not just hand-built policies.
    #[test]
    fn production_policy_profile_grants_project_root() {
        let proj = tempfile::tempdir().unwrap();
        let proj_root = proj.path().canonicalize().unwrap();
        let sb = CommandSandbox::new(SandboxBackend::test_seatbelt(), proj.path());
        let profile = macos::generate_profile(&sb.policy());
        assert!(profile.contains(&format!("(subpath \"{}\")", proj_root.display())));
    }

    /// Regression: macOS BSD `mktemp` resolves temp via confstr and ignores
    /// `$TMPDIR`, so the production policy must allowlist the OS temp dir —
    /// observed live as `git commit` failing confined when a pre-commit hook
    /// called bare `mktemp` ("mkstemp ... Operation not permitted").
    #[tokio::test]
    async fn production_policy_allows_bsd_mktemp_confined() {
        let proj = tempfile::tempdir().unwrap();
        let sb = macos_sandbox(proj.path());
        let (mut cmd, decision) = sb.command("/bin/sh", "mktemp && mktemp -d", proj.path());
        assert!(decision.confined, "test requires the confined path");
        let status = cmd
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(
            status.success(),
            "bare mktemp must succeed under the production confined policy"
        );
    }

    /// The reported end-to-end scenario: a confined `git commit` in a repo whose
    /// pre-commit hook uses `mktemp` must succeed without a sandbox escape.
    #[tokio::test]
    async fn confined_git_commit_with_mktemp_hook_succeeds() {
        let proj = tempfile::tempdir().unwrap();
        let root = proj.path().canonicalize().unwrap();
        let git = |args: &str| {
            let root = root.clone();
            let args = args.to_string();
            async move {
                let status = tokio::process::Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args([
                        "-c",
                        "user.email=t@t",
                        "-c",
                        "user.name=t",
                        "-c",
                        "commit.gpgsign=false",
                    ])
                    .args(args.split_whitespace())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await
                    .unwrap();
                assert!(status.success(), "git {args} failed");
            }
        };
        git("init --quiet").await;
        std::fs::create_dir_all(root.join("hooks")).unwrap();
        std::fs::write(
            root.join("hooks/pre-commit"),
            "#!/bin/sh\nset -e\nt=$(mktemp)\necho probe > \"$t\"\nrm -f \"$t\"\n",
        )
        .unwrap();
        std::fs::set_permissions(
            root.join("hooks/pre-commit"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        git("config core.hooksPath hooks").await;
        std::fs::write(root.join("file.txt"), "content").unwrap();
        git("add file.txt").await;

        let sb = macos_sandbox(&root);
        let (mut cmd, decision) = sb.command(
            "/bin/sh",
            "git -c user.email=t@t -c user.name=t -c commit.gpgsign=false commit --quiet -m confined",
            &root,
        );
        assert!(decision.confined);
        let output = cmd.output().await.unwrap();
        assert!(
            output.status.success(),
            "confined commit with a mktemp hook must succeed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// The core enforcement proof: with only the project dir writable
    /// (deliberately excluding `$TMPDIR`), an in-root write succeeds and an
    /// out-of-root write is blocked by the kernel.
    #[tokio::test]
    async fn blocks_write_outside_project_root() {
        let proj = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let proj_root = proj.path().canonicalize().unwrap();
        let policy = SandboxPolicy {
            writable_roots: vec![proj_root.clone()],
            deny_network: false,
        };

        let in_path = proj_root.join("in.txt");
        let status = run(
            &policy,
            &proj_root,
            &format!("echo ok > '{}'", in_path.display()),
        )
        .await;
        assert!(status.success(), "in-root write should succeed");
        assert!(in_path.exists());

        let escape = outside.path().canonicalize().unwrap().join("escape.txt");
        let status = run(
            &policy,
            &proj_root,
            &format!("echo pwned > '{}'", escape.display()),
        )
        .await;
        assert!(!status.success(), "out-of-root write must be blocked");
        assert!(!escape.exists(), "write escaped the sandbox");

        let symlink = proj_root.join("outside-link");
        std::os::unix::fs::symlink(outside.path(), &symlink).unwrap();
        let through_symlink = outside.path().join("through-symlink.txt");
        let status = run(
            &policy,
            &proj_root,
            &format!("echo pwned > '{}/through-symlink.txt'", symlink.display()),
        )
        .await;
        assert!(!status.success(), "symlink write escape must be blocked");
        assert!(!through_symlink.exists());

        let child_escape = outside.path().join("child-escape.txt");
        let status = run(
            &policy,
            &proj_root,
            &format!("sh -c \"echo pwned > '{}'\"", child_escape.display()),
        )
        .await;
        assert!(
            !status.success(),
            "child-process write escape must be blocked"
        );
        assert!(!child_escape.exists());
    }

    /// The core M3.3 proof: a write the confined sandbox blocks succeeds when the
    /// *same* command is spawned via an approved escape (`command_unconfined`),
    /// while the shared sandbox stays active.
    #[tokio::test]
    async fn escape_runs_what_confinement_blocks() {
        let proj = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let proj_root = proj.path().canonicalize().unwrap();
        let escape_file = outside.path().canonicalize().unwrap().join("escape.txt");
        let script = format!("echo pwned > '{}'", escape_file.display());

        // Confined: writing outside the single allowed root is blocked.
        let policy = SandboxPolicy {
            writable_roots: vec![proj_root.clone()],
            deny_network: false,
        };
        let status = run(&policy, &proj_root, &script).await;
        assert!(
            !status.success(),
            "confined out-of-root write must be blocked"
        );
        assert!(!escape_file.exists());

        // Escaped: the same command, spawned unconfined, succeeds.
        let sb = CommandSandbox::new(SandboxBackend::test_seatbelt(), &proj_root);
        let (mut cmd, decision) = sb.command_unconfined("/bin/sh", &script, &proj_root);
        assert!(decision.escaped, "escape spawn must be flagged");
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        let status = cmd.status().await.unwrap();
        assert!(status.success(), "escaped out-of-root write should succeed");
        assert!(escape_file.exists(), "escaped write should land");
    }

    /// Opt-in network proof — needs real network, so it is ignored by default.
    #[tokio::test]
    #[ignore = "requires network egress; run manually"]
    async fn denies_network_when_configured() {
        let proj = tempfile::tempdir().unwrap();
        let root = proj.path().canonicalize().unwrap();
        let policy = SandboxPolicy {
            writable_roots: vec![root.clone()],
            deny_network: true,
        };
        let status = run(&policy, &root, "curl -m 2 -s https://example.com").await;
        assert!(!status.success(), "network egress must be denied");
    }

    async fn run(policy: &SandboxPolicy, cwd: &Path, script: &str) -> std::process::ExitStatus {
        let (mut cmd, _) = macos::wrap("/bin/sh", script, cwd, policy);
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        cmd.status().await.unwrap()
    }
}
