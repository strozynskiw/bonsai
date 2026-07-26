//! Project-awareness for the system prompt: a compact block describing where
//! the agent is running (cwd, OS, project type, git state) plus any steering
//! files (`AGENTS.md` / `CLAUDE.md` / `.cursorrules`) discovered from the
//! working directory upward. Built once at startup in `main.rs` and handed to
//! the `Agent`, which appends it to its persona prompt.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Markdown heading that introduces the per-turn volatile project state (git
/// status, etc.). It marks the boundary between the byte-stable, cacheable
/// prefix of the system prompt and its volatile tail, so the Anthropic provider
/// can land a `cache_control` breakpoint exactly here. Keep this in sync with
/// [`volatile_state_section`]; the provider matches on it verbatim.
pub const VOLATILE_STATE_HEADING: &str = "## Volatile state";
/// Wire marker used for immutable project-state snapshots in stored history.
pub const PROJECT_STATE_MESSAGE_NAME: &str = "bonsai_project_state";
/// Human-readable envelope around a project-state snapshot. Carries the
/// `Harness note:` provenance convention in the text itself: the snapshot rides
/// as a named user-role message for cache reasons, but strict chat templates
/// (Qwen-class) drop the wire `name`, and models then read the bare user turn
/// as the human speaking — burning thinking rounds on "the user sent an empty
/// message" / "the user's volatile state shows…" misattributions observed live.
pub const PROJECT_STATE_UPDATE_PREFIX: &str =
    "Harness note: automated project-state update (not a user message; continue the task):";
/// Envelope used by earlier releases; resumed sessions still carry snapshots
/// with this prefix, so matchers must accept both.
pub const LEGACY_PROJECT_STATE_UPDATE_PREFIX: &str = "Context update for the request above:";
/// Snapshot body emitted when prior volatile state is no longer active.
pub const PROJECT_STATE_CLEARED_BODY: &str = "## Volatile state\nNo volatile project-state advisories are active; earlier snapshots are historical only.";

/// Steering file names, in priority order within a directory.
const STEERING_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md", ".cursorrules"];
/// Cap each steering file so a long one can't blow up the prompt budget.
const MAX_STEERING_BYTES: usize = 16 * 1024;
const SMOL_STEERING_BYTES: usize = 2 * 1024;
/// Cap how far up the tree we walk looking for steering files.
const MAX_PARENT_DEPTH: usize = 16;

/// Structured project-context pieces used by prompt construction and `/ctx`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectContextSnapshot {
    pub environment: String,
    pub volatile_state: String,
    pub steering_files: Vec<SteeringFileContext>,
    /// Ranked, token-budgeted repository map. Empty when unavailable
    /// (non-Rust project) or not built (eval fixtures). Lives in the cacheable
    /// prefix because it is byte-stable for the session.
    pub repo_map: String,
    /// Byte-stable index of available skills, pre-rendered as a cacheable
    /// prefix section. Empty until populated at startup. Skill bodies
    /// load on demand into the conversation tail, so this index never churns the
    /// prompt cache.
    pub skills_index: String,
    /// The user-provided (non-built-in) subset of the skills index, rendered by
    /// [`render_smol`](Self::render_smol) only: SMOL keeps the user's own
    /// skills discoverable while built-in guidance stays out of its
    /// deliberately tiny prompt budget. Byte-stable like `skills_index`.
    pub smol_skills_index: String,
    /// Byte-stable index of available subagents, pre-rendered as a
    /// cacheable prefix section. Empty until populated at startup. Same
    /// cache rationale as `skills_index`.
    pub agents_index: String,
    /// Byte-stable index of persistent memory entries, frozen at session
    /// start for diagnostics and memory commands. This is deliberately not
    /// rendered into the system prompt because project-tier memory can come
    /// from an untrusted cloned repository; relevant entries are recalled into
    /// the conversation tail per turn as background data instead.
    pub memory_index: String,
    /// Pre-rendered "files changed since you read them" advisory, refreshed by
    /// the agent before each request. Lives in the volatile tail — never in the
    /// cacheable prefix, and never rewritten into historical tool messages —
    /// so flagging a stale read costs no cached bytes.
    ///  Empty when every tracked read is still
    /// fresh.
    pub stale_read_advisory: String,
    /// Pre-rendered "other live bonsai sessions" block (peers P4): ids, titles,
    /// state, claims, and pending wake relationships. Same volatile-tail cache
    /// rationale as the stale-read advisory; rendered without timestamps so it
    /// only changes when coordination state changes. Empty when no live peers.
    pub peer_status: String,
}

