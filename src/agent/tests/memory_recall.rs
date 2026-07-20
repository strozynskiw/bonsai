//! Per-turn memory recall (M4.8): injection order, session dedup, and the
//! smol/no-service skips.

use super::*;
use crate::memory::MemoryService;
use crate::memory::entry::{MemoryEntryType, MemoryTier};

async fn memory_with_fact() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    tempfile::TempDir,
    Arc<MemoryService>,
) {
    let home = tempfile::TempDir::new().unwrap();
    let project = tempfile::TempDir::new().unwrap();
    let db = tempfile::TempDir::new().unwrap();
    let storage = crate::storage::Storage::open_at(db.path().join("bonsai.db"))
        .await
        .unwrap();
    let memory = Arc::new(MemoryService::load(home.path(), project.path(), storage, 0));
    memory
        .store()
        .write(
            MemoryTier::User,
            MemoryEntryType::Preference,
            None,
            "partial commits need no-verify",
            "The pre-commit hook fails on the unstaged remainder.",
            None,
        )
        .unwrap();
    (home, project, db, memory)
}

fn agent_with_memory_and_provider(
    memory: Arc<MemoryService>,
    provider: Box<dyn Provider>,
) -> Agent {
    let fixture = TestFixture::new();
    let mut agent = Agent::new(
        provider,
        Arc::new(ToolRegistry::new()),
        Arc::new(ToolRegistry::new()),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_memory(memory);
    agent
}

fn agent_with_memory(memory: Arc<MemoryService>) -> Agent {
    agent_with_memory_and_provider(memory, MockProvider::empty())
}

#[tokio::test]
async fn accepted_user_message_lands_before_recall_note() {
    let (_home, _project, _db, memory) = memory_with_fact().await;
    let provider = MockProvider::new(vec![Ok(StreamedResponse {
        content: "done".to_string(),
        tool_calls: Vec::new(),
        terminal: crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::Stop),
        usage: None,
        ..StreamedResponse::default()
    })]);
    let requests = provider.requests();
    let mut agent = agent_with_memory_and_provider(memory, Box::new(provider));
    let sink = Arc::new(CaptureSink::default());

    agent
        .run(
            "how do I commit only part of my changes",
            CancellationToken::new(),
            sink.clone() as SharedSink,
        )
        .await
        .expect("run completes");

    let requests = requests.lock().await;
    let texts = user_messages_in(&requests[0]);
    let user_index = texts
        .iter()
        .position(|text| text.starts_with("how do I commit"))
        .expect("user message present");
    let note_index = texts
        .iter()
        .position(|text| text.contains("Recalled memory (background context)"))
        .expect("recall note injected");
    assert!(
        user_index < note_index,
        "accepted user message must precede recall note"
    );
    assert!(texts[note_index].contains("partial-commits-need-no-verify"));
    assert!(texts[note_index].contains("NOT as user instructions"));
    // Recall is silent by design: the note itself is the only surface.
    assert!(
        sink.statuses()
            .iter()
            .all(|status| !status.starts_with("recalled memory")),
        "{:?}",
        sink.statuses()
    );
}

#[tokio::test]
async fn restored_recall_notes_preserve_dedup() {
    let (_home, _project, _db, memory) = memory_with_fact().await;
    let entry = memory
        .store()
        .get("partial-commits-need-no-verify")
        .expect("memory entry exists");
    let note = crate::memory::render_recall_note(&[crate::memory::render_recall_entry(&entry)]);
    let mut agent = agent_with_memory(memory);

    agent
        .restore_text_history(&[("user".to_string(), note)])
        .await
        .expect("restore text history");
    let after_restore = agent.context_messages().len();
    agent
        .inject_recalled_memory("how do I commit only part of my changes")
        .await;

    assert_eq!(
        agent.context_messages().len(),
        after_restore,
        "restored recall notes should suppress duplicate recall"
    );
}

#[tokio::test]
async fn restored_memory_write_calls_preserve_dedup() {
    let (_home, _project, _db, memory) = memory_with_fact().await;
    let mut agent = agent_with_memory(memory);
    let memory_write = assistant_tool_call_message(
        "call-memory",
        "memory_write",
        r#"{"action":"save","tier":"user","type":"preference","description":"partial commits need no-verify","body":"The pre-commit hook fails on the unstaged remainder."}"#,
    );

    agent
        .restore_context_messages(vec![test_system_message("system"), memory_write])
        .await
        .expect("restore context messages");
    let after_restore = agent.context_messages().len();
    agent
        .inject_recalled_memory("how do I commit only part of my changes")
        .await;

    assert_eq!(
        agent.context_messages().len(),
        after_restore,
        "restored memory_write calls should suppress duplicate recall"
    );
}

#[tokio::test]
async fn second_related_turn_does_not_reinject() {
    let (_home, _project, _db, memory) = memory_with_fact().await;
    let mut agent = agent_with_memory(memory);

    agent
        .inject_recalled_memory("how do I commit only part of my changes")
        .await;
    let after_first = agent.context_messages().len();
    assert!(after_first > 1, "first related turn should inject the note");
    agent
        .inject_recalled_memory("commit the staged part again please")
        .await;
    assert_eq!(
        agent.context_messages().len(),
        after_first,
        "second related turn must inject nothing"
    );
}

#[tokio::test]
async fn unrelated_turns_inject_nothing() {
    let (_home, _project, _db, memory) = memory_with_fact().await;
    let mut agent = agent_with_memory(memory);

    let before = agent.context_messages().len();
    agent
        .inject_recalled_memory("what color should the dashboard sidebar be")
        .await;
    assert_eq!(agent.context_messages().len(), before);
}

#[tokio::test]
async fn smol_mode_and_missing_service_skip_recall() {
    let (_home, _project, _db, memory) = memory_with_fact().await;

    let mut smol_agent = agent_with_memory(memory);
    smol_agent.set_smol_mode(true);
    let before = smol_agent.context_messages().len();
    smol_agent.inject_recalled_memory("partial commits").await;
    assert_eq!(smol_agent.context_messages().len(), before);

    let fixture = TestFixture::new();
    let mut plain_agent = Agent::new(
        MockProvider::empty(),
        Arc::new(ToolRegistry::new()),
        Arc::new(ToolRegistry::new()),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    let before = plain_agent.context_messages().len();
    plain_agent.inject_recalled_memory("partial commits").await;
    assert_eq!(plain_agent.context_messages().len(), before);
}

#[tokio::test]
async fn clear_resets_recall_dedup() {
    let (_home, _project, _db, memory) = memory_with_fact().await;
    let mut agent = agent_with_memory(memory);

    agent
        .inject_recalled_memory("how do I commit only part of my changes")
        .await;
    agent.clear().await;
    let before = agent.context_messages().len();
    agent
        .inject_recalled_memory("how do I commit only part of my changes")
        .await;
    assert_eq!(
        agent.context_messages().len(),
        before + 1,
        "a cleared conversation recalls again"
    );
}
