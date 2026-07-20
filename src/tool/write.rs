use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;

use crate::diff::build_file_diff;
use crate::tool::file_mutation::{
    FileMutationAction, FileMutationContext, FileWriteRequest, ParentDirs, WriteContentContext,
    WritePrecondition, build_mutation_output, noop_mutation_output, reject_replayed_elision_marker,
    write_effects,
};
use crate::tool::schema::{object, parse_args, path_property, string_property};
use crate::tool::{ActionPlan, ActionPolicy, ReadTracker, Tool, ToolOutput, WorkspaceLockContext};
use crate::yolo::YoloMode;

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

pub struct WriteTool {
    ctx: FileMutationContext,
}

impl WriteTool {
    #[cfg(test)]
    pub fn new(project_root: PathBuf, read_tracker: ReadTracker) -> Self {
        Self::with_yolo_mode(project_root, read_tracker, YoloMode::new())
    }

    #[cfg(test)]
    pub fn with_yolo_mode(
        project_root: PathBuf,
        read_tracker: ReadTracker,
        yolo_mode: YoloMode,
    ) -> Self {
        Self::with_hooks_and_locks(
            project_root.clone(),
            read_tracker,
            yolo_mode,
            std::sync::Arc::new(crate::hooks::HookEngine::disabled()),
            WorkspaceLockContext::disabled(project_root),
            ActionPolicy::testing(),
        )
    }