impl ProjectContextSnapshot {
    pub fn render(&self) -> String {
        let mut sections = vec![self.cacheable_prefix()];
        let volatile = self.volatile_tail();
        if !volatile.is_empty() {
            sections.push(volatile);
        }
        sections.join("\n\n")
    }

    /// Remove repository-authored steering instructions from the cacheable
    /// system prompt. Used before workspace trust is granted: the user can
    /// still read those files through ordinary tools, where they arrive as
    /// data, but opening a repository must not promote them to instructions.
    #[must_use]
    pub fn restrict_untrusted_workspace(mut self) -> Self {
        self.steering_files.clear();
        self.environment.push_str(
            "\n- workspace trust: restricted (project steering, hooks, MCP, skills, and agents are inert until trusted)",
        );
        self
    }

    pub fn render_smol(&self) -> String {
        let mut sections = vec![self.cacheable_prefix_smol()];
        let volatile = self.volatile_tail();
        if !volatile.trim().is_empty() {
            sections.push(volatile);
        }
        sections.join("\n\n")
    }

    /// The byte-stable project context used by the small-model system prompt.
    /// Volatile state is delivered as append-only conversation data by the
    /// agent, never by rewriting this prefix.
    pub fn cacheable_prefix_smol(&self) -> String {
        let mut sections = vec![self.environment.clone()];
        if !self.steering_files.is_empty() {
            let blocks = self
                .steering_files
                .iter()
                .map(SteeringFileContext::render_smol)
                .collect::<Vec<_>>()
                .join("\n\n");
            sections.push(format!(
                "## Project instructions\nFollow these steering files (most specific first, capped for SMOL):\n\n{blocks}"
            ));
        }
        if !self.smol_skills_index.trim().is_empty() {
            sections.push(self.smol_skills_index.clone());
        }
        sections.join("\n\n")
    }

    pub fn cacheable_prefix(&self) -> String {
        let mut sections = vec![self.environment.clone()];
        if !self.steering_files.is_empty() {
            let blocks = self
                .steering_files
                .iter()
                .map(SteeringFileContext::render)
                .collect::<Vec<_>>()
                .join("\n\n");
            sections.push(format!(
                "## Project instructions\nFollow these steering files (most specific first):\n\n{blocks}"
            ));
        }
        if !self.skills_index.trim().is_empty() {
            sections.push(self.skills_index.clone());
        }
        if !self.agents_index.trim().is_empty() {
            sections.push(self.agents_index.clone());
        }
        if !self.repo_map.trim().is_empty() {
            sections.push(self.repo_map.clone());
        }
        sections.join("\n\n")
    }

    /// The per-turn volatile section: git state, the stale-read advisory, and
    /// the peer-status block, always introduced by [`VOLATILE_STATE_HEADING`]
    /// so the provider transports can split it out of the cached region no
    /// matter which parts are present.
    pub fn volatile_tail(&self) -> String {
        let extras = [&self.stale_read_advisory, &self.peer_status]
            .into_iter()
            .filter(|part| !part.is_empty())
            .cloned()
            .collect::<Vec<_>>();
        match (self.volatile_state.is_empty(), extras.is_empty()) {
            (true, true) => String::new(),
            (false, true) => self.volatile_state.clone(),
            (true, false) => format!("{VOLATILE_STATE_HEADING}\n{}", extras.join("\n\n")),
            (false, false) => {
                format!("{}\n\n{}", self.volatile_state, extras.join("\n\n"))
            }
        }
    }

    /// Attach a repository map, returning `self` so it chains off the
    /// `project_context_snapshot` constructors at the call site.
    #[must_use]
    pub fn with_repo_map(mut self, repo_map: String) -> Self {
        self.repo_map = repo_map;
        self
    }

    /// Attach a pre-rendered skills index, returning `self` so it chains off the
    /// `project_context_snapshot` constructors.
    #[must_use]
    pub fn with_skills_index(mut self, skills_index: String) -> Self {
        self.skills_index = skills_index;
        self
    }

    /// Attach the pre-rendered user-skills-only index that SMOL mode renders,
    /// returning `self` so it chains off the `project_context_snapshot`
    /// constructors.
    #[must_use]
    pub fn with_smol_skills_index(mut self, smol_skills_index: String) -> Self {
        self.smol_skills_index = smol_skills_index;
        self
    }

