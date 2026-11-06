"""Reproduce Git metadata-isolation safety and fidelity experiments.

Run with `python3 eval/evidence/git-isolation-probe.py`. This is a research
probe, not an implementation of a safe Git sandbox.
"""

import os
import pathlib
import subprocess
import tempfile
import time

BASE_ENV = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
BASE_ENV.update(GIT_CONFIG_SYSTEM=os.devnull, GIT_CONFIG_GLOBAL=os.devnull)


def git(root, *args, env=None):
    return subprocess.run(
        ["git", "-C", str(root), *args],
        env=BASE_ENV if env is None else env,
        capture_output=True,
        text=True,
    )


def describe(label, result, marker=None):
    print(
        label,
        "exit", result.returncode,
        "filter", marker.exists() if marker else "n/a",
        "output", repr(result.stdout[:160]),
        "error", repr(result.stderr[:160]),
    )


with tempfile.TemporaryDirectory() as directory:
    base = pathlib.Path(directory)
    project = base / "project"
    project.mkdir()
    for args in [
        ("init",),
        ("config", "user.email", "test@example.com"),
        ("config", "user.name", "Test"),
    ]:
        assert git(project, *args).returncode == 0
    (project / ".gitattributes").write_text("*.txt filter=probe\n")
    (project / "data.txt").write_text("before\n")
    assert git(project, "add", ".").returncode == 0
    assert git(project, "commit", "-m", "initial").returncode == 0
    assert git(project, "config", "filter.probe.clean", "touch filter-ran; cat").returncode == 0
    (project / ".git/info/attributes").write_text("*.txt filter=probe\n")
    (project / "data.txt").write_text("after!\n")
    raw = git(project, "diff", "--no-ext-diff", "--no-textconv")
    marker = project / "filter-ran"
    assert raw.returncode == 0 and marker.exists()
    marker.unlink()

    safe = base / "safe-common"
    safe.mkdir()
    (safe / "config").write_text("[core]\n\trepositoryformatversion = 0\n\tbare = false\n")
    for name in ["objects", "refs", "packed-refs", "shallow"]:
        source = project / ".git" / name
        if source.exists():
            (safe / name).symlink_to(source, target_is_directory=source.is_dir())
    (safe / "info").mkdir()
    (safe / "info/attributes").symlink_to(project / ".git/info/attributes")
    env = dict(BASE_ENV)
    env.update(
        GIT_DIR=str(project / ".git"),
        GIT_COMMON_DIR=str(safe),
        GIT_WORK_TREE=str(project),
        GIT_OPTIONAL_LOCKS="0",
    )
    operations = [
        ("rev-parse", "--show-toplevel"),
        ("status", "--porcelain"),
        ("diff", "--no-ext-diff", "--no-textconv"),
    ]
    for args in operations:
        result = git(project, *args, env=env)
        describe(" ".join(args), result, marker)
        assert result.returncode == 0 and not marker.exists()
        if args[0] == "status":
            assert " M data.txt" in result.stdout
        if args[0] == "diff":
            assert "+after!" in result.stdout

    linked = base / "linked"
    added = git(project, "worktree", "add", "-b", "probe-linked", str(linked))
    assert added.returncode == 0, added.stderr
    linked_git = pathlib.Path(git(linked, "rev-parse", "--absolute-git-dir").stdout.strip())
    (linked / "data.txt").write_text("linked!\n")
    linked_marker = linked / "filter-ran"
    linked_marker.unlink(missing_ok=True)
    linked_env = dict(env, GIT_DIR=str(linked_git), GIT_WORK_TREE=str(linked))
    for args in operations:
        result = git(linked, *args, env=linked_env)
        describe("linked " + " ".join(args), result, linked_marker)
        assert result.returncode == 0 and not linked_marker.exists()
        if args[0] == "status":
            assert " M data.txt" in result.stdout
        if args[0] == "diff":
            assert "+linked!" in result.stdout

    sha = base / "sha256"
    sha.mkdir()
    initialized = git(sha, "init", "--object-format=sha256")
    if initialized.returncode == 0:
        assert git(sha, "config", "user.email", "test@example.com").returncode == 0
        assert git(sha, "config", "user.name", "Test").returncode == 0
        (sha / "data.txt").write_text("sha256 content\n")
        assert git(sha, "add", "data.txt").returncode == 0
        assert git(sha, "commit", "-m", "initial").returncode == 0
        sha_safe = base / "safe-sha256"
        sha_safe.mkdir()
        (sha_safe / "config").write_text("[core]\n\trepositoryformatversion = 0\n\tbare = false\n")
        for name in ["objects", "refs"]:
            (sha_safe / name).symlink_to(sha / ".git" / name, target_is_directory=True)
        sha_env = dict(
            env, GIT_DIR=str(sha / ".git"),
            GIT_COMMON_DIR=str(sha_safe), GIT_WORK_TREE=str(sha),
        )
        result = git(sha, "rev-parse", "--show-object-format", env=sha_env)
        describe("sha256 minimal-config", result)
        assert result.returncode == 0 and result.stdout.strip() == "sha1"
        (sha_safe / "config").write_text(
            "[core]\n\trepositoryformatversion = 1\n\tbare = false\n"
            "[extensions]\n\tobjectFormat = sha256\n"
        )
        result = git(sha, "status", "--porcelain", env=sha_env)
        describe("sha256 corrected-config status", result)
        assert result.returncode == 0 and result.stdout == ""
        assert git(sha, "rev-parse", "--show-object-format", env=sha_env).stdout.strip() == "sha256"
    else:
        print("sha256 repository support unavailable; format check skipped")

    transformed = base / "transformed"
    transformed.mkdir()
    for args in [
        ("init",),
        ("config", "user.email", "test@example.com"),
        ("config", "user.name", "Test"),
        ("config", "filter.upper.clean", "tr a-z A-Z"),
    ]:
        assert git(transformed, *args).returncode == 0
    (transformed / ".gitattributes").write_text("*.txt filter=upper\n")
    (transformed / "data.txt").write_text("lowercase\n")
    assert git(transformed, "add", ".").returncode == 0
    assert git(transformed, "commit", "-m", "initial").returncode == 0
    changed_time = time.time() + 5
    os.utime(transformed / "data.txt", (changed_time, changed_time))
    transform_safe = base / "safe-transformed"
    transform_safe.mkdir()
    (transform_safe / "config").write_text("[core]\n\trepositoryformatversion = 0\n\tbare = false\n")
    for name in ["objects", "refs"]:
        (transform_safe / name).symlink_to(transformed / ".git" / name, target_is_directory=True)
    transform_env = dict(
        env, GIT_DIR=str(transformed / ".git"),
        GIT_COMMON_DIR=str(transform_safe), GIT_WORK_TREE=str(transformed),
    )
    raw = git(transformed, "status", "--porcelain")
    projected = git(transformed, "status", "--porcelain", env=transform_env)
    print("content-transform status", "normal", repr(raw.stdout), "projected", repr(projected.stdout), "exit", projected.returncode)
    assert raw.returncode == projected.returncode == 0
    assert raw.stdout == "" and " M data.txt" in projected.stdout
