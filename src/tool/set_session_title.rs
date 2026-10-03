use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::episode::EpisodeAction;
use crate::storage::{SessionId, Storage};
use crate::tool::schema::{object, parse_args, string_enum_property, string_property};
use crate::tool::{ParallelPolicy, Tool, ToolOutput};

pub type SharedActiveSessionId = Arc<Mutex<Option<SessionId>>>;

pub(crate) fn normalize_session_title(title: &str) -> Result<&str> {
    let title = title.trim();
    if title.is_empty() {
        anyhow::bail!("title is required");
    }
    if title.chars().count() > 80 {
        anyhow::bail!("title must be 80 characters or fewer");
    }
    Ok(title)
}

#[derive(Deserialize)]
struct SetSessionTitleArgs {
    title: String,
    #[serde(default, rename = "episode_action")]
    _episode_action: EpisodeAction,
}

pub struct SetSessionTitleTool {
    storage: Storage,
    active_session_id: SharedActiveSessionId,
}

impl SetSessionTitleTool {
    pub fn new(storage: Storage, active_session_id: SharedActiveSessionId) -> Self {
        Self {
            storage,
            active_session_id,
        }
    }
}

#[async_trait]
impl Tool for SetSessionTitleTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::LocalState
    }

    fn name(&self) -> &str {
        "set_session_title"
    }

    fn description(&self) -> &str {
        "Set the title once when an explicit human message introduces a distinct task and the current title no longer fits. Never call this for harness notes, retries, continuations, phase changes, or an unchanged title. Use new_topic only for a distinct human goal; same_topic only for a genuine rename within the current goal."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object(
            [
                (
                    "title",
                    string_property("Short session title, usually 3-8 words"),
                ),
                (
                    "episode_action",
                    string_enum_property(
                        "Whether this starts a distinct user topic or only renames the current one",
                        EpisodeAction::WIRE_VALUES,
                    ),
                ),
            ],
            &["title", "episode_action"],
        )
    }

    fn parallel_policy(&self) -> ParallelPolicy {
        ParallelPolicy::Serialized
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let SetSessionTitleArgs {
            title,
            _episode_action: _,
        } = parse_args("set_session_title", args)?;
        let title = normalize_session_title(&title)?;

        let Some(session_id) = *self.active_session_id.lock().await else {
            anyhow::bail!("No active persisted session is available");
        };
        let Some(current) = self.storage.session_summary(session_id).await? else {
            anyhow::bail!("No active persisted session is available");
        };
        if current.summary.trim() == title {
            anyhow::bail!(
                "Session title is already '{title}'. Do not call set_session_title again for this \
                 task; continue with the next substantive action."
            );
        }
        self.storage.set_session_summary(session_id, title).await?;
        Ok(ToolOutput::Text(format!("Session title set to: {title}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ReasoningSelection;
    use serde_json::json;

    async fn new_storage() -> (tempfile::TempDir, Storage) {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
            .await
            .unwrap();
        (temp_dir, storage)
    }

    #[tokio::test]
    async fn title_schema_requires_explicit_episode_action() {
        let (_temp_dir, storage) = new_storage().await;
        let tool = SetSessionTitleTool::new(storage, Arc::new(Mutex::new(None)));
        let schema = tool.parameters_schema();

        assert_eq!(
            schema["properties"]["episode_action"]["enum"],
            json!(["new_topic", "same_topic"])
        );
        assert!(
            schema["required"]
                .as_array()
                .is_some_and(|required| required.contains(&json!("episode_action")))
        );
    }

    #[tokio::test]
    async fn set_session_title_rejects_empty_title() {
        let (_temp_dir, storage) = new_storage().await;
        let tool = SetSessionTitleTool::new(storage, Arc::new(Mutex::new(None)));

        let err = tool
            .execute(json!({"title": "   "}))
            .await
            .expect_err("empty title should be rejected");

        assert!(
            err.to_string().contains("title is required"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn set_session_title_rejects_overlong_title() {
        let (_temp_dir, storage) = new_storage().await;
        let tool = SetSessionTitleTool::new(storage, Arc::new(Mutex::new(None)));

        let err = tool
            .execute(json!({"title": "x".repeat(81)}))
            .await
            .expect_err("titles longer than 80 chars should be rejected");

        assert!(
            err.to_string().contains("80 characters or fewer"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn set_session_title_requires_active_session() {
        let (_temp_dir, storage) = new_storage().await;
        let tool = SetSessionTitleTool::new(storage, Arc::new(Mutex::new(None)));

        let err = tool
            .execute(json!({"title": "A valid title"}))
            .await
            .expect_err("missing active session should be rejected");

        assert!(
            err.to_string().contains("No active persisted session"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn set_session_title_persists_summary() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
            .await
            .unwrap();
        let session_id = storage
            .start_session(
                temp_dir.path(),
                "codex",
                "test-model",
                ReasoningSelection::default(),
            )
            .await
            .unwrap();
        let active_session_id = Arc::new(Mutex::new(Some(session_id)));
        let tool = SetSessionTitleTool::new(storage.clone(), active_session_id);

        tool.execute(json!({"title": "Session picker work"}))
            .await
            .unwrap();

        let sessions = storage
            .recent_sessions_for_project(temp_dir.path(), 10)
            .await
            .unwrap();
        assert_eq!(sessions[0].summary, "Session picker work");
    }

    #[tokio::test]
    async fn set_session_title_rejects_unchanged_title() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
            .await
            .unwrap();
        let session_id = storage
            .start_session(
                temp_dir.path(),
                "codex",
                "test-model",
                ReasoningSelection::default(),
            )
            .await
            .unwrap();
        storage
            .set_session_summary(session_id, "Isolate TUI verification")
            .await
            .unwrap();
        let tool = SetSessionTitleTool::new(storage, Arc::new(Mutex::new(Some(session_id))));

        let error = tool
            .execute(json!({
                "title": " Isolate TUI verification ",
                "episode_action": "same_topic"
            }))
            .await
            .expect_err("an unchanged title should be rejected");

        assert!(
            error
                .to_string()
                .contains("already 'Isolate TUI verification'")
        );
    }
}