    /// Attach a pre-rendered subagents index, returning `self` so it chains off
    /// the `project_context_snapshot` constructors.
    #[must_use]
    pub fn with_agents_index(mut self, agents_index: String) -> Self {
        self.agents_index = agents_index;
        self
    }

    /// Attach a pre-rendered memory index for diagnostics and memory commands,
    /// returning `self` so it chains off the `project_context_snapshot`
    /// constructors. The index is not rendered into system prompt text.
    #[must_use]
    pub fn with_memory_index(mut self, memory_index: String) -> Self {
        self.memory_index = memory_index;
        self
    }
}

/// One steering file discovered for the project-context block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteeringFileContext {
    pub name: String,
    pub directory: PathBuf,
    pub body: String,
    pub truncated: bool,
}

impl SteeringFileContext {
    pub fn render(&self) -> String {
        format!(
            "### {} ({})\n{}",
            self.name,
            self.directory.display(),
            self.body
        )
    }

    pub fn render_smol(&self) -> String {
        format!(
            "### {} ({})\n{}",
            self.name,
            self.directory.display(),
            truncate_chars(&self.body, SMOL_STEERING_BYTES, "\n…(truncated for SMOL)")
        )
    }
}

/// Collect structured project-context contributors for `root`.
pub fn project_context_snapshot(root: &Path) -> ProjectContextSnapshot {
    let env = project_environment_lines(root);
    let volatile = git_summary(root, None);

    ProjectContextSnapshot {
        environment: format!("## Environment\n{}", env.join("\n")),
        volatile_state: volatile_state_section(volatile),
        steering_files: collect_steering_files(root),
        repo_map: String::new(),
        skills_index: String::new(),
        smol_skills_index: String::new(),
        agents_index: String::new(),
        memory_index: String::new(),
        stale_read_advisory: String::new(),
        peer_status: String::new(),
    }
}

/// Collect project context for an isolated fixture root without walking into a
/// parent repository or parent steering files.
pub fn isolated_project_context_snapshot(root: &Path) -> ProjectContextSnapshot {
    let env = project_environment_lines(root);
    let volatile = if root.join(".git").exists() {
        git_summary(root, None)
    } else {
        Vec::new()
    };

    ProjectContextSnapshot {
        environment: format!("## Environment\n{}", env.join("\n")),
        volatile_state: volatile_state_section(volatile),
        steering_files: collect_steering_file_in_dir(root).into_iter().collect(),
        repo_map: String::new(),
        skills_index: String::new(),
        smol_skills_index: String::new(),
        agents_index: String::new(),
        memory_index: String::new(),
        stale_read_advisory: String::new(),
        peer_status: String::new(),
    }
}

/// Recompute the volatile project-state section (git branch/status) for `root`.
/// Byte-comparable to `project_context_snapshot(root).volatile_state` when
/// `baseline` is `None`, so a live session can refresh the frozen session-start
/// snapshot without changing the cacheable prefix. Paths already present in a
/// supplied baseline are annotated as pre-existing WIP the model must edit on
/// top of rather than reconcile. Returns an empty string outside a git
/// repository (see [`git_summary`]).
pub fn recompute_volatile_state_with_baseline(
    root: &Path,
    baseline: Option<&BTreeSet<String>>,
) -> String {
    volatile_state_section(git_summary(root, baseline))
}

/// The set of paths `git status --porcelain` reports as dirty in `root`.
/// Empty outside a git repository. Used to snapshot the run-start baseline
/// for [`recompute_volatile_state_with_baseline`].
pub fn dirty_worktree_paths(root: &Path) -> BTreeSet<String> {
    git(root, &["status", "--porcelain"])
        .map(|status| porcelain_dirty_paths(&status).into_iter().collect())
        .unwrap_or_default()
}

/// Parse the paths out of `git status --porcelain` output. Rename lines
/// (`R  old -> new`) take the new side — that is the path present on disk.
fn porcelain_dirty_paths(status: &str) -> Vec<String> {
    status
        .lines()
        .filter(|line| line.len() > 3)
        .map(|line| {
            let path = &line[3..];
            let path = path.rsplit(" -> ").next().unwrap_or(path);
            path.trim().trim_matches('"').to_string()
        })
        .filter(|path| !path.is_empty())
        .collect()
}

fn volatile_state_section(lines: Vec<String>) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        format!("{VOLATILE_STATE_HEADING}\n{}", lines.join("\n"))
    }
}