    pub fn with_hooks_and_locks(
        project_root: PathBuf,
        read_tracker: ReadTracker,
        yolo_mode: YoloMode,
        hooks: std::sync::Arc<crate::hooks::HookEngine>,
        workspace_locks: WorkspaceLockContext,
        policy: ActionPolicy,
    ) -> Self {
        Self {
            ctx: FileMutationContext::new_with_locks(
                project_root,
                read_tracker,
                yolo_mode,
                hooks,
                workspace_locks,
                policy,
            ),
        }
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::SelfAuthorized
    }

    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Create or overwrite a file with the given content. Creates parent directories automatically. Requires reading the file first before overwriting existing files. To modify an existing file, prefer the edit tool with targeted replacements; use write only for new files or a deliberate full rewrite — rewriting an unchanged file wastes output tokens and invalidates the prompt cache."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object(
            [
                (
                    "path",
                    path_property("File path relative to project root, or absolute path inside it"),
                ),
                (
                    "content",
                    string_property("Full content to write to the file"),
                ),
            ],
            &["path", "content"],
        )
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let args: WriteArgs = parse_args("write tool", args)?;
        reject_replayed_elision_marker(&args.content, &args.path)?;
        let guard = self.ctx.guard(FileMutationAction::Write);
        let workspace_lock = guard.acquire_write_lock().await?;
        let resolved_path = guard.resolve_path(&args.path)?;

        let old_content_opt: Option<String> =
            if let Some(canonical) = resolved_path.canonical_path.as_deref() {
                let old_content = guard.read_existing_content(canonical, &args.path).await?;
                if old_content == args.content {
                    return Ok(noop_mutation_output(&args.path));
                }
                Some(old_content)
            } else {
                None
            };

        let effects = write_effects(old_content_opt.as_deref(), &args.content);
        let plan = ActionPlan::file_mutation(
            "write",
            std::iter::once(&resolved_path.mutation_target),
            effects,
        );
        let diff = build_file_diff(args.path.clone(), old_content_opt.as_deref(), &args.content);
        guard.authorize(&plan).await?;

        let write = FileWriteRequest::new(
            &resolved_path.path,
            &args.content,
            old_content_opt
                .as_deref()
                .map_or(WritePrecondition::Absent, WritePrecondition::Exact),
            WriteContentContext::WriteFile,
            ParentDirs::Create,
        )
        .with_workspace_lock(workspace_lock.as_ref())
        .with_project_relative_path(resolved_path.project_relative_path.as_deref())
        .with_read_tracking_path(resolved_path.canonical_path.as_deref());
        guard
            .write_content("write", &args.path, &diff, write)
            .await?;

        let bytes = args.content.len();
        Ok(build_mutation_output(diff, |stats| {
            format!(
                "File successfully written: {}\nSize: {} bytes\nLines added: {}, Lines removed: {}\n\
                     (The file is saved. Later turns may show this content collapsed to save space \
                     — the file on disk is unchanged.)",
                args.path, bytes, stats.added, stats.removed
            )
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::{
        Config, ConfigSource, FailureBehavior, HookAction, HookDef, HookEvent, HookMatcher,
    };
    use crate::extension::status::ExtensionRegistry;
    use crate::interaction::InteractionService;
    use crate::output::SharedSink;
    use crate::permissions::PermissionManager;
    use crate::provider::{Provider, StreamedResponse};
    use crate::tool::test_utils::TestFixture;
    use crate::yolo::YoloMode;
    use async_trait::async_trait;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    struct RemovedSchemaGate;

    #[async_trait]
    impl Provider for RemovedSchemaGate {
        async fn chat_stream(
            &self,
            messages: &[async_openai::types::chat::ChatCompletionRequestMessage],
            _tools: &[async_openai::types::chat::ChatCompletionTool],
            _cancellation_token: CancellationToken,
            _sink: SharedSink,
        ) -> crate::provider::ProviderResult<StreamedResponse> {
            let prompt = serde_json::to_string(messages).unwrap_or_default();
            let content = if prompt.contains("-CREATE TABLE important_records") {
                r#"{"decision":"block","reason":"schema removal detected"}"#
            } else {
                r#"{"decision":"allow","reason":"no removal"}"#
            };
            Ok(StreamedResponse {
                content: content.to_string(),
                ..StreamedResponse::default()
            })
        }

        async fn list_models(&self) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    fn balanced_tool(fixture: &TestFixture) -> WriteTool {
        let yolo_mode = YoloMode::with_level(crate::tool::ApprovalLevel::Balanced);
        let policy = ActionPolicy::new(
            PermissionManager::memory_only(),
            Arc::new(InteractionService::noninteractive()),
            yolo_mode.clone(),
        );
        WriteTool::with_hooks_and_locks(
            fixture.project_root.clone(),
            fixture.read_tracker.clone(),
            yolo_mode,
            Arc::new(crate::hooks::HookEngine::disabled()),
            WorkspaceLockContext::disabled(fixture.project_root.clone()),
            policy,
        )
    }

    #[tokio::test]
    async fn test_write_new_file() {
        let fixture = TestFixture::new();

        let tool = WriteTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "new.txt",
                "content": "Hello, world!"
            }))
            .await
            .unwrap();

        let output = match result {
            ToolOutput::Edit { summary, .. } => summary,
            _ => panic!("Expected Edit output"),
        };

        assert!(output.contains("File successfully written"));
        assert!(output.contains("Lines added: 1, Lines removed: 0"));
        // Preempt the misread with positive framing: the success
        // result tells the model the disk file remains saved after elision.
        assert!(
            output.contains("the file on disk is unchanged"),
            "write result must preempt the elision misread: {output}"
        );

        let file_path = fixture.project_root.join("new.txt");
        assert!(file_path.exists());
        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "Hello, world!");
    }

    #[tokio::test]
    async fn write_rejects_replayed_elision_marker_as_content() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("kept.rs", "fn real() {}\n");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = WriteTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        // Both the legacy and current marker spellings must be refused.
        for marker in [
            // Legacy short form.
            "<omitted: 9053 bytes, 296 lines>",
            // Prior verbose form (may still appear in old sessions' history).
            "<content elided: 9053 bytes, 296 lines — full text lives in the file on disk; re-read it if needed>",
            // Shortened form a model reconstructed and replayed.
            "<content elided: 31310 bytes, 572 lines>",
            // Current self-describing form (~230 bytes — must still be caught).
            "<content elided: your write SUCCEEDED and the real body (9053 bytes, 296 lines) is on disk, not lost — this is only a context-saving view of what you sent. Do not resend this or rewrite the file; re-read it if you need the content.>",
        ] {
            let err = tool
                .execute(json!({"path": "kept.rs", "content": marker}))
                .await
                .expect_err("a replayed elision marker must never overwrite a file");
            assert!(
                err.to_string().contains("context-elision placeholder"),
                "{err:#}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(file_path).unwrap(),
            "fn real() {}\n",
            "the file must be untouched"
        );
    }

    #[tokio::test]
    async fn test_write_new_file_with_absolute_project_path() {
        let fixture = TestFixture::new();
        let file_path = fixture.project_root.join("absolute-new.txt");
        let path_arg = file_path.display().to_string();

        let tool = WriteTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        tool.execute(json!({
            "path": path_arg,
            "content": "absolute content"
        }))
        .await
        .unwrap();

        assert!(file_path.exists());
        assert_eq!(
            std::fs::read_to_string(file_path).unwrap(),
            "absolute content"
        );
    }

    #[tokio::test]
    async fn test_write_trailing_newline_counts_visible_lines() {
        let fixture = TestFixture::new();

        let tool = WriteTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "new.txt",
                "content": "one\ntwo\n"
            }))
            .await
            .unwrap();

        let output = match result {
            ToolOutput::Edit { summary, .. } => summary,
            _ => panic!("Expected Edit output"),
        };

        assert!(output.contains("Lines added: 2, Lines removed: 0"));
    }

    #[tokio::test]
    async fn test_write_empty_file() {
        let fixture = TestFixture::new();

        let tool = WriteTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        tool.execute(json!({
            "path": "empty.txt",
            "content": ""
        }))
        .await
        .unwrap();

        let file_path = fixture.project_root.join("empty.txt");
        assert!(file_path.exists());
        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "");
    }

    #[tokio::test]
    async fn test_write_overwrite_after_read() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "Old content");
        let canonical_path = file_path.canonicalize().unwrap();

        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = WriteTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "content": "New content"
            }))
            .await
            .unwrap();

        let output = match result {
            ToolOutput::Edit { summary, .. } => summary,
            _ => panic!("Expected Edit output"),
        };

        assert!(output.contains("File successfully written"));
        assert!(output.contains("Lines added: 1, Lines removed: 1"));
        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "New content");
    }

    #[tokio::test]
    async fn blocking_llm_file_hook_sees_removed_line_and_vetoes_write() {
        let fixture = TestFixture::new();
        let original = "CREATE TABLE important_records (id INTEGER);\n";
        let file_path = fixture.create_file("migration.sql", original);
        fixture
            .read_tracker
            .mark_read(&file_path.canonicalize().unwrap())
            .await;
        let hook = HookDef {
            name: "migration-review".to_string(),
            event: HookEvent::PreFileWrite,
            matcher: HookMatcher::default(),
            action: HookAction::LlmPrompt {
                prompt: "Review the proposed migration:\n{{diff}}".to_string(),
            },
            timeout_secs: 5,
            blocking: true,
            on_failure: FailureBehavior::Block,
            capabilities: Vec::new(),
            enabled: true,
        };
        let config = Config {
            hooks: vec![(hook, ConfigSource::Global)],
            ..Config::default()
        };
        let hooks = Arc::new(
            crate::hooks::HookEngine::build(
                &config,
                fixture.project_root.clone(),
                PermissionManager::memory_only_hooks(),
                Arc::new(InteractionService::noninteractive()),
                Arc::new(ExtensionRegistry::new()),
                Some(Arc::new(RemovedSchemaGate)),
            )
            .await,
        );
        let tool = WriteTool::with_hooks_and_locks(
            fixture.project_root.clone(),
            fixture.read_tracker.clone(),
            YoloMode::new(),
            hooks,
            WorkspaceLockContext::disabled(fixture.project_root.clone()),
            ActionPolicy::testing(),
        );

        let error = tool
            .execute(json!({"path": "migration.sql", "content": "-- removed\n"}))
            .await
            .expect_err("the LLM hook should veto the removed schema line");

        assert!(
            error.to_string().contains("schema removal detected"),
            "{error:#}"
        );
        assert_eq!(std::fs::read_to_string(file_path).unwrap(), original);
    }

    #[tokio::test]
    async fn write_rejects_overwrite_after_only_a_partial_read() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "a\nb\nc\nd\ne");
        let canonical = file_path.canonicalize().unwrap();

        // A read-tool partial window (offset/limit, or large-file paging) — the
        // model has not seen the whole file.
        fixture.read_tracker.mark_read_partial(&canonical).await;

        let tool = WriteTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let err = tool
            .execute(json!({ "path": "test.txt", "content": "rewritten" }))
            .await
            .expect_err("overwriting after only a partial read must be rejected");
        assert!(err.to_string().contains("only read in part"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&file_path).unwrap(),
            "a\nb\nc\nd\ne"
        );

        // A full read clears the way for the same overwrite.
        fixture.read_tracker.mark_read(&canonical).await;
        tool.execute(json!({ "path": "test.txt", "content": "rewritten" }))
            .await
            .expect("overwrite after a full read should succeed");
        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "rewritten");
    }

    #[tokio::test]
    async fn test_write_overwrite_with_absolute_project_path() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "Old content");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;
        let path_arg = file_path.display().to_string();

        let tool = WriteTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        tool.execute(json!({
            "path": path_arg,
            "content": "New content"
        }))
        .await
        .unwrap();

        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "New content");
    }

    #[tokio::test]
    async fn test_write_without_read() {
        let fixture = TestFixture::new();
        fixture.create_file("test.txt", "Old content");

        let tool = WriteTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "content": "New content"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("must be read before overwriting"));
    }

    #[tokio::test]
    async fn test_write_creates_parent_dirs() {
        let fixture = TestFixture::new();

        let tool = WriteTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        tool.execute(json!({
            "path": "subdir/nested/file.txt",
            "content": "content"
        }))
        .await
        .unwrap();

        let file_path = fixture.project_root.join("subdir/nested/file.txt");
        assert!(file_path.exists());
    }

    #[tokio::test]
    async fn test_write_identical_content() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "Same content");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = WriteTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "content": "Same content"
            }))
            .await;

        // A no-op write succeeds: the desired end state already holds, and an
        // error here only triggers re-read/retry loops.
        let output = result.unwrap();
        assert!(
            output
                .rendered_summary()
                .contains("already contains exactly this content"),
            "{}",
            output.rendered_summary()
        );
    }

    #[tokio::test]
    async fn test_write_rejects_parent_dir_traversal() {
        let fixture = TestFixture::new();

        let tool = WriteTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "../outside.txt",
                "content": "content"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("outside project root"));
    }

    #[tokio::test]
    async fn test_write_rejects_absolute_path_outside_project() {
        let fixture = TestFixture::new();
        let outside_dir = tempfile::TempDir::new().unwrap();
        let outside_path = outside_dir.path().join("outside.txt");

        let tool = WriteTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": outside_path.display().to_string(),
                "content": "content"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("outside project root"));
        assert!(!outside_path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn protected_symlink_targets_require_permission_even_with_safe_aliases() {
        use std::os::unix::fs::symlink;

        let fixture = TestFixture::new();
        let cases = [
            (".env", "safe-env.txt"),
            (".git/config", "safe-git.txt"),
            (".ssh/config", "safe-ssh.txt"),
            ("certificate.pem", "safe-certificate.txt"),
        ];
        for (target, alias) in cases {
            let target_path = fixture.create_file(target, "secret");
            fixture
                .read_tracker
                .mark_read(&target_path.canonicalize().unwrap())
                .await;
            symlink(target, fixture.project_root.join(alias)).unwrap();
        }
        let tool = balanced_tool(&fixture);

        for (target, alias) in cases {
            let error = tool
                .execute(json!({"path": alias, "content": "replacement"}))
                .await
                .expect_err("a protected resolved target must require permission");
            assert!(
                error.to_string().contains("Permission prompt unavailable"),
                "{alias} -> {target}: {error:#}"
            );
            assert_eq!(
                std::fs::read_to_string(fixture.project_root.join(target)).unwrap(),
                "secret"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn create_beneath_symlinked_protected_directory_requires_permission() {
        use std::os::unix::fs::symlink;

        let fixture = TestFixture::new();
        std::fs::create_dir(fixture.project_root.join(".ssh")).unwrap();
        symlink(".ssh", fixture.project_root.join("safe-dir")).unwrap();
        let tool = balanced_tool(&fixture);

        let error = tool
            .execute(json!({"path": "safe-dir/config", "content": "secret"}))
            .await
            .expect_err("a protected target beneath an aliased ancestor must require permission");

        assert!(
            error.to_string().contains("Permission prompt unavailable"),
            "{error:#}"
        );
        assert!(!fixture.project_root.join(".ssh/config").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ordinary_symlink_target_keeps_balanced_write_behavior() {
        use std::os::unix::fs::symlink;

        let fixture = TestFixture::new();
        let target = fixture.create_file("ordinary.txt", "old");
        fixture
            .read_tracker
            .mark_read(&target.canonicalize().unwrap())
            .await;
        symlink("ordinary.txt", fixture.project_root.join("friendly.txt")).unwrap();
        let tool = balanced_tool(&fixture);

        tool.execute(json!({"path": "friendly.txt", "content": "new"}))
            .await
            .expect("an ordinary resolved target should stay auto-approved at Balanced");

        assert_eq!(std::fs::read_to_string(target).unwrap(), "new");
    }

    #[tokio::test]
    async fn yolo_write_overwrites_without_prior_read() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "Old content");
        let yolo_mode = YoloMode::new();
        yolo_mode.set_enabled(true);

        let tool = WriteTool::with_yolo_mode(
            fixture.project_root.clone(),
            fixture.read_tracker.clone(),
            yolo_mode,
        );
        tool.execute(json!({
            "path": "test.txt",
            "content": "New content"
        }))
        .await
        .unwrap();

        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "New content");
    }

    #[tokio::test]
    async fn write_rejects_a_file_changed_after_the_observed_full_read() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "Old content");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture
            .read_tracker
            .mark_read_with_content(&canonical_path, b"Old content", true)
            .await;
        std::fs::write(&file_path, "Newer user content").unwrap();

        let tool = WriteTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let error = tool
            .execute(json!({
                "path": "test.txt",
                "content": "Agent replacement"
            }))
            .await
            .expect_err("a stale whole-file write must be rejected");

        assert!(error.to_string().contains("changed after it was read"));
        assert_eq!(
            std::fs::read_to_string(file_path).unwrap(),
            "Newer user content"
        );
    }

    #[tokio::test]
    async fn yolo_write_ignores_stale_read_check() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "Old");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;
        std::fs::write(&file_path, "Changed after read").unwrap();
        let yolo_mode = YoloMode::new();
        yolo_mode.set_enabled(true);

        let tool = WriteTool::with_yolo_mode(
            fixture.project_root.clone(),
            fixture.read_tracker.clone(),
            yolo_mode,
        );
        tool.execute(json!({
            "path": "test.txt",
            "content": "YOLO content"
        }))
        .await
        .unwrap();

        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "YOLO content");
    }

    #[tokio::test]
    async fn yolo_write_allows_parent_dir_outside_project() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let project_root = temp_dir.path().join("project");
        std::fs::create_dir(&project_root).unwrap();
        let outside_path = temp_dir.path().join("outside.txt");
        let yolo_mode = YoloMode::new();
        yolo_mode.set_enabled(true);

        let tool = WriteTool::with_yolo_mode(project_root, ReadTracker::new(), yolo_mode);
        tool.execute(json!({
            "path": "../outside.txt",
            "content": "outside"
        }))
        .await
        .unwrap();

        assert_eq!(std::fs::read_to_string(outside_path).unwrap(), "outside");
    }

    #[tokio::test]
    async fn yolo_write_allows_absolute_path_outside_project() {
        let fixture = TestFixture::new();
        let outside_dir = tempfile::TempDir::new().unwrap();
        let outside_path = outside_dir.path().join("outside.txt");
        let yolo_mode = YoloMode::new();
        yolo_mode.set_enabled(true);

        let tool = WriteTool::with_yolo_mode(
            fixture.project_root.clone(),
            fixture.read_tracker.clone(),
            yolo_mode,
        );
        tool.execute(json!({
            "path": outside_path.display().to_string(),
            "content": "outside"
        }))
        .await
        .unwrap();

        assert_eq!(std::fs::read_to_string(outside_path).unwrap(), "outside");
    }
}
