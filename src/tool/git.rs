use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;

use crate::tool::output::cap_text;
use crate::tool::schema::{
    boolean_property, bounded_integer_property, closed_object, parse_args, path_property,
    string_enum_property, string_property,
};
use crate::tool::{ParallelPolicy, ProjectPathResolver, Tool, ToolOutput};

const GIT_TIMEOUT_SECS: u64 = 15;
const MAX_OUTPUT_CHARS: usize = 40_000;

const OPS: &[&str] = &["status", "diff", "log", "blame", "show"];

pub struct GitTool {
    project_root: PathBuf,
}

impl GitTool {
    pub fn new(project_root: PathBuf) -> Self {
        Self { project_root }
    }
}

/// Read-only git operations the tool supports. Snake-case serde so an invalid
/// `op` is rejected at parse time and the dispatch match stays exhaustive. The
/// model-facing schema enum is still driven by [`OPS`].
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
enum GitOp {
    Status,
    Diff,
    Log,
    Blame,
    Show,
}

#[derive(Deserialize)]
struct GitArgs {
    op: GitOp,
    /// File path for diff (restrict to file), blame, or log (restrict to file).
    #[serde(default)]
    path: Option<String>,
    /// Commit ref for diff base, show, or log start. Defaults vary per op.
    #[serde(default)]
    target: Option<String>,
    /// Diff the staged index (`--cached`) instead of the working tree. `diff` op only.
    #[serde(default)]
    staged: bool,
    /// For `diff`, show only `--stat` instead of the stat plus patch body.
    #[serde(default)]
    stat_only: bool,
    /// Max commits for log (default 20).
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    20
}

async fn run_git(args: &[&str], cwd: &Path) -> Result<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(GIT_TIMEOUT_SECS),
        tokio::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("git timed out after {GIT_TIMEOUT_SECS}s"))??;

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    if !out.status.success() {
        anyhow::bail!(git_failure_message(&stdout, &stderr, out.status));
    }

    Ok(stdout)
}

fn git_failure_message(stdout: &str, stderr: &str, status: ExitStatus) -> String {
    let mut message = format!("git failed with {status}");
    let stdout = stdout.trim_end();
    if !stdout.is_empty() {
        message.push_str("\nstdout:\n");
        message.push_str(stdout);
    }
    let stderr = stderr.trim_end();
    if !stderr.is_empty() {
        message.push_str("\nstderr:\n");
        message.push_str(stderr);
    }
    message
}

/// A bare "no diff" misleads the model right after it creates a project: `git
/// diff` never shows untracked files, and (without `--cached`) hides staged
/// ones. On a freshly-scaffolded project every file is untracked, so the diff
/// is empty even though the model just wrote the whole app — it then puzzles
/// over the "empty"/"stale" diff instead of moving on. When the
/// diff comes back empty, name where the changes actually are.
async fn empty_diff_message(
    root: &Path,
    staged: bool,
    scoped_path: Option<&str>,
    scoped_ref: bool,
) -> String {
    if let Some(path) = scoped_path {
        if let Ok(status) = run_git(&["status", "--porcelain", "--", path], root).await
            && status.lines().any(|line| line.starts_with("??"))
        {
            return format!(
                "no diff for {path}: file is untracked — `git diff` never shows untracked files; use `git status` or `git add -N {path}` then diff"
            );
        }
        return "no diff".to_string();
    }
    // A ref-only diff being empty is unambiguous — the model asked a specific
    // question and got a specific empty answer.
    if scoped_ref {
        return "no diff".to_string();
    }
    let Ok(status) = run_git(&["status", "--porcelain"], root).await else {
        return "no diff".to_string();
    };
    let untracked = status.lines().filter(|line| line.starts_with("??")).count();
    // Porcelain column 1 is the index (staged) status; any non-space,
    // non-`?` there means a staged change `git diff` (unstaged) won't show.
    let staged_count = status
        .lines()
        .filter(|line| {
            line.as_bytes()
                .first()
                .is_some_and(|c| !matches!(c, b' ' | b'?'))
        })
        .count();

    let mut hints = Vec::new();
    if !staged && staged_count > 0 {
        hints.push(format!("{staged_count} staged (see `git diff --cached`)"));
    }
    if untracked > 0 {
        hints.push(format!(
            "{untracked} new/untracked file(s) — `git diff` never shows these; \
             `git status` lists them, or `git add -N <path>` then diff to include them"
        ));
    }
    if hints.is_empty() {
        return "no diff (working tree clean)".to_string();
    }
    format!(
        "no {} changes to tracked files — but the tree is NOT clean: {}.",
        if staged { "staged" } else { "unstaged" },
        hints.join("; ")
    )
}