fn project_environment_lines(root: &Path) -> Vec<String> {
    let mut env = vec![
        format!("- cwd: {}", root.display()),
        format!(
            "- platform: {} ({})",
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
    ];
    if let Some(kind) = project_type(root) {
        env.push(format!("- project: {kind}"));
    }
    if let Ok(paths) = crate::storage::BonsaiPaths::discover() {
        env.push(format!(
            "- local data: {} (key tables: sessions, usage_turns, tool_calls, inspection_events, verification_runs, self_review_runs, episodes)",
            paths.db_path().display()
        ));
    }
    if root.join("ROADMAP.md").is_file() {
        env.push(format!("- roadmap: {}", root.join("ROADMAP.md").display()));
    }
    env
}

/// Detect the project's ecosystems from well-known marker files in `root`.
pub(crate) fn detect_project_ecosystems(root: &Path) -> Vec<&'static str> {
    const MARKERS: &[(&str, &str)] = &[
        ("Cargo.toml", "Rust"),
        ("package.json", "Node/JavaScript"),
        ("pyproject.toml", "Python"),
        ("setup.py", "Python"),
        ("requirements.txt", "Python"),
        ("go.mod", "Go"),
        ("pom.xml", "Java/Maven"),
        ("build.gradle", "Java/Gradle"),
        ("Gemfile", "Ruby"),
        ("composer.json", "PHP"),
    ];
    let mut kinds: Vec<&'static str> = Vec::new();
    for (marker, kind) in MARKERS {
        if root.join(marker).exists() && !kinds.contains(kind) {
            kinds.push(kind);
        }
    }
    kinds
}

/// Detect the project's ecosystem(s) from well-known marker files in `root`.
fn project_type(root: &Path) -> Option<String> {
    let kinds = detect_project_ecosystems(root);
    (!kinds.is_empty()).then(|| kinds.join(", "))
}

/// Summarize git state as prompt lines: branch, uncommitted-change count, and
/// the most recent commit subject. Empty when `root` is not a git repo.
///
/// When `baseline` is set (the dirty paths at run start), the uncommitted-change
/// line splits pre-existing WIP from changes made during this run and tells the
/// model the baseline is not its concern. A session looped for a long time
/// "assessing uncommitted peer changes" because a bare count gave it no way to
/// tell unrelated WIP from its own edits. `None` treats every dirty path as
/// baseline, keeping the snapshot constructors byte-identical to a
/// baseline-free recompute.
fn git_summary(root: &Path, baseline: Option<&BTreeSet<String>>) -> Vec<String> {
    let branch =
        git(root, &["rev-parse", "--abbrev-ref", "HEAD"]).filter(|b| !b.is_empty() && b != "HEAD");
    let Some(branch) = branch else {
        return Vec::new();
    };

    let mut lines = vec![format!("- git branch: {branch}")];

    if let Some(status) = git(root, &["status", "--porcelain"]) {
        let dirty_paths = porcelain_dirty_paths(&status);
        lines.push(uncommitted_changes_line(&dirty_paths, baseline));
    }
    if let Some(last) = git(root, &["log", "-1", "--pretty=%s"]).filter(|s| !s.is_empty()) {
        lines.push(format!("- last commit: {last}"));
    }
    lines
}

const BASELINE_WIP_NOTE: &str = "pre-existing baseline WIP: present before this run, \
     not produced by it — edit on top; do not reconcile, protect, or re-verify them";

fn uncommitted_changes_line(dirty_paths: &[String], baseline: Option<&BTreeSet<String>>) -> String {
    let dirty = dirty_paths.len();
    let plural = if dirty == 1 { "" } else { "s" };
    if dirty == 0 {
        return format!("- git: {dirty} uncommitted change{plural}");
    }
    let baseline_count = match baseline {
        None => dirty,
        Some(baseline) => dirty_paths
            .iter()
            .filter(|path| baseline.contains(*path))
            .count(),
    };
    let new_count = dirty - baseline_count;
    match (baseline_count, new_count) {
        (0, _) => format!("- git: {dirty} uncommitted change{plural} (all made during this run)"),
        (_, 0) => format!("- git: {dirty} uncommitted change{plural} (all {BASELINE_WIP_NOTE})"),
        _ => format!(
            "- git: {dirty} uncommitted change{plural} ({baseline_count} {BASELINE_WIP_NOTE}; \
             {new_count} changed during this run)"
        ),
    }
}

