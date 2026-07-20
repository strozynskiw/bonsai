use std::path::Path;

use tempfile::TempDir;

use crate::provider::ReasoningSelection;
use crate::storage::{SessionId, Storage};

pub(crate) struct TestStorage {
    pub temp_dir: TempDir,
    pub storage: Storage,
}

impl TestStorage {
    pub(crate) async fn new() -> Self {
        let temp_dir = tempfile::TempDir::new().expect("test temp dir should be created");
        let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
            .await
            .expect("test storage should open");
        Self { temp_dir, storage }
    }

    pub(crate) fn project_path(&self) -> &Path {
        self.temp_dir.path()
    }

    pub(crate) async fn start_session(&self) -> SessionId {
        self.start_session_with("anthropic", "claude-sonnet-4-5")
            .await
    }

    pub(crate) async fn start_session_with(&self, provider_id: &str, model: &str) -> SessionId {
        self.storage
            .start_session(
                self.project_path(),
                provider_id,
                model,
                ReasoningSelection::default(),
            )
            .await
            .expect("test session should start")
    }
}