#[async_trait]
impl Tool for GitTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::ReadOnly
    }

    fn name(&self) -> &str {
        "git"
    }

    fn description(&self) -> &str {
        "Run read-only git operations and return compact structured output. Prefer this over \
         bash for status, diff, log, blame, and show — it caps output automatically and \
         requires no extra parsing turns.\n\
         ops: status (working-tree state), diff (uncommitted, staged, or between refs), \
         log (commit history), blame (line attribution for a file), \
         show (diff + metadata for a commit)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        closed_object(
            [
                ("op", string_enum_property("Git operation to run", OPS)),
                (
                    "path",
                    path_property(
                        "File path to scope diff/log/blame to (relative to project root)",
                    ),
                ),
                (
                    "target",
                    string_property(
                        "Commit ref, branch, or diff range (e.g. HEAD~1, main..HEAD). \
                         Diff defaults to uncommitted changes; show defaults to HEAD; \
                         log defaults to current branch.",
                    ),
                ),
                (
                    "staged",
                    boolean_property(
                        "diff op: show the staged index (git diff --cached) instead of the \
                         working tree (default: false).",
                    ),
                ),
                (
                    "stat_only",
                    boolean_property(
                        "diff op: show only the changed-file stat instead of stat plus full patch \
                         body (default: false).",
                    ),
                ),
                (
                    "limit",
                    bounded_integer_property(
                        "Max commits for log (default 20)",
                        Some(1),
                        Some(200),
                    ),
                ),
            ],
            &["op"],
        )
    }

    fn parallel_policy(&self) -> ParallelPolicy {
        ParallelPolicy::AlwaysSafe
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let args: GitArgs = parse_args("git", args)?;

        // A ref or range never begins with `-`; a leading dash means the model
        // leaked git CLI syntax (commonly `-- file1 file2`) into `target`, which
        // git rejects with an opaque "invalid option". Steer it to `path` before
        // shelling out.
        if let Some(target) = args.target.as_deref()
            && target.starts_with('-')
        {
            anyhow::bail!(
                "git target '{target}' starts with '-', which git reads as an option, not a ref or \
                 range. To scope to files use the `path` parameter (one path per call); put only a \
                 ref or range like 'HEAD~1' or 'main..HEAD' in `target`."
            );
        }

        // Validate optional path if provided.
        let scoped_path: Option<String> = if let Some(ref p) = args.path {
            let resolved = ProjectPathResolver::new(&self.project_root)
                .action("git")
                .resolve_existing(p)?;
            // Use root-relative display path for git args.
            let rel = resolved
                .canonical_path()
                .strip_prefix(&self.project_root)
                .unwrap_or(resolved.canonical_path())
                .to_string_lossy()
                .into_owned();
            Some(rel)
        } else {
            None
        };

        let root = &self.project_root;
        let output = match args.op {
            GitOp::Status => {
                let raw = run_git(&["status", "--short", "--branch"], root).await?;
                if raw.trim().is_empty() {
                    "nothing to commit, working tree clean".to_string()
                } else {
                    raw
                }
            }

            GitOp::Diff => {
                let mut cmd = vec!["diff", "--stat"];
                if !args.stat_only {
                    cmd.push("-p");
                }
                if args.staged {
                    cmd.push("--cached");
                }
                let target_str;
                if let Some(ref t) = args.target {
                    target_str = t.clone();
                    cmd.push(&target_str);
                }
                if let Some(ref p) = scoped_path {
                    cmd.push("--");
                    cmd.push(p.as_str());
                }
                let raw = run_git(&cmd, root).await?;
                if raw.trim().is_empty() {
                    empty_diff_message(
                        root,
                        args.staged,
                        scoped_path.as_deref(),
                        args.target.is_some(),
                    )
                    .await
                } else {
                    raw
                }
            }

            GitOp::Log => {
                let limit = format!("-{}", args.limit);
                let mut cmd = vec!["log", "--oneline", "--decorate", limit.as_str()];
                let target_str;
                if let Some(ref t) = args.target {
                    target_str = t.clone();
                    cmd.push(&target_str);
                }
                if let Some(ref p) = scoped_path {
                    cmd.push("--");
                    cmd.push(p.as_str());
                }
                let raw = run_git(&cmd, root).await?;
                if raw.trim().is_empty() {
                    "no commits".to_string()
                } else {
                    raw
                }
            }

            GitOp::Blame => {
                let p = scoped_path
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("blame requires a path"))?;
                let mut cmd = vec!["blame", "--porcelain"];
                let target_str;
                if let Some(ref t) = args.target {
                    target_str = t.clone();
                    cmd.push(&target_str);
                }
                cmd.push("--");
                cmd.push(p);
                let raw = run_git(&cmd, root).await?;
                // Porcelain blame is verbose; condense to author + line.
                condense_blame(&raw)
            }

            GitOp::Show => {
                let target = args.target.as_deref().unwrap_or("HEAD");
                let mut cmd = vec!["show", "--stat", "-p", target];
                if let Some(ref p) = scoped_path {
                    cmd.push("--");
                    cmd.push(p.as_str());
                }
                run_git(&cmd, root).await?
            }
        };

        Ok(ToolOutput::Text(cap_text(
            output,
            MAX_OUTPUT_CHARS,
            "Use a narrower path or target to reduce output.",
        )))
    }
}