/// Run a git subcommand in `root`, returning trimmed stdout on success.
fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn collect_steering_files(root: &Path) -> Vec<SteeringFileContext> {
    let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf());
    let mut files = Vec::new();
    let mut dir = Some(root);
    let mut depth = 0;

    while let Some(current) = dir {
        if depth > MAX_PARENT_DEPTH {
            break;
        }
        if let Some(file) = collect_steering_file_in_dir(current) {
            files.push(file);
        }
        // Stop after scanning the home directory; don't crawl above it.
        if home.as_deref() == Some(current) {
            break;
        }
        dir = current.parent();
        depth += 1;
    }
    files
}

fn collect_steering_file_in_dir(dir: &Path) -> Option<SteeringFileContext> {
    for name in STEERING_FILES {
        let path = dir.join(name);
        let Ok(raw) = std::fs::read_to_string(path) else {
            continue;
        };
        let text = raw.trim();
        if text.is_empty() {
            continue;
        }
        let (body, truncated) = truncate_steering(text);
        return Some(SteeringFileContext {
            name: (*name).to_string(),
            directory: dir.to_path_buf(),
            body,
            truncated,
        });
    }
    None
}

fn truncate_steering(text: &str) -> (String, bool) {
    if text.len() <= MAX_STEERING_BYTES {
        return (text.to_string(), false);
    }
    let mut end = MAX_STEERING_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}\n…(truncated)", &text[..end]), true)
}

fn truncate_chars(text: &str, max_chars: usize, marker: &str) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated = text.chars().take(max_chars).collect::<String>();
    truncated.push_str(marker);
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_type_detects_rust_and_node() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        let kind = project_type(dir.path()).unwrap();
        assert!(kind.contains("Rust"));
        assert!(kind.contains("Node"));
    }

    #[test]
    fn project_type_none_for_bare_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(project_type(dir.path()).is_none());
    }

    #[test]
    fn context_includes_cwd_and_platform() {
        let dir = tempfile::TempDir::new().unwrap();
        let context = project_context_snapshot(dir.path()).render();
        assert!(context.contains("## Environment"));
        assert!(context.contains("cwd:"));
        assert!(context.contains("platform:"));
        assert!(context.contains("local data:"));
        assert!(context.contains("usage_turns"));
    }

    #[test]
    fn collect_steering_reads_nearest_first() {
        let dir = tempfile::TempDir::new().unwrap();
        let nested = dir.path().join("sub");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "root rules").unwrap();
        std::fs::write(nested.join("AGENTS.md"), "nested rules").unwrap();

        let steering = project_context_snapshot(&nested)
            .steering_files
            .iter()
            .map(SteeringFileContext::render)
            .collect::<Vec<_>>()
            .join("\n\n");
        let nested_pos = steering.find("nested rules").unwrap();
        let root_pos = steering.find("root rules").unwrap();
        assert!(
            nested_pos < root_pos,
            "nearest steering file should appear first"
        );
    }

    #[test]
    fn volatile_state_does_not_change_cacheable_prefix_bytes() {
        let stable = ProjectContextSnapshot {
            environment: "## Environment\n- cwd: /repo\n- platform: test\n- project: Rust"
                .to_string(),
            volatile_state: "## Volatile state\n- git branch: main\n- git: 0 uncommitted changes"
                .to_string(),
            steering_files: vec![SteeringFileContext {
                name: "AGENTS.md".to_string(),
                directory: PathBuf::from("/repo"),
                body: "stable rules".to_string(),
                truncated: false,
            }],
            repo_map: "## Repository map\nsrc/lib.rs\n  fn entry".to_string(),
            skills_index: String::new(),
            smol_skills_index: String::new(),
            agents_index: String::new(),
            memory_index: String::new(),
            stale_read_advisory: String::new(),
            peer_status: String::new(),
        };
        let changed = ProjectContextSnapshot {
            volatile_state:
                "## Volatile state\n- git branch: feature\n- git: 4 uncommitted changes\n- last commit: work"
                    .to_string(),
            ..stable.clone()
        };

        assert_ne!(stable.render(), changed.render());
        assert_eq!(
            stable.cacheable_prefix().len(),
            changed.cacheable_prefix().len()
        );

        // The stale-read advisory is volatile too: it must never leak into the
        // cacheable prefix bytes.
        let advisory = ProjectContextSnapshot {
            stale_read_advisory:
                "### Files changed since you read them\n- src/foo.rs — changed after your read"
                    .to_string(),
            ..stable.clone()
        };
        assert_ne!(stable.render(), advisory.render());
        assert_eq!(stable.cacheable_prefix(), advisory.cacheable_prefix());
        assert!(advisory.volatile_tail().contains("Files changed"));

        // The peer-status block is the third volatile part: same rules, and it
        // composes with the other two.
        let with_peers = ProjectContextSnapshot {
            peer_status: "### Other bonsai sessions in this project\n- #45 \"tests\"".to_string(),
            ..advisory.clone()
        };
        assert_eq!(stable.cacheable_prefix(), with_peers.cacheable_prefix());
        let tail = with_peers.volatile_tail();
        assert!(tail.contains("Files changed"), "{tail}");
        assert!(tail.contains("Other bonsai sessions"), "{tail}");
        assert!(tail.starts_with(VOLATILE_STATE_HEADING), "{tail}");

        // Peer status alone (no git state, no advisory) still brings the
        // heading so it stays outside every transport's cached region.
        let peers_only = ProjectContextSnapshot {
            volatile_state: String::new(),
            stale_read_advisory: String::new(),
            peer_status: "### Other bonsai sessions in this project\n- #45".to_string(),
            ..stable.clone()
        };
        let tail = peers_only.volatile_tail();
        assert!(tail.starts_with(VOLATILE_STATE_HEADING), "{tail}");
        assert!(tail.contains("#45"), "{tail}");
    }

    #[test]
    fn advisory_alone_still_opens_with_volatile_heading() {
        // The provider transports split the system prompt at
        // `VOLATILE_STATE_HEADING`; outside a git repo `volatile_state` is
        // empty, so the advisory must bring the heading itself to stay outside
        // the cached region.
        let snapshot = ProjectContextSnapshot {
            environment: "## Environment\n- cwd: /repo".to_string(),
            volatile_state: String::new(),
            steering_files: Vec::new(),
            repo_map: String::new(),
            skills_index: String::new(),
            smol_skills_index: String::new(),
            agents_index: String::new(),
            memory_index: String::new(),
            stale_read_advisory:
                "### Files changed since you read them\n- src/foo.rs — changed after your read"
                    .to_string(),
            peer_status: String::new(),
        };

        let tail = snapshot.volatile_tail();
        assert!(tail.starts_with(VOLATILE_STATE_HEADING));
        assert!(
            snapshot
                .render()
                .contains(&format!("\n\n{VOLATILE_STATE_HEADING}\n")),
            "render must keep the split needle intact: {}",
            snapshot.render()
        );
    }

    #[test]
    fn recompute_volatile_state_matches_constructor() {
        // The per-turn refresh (`Agent::refresh_volatile_project_state`) relies on
        // `recompute_volatile_state` producing byte-identical output to the volatile
        // section the constructor builds, so a refresh only ever changes the tail —
        // never the cacheable prefix. A bare temp dir isn't a git repo, so both
        // resolve to an empty volatile section; the point is that they agree.
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(
            recompute_volatile_state_with_baseline(dir.path(), None),
            project_context_snapshot(dir.path()).volatile_state,
        );
    }

    #[test]
    fn porcelain_paths_parse_modified_untracked_and_renames() {
        let status =
            " M src/a.rs\n?? src/new.rs\nR  src/old.rs -> src/renamed.rs\nA  src/added.rs\n";
        assert_eq!(
            porcelain_dirty_paths(status),
            vec![
                "src/a.rs".to_string(),
                "src/new.rs".to_string(),
                "src/renamed.rs".to_string(),
                "src/added.rs".to_string(),
            ]
        );
    }

    #[test]
    fn uncommitted_changes_line_splits_baseline_and_new() {
        let dirty = vec![
            "src/a.rs".to_string(),
            "src/b.rs".to_string(),
            "src/c.rs".to_string(),
        ];
        // No baseline (snapshot constructors, plain recompute): all-baseline.
        let line = uncommitted_changes_line(&dirty, None);
        assert!(
            line.starts_with("- git: 3 uncommitted changes (all pre-existing"),
            "{line}"
        );

        // A baseline covering a subset: split render.
        let baseline: BTreeSet<String> = ["src/a.rs".to_string(), "src/b.rs".to_string()].into();
        let line = uncommitted_changes_line(&dirty, Some(&baseline));
        assert!(line.contains("3 uncommitted changes"), "{line}");
        assert!(line.contains("2 pre-existing baseline WIP"), "{line}");
        assert!(line.contains("1 changed during this run"), "{line}");

        // An empty baseline (clean tree at run start): everything is this run's.
        let line = uncommitted_changes_line(&dirty, Some(&BTreeSet::new()));
        assert!(line.ends_with("(all made during this run)"), "{line}");

        // A clean tree renders exactly the historical line.
        assert_eq!(
            uncommitted_changes_line(&[], Some(&baseline)),
            "- git: 0 uncommitted changes"
        );
    }

    #[test]
    fn baseline_annotation_matches_constructor_and_stays_in_volatile_tail() {
        // In a real repo with dirty files, the constructor (no baseline) and the
        // plain recompute must stay byte-identical — the annotated line included —
        // so a per-turn refresh never perturbs the cacheable prefix.
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(root.join("committed.rs"), "fn main() {}").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        std::fs::write(root.join("baseline.rs"), "old wip").unwrap();

        assert_eq!(
            recompute_volatile_state_with_baseline(root, None),
            project_context_snapshot(root).volatile_state,
        );

        // With the run-start baseline captured, a file dirtied afterwards renders
        // as this run's change — and the annotation lives under the volatile
        // heading, never in the cacheable prefix.
        let baseline = dirty_worktree_paths(root);
        std::fs::write(root.join("fresh.rs"), "new edit").unwrap();
        let volatile = recompute_volatile_state_with_baseline(root, Some(&baseline));
        assert!(volatile.starts_with(VOLATILE_STATE_HEADING), "{volatile}");
        assert!(
            volatile.contains("1 pre-existing baseline WIP"),
            "{volatile}"
        );
        assert!(volatile.contains("1 changed during this run"), "{volatile}");
    }

    #[test]
    fn repo_map_lands_in_cacheable_prefix_not_volatile_tail() {
        let snapshot = ProjectContextSnapshot {
            environment: "## Environment\n- cwd: /repo".to_string(),
            volatile_state: "## Volatile state\n- git branch: main".to_string(),
            steering_files: Vec::new(),
            repo_map: String::new(),
            skills_index: String::new(),
            smol_skills_index: String::new(),
            agents_index: String::new(),
            memory_index: String::new(),
            stale_read_advisory: String::new(),
            peer_status: String::new(),
        }
        .with_repo_map("## Repository map\nsrc/lib.rs\n  fn entry".to_string());

        assert!(snapshot.cacheable_prefix().contains("## Repository map"));
        assert!(!snapshot.volatile_tail().contains("## Repository map"));
        assert!(snapshot.render().contains("## Repository map"));
    }

    #[test]
    fn trusted_resource_indices_land_in_cacheable_prefix_before_repo_map() {
        let snapshot = ProjectContextSnapshot {
            environment: "## Environment\n- cwd: /repo".to_string(),
            volatile_state: "## Volatile state\n- git branch: main".to_string(),
            steering_files: Vec::new(),
            repo_map: String::new(),
            skills_index: String::new(),
            smol_skills_index: String::new(),
            agents_index: String::new(),
            memory_index: String::new(),
            stale_read_advisory: String::new(),
            peer_status: String::new(),
        }
        .with_skills_index("## Skills\n- deploy — ship it".to_string())
        .with_agents_index("## Subagents\n- explore — orient".to_string())
        .with_memory_index("## Memory\n- prefers-tabs — indentation (preference)".to_string())
        .with_repo_map("## Repository map\nsrc/lib.rs".to_string());

        let prefix = snapshot.cacheable_prefix();
        assert!(prefix.contains("## Skills"));
        assert!(prefix.contains("## Subagents"));
        assert!(!prefix.contains("## Memory"));
        assert!(!snapshot.volatile_tail().contains("## Skills"));
        assert!(!snapshot.volatile_tail().contains("## Memory"));
        assert!(!snapshot.render().contains("## Memory"));

        // Order: skills, then subagents, then repo map. Memory is intentionally
        // excluded because project-tier memory can be untrusted repository data.
        let skills = prefix.find("## Skills").unwrap();
        let agents = prefix.find("## Subagents").unwrap();
        let repo = prefix.find("## Repository map").unwrap();
        assert!(skills < agents && agents < repo);
    }

    #[test]
    fn smol_render_omits_generated_indices_and_repo_map() {
        let snapshot = ProjectContextSnapshot {
            environment: "## Environment\n- cwd: /repo".to_string(),
            volatile_state: "## Volatile state\n- git branch: main".to_string(),
            steering_files: vec![SteeringFileContext {
                name: "AGENTS.md".to_string(),
                directory: PathBuf::from("/repo"),
                body: "rules".to_string(),
                truncated: false,
            }],
            repo_map: "## Repository map\nsrc/lib.rs".to_string(),
            skills_index: "## Skills\n- skill".to_string(),
            smol_skills_index: String::new(),
            agents_index: "## Subagents\n- agent".to_string(),
            memory_index: "## Memory\n- fact".to_string(),
            stale_read_advisory: String::new(),
            peer_status: String::new(),
        };

        let rendered = snapshot.render_smol();

        assert!(rendered.contains("## Environment"));
        assert!(rendered.contains("## Volatile state"));
        assert!(rendered.contains("rules"));
        assert!(!rendered.contains("## Repository map"));
        assert!(!rendered.contains("## Skills"));
        assert!(!rendered.contains("## Subagents"));
        assert!(!rendered.contains("## Memory"));
    }

    #[test]
    fn smol_render_keeps_user_skills_index_but_not_full_index() {
        let snapshot = ProjectContextSnapshot {
            environment: "## Environment\n- cwd: /repo".to_string(),
            volatile_state: String::new(),
            steering_files: Vec::new(),
            repo_map: String::new(),
            // Full index carries built-ins; the smol subset only user skills.
            skills_index: "## Skills\n- deploy — ship it\n- rust-writer — builtin".to_string(),
            smol_skills_index: "## Skills\n- deploy — ship it".to_string(),
            agents_index: String::new(),
            memory_index: String::new(),
            stale_read_advisory: String::new(),
            peer_status: String::new(),
        };

        let rendered = snapshot.render_smol();
        assert!(rendered.contains("- deploy — ship it"));
        assert!(!rendered.contains("rust-writer"));
        // The full render is unchanged by the smol subset.
        assert!(snapshot.render().contains("rust-writer"));
    }

    #[test]
    fn smol_render_caps_steering_file_body() {
        let snapshot = ProjectContextSnapshot {
            environment: "## Environment\n- cwd: /repo".to_string(),
            volatile_state: String::new(),
            steering_files: vec![SteeringFileContext {
                name: "AGENTS.md".to_string(),
                directory: PathBuf::from("/repo"),
                body: "x".repeat(SMOL_STEERING_BYTES + 100),
                truncated: false,
            }],
            repo_map: String::new(),
            skills_index: String::new(),
            smol_skills_index: String::new(),
            agents_index: String::new(),
            memory_index: String::new(),
            stale_read_advisory: String::new(),
            peer_status: String::new(),
        };

        let rendered = snapshot.render_smol();

        assert!(rendered.contains("truncated for SMOL"));
        assert!(rendered.len() < snapshot.render().len());
    }

    #[test]
    fn empty_resource_indices_do_not_change_cacheable_prefix_bytes() {
        let base = ProjectContextSnapshot {
            environment: "## Environment\n- cwd: /repo".to_string(),
            volatile_state: "## Volatile state\n- git: clean".to_string(),
            steering_files: Vec::new(),
            repo_map: "## Repository map\nsrc/lib.rs".to_string(),
            skills_index: String::new(),
            smol_skills_index: String::new(),
            agents_index: String::new(),
            memory_index: String::new(),
            stale_read_advisory: String::new(),
            peer_status: String::new(),
        };
        let with_empty = base
            .clone()
            .with_skills_index(String::new())
            .with_agents_index(String::new())
            .with_memory_index(String::new());
        assert_eq!(base.cacheable_prefix(), with_empty.cacheable_prefix());
    }

    #[test]
    fn isolated_context_does_not_read_parent_steering_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let nested = dir.path().join("target/eval/run/worktrees/task");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "parent rules").unwrap();
        std::fs::write(nested.join("AGENTS.md"), "fixture rules").unwrap();

        let context = isolated_project_context_snapshot(&nested).render();

        assert!(context.contains("fixture rules"));
        assert!(!context.contains("parent rules"));
    }

    #[test]
    fn restricted_workspace_removes_project_steering_instructions() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "run project instructions").unwrap();

        let snapshot = isolated_project_context_snapshot(dir.path()).restrict_untrusted_workspace();
        let rendered = snapshot.render();

        assert!(snapshot.steering_files.is_empty());
        assert!(!rendered.contains("run project instructions"));
        assert!(rendered.contains("workspace trust: restricted"));
    }

    #[test]
    fn collect_steering_empty_when_none() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(
            project_context_snapshot(dir.path())
                .steering_files
                .is_empty()
        );
    }

    #[test]
    fn truncate_steering_caps_long_files() {
        let long = "x".repeat(MAX_STEERING_BYTES + 100);
        let (out, truncated) = truncate_steering(&long);
        assert!(truncated);
        assert!(out.len() < long.len());
        assert!(out.ends_with("…(truncated)"));
    }
}