/// Condense `git blame --porcelain` output to one line per source line:
/// `<short-hash> (<author> <date>) <line-no>: <content>`
fn condense_blame(raw: &str) -> String {
    let mut out = String::new();
    let mut cur_hash = String::new();
    let mut cur_author = String::new();
    let mut cur_date = String::new();

    for line in raw.lines() {
        if let Some(content) = line.strip_prefix('\t') {
            // Source line: `<TAB>content`
            use std::fmt::Write as _;
            let _ = writeln!(
                out,
                "{} ({} {}) {}",
                &cur_hash[..cur_hash.len().min(8)],
                cur_author,
                cur_date,
                content
            );
        } else if let Some(hash) = line.split_whitespace().next() {
            if hash.len() == 40 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
                cur_hash = hash.to_string();
            } else if let Some(v) = line.strip_prefix("author ") {
                cur_author = v.to_string();
            } else if let Some(v) = line.strip_prefix("author-time ") {
                // Unix timestamp → just show year for compactness
                if let Ok(ts) = v.trim().parse::<i64>() {
                    let year = 1970 + ts / 31_557_600;
                    cur_date = year.to_string();
                }
            }
        }
    }

    if out.is_empty() {
        raw.to_string()
    } else {
        out.trim_end().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::test_utils::TestFixture;
    use serde_json::json;

    fn init_git_repo(root: &Path) {
        for args in [
            vec!["init"],
            vec!["config", "user.email", "test@test.com"],
            vec!["config", "user.name", "Test"],
        ] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(root)
                .output()
                .unwrap();
        }
    }

    fn git_add_commit(root: &Path, msg: &str) {
        for args in [vec!["add", "."], vec!["commit", "-m", msg]] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(root)
                .output()
                .unwrap();
        }
    }

    fn git_stage_all(root: &Path) {
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
    }

    #[tokio::test]
    async fn diff_rejects_cli_syntax_leaked_into_target() {
        // Model packed `-- file1 file2` into `target`; guide it to `path`
        // instead of letting git fail with an opaque "invalid option".
        let fixture = TestFixture::new();
        init_git_repo(&fixture.project_root);
        fixture.create_file("README.md", "hello");
        git_add_commit(&fixture.project_root, "init");

        let tool = GitTool::new(fixture.project_root.clone());
        let err = tool
            .execute(json!({"op": "diff", "target": "-- README.md"}))
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("starts with '-'"), "{err}");
        assert!(err.contains("`path`"), "{err}");
        assert!(!err.contains("invalid option"), "{err}");
    }

    #[tokio::test]
    async fn run_git_reports_nonzero_exit_even_with_stdout() {
        let fixture = TestFixture::new();
        init_git_repo(&fixture.project_root);
        fixture.create_file("README.md", "hello");
        git_add_commit(&fixture.project_root, "init");

        let err = run_git(
            &["rev-parse", "HEAD", "definitely-not-a-ref"],
            &fixture.project_root,
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(err.contains("git failed with"), "{err}");
        assert!(err.contains("exit status"), "{err}");
        assert!(err.contains("stdout:"), "{err}");
        assert!(err.contains("definitely-not-a-ref"), "{err}");
        assert!(err.contains("stderr:"), "{err}");
        assert!(err.contains("unknown revision"), "{err}");
    }

    #[tokio::test]
    async fn status_clean_repo() {
        let fixture = TestFixture::new();
        init_git_repo(&fixture.project_root);
        fixture.create_file("README.md", "hello");
        git_add_commit(&fixture.project_root, "init");

        let tool = GitTool::new(fixture.project_root.clone());
        let result = tool.execute(json!({"op": "status"})).await.unwrap();
        let text = match result {
            ToolOutput::Text(t) => t,
            _ => panic!("expected Text"),
        };
        assert!(
            text.contains("nothing to commit") || text.contains("main") || text.contains("master"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn log_shows_commits() {
        let fixture = TestFixture::new();
        init_git_repo(&fixture.project_root);
        fixture.create_file("a.txt", "a");
        git_add_commit(&fixture.project_root, "first commit");

        let tool = GitTool::new(fixture.project_root.clone());
        let result = tool
            .execute(json!({"op": "log", "limit": 5}))
            .await
            .unwrap();
        let text = match result {
            ToolOutput::Text(t) => t,
            _ => panic!("expected Text"),
        };
        assert!(text.contains("first commit"), "{text}");
    }

    #[tokio::test]
    async fn diff_empty_on_clean_tree() {
        let fixture = TestFixture::new();
        init_git_repo(&fixture.project_root);
        fixture.create_file("a.txt", "a");
        git_add_commit(&fixture.project_root, "init");

        let tool = GitTool::new(fixture.project_root.clone());
        let result = tool.execute(json!({"op": "diff"})).await.unwrap();
        let text = match result {
            ToolOutput::Text(t) => t,
            _ => panic!("expected Text"),
        };
        assert!(text.contains("no diff"), "{text}");
    }

    #[tokio::test]
    async fn staged_diff_shows_index_not_working_tree() {
        let fixture = TestFixture::new();
        init_git_repo(&fixture.project_root);
        fixture.create_file("a.txt", "one\n");
        git_add_commit(&fixture.project_root, "init");

        // Modify and stage the change: working tree == index, so the default
        // (unstaged) diff is empty, but the staged diff shows it.
        fixture.create_file("a.txt", "one\ntwo\n");
        git_stage_all(&fixture.project_root);

        let tool = GitTool::new(fixture.project_root.clone());

        let unstaged = match tool.execute(json!({"op": "diff"})).await.unwrap() {
            ToolOutput::Text(t) => t,
            _ => panic!("expected Text"),
        };
        // Empty unstaged diff, but the change isn't lost — it's staged. The
        // message must point there instead of a bare "no diff" that reads as
        // "nothing changed".
        assert!(unstaged.contains("staged"), "unstaged: {unstaged}");
        assert!(unstaged.contains("--cached"), "unstaged: {unstaged}");

        let staged = match tool
            .execute(json!({"op": "diff", "staged": true}))
            .await
            .unwrap()
        {
            ToolOutput::Text(t) => t,
            _ => panic!("expected Text"),
        };
        assert!(staged.contains("a.txt"), "staged: {staged}");
        assert!(staged.contains("+two"), "staged: {staged}");
    }

    #[tokio::test]
    async fn diff_stat_only_omits_patch_body() {
        let fixture = TestFixture::new();
        init_git_repo(&fixture.project_root);
        fixture.create_file("a.txt", "one\n");
        git_add_commit(&fixture.project_root, "init");
        fixture.create_file("a.txt", "one\ntwo\n");

        let tool = GitTool::new(fixture.project_root.clone());
        let text = match tool
            .execute(json!({"op": "diff", "stat_only": true}))
            .await
            .unwrap()
        {
            ToolOutput::Text(t) => t,
            _ => panic!("expected Text"),
        };

        assert!(text.contains("a.txt"), "{text}");
        assert!(
            !text.contains("+two"),
            "stat-only diff should omit patch body: {text}"
        );
    }

    #[tokio::test]
    async fn empty_diff_names_untracked_files_instead_of_stale_no_diff() {
        // A freshly-scaffolded project is entirely untracked, so
        // `git diff` is empty even though the model just wrote every file. A
        // bare "no diff" read as "nothing changed / stale result" and sent the
        // model puzzling. The message must instead point at the untracked files.
        let fixture = TestFixture::new();
        init_git_repo(&fixture.project_root);
        fixture.create_file("Cargo.toml", "[package]\nname = \"x\"\n");
        fixture.create_file("src/main.rs", "fn main() {}\n");

        let tool = GitTool::new(fixture.project_root.clone());
        let text = match tool.execute(json!({"op": "diff"})).await.unwrap() {
            ToolOutput::Text(t) => t,
            _ => panic!("expected Text"),
        };

        assert!(text.contains("untracked"), "{text}");
        assert!(text.contains("NOT clean"), "{text}");
        // Points at the recovery paths, doesn't just say "no diff".
        assert!(
            text.contains("git status") || text.contains("git add -N"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn scoped_empty_diff_stays_terse() {
        // A diff scoped to a specific path is an unambiguous question; an empty
        // answer should stay "no diff", not get the untracked-file lecture.
        let fixture = TestFixture::new();
        init_git_repo(&fixture.project_root);
        fixture.create_file("a.txt", "a");
        git_add_commit(&fixture.project_root, "init");
        fixture.create_file("untracked.txt", "new");

        let tool = GitTool::new(fixture.project_root.clone());
        let text = match tool
            .execute(json!({"op": "diff", "path": "a.txt"}))
            .await
            .unwrap()
        {
            ToolOutput::Text(t) => t,
            _ => panic!("expected Text"),
        };
        assert_eq!(text, "no diff", "{text}");
    }

    #[tokio::test]
    async fn scoped_empty_diff_on_existing_untracked_file_explains_status() {
        let fixture = TestFixture::new();
        init_git_repo(&fixture.project_root);
        fixture.create_file("a.txt", "a");
        git_add_commit(&fixture.project_root, "init");
        fixture.create_file("untracked.txt", "new");

        let tool = GitTool::new(fixture.project_root.clone());
        let text = match tool
            .execute(json!({"op": "diff", "path": "untracked.txt"}))
            .await
            .unwrap()
        {
            ToolOutput::Text(t) => t,
            _ => panic!("expected Text"),
        };

        assert!(text.contains("untracked"), "{text}");
        assert!(text.contains("git add -N"), "{text}");
    }

    #[tokio::test]
    async fn rejects_path_outside_root() {
        let fixture = TestFixture::new();
        let tool = GitTool::new(fixture.project_root.clone());
        let result = tool
            .execute(json!({"op": "blame", "path": "../etc/passwd"}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_unknown_op() {
        let fixture = TestFixture::new();
        let tool = GitTool::new(fixture.project_root.clone());
        let result = tool.execute(json!({"op": "push"})).await;
        // Serde rejects the unknown enum variant at parse time; the error names
        // the bad value and lists the valid ops.
        let err = result.unwrap_err().to_string();
        assert!(err.contains("push"), "error should name the bad op: {err}");
        assert!(err.contains("status"), "error should list valid ops: {err}");
    }

    #[tokio::test]
    async fn blame_requires_path() {
        let fixture = TestFixture::new();
        let tool = GitTool::new(fixture.project_root.clone());
        let result = tool.execute(json!({"op": "blame"})).await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("path"),
            "error should mention path"
        );
    }
}
