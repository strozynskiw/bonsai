//! Episode boundary + persistence tests. These drive the
//! agent hooks directly — user pushes, scripted title-call groups, preflight
//! close application — and assert ledger state through the shared store.

use std::collections::HashSet;

use super::*;
use crate::episode::{
    ArchivedEpisodeItem, Episode, EpisodeCloseReason, EpisodeEvictionPass, EpisodeEvictionRecord,
    EpisodeStatus, PersistedEpisode, SharedEpisodeStore,
};

macro_rules! persisted_episode {
    ($($field:tt)*) => {
        Episode::from_persisted(PersistedEpisode { $($field)* })
    };
}

fn episode_agent() -> (Agent, SharedEpisodeStore, TestFixture) {
    let fixture = TestFixture::new();
    let store = SharedEpisodeStore::default();
    let mut agent = Agent::builder(
        MockProvider::empty(),
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .build()
    .expect("test agent should build");
    agent.set_episode_store(store.clone());
    (agent, store, fixture)
}

fn episodes_of(store: &SharedEpisodeStore) -> Vec<Episode> {
    store
        .lock()
        .expect("episode store mutex should not be poisoned")
        .snapshot()
}

fn push_user_turn(agent: &mut Agent, text: &str) -> String {
    let id = agent.push_user_message_raw(text);
    agent.observe_user_turn_for_episodes(text);
    id
}

/// A complete assistant tool-call group (one call + its result).
fn push_tool_group(agent: &mut Agent, call_id: &str, path: &str) {
    agent.push_message(assistant_tool_call_message(
        call_id,
        "read",
        &format!(r#"{{"path":"{path}"}}"#),
    ));
    agent.push_message(tool_result_message(call_id, "file contents"));
}

/// Push a complete `set_session_title` group, record its signal, and apply the
/// preflight close resolution — the full boundary pipeline for one retitle.
fn set_title(agent: &mut Agent, call_id: &str, title: &str) {
    set_title_with_action(agent, call_id, title, "new_topic");
}

fn set_title_with_action(agent: &mut Agent, call_id: &str, title: &str, action: &str) {
    let args = format!(r#"{{"title":"{title}","episode_action":"{action}"}}"#);
    agent.push_message(assistant_tool_call_message(
        call_id,
        "set_session_title",
        &args,
    ));
    agent.push_message(tool_result_message(
        call_id,
        &format!("Session title set to: {title}"),
    ));
    agent.observe_episode_title_result(&test_tool_call(call_id, "set_session_title", &args));
    agent.apply_pending_episode_title_signal();
}

#[tokio::test]
async fn user_turn_opens_episode_and_first_titling_never_closes() {
    let (mut agent, store, _fixture) = episode_agent();

    let user_id = push_user_turn(&mut agent, "please fix the resume flake in storage");
    let episodes = episodes_of(&store);
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[0].status(), EpisodeStatus::Active);
    assert_eq!(episodes[0].start_stable_id(), user_id);
    assert_eq!(episodes[0].title(), "");
    assert_eq!(episodes[0].goal(), "please fix the resume flake in storage");

    push_tool_group(&mut agent, "call-r1", "src/storage/mod.rs");
    set_title(&mut agent, "call-t1", "Fix resume flake");

    let episodes = episodes_of(&store);
    assert_eq!(episodes.len(), 1, "first titling must not close");
    assert_eq!(episodes[0].status(), EpisodeStatus::Active);
    assert_eq!(episodes[0].title(), "Fix resume flake");
}

#[tokio::test]
async fn title_change_with_intervening_user_message_closes_at_user_boundary() {
    let (mut agent, store, _fixture) = episode_agent();

    push_user_turn(&mut agent, "fix the flake");
    set_title(&mut agent, "call-t1", "Task A");
    push_tool_group(&mut agent, "call-r1", "src/a.rs");
    push_tool_group(&mut agent, "call-r2", "src/b.rs");

    // The user pivots; the model retitles afterwards. Boundary lands on the
    // user message, so the pre-boundary end is the last topic-A row.
    let pre_boundary_id = agent.message_ids.last().cloned().unwrap();
    let user_b_id = push_user_turn(&mut agent, "now build the exporter");
    set_title(&mut agent, "call-t2", "Task B");

    let episodes = episodes_of(&store);
    assert_eq!(episodes.len(), 2);
    let closed = &episodes[0];
    assert_eq!(closed.status(), EpisodeStatus::Closed);
    assert_eq!(closed.close_reason(), Some(EpisodeCloseReason::TitleChange));
    assert_eq!(closed.title(), "Task A");
    assert_eq!(closed.end_stable_id(), Some(pre_boundary_id.as_str()));
    assert_eq!(
        closed.files_touched(),
        vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
        "span tool-call path args are captured"
    );
    let successor = &episodes[1];
    assert_eq!(successor.status(), EpisodeStatus::Active);
    assert_eq!(successor.title(), "Task B");
    assert_eq!(successor.start_stable_id(), user_b_id);
    assert_eq!(successor.goal(), "now build the exporter");
}

#[tokio::test]
async fn same_topic_correction_renames_without_closing() {
    let (mut agent, store, _fixture) = episode_agent();

    push_user_turn(&mut agent, "fix the flake");
    set_title(&mut agent, "call-t1", "Fix storage flake");
    push_tool_group(&mut agent, "call-r1", "src/a.rs");
    push_tool_group(&mut agent, "call-r2", "src/b.rs");
    let original = episodes_of(&store)[0].clone();

    push_user_turn(&mut agent, "No, keep the existing storage format");
    set_title_with_action(
        &mut agent,
        "call-t2",
        "Fix storage flake compatibly",
        "same_topic",
    );

    let episodes = episodes_of(&store);
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[0].status(), EpisodeStatus::Active);
    assert_eq!(episodes[0].title(), "Fix storage flake compatibly");
    assert_eq!(episodes[0].start_stable_id(), original.start_stable_id());
    assert_eq!(episodes[0].goal(), original.goal());
}

#[tokio::test]
async fn plan_replace_draft_is_observed_as_new_topic_boundary() {
    let (mut agent, store, _fixture) = episode_agent();

    push_user_turn(&mut agent, "fix task A");
    set_title(&mut agent, "call-t1", "Task A");
    push_tool_group(&mut agent, "call-r1", "src/a.rs");
    push_tool_group(&mut agent, "call-r2", "src/b.rs");
    let user_b_id = push_user_turn(&mut agent, "plan task B");
    let args = r#"{
        "title":"Plan task B",
        "episode_action":"new_topic",
        "sections":[],
        "tasks":["Inspect task B"],
        "phases":[],
        "questions":[]
    }"#;
    agent.push_message(assistant_tool_call_message(
        "call-p1",
        "plan_replace_draft",
        args,
    ));
    agent.push_message(tool_result_message("call-p1", "Plan draft replaced"));
    agent.observe_episode_title_result(&test_tool_call("call-p1", "plan_replace_draft", args));
    agent.apply_pending_episode_title_signal();

    let episodes = episodes_of(&store);
    assert_eq!(episodes.len(), 2);
    assert_eq!(episodes[0].status(), EpisodeStatus::Closed);
    assert_eq!(episodes[1].status(), EpisodeStatus::Active);
    assert_eq!(episodes[1].title(), "Plan task B");
    assert_eq!(episodes[1].start_stable_id(), user_b_id);
    assert_eq!(episodes[1].goal(), "plan task B");
}

#[tokio::test]
async fn first_plan_title_reanchors_an_untitled_greeting_episode() {
    let (mut agent, store, _fixture) = episode_agent();

    push_user_turn(&mut agent, "Hi");
    let task_id = push_user_turn(&mut agent, "plan the cache rewrite");
    let args = r#"{
        "title":"Plan cache rewrite",
        "episode_action":"new_topic",
        "sections":[],
        "tasks":["Inspect cache path"],
        "phases":[],
        "questions":[]
    }"#;
    agent.push_message(assistant_tool_call_message(
        "call-p1",
        "plan_replace_draft",
        args,
    ));
    agent.push_message(tool_result_message("call-p1", "Plan draft replaced"));
    agent.observe_episode_title_result(&test_tool_call("call-p1", "plan_replace_draft", args));
    agent.apply_pending_episode_title_signal();

    let episodes = episodes_of(&store);
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[0].title(), "Plan cache rewrite");
    assert_eq!(episodes[0].start_stable_id(), task_id);
    assert_eq!(episodes[0].goal(), "plan the cache rewrite");
}

#[tokio::test]
async fn new_topic_title_without_user_message_renames_active_episode() {
    let (mut agent, store, _fixture) = episode_agent();

    push_user_turn(&mut agent, "do the whole migration");
    set_title(&mut agent, "call-t1", "Phase A");
    push_tool_group(&mut agent, "call-r1", "src/a.rs");
    push_tool_group(&mut agent, "call-r2", "src/b.rs");

    // A model-only phase change has no human new-topic anchor, even if the
    // caller declared new_topic. It is a rename, not a synthetic boundary.
    let start_id = episodes_of(&store)[0].start_stable_id().to_string();
    set_title(&mut agent, "call-t2", "Phase B");

    let episodes = episodes_of(&store);
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[0].status(), EpisodeStatus::Active);
    assert_eq!(episodes[0].title(), "Phase B");
    assert_eq!(episodes[0].start_stable_id(), start_id);
}

#[tokio::test]
async fn tiny_episode_is_retitled_in_place_instead_of_closed() {
    let (mut agent, store, _fixture) = episode_agent();

    push_user_turn(&mut agent, "quick question");
    set_title(&mut agent, "call-t1", "Task A");
    // No work happened between the titles: the closing span holds only the
    // first title group — below EPISODE_MIN_CLOSED_GROUPS.
    set_title(&mut agent, "call-t2", "Task B");

    let episodes = episodes_of(&store);
    assert_eq!(episodes.len(), 1, "tiny episodes never close");
    assert_eq!(episodes[0].status(), EpisodeStatus::Active);
    assert_eq!(episodes[0].title(), "Task B");
}

#[tokio::test]
async fn same_title_reemit_and_case_variants_never_close() {
    let (mut agent, store, _fixture) = episode_agent();

    push_user_turn(&mut agent, "fix it");
    set_title(&mut agent, "call-t1", "Fix Resume Flake");
    push_tool_group(&mut agent, "call-r1", "src/a.rs");
    push_tool_group(&mut agent, "call-r2", "src/b.rs");
    set_title(&mut agent, "call-t2", "fix resume flake");

    let episodes = episodes_of(&store);
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[0].status(), EpisodeStatus::Active);
    assert_eq!(
        episodes[0].title(),
        "Fix Resume Flake",
        "a case-only variant is not a boundary"
    );
}

#[tokio::test]
async fn steering_does_not_close_active_episode() {
    let (mut agent, store, _fixture) = episode_agent();

    push_user_turn(&mut agent, "start the task");
    set_title(&mut agent, "call-t1", "Task A");
    push_tool_group(&mut agent, "call-r1", "src/a.rs");

    // Mid-run steering: a second human message while the episode runs.
    push_user_turn(&mut agent, "also check the tests");

    let episodes = episodes_of(&store);
    assert_eq!(episodes.len(), 1, "steering never closes");
    assert_eq!(episodes[0].status(), EpisodeStatus::Active);
}

#[tokio::test]
async fn completed_or_cancelled_todos_arm_episode_without_closing_it() {
    let (mut agent, store, _fixture) = episode_agent();
    push_user_turn(&mut agent, "finish the task list");
    let todo_store = Arc::new(tokio::sync::Mutex::new(crate::todo::TodoStore::new()));
    agent.set_todo_store(todo_store.clone());

    todo_store
        .lock()
        .await
        .set_todos(vec![crate::todo::TodoItem {
            content: "still working".to_string(),
            status: crate::todo::TodoStatus::InProgress,
        }]);
    agent.observe_todowrite_result().await;
    assert!(!episodes_of(&store)[0].is_completable());

    todo_store.lock().await.set_todos(vec![
        crate::todo::TodoItem {
            content: "implemented".to_string(),
            status: crate::todo::TodoStatus::Completed,
        },
        crate::todo::TodoItem {
            content: "obsolete".to_string(),
            status: crate::todo::TodoStatus::Cancelled,
        },
    ]);
    agent.observe_todowrite_result().await;

    let episode = &episodes_of(&store)[0];
    assert!(episode.is_completable());
    assert_eq!(episode.status(), EpisodeStatus::Active, "arm-only in v1");
}

#[tokio::test]
async fn incomplete_title_group_defers_close_until_group_completes() {
    let (mut agent, store, _fixture) = episode_agent();

    push_user_turn(&mut agent, "task A work");
    set_title(&mut agent, "call-t1", "Task A");
    push_tool_group(&mut agent, "call-r1", "src/a.rs");
    push_tool_group(&mut agent, "call-r2", "src/b.rs");
    push_user_turn(&mut agent, "switch to task B");

    // A multi-call assistant turn: retitle + a read, with only the retitle
    // result applied when preflight runs (interrupted batch).
    let args = r#"{"title":"Task B","episode_action":"new_topic"}"#;
    agent.push_message(ChatCompletionRequestMessage::Assistant(
        async_openai::types::chat::ChatCompletionRequestAssistantMessageArgs::default()
            .tool_calls(vec![
                async_openai::types::chat::ChatCompletionMessageToolCalls::Function(
                    async_openai::types::chat::ChatCompletionMessageToolCall {
                        id: "call-t2".to_string(),
                        function: async_openai::types::chat::FunctionCall {
                            name: "set_session_title".to_string(),
                            arguments: args.to_string(),
                        },
                    },
                ),
                async_openai::types::chat::ChatCompletionMessageToolCalls::Function(
                    async_openai::types::chat::ChatCompletionMessageToolCall {
                        id: "call-r3".to_string(),
                        function: async_openai::types::chat::FunctionCall {
                            name: "read".to_string(),
                            arguments: r#"{"path":"src/c.rs"}"#.to_string(),
                        },
                    },
                ),
            ])
            .build()
            .unwrap(),
    ));
    agent.push_message(tool_result_message(
        "call-t2",
        "Session title set to: Task B",
    ));
    agent.observe_episode_title_result(&test_tool_call("call-t2", "set_session_title", args));

    agent.apply_pending_episode_title_signal();
    assert_eq!(
        episodes_of(&store).len(),
        1,
        "incomplete group must defer the close"
    );
    assert!(
        store.lock().unwrap().pending_title_signal().is_some(),
        "the signal stays armed for the next preflight"
    );

    // The missing result lands; the deferred close now applies.
    agent.push_message(tool_result_message("call-r3", "file contents"));
    agent.apply_pending_episode_title_signal();
    let episodes = episodes_of(&store);
    assert_eq!(episodes.len(), 2);
    assert_eq!(episodes[0].status(), EpisodeStatus::Closed);
    assert_eq!(episodes[1].title(), "Task B");
}

#[tokio::test]
async fn boundary_application_never_mutates_wire_bytes() {
    let (mut agent, store, _fixture) = episode_agent();

    push_user_turn(&mut agent, "task A work");
    set_title(&mut agent, "call-t1", "Task A");
    push_tool_group(&mut agent, "call-r1", "src/a.rs");
    push_tool_group(&mut agent, "call-r2", "src/b.rs");
    push_user_turn(&mut agent, "switch to task B");

    // Arm a closeable boundary, then apply it: the ledger changes, the
    // model-facing buffer must not — P1 tracking is zero wire-byte by gate.
    let args = r#"{"title":"Task B","episode_action":"new_topic"}"#;
    agent.push_message(assistant_tool_call_message(
        "call-t2",
        "set_session_title",
        args,
    ));
    agent.push_message(tool_result_message(
        "call-t2",
        "Session title set to: Task B",
    ));
    agent.observe_episode_title_result(&test_tool_call("call-t2", "set_session_title", args));

    let bytes_before = serde_json::to_string(&agent.messages).unwrap();
    let ids_before = agent.message_ids.clone();
    agent.apply_pending_episode_title_signal();
    assert_eq!(episodes_of(&store).len(), 2, "the close applied");
    assert_eq!(
        serde_json::to_string(&agent.messages).unwrap(),
        bytes_before
    );
    assert_eq!(agent.message_ids, ids_before);
}

#[tokio::test]
async fn hard_reset_closes_active_episode_and_clears_pending_signal() {
    let (mut agent, store, _fixture) = episode_agent();

    push_user_turn(&mut agent, "task A");
    set_title(&mut agent, "call-t1", "Task A");
    // Arm a pending signal that must NOT survive the reset.
    agent.observe_episode_title_result(&test_tool_call(
        "call-t9",
        "set_session_title",
        r#"{"title":"Task Z","episode_action":"new_topic"}"#,
    ));

    agent.clear().await;

    let episodes = episodes_of(&store);
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[0].status(), EpisodeStatus::Closed);
    assert_eq!(
        episodes[0].close_reason(),
        Some(EpisodeCloseReason::HardBoundary)
    );
    assert!(store.lock().unwrap().pending_title_signal().is_none());
}

#[tokio::test]
async fn interrupted_turn_voids_signal_whose_group_never_landed() {
    let (mut agent, store, _fixture) = episode_agent();

    push_user_turn(&mut agent, "task A");
    set_title(&mut agent, "call-t1", "Task A");
    push_tool_group(&mut agent, "call-r1", "src/a.rs");
    push_tool_group(&mut agent, "call-r2", "src/b.rs");

    // Signal recorded, but the assistant message carrying the call never
    // reached context (discarded attempt).
    agent.observe_episode_title_result(&test_tool_call(
        "call-ghost",
        "set_session_title",
        r#"{"title":"Task B","episode_action":"new_topic"}"#,
    ));
    agent.apply_pending_episode_title_signal();

    assert_eq!(episodes_of(&store).len(), 1, "void signal must not close");
    assert!(
        store.lock().unwrap().pending_title_signal().is_none(),
        "a void signal is dropped, not deferred forever"
    );
}

#[tokio::test]
async fn implement_plan_from_closes_hard_and_opens_implementation_episode() {
    let (mut agent, store, _fixture) = episode_agent();

    push_user_turn(&mut agent, "plan the feature");
    set_title(&mut agent, "call-t1", "Planning");

    let mut plan = crate::plan::PlanDoc::default();
    plan.sections.push(crate::plan::PlanSection {
        heading: "Approach".to_string(),
        body: "Do the thing.".to_string(),
    });
    let dispatched = agent.implement_plan_from(&plan, None).await;
    assert!(dispatched);

    let episodes = episodes_of(&store);
    assert_eq!(episodes.len(), 2);
    assert_eq!(episodes[0].status(), EpisodeStatus::Closed);
    assert_eq!(
        episodes[0].close_reason(),
        Some(EpisodeCloseReason::HardBoundary)
    );
    let implementation = &episodes[1];
    assert_eq!(implementation.status(), EpisodeStatus::Active);
    assert!(
        agent
            .message_ids
            .iter()
            .any(|id| id == implementation.start_stable_id()),
        "implementation episode starts on the seeded message"
    );
}

#[tokio::test]
async fn restore_episodes_repairs_active_span_skew() {
    let (mut agent, store, _fixture) = episode_agent();

    let user_id = push_user_turn(&mut agent, "current task in this context");

    // A persisted ledger whose active episode points at a stable id that no
    // longer exists (e.g. the context was rebuilt across versions).
    let stale = vec![persisted_episode! {
        seq: 1,
        title: "Old task".to_string(),
        status: EpisodeStatus::Active,
        goal: "old goal".to_string(),
        card_md: String::new(),
        close_reason: None,
        start_stable_id: "msg-9999".to_string(),
        end_stable_id: None,
        marker_stable_id: None,
        files_touched: Vec::new(),
        opened_at_ms: 5,
        closed_at_ms: None,
        evicted_at_ms: None,
        evicted_tokens: None,
        recall_count: 0,
        completable: false,
        archive: Vec::new(),
    }];
    agent.restore_episodes(stale);

    let episodes = episodes_of(&store);
    assert_eq!(episodes.len(), 2);
    assert_eq!(episodes[0].status(), EpisodeStatus::Closed);
    assert_eq!(episodes[0].close_reason(), Some(EpisodeCloseReason::Manual));
    assert_eq!(episodes[1].status(), EpisodeStatus::Active);
    assert_eq!(
        episodes[1].start_stable_id(),
        user_id,
        "reopened at the last human user message"
    );
}

#[tokio::test]
async fn restore_episodes_keeps_resolvable_active_span() {
    let (mut agent, store, _fixture) = episode_agent();

    let user_id = push_user_turn(&mut agent, "the task");
    let snapshot = episodes_of(&store);
    assert_eq!(snapshot.len(), 1);

    // Round-trip through restore: the span still resolves, nothing changes.
    agent.restore_episodes(snapshot.clone());
    let episodes = episodes_of(&store);
    assert_eq!(episodes, snapshot);
    assert_eq!(episodes[0].start_stable_id(), user_id);
}

#[tokio::test]
async fn restore_episodes_repairs_invalid_closed_and_evicted_links() {
    let (mut agent, store, _fixture) = episode_agent();
    let live_id = push_user_turn(&mut agent, "live context");
    let archived = ArchivedEpisodeItem {
        stable_id: "archive-1".to_string(),
        message: test_user_message("preserved bytes"),
    };
    let rows = vec![
        persisted_episode! {
            seq: 1,
            title: "stale closed".to_string(),
            status: EpisodeStatus::Closed,
            goal: String::new(),
            card_md: String::new(),
            close_reason: Some(EpisodeCloseReason::TitleChange),
            start_stable_id: "missing-start".to_string(),
            end_stable_id: Some(live_id.clone()),
            marker_stable_id: None,
            files_touched: Vec::new(),
            opened_at_ms: 1,
            closed_at_ms: Some(2),
            evicted_at_ms: None,
            evicted_tokens: None,
            recall_count: 0,
            completable: false,
            archive: vec![archived.clone()],
        },
        persisted_episode! {
            seq: 2,
            title: "stale evicted".to_string(),
            status: EpisodeStatus::Evicted,
            goal: String::new(),
            card_md: "## Episode card".to_string(),
            close_reason: Some(EpisodeCloseReason::TitleChange),
            start_stable_id: "old-start".to_string(),
            end_stable_id: Some("old-end".to_string()),
            marker_stable_id: Some("missing-marker".to_string()),
            files_touched: Vec::new(),
            opened_at_ms: 1,
            closed_at_ms: Some(2),
            evicted_at_ms: Some(3),
            evicted_tokens: Some(9_000),
            recall_count: 0,
            completable: false,
            archive: vec![archived],
        },
    ];

    agent.restore_episodes(rows);

    let repaired = episodes_of(&store);
    assert!(
        repaired
            .iter()
            .all(|episode| episode.status() == EpisodeStatus::Restored)
    );
    assert!(
        repaired
            .iter()
            .all(|episode| episode.marker_stable_id().is_none())
    );
    assert!(
        repaired
            .iter()
            .all(|episode| episode.evicted_at_ms().is_some())
    );
    assert!(repaired.iter().all(|episode| !episode.archive().is_empty()));
    assert!(
        agent
            .episodes_command_report()
            .contains("repaired after resume skew")
    );
}

#[tokio::test]
async fn snapshot_capture_gates_on_wired_store_and_signature_tracks_mutations() {
    let fixture = TestFixture::new();
    let unwired = Agent::builder(
        MockProvider::empty(),
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .build()
    .unwrap();
    let snapshot = crate::session_persist::AgentStateSnapshot::capture(&unwired);
    assert!(
        snapshot.episodes().is_none(),
        "unwired agents must not touch episode persistence"
    );
    assert_eq!(
        crate::session_persist::agent_state_signatures(&snapshot).episodes,
        0
    );

    let (mut agent, _store, _fixture2) = episode_agent();
    let empty = crate::session_persist::AgentStateSnapshot::capture(&agent);
    assert!(empty.episodes().is_some());
    let empty_signature = crate::session_persist::agent_state_signatures(&empty).episodes;

    push_user_turn(&mut agent, "task");
    let opened = crate::session_persist::AgentStateSnapshot::capture(&agent);
    let opened_signature = crate::session_persist::agent_state_signatures(&opened).episodes;
    assert_ne!(empty_signature, opened_signature);

    // A recall-counter-only mutation must still flush.
    if let Some(mut ledger) = agent.episode_ledger() {
        let seq = ledger
            .close_active(EpisodeCloseReason::Manual, "msg-end".to_string(), 2)
            .expect("active episode closes");
        assert!(ledger.evict_episodes(vec![EpisodeEvictionRecord {
            seq,
            marker_stable_id: "msg-marker".to_string(),
            evicted_at_ms: 3,
            evicted_tokens: 100,
            card_md: "## Episode card".to_string(),
            archive: vec![ArchivedEpisodeItem {
                stable_id: "msg-archive".to_string(),
                message: test_user_message("archived bytes"),
            }],
        }]));
    }
    let archived = crate::session_persist::AgentStateSnapshot::capture(&agent);
    let archived_signature = crate::session_persist::agent_state_signatures(&archived).episodes;
    agent
        .episode_ledger()
        .expect("episode ledger wired")
        .record_recall(1);
    let recalled = crate::session_persist::AgentStateSnapshot::capture(&agent);
    assert_ne!(
        archived_signature,
        crate::session_persist::agent_state_signatures(&recalled).episodes
    );
}

#[tokio::test]
async fn context_report_and_command_surface_episode_state() {
    let (mut agent, _store, _fixture) = episode_agent();
    push_user_turn(&mut agent, "surface me");
    set_title(&mut agent, "call-t1", "Surface task");

    let report = agent.context_report();
    assert_eq!(report.episodes.len(), 1);
    assert_eq!(report.episodes[0].title, "Surface task");
    assert_eq!(report.episodes[0].status_label, "active");
    assert!(report.episodes[0].live_span_messages.unwrap() >= 3);

    let text = agent.episodes_command_report();
    assert!(text.contains("#1"), "{text}");
    assert!(text.contains("Surface task"), "{text}");

    let fixture = TestFixture::new();
    let unwired = Agent::builder(
        MockProvider::empty(),
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .build()
    .unwrap();
    assert!(unwired.episodes_command_report().contains("disabled"));
    assert!(unwired.context_report().episodes.is_empty());
}

#[tokio::test]
async fn episode_archive_roundtrips_through_storage() {
    let fixture = crate::storage::test_utils::TestStorage::new().await;
    let session_id = fixture.start_session().await;

    let episodes = vec![
        persisted_episode! {
            seq: 1,
            title: "Evicted task".to_string(),
            status: EpisodeStatus::Evicted,
            goal: "the first task".to_string(),
            card_md: "## Episode card\n- Goal: the first task".to_string(),
            close_reason: Some(EpisodeCloseReason::TitleChange),
            start_stable_id: "msg-1".to_string(),
            end_stable_id: Some("msg-7".to_string()),
            marker_stable_id: Some("msg-20".to_string()),
            files_touched: vec!["src/a.rs".to_string()],
            opened_at_ms: 100,
            closed_at_ms: Some(200),
            evicted_at_ms: Some(300),
            evicted_tokens: Some(21_000),
            recall_count: 2,
            completable: true,
            archive: vec![
                ArchivedEpisodeItem {
                    stable_id: "msg-1".to_string(),
                    message: test_user_message("archived user turn"),
                },
                ArchivedEpisodeItem {
                    stable_id: "msg-2".to_string(),
                    message: tool_result_message("call-1", "archived tool output"),
                },
            ],
        },
        persisted_episode! {
            seq: 2,
            title: "Live task".to_string(),
            status: EpisodeStatus::Active,
            goal: "the second task".to_string(),
            card_md: String::new(),
            close_reason: None,
            start_stable_id: "msg-21".to_string(),
            end_stable_id: None,
            marker_stable_id: None,
            files_touched: Vec::new(),
            opened_at_ms: 400,
            closed_at_ms: None,
            evicted_at_ms: None,
            evicted_tokens: None,
            recall_count: 0,
            completable: false,
            archive: Vec::new(),
        },
    ];

    fixture
        .storage
        .replace_episodes_snapshot(session_id, &episodes)
        .await
        .expect("episodes snapshot should persist");
    let loaded = fixture
        .storage
        .load_session_snapshot(session_id)
        .await
        .expect("snapshot should load")
        .expect("session exists");
    assert_eq!(loaded.episodes, episodes);

    // Snapshot-replace semantics: a smaller ledger fully replaces the old one,
    // and the FK cascade drops the orphaned archive rows.
    fixture
        .storage
        .replace_episodes_snapshot(session_id, &episodes[1..])
        .await
        .expect("replacement snapshot should persist");
    let reloaded = fixture
        .storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.episodes, episodes[1..]);
    assert_eq!(
        fixture
            .storage
            .count_episode_archive_rows(session_id)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn episode_schema_check_rejects_contradictory_status_rows() {
    let fixture = crate::storage::test_utils::TestStorage::new().await;
    let session_id = fixture.start_session().await;

    // A closed row without end/close_reason violates the status CHECK.
    let invalid = vec![persisted_episode! {
        seq: 1,
        title: String::new(),
        status: EpisodeStatus::Closed,
        goal: String::new(),
        card_md: String::new(),
        close_reason: None,
        start_stable_id: "msg-1".to_string(),
        end_stable_id: None,
        marker_stable_id: None,
        files_touched: Vec::new(),
        opened_at_ms: 100,
        closed_at_ms: Some(200),
        evicted_at_ms: None,
        evicted_tokens: None,
        recall_count: 0,
        completable: false,
        archive: Vec::new(),
    }];
    let result = fixture
        .storage
        .replace_episodes_snapshot(session_id, &invalid)
        .await;
    assert!(result.is_err(), "schema CHECK must reject the row");
}

// ─── P2: close-and-card + eviction ─────────────────────────────────────────

fn episode_agent_with_responses(
    responses: Vec<crate::provider::ProviderResult<StreamedResponse>>,
) -> (Agent, SharedEpisodeStore, TestFixture) {
    let fixture = TestFixture::new();
    let store = SharedEpisodeStore::default();
    let mut agent = Agent::builder(
        Box::new(MockProvider::new(responses)),
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .build()
    .expect("test agent should build");
    agent.set_episode_store(store.clone());
    (agent, store, fixture)
}

fn card_response(text: &str) -> crate::provider::ProviderResult<StreamedResponse> {
    Ok(StreamedResponse {
        content: text.to_string(),
        tool_calls: vec![],
        terminal: crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::Stop),
        usage: None,
        ..StreamedResponse::default()
    })
}

/// A complete tool group whose result carries `chars` of payload — bulk for
/// clearing the eviction economics guard.
fn push_bulky_tool_group(agent: &mut Agent, call_id: &str, path: &str, chars: usize) {
    agent.push_message(assistant_tool_call_message(
        call_id,
        "read",
        &format!(r#"{{"path":"{path}"}}"#),
    ));
    agent.push_message(tool_result_message(
        call_id,
        &format!("TOPIC_BULK {}", "x".repeat(chars)),
    ));
}

/// Standard closeable-then-evictable shape: topic A with two bulky groups,
/// then a user pivot and retitle to topic B. Returns the closed episode's seq.
fn close_bulky_episode(agent: &mut Agent, bulk_chars: usize) -> usize {
    push_user_turn(agent, "work on topic A");
    set_title(agent, "call-ta", "Topic A");
    push_bulky_tool_group(agent, "call-a1", "src/a.rs", bulk_chars);
    push_bulky_tool_group(agent, "call-a2", "src/b.rs", bulk_chars);
    push_user_turn(agent, "now switch to topic B");
    set_title(agent, "call-tb", "Topic B");
    1
}

/// Wire validity: every Tool message's call id must be pending from the
/// assistant message group it follows — the invariant Codex reasoning
/// threading and every chat transport depend on.
fn assert_wire_valid(messages: &[ChatCompletionRequestMessage]) {
    let mut pending: HashSet<String> = HashSet::new();
    for message in messages {
        if let Some(calls) = crate::context_view::assistant_tool_calls(message) {
            pending = calls.into_iter().map(|call| call.call_id).collect();
            continue;
        }
        if let Some(call_id) = crate::context_view::tool_message_call_id(message) {
            assert!(
                pending.remove(&call_id),
                "orphaned tool result for call {call_id}"
            );
        }
    }
}

fn assistant_text_message_for_tests(text: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::Assistant(
        async_openai::types::chat::ChatCompletionRequestAssistantMessageArgs::default()
            .content(text.to_string())
            .build()
            .unwrap(),
    )
}

/// Stable id of the first message whose content contains `needle`.
fn stable_id_containing(agent: &Agent, needle: &str) -> String {
    agent
        .messages
        .iter()
        .zip(agent.message_ids.iter())
        .find_map(|(message, id)| {
            message_content(message)
                .contains(needle)
                .then(|| id.clone())
        })
        .unwrap_or_else(|| panic!("no message contains {needle:?}"))
}

fn capture_sink() -> SharedSink {
    Arc::new(CaptureSink::default())
}

#[tokio::test]
async fn eviction_replaces_closed_episode_with_card_marker() {
    let (mut agent, store, _fixture) = episode_agent_with_responses(vec![card_response(
        "## Episode card\n- Goal: work on topic A\n- Outcome: topic A landed\n- Decisions: kept it simple\n- Files touched: src/a.rs, src/b.rs\n- Gotchas: none",
    )]);
    close_bulky_episode(&mut agent, 20_000);
    let before_len = agent.messages.len();

    let evicted = agent
        .apply_episode_evictions(
            &capture_sink(),
            CancellationToken::new(),
            EpisodeEvictionPass::CloseTime,
        )
        .await
        .unwrap();

    assert_eq!(evicted, 1);
    assert!(agent.messages.len() < before_len);
    assert_wire_valid(&agent.messages);

    let episodes = episodes_of(&store);
    let archived = &episodes[0];
    assert_eq!(archived.status(), EpisodeStatus::Evicted);
    assert!(archived.marker_stable_id().is_some());
    assert!(archived.evicted_at_ms().is_some());
    assert!(archived.evicted_tokens().unwrap() > 8_000);
    assert!(
        !archived.archive().is_empty(),
        "archive holds the span bytes"
    );
    assert!(
        archived.card_md().contains("topic A landed"),
        "provider polish used"
    );

    // The marker is self-describing: sentinel, card, recall + /ctx recovery.
    let marker_id = archived.marker_stable_id().unwrap().to_string();
    let marker_index = agent
        .message_ids
        .iter()
        .position(|id| *id == marker_id)
        .expect("marker is live");
    let marker_text = message_content(&agent.messages[marker_index]);
    assert!(marker_text.starts_with("Harness note:"), "{marker_text}");
    assert!(
        marker_text.contains("[Episode archived] #1 \"Topic A\""),
        "{marker_text}"
    );
    assert!(marker_text.contains("## Episode card"), "{marker_text}");
    assert!(
        marker_text.contains("recall with {\"episode\": 1}"),
        "{marker_text}"
    );
    assert!(marker_text.contains("/ctx"), "{marker_text}");
    // The bulk is gone from live context.
    assert!(
        !agent
            .messages
            .iter()
            .any(|message| message_content(message).contains("TOPIC_BULK")),
        "span bulk must leave the live buffer"
    );
    // Restore source rides the marker id.
    assert!(agent.summary_sources().contains_key(&marker_id));
    // Telemetry: one Episode rewrite attributed to the single evicted episode.
    assert_eq!(
        agent.pending_context_rewrite.kind,
        ContextRewriteKind::Episode
    );
    assert_eq!(agent.pending_context_rewrite.episode_seq, Some(1));
    assert!(agent.pending_context_rewrite.saved_tokens.unwrap() > 8_000);
}

#[tokio::test]
async fn episode_card_provider_usage_is_attributed_to_compaction_lane() {
    let response = StreamedResponse {
        content: "## Episode card\n- Goal: A\n- Outcome: done\n- Decisions: -\n- Files touched: -\n- Gotchas: -"
            .to_string(),
        terminal: crate::provider::StreamTerminal::Completed(
            crate::provider::FinishReason::Stop,
        ),
        usage: Some(crate::provider::TokenUsage {
            prompt_tokens: 42,
            completion_tokens: 7,
            input_cache: None,
        }),
        ..StreamedResponse::default()
    };
    let (mut agent, _store, _fixture) = episode_agent_with_responses(vec![Ok(response)]);
    close_bulky_episode(&mut agent, 20_000);

    agent
        .apply_episode_evictions(
            &capture_sink(),
            CancellationToken::new(),
            EpisodeEvictionPass::CloseTime,
        )
        .await
        .unwrap();

    let turn = agent.usage_turns().last().expect("hidden card usage turn");
    assert_eq!(turn.lane_kind, crate::agent::ExecutionLaneKind::Compaction);
    assert_eq!(turn.prompt_tokens, Some(42));
    assert_eq!(turn.completion_tokens, Some(7));
}

#[tokio::test]
async fn eviction_card_falls_back_to_deterministic_on_provider_error() {
    let (mut agent, store, _fixture) =
        episode_agent_with_responses(vec![Err(ProviderFailure::configuration("provider down"))]);
    close_bulky_episode(&mut agent, 20_000);

    let evicted = agent
        .apply_episode_evictions(
            &capture_sink(),
            CancellationToken::new(),
            EpisodeEvictionPass::CloseTime,
        )
        .await
        .unwrap();

    assert_eq!(evicted, 1);
    let episodes = episodes_of(&store);
    assert_eq!(episodes[0].status(), EpisodeStatus::Evicted);
    assert!(
        episodes[0].card_md().contains("- Goal: work on topic A"),
        "deterministic card fields present: {}",
        episodes[0].card_md()
    );
    assert!(
        episodes[0]
            .card_md()
            .contains("- Files touched: src/a.rs, src/b.rs")
    );
}

#[tokio::test]
async fn cancelled_card_call_leaves_episode_closed_and_context_untouched() {
    let (mut agent, store, _fixture) = episode_agent_with_responses(vec![card_response("ignored")]);
    close_bulky_episode(&mut agent, 20_000);
    let bytes_before = serde_json::to_string(&agent.messages).unwrap();

    let token = CancellationToken::new();
    token.cancel();
    let result = agent
        .apply_episode_evictions(&capture_sink(), token, EpisodeEvictionPass::CloseTime)
        .await;

    assert!(result.is_err(), "cancellation must propagate");
    assert!(crate::agent::compaction::is_compaction_cancelled(
        result.as_ref().unwrap_err()
    ));
    assert_eq!(
        serde_json::to_string(&agent.messages).unwrap(),
        bytes_before
    );
    assert_eq!(episodes_of(&store)[0].status(), EpisodeStatus::Closed);
}

#[tokio::test]
async fn small_closed_episode_defers_when_pressure_rewrite_cannot_resolve_pressure() {
    let (mut agent, store, _fixture) = episode_agent_with_responses(vec![card_response(
        "## Episode card\n- Goal: small\n- Outcome: done\n- Decisions: none\n- Files touched: src/a.rs\n- Gotchas: none",
    )]);
    // Two modest groups: enough to close, not enough to clear >=8k tokens.
    push_user_turn(&mut agent, "small task");
    set_title(&mut agent, "call-ta", "Small task");
    push_bulky_tool_group(&mut agent, "call-a1", "src/a.rs", 3_000);
    push_bulky_tool_group(&mut agent, "call-a2", "src/b.rs", 3_000);
    push_user_turn(&mut agent, "next topic");
    set_title(&mut agent, "call-tb", "Next topic");
    assert_eq!(episodes_of(&store)[0].status(), EpisodeStatus::Closed);
    // The live successor topic dwarfs the closed span, so the closed episode
    // clears neither the absolute nor the percent arm of the economics guard.
    push_bulky_tool_group(&mut agent, "call-b1", "src/big.rs", 400_000);

    let evicted = agent
        .apply_episode_evictions(
            &capture_sink(),
            CancellationToken::new(),
            EpisodeEvictionPass::CloseTime,
        )
        .await
        .unwrap();
    assert_eq!(
        evicted, 0,
        "below the economics guard, close-time pass defers"
    );
    assert_eq!(episodes_of(&store)[0].status(), EpisodeStatus::Closed);

    let drained = agent
        .apply_episode_evictions(
            &capture_sink(),
            CancellationToken::new(),
            EpisodeEvictionPass::Pressure,
        )
        .await
        .unwrap();
    assert_eq!(
        drained, 0,
        "a small episode rewrite must not run immediately before pressure GC"
    );
    assert_eq!(episodes_of(&store)[0].status(), EpisodeStatus::Closed);
}

#[tokio::test]
async fn smol_mode_never_evicts_closed_episodes() {
    let (mut agent, store, _fixture) = episode_agent_with_responses(vec![card_response(
        "## Episode card\n- Goal: A\n- Outcome: done\n- Decisions: -\n- Files touched: -\n- Gotchas: -",
    )]);
    close_bulky_episode(&mut agent, 20_000);
    assert!(agent.set_smol_mode(true));

    let evicted = agent
        .apply_episode_evictions(
            &capture_sink(),
            CancellationToken::new(),
            EpisodeEvictionPass::Pressure,
        )
        .await
        .unwrap();

    assert_eq!(evicted, 0);
    assert_eq!(episodes_of(&store)[0].status(), EpisodeStatus::Closed);
}

#[tokio::test]
async fn pinned_row_defers_the_whole_episode() {
    let (mut agent, store, _fixture) = episode_agent_with_responses(vec![]);
    close_bulky_episode(&mut agent, 20_000);
    // Pin one row inside the closed span.
    let pinned_id = stable_id_containing(&agent, "TOPIC_BULK");
    agent.context_controls.insert(
        pinned_id,
        crate::agent::ContextControlState {
            pinned: true,
            ..Default::default()
        },
    );

    let evicted = agent
        .apply_episode_evictions(
            &capture_sink(),
            CancellationToken::new(),
            EpisodeEvictionPass::Pressure,
        )
        .await
        .unwrap();
    assert_eq!(evicted, 0, "a pinned row defers eviction wholesale");
    assert_eq!(episodes_of(&store)[0].status(), EpisodeStatus::Closed);
}

#[tokio::test]
async fn read_reuse_pointer_target_defers_the_whole_episode() {
    let (mut agent, store, _fixture) = episode_agent_with_responses(vec![]);
    close_bulky_episode(&mut agent, 20_000);
    agent.tool_context_details.insert(
        "reuse-call".to_string(),
        ToolContextDetail {
            call_id: "reuse-call".to_string(),
            name: "read".to_string(),
            arguments: r#"{"path":"src/a.rs"}"#.to_string(),
            read_evidence: None,
            result: ToolContextResult::Text {
                rendered: "reused earlier read".to_string(),
            },
            reuse_target_call_id: Some("call-a1".to_string()),
        },
    );

    let evicted = agent
        .apply_episode_evictions(
            &capture_sink(),
            CancellationToken::new(),
            EpisodeEvictionPass::Pressure,
        )
        .await
        .unwrap();

    assert_eq!(evicted, 0, "pointer targets are never amputated");
    assert_eq!(episodes_of(&store)[0].status(), EpisodeStatus::Closed);
}

#[tokio::test]
async fn system_row_in_span_defers_the_whole_episode() {
    let (mut agent, store, _fixture) = episode_agent_with_responses(vec![]);
    push_user_turn(&mut agent, "work on topic A");
    set_title(&mut agent, "call-ta", "Topic A");
    push_bulky_tool_group(&mut agent, "call-a1", "src/a.rs", 20_000);
    // A trusted harness note lands mid-episode as a System row.
    agent.push_harness_note("mid-episode guidance");
    push_bulky_tool_group(&mut agent, "call-a2", "src/b.rs", 20_000);
    push_user_turn(&mut agent, "switch to topic B");
    set_title(&mut agent, "call-tb", "Topic B");

    let evicted = agent
        .apply_episode_evictions(
            &capture_sink(),
            CancellationToken::new(),
            EpisodeEvictionPass::Pressure,
        )
        .await
        .unwrap();
    assert_eq!(evicted, 0, "a System row defers eviction wholesale");
    assert_eq!(episodes_of(&store)[0].status(), EpisodeStatus::Closed);
}

#[tokio::test]
async fn incomplete_group_in_span_defers_the_whole_episode() {
    let (mut agent, store, _fixture) = episode_agent_with_responses(vec![]);
    push_user_turn(&mut agent, "work on topic A");
    set_title(&mut agent, "call-ta", "Topic A");
    push_bulky_tool_group(&mut agent, "call-a1", "src/a.rs", 20_000);
    push_bulky_tool_group(&mut agent, "call-a2", "src/b.rs", 20_000);
    // An interrupted multi-call batch: two calls, one result.
    agent.push_message(ChatCompletionRequestMessage::Assistant(
        async_openai::types::chat::ChatCompletionRequestAssistantMessageArgs::default()
            .tool_calls(vec![
                async_openai::types::chat::ChatCompletionMessageToolCalls::Function(
                    async_openai::types::chat::ChatCompletionMessageToolCall {
                        id: "call-x1".to_string(),
                        function: async_openai::types::chat::FunctionCall {
                            name: "read".to_string(),
                            arguments: r#"{"path":"src/c.rs"}"#.to_string(),
                        },
                    },
                ),
                async_openai::types::chat::ChatCompletionMessageToolCalls::Function(
                    async_openai::types::chat::ChatCompletionMessageToolCall {
                        id: "call-x2".to_string(),
                        function: async_openai::types::chat::FunctionCall {
                            name: "read".to_string(),
                            arguments: r#"{"path":"src/d.rs"}"#.to_string(),
                        },
                    },
                ),
            ])
            .build()
            .unwrap(),
    ));
    agent.push_message(tool_result_message("call-x1", "partial"));
    push_user_turn(&mut agent, "switch to topic B");
    set_title(&mut agent, "call-tb", "Topic B");
    assert_eq!(episodes_of(&store)[0].status(), EpisodeStatus::Closed);

    let evicted = agent
        .apply_episode_evictions(
            &capture_sink(),
            CancellationToken::new(),
            EpisodeEvictionPass::Pressure,
        )
        .await
        .unwrap();
    assert_eq!(evicted, 0, "an incomplete group defers eviction wholesale");
}

#[tokio::test]
async fn multi_episode_batch_splices_in_one_pass_without_misalignment() {
    let (mut agent, store, _fixture) = episode_agent_with_responses(vec![
        card_response(
            "## Episode card\n- Goal: A\n- Outcome: done A\n- Decisions: -\n- Files touched: src/a.rs\n- Gotchas: -",
        ),
        card_response(
            "## Episode card\n- Goal: B\n- Outcome: done B\n- Decisions: -\n- Files touched: src/b.rs\n- Gotchas: -",
        ),
    ]);
    // Episode 1 (Topic A) and episode 2 (Topic B) both close; Topic C active.
    push_user_turn(&mut agent, "topic A");
    set_title(&mut agent, "call-ta", "Topic A");
    push_bulky_tool_group(&mut agent, "call-a1", "src/a.rs", 20_000);
    push_bulky_tool_group(&mut agent, "call-a2", "src/a2.rs", 20_000);
    push_user_turn(&mut agent, "topic B");
    set_title(&mut agent, "call-tb", "Topic B");
    push_bulky_tool_group(&mut agent, "call-b1", "src/b.rs", 20_000);
    push_bulky_tool_group(&mut agent, "call-b2", "src/b2.rs", 20_000);
    push_user_turn(&mut agent, "topic C");
    set_title(&mut agent, "call-tc", "Topic C");

    let evicted = agent
        .apply_episode_evictions(
            &capture_sink(),
            CancellationToken::new(),
            EpisodeEvictionPass::Pressure,
        )
        .await
        .unwrap();

    assert_eq!(evicted, 2);
    assert_eq!(
        agent.messages.len(),
        agent.message_ids.len(),
        "no id misalignment"
    );
    assert_wire_valid(&agent.messages);
    let episodes = episodes_of(&store);
    assert_eq!(episodes[0].status(), EpisodeStatus::Evicted);
    assert_eq!(episodes[1].status(), EpisodeStatus::Evicted);
    assert_eq!(episodes[2].status(), EpisodeStatus::Active);
    // Marker order preserved: episode 1's marker precedes episode 2's.
    let first_marker = agent
        .message_ids
        .iter()
        .position(|id| Some(id.as_str()) == episodes[0].marker_stable_id())
        .unwrap();
    let second_marker = agent
        .message_ids
        .iter()
        .position(|id| Some(id.as_str()) == episodes[1].marker_stable_id())
        .unwrap();
    assert!(first_marker < second_marker);
    // Multi-episode batches deliberately leave per-turn attribution NULL.
    assert_eq!(
        agent.pending_context_rewrite.kind,
        ContextRewriteKind::Episode
    );
    assert_eq!(agent.pending_context_rewrite.episode_seq, None);
}

#[tokio::test]
async fn marker_bytes_stay_stable_across_subsequent_passes() {
    let (mut agent, _store, _fixture) = episode_agent_with_responses(vec![card_response(
        "## Episode card\n- Goal: A\n- Outcome: done\n- Decisions: -\n- Files touched: -\n- Gotchas: -",
    )]);
    close_bulky_episode(&mut agent, 20_000);
    agent
        .apply_episode_evictions(
            &capture_sink(),
            CancellationToken::new(),
            EpisodeEvictionPass::CloseTime,
        )
        .await
        .unwrap();

    let bytes = serde_json::to_string(&agent.messages).unwrap();
    // Re-running the eviction and byte-stable GC passes must not touch a byte.
    agent
        .apply_episode_evictions(
            &capture_sink(),
            CancellationToken::new(),
            EpisodeEvictionPass::Pressure,
        )
        .await
        .unwrap();
    agent.apply_context_gc(false);
    assert_eq!(serde_json::to_string(&agent.messages).unwrap(), bytes);
}

#[tokio::test]
async fn ctx_restore_splices_originals_back_and_marks_episode_restored() {
    let (mut agent, store, _fixture) = episode_agent_with_responses(vec![card_response(
        "## Episode card\n- Goal: A\n- Outcome: done\n- Decisions: -\n- Files touched: -\n- Gotchas: -",
    )]);
    close_bulky_episode(&mut agent, 20_000);
    agent
        .apply_episode_evictions(
            &capture_sink(),
            CancellationToken::new(),
            EpisodeEvictionPass::CloseTime,
        )
        .await
        .unwrap();
    let marker_id = episodes_of(&store)[0]
        .marker_stable_id()
        .map(str::to_string)
        .expect("evicted episode has a marker");

    let restored = agent.apply_context_control_action(
        &marker_id,
        crate::agent::ContextControlAction::RestoreSummarySource,
    );

    assert!(restored);
    assert!(
        agent
            .messages
            .iter()
            .any(|message| message_content(message).contains("TOPIC_BULK")),
        "originals return verbatim"
    );
    assert_wire_valid(&agent.messages);
    let episodes = episodes_of(&store);
    let episode = &episodes[0];
    assert_eq!(episode.status(), EpisodeStatus::Restored);
    assert_eq!(episode.marker_stable_id(), None);
    // The ledger span was rewritten onto the newly allocated live ids.
    assert!(
        agent
            .message_ids
            .iter()
            .any(|id| id == episode.start_stable_id())
    );
    assert!(
        agent
            .message_ids
            .iter()
            .any(|id| Some(id.as_str()) == episode.end_stable_id())
    );
    assert!(
        !episode.archive().is_empty(),
        "the archive stays canonical for recall"
    );

    // A restored episode never becomes an eviction candidate again.
    let re_evicted = agent
        .apply_episode_evictions(
            &capture_sink(),
            CancellationToken::new(),
            EpisodeEvictionPass::Pressure,
        )
        .await
        .unwrap();
    assert_eq!(re_evicted, 0);
}

#[tokio::test]
async fn ctx_restore_recognizes_episode_folded_into_later_summary_source() {
    let (mut agent, store, _fixture) = episode_agent_with_responses(vec![card_response(
        "## Episode card\n- Goal: A\n- Outcome: done\n- Decisions: -\n- Files touched: -\n- Gotchas: -",
    )]);
    close_bulky_episode(&mut agent, 20_000);
    agent
        .apply_episode_evictions(
            &capture_sink(),
            CancellationToken::new(),
            EpisodeEvictionPass::CloseTime,
        )
        .await
        .unwrap();
    let episode = episodes_of(&store)[0].clone();
    let marker_id = episode.marker_stable_id().unwrap().to_string();
    let marker_index = agent
        .message_ids
        .iter()
        .position(|id| id == &marker_id)
        .unwrap();
    let nested_source = episode
        .archive()
        .iter()
        .map(|item| item.message.clone())
        .collect::<Vec<_>>();
    let summary_id = "msg-nested-summary".to_string();
    agent.messages[marker_index] = test_user_message("# Compacted Context Summary");
    agent.message_ids[marker_index] = summary_id.clone();
    agent.summary_sources.remove(&marker_id);
    agent
        .summary_sources
        .insert(summary_id.clone(), nested_source);

    assert!(agent.apply_context_control_action(
        &summary_id,
        crate::agent::ContextControlAction::RestoreSummarySource,
    ));

    let restored = &episodes_of(&store)[0];
    assert_eq!(restored.status(), EpisodeStatus::Restored);
    assert!(
        agent
            .message_ids
            .iter()
            .any(|id| id == restored.start_stable_id())
    );
    assert!(
        agent
            .message_ids
            .iter()
            .any(|id| Some(id.as_str()) == restored.end_stable_id())
    );
}

#[tokio::test]
async fn evicted_episode_roundtrips_through_persistence() {
    let (mut agent, store, _fixture) = episode_agent_with_responses(vec![card_response(
        "## Episode card\n- Goal: A\n- Outcome: done\n- Decisions: -\n- Files touched: -\n- Gotchas: -",
    )]);
    close_bulky_episode(&mut agent, 20_000);
    agent
        .apply_episode_evictions(
            &capture_sink(),
            CancellationToken::new(),
            EpisodeEvictionPass::CloseTime,
        )
        .await
        .unwrap();
    let snapshot = episodes_of(&store);

    let fixture = crate::storage::test_utils::TestStorage::new().await;
    let session_id = fixture.start_session().await;
    fixture
        .storage
        .replace_episodes_snapshot(session_id, &snapshot)
        .await
        .expect("evicted ledger persists");
    let loaded = fixture
        .storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.episodes, snapshot);
}

#[tokio::test]
async fn pressure_drain_prefers_episodes_over_gc_and_compaction() {
    // 48k window: usable input = 32k, GC trigger = 24k. One closed episode
    // carries ~26k tokens of bulk, so preflight crosses the trigger; draining
    // the episode alone must bring the prompt back down with no Tier-1 stubs
    // and no compaction summary.
    let (mut agent, store, _fixture) = episode_agent_with_responses(vec![card_response(
        "## Episode card\n- Goal: A\n- Outcome: done\n- Decisions: -\n- Files touched: -\n- Gotchas: -",
    )]);
    agent.set_context_budget_tokens(48_000);
    push_user_turn(&mut agent, "topic A");
    set_title(&mut agent, "call-ta", "Topic A");
    push_bulky_tool_group(&mut agent, "call-a1", "src/a.rs", 50_000);
    push_bulky_tool_group(&mut agent, "call-a2", "src/b.rs", 54_000);
    push_user_turn(&mut agent, "topic B");
    // Arm the boundary but skip the helper's own preflight so the close and
    // the eviction both happen inside prepare_context_for_model.
    let args = r#"{"title":"Topic B","episode_action":"new_topic"}"#;
    agent.push_message(assistant_tool_call_message(
        "call-tb",
        "set_session_title",
        args,
    ));
    agent.push_message(tool_result_message(
        "call-tb",
        "Session title set to: Topic B",
    ));
    agent.observe_episode_title_result(&test_tool_call("call-tb", "set_session_title", args));

    let tool_schema = agent.active_tool_schema();
    let mut perf = PreflightPerfCapture::default();
    let request_messages = agent
        .prepare_context_for_model(
            &tool_schema,
            &capture_sink(),
            CancellationToken::new(),
            &mut perf,
        )
        .await
        .expect("preflight succeeds");

    let episodes = episodes_of(&store);
    assert_eq!(
        episodes[0].status(),
        EpisodeStatus::Evicted,
        "episode drained"
    );
    assert_eq!(
        agent.pending_context_rewrite.kind,
        ContextRewriteKind::Episode
    );
    assert!(
        !agent
            .context_controls
            .values()
            .any(|state| state.stub_reason
                == Some(crate::agent::ContextStubReason::OldSuccessfulToolOutput)),
        "relevance-driven eviction ran before Tier-1 old-output stubs"
    );
    assert!(
        !request_messages
            .iter()
            .any(|message| message_content(message).contains("# Compacted Context Summary")),
        "no pressure compaction was needed"
    );
    assert!(
        !request_messages
            .iter()
            .any(|message| message_content(message).contains("TOPIC_BULK")),
        "the drained episode's bulk left the outgoing prompt"
    );
}

#[test]
fn episode_rewrite_kind_roundtrips_and_ranks_between_manual_and_compaction() {
    assert_eq!(ContextRewriteKind::Episode.as_db_str(), "episode");
    assert_eq!(
        ContextRewriteKind::from_db_str("episode"),
        ContextRewriteKind::Episode
    );
    assert!(ContextRewriteKind::Episode.priority() > ContextRewriteKind::Manual.priority());
    assert!(ContextRewriteKind::Episode.priority() < ContextRewriteKind::Compaction.priority());
}

#[test]
fn pending_rewrite_aggregates_priority_and_single_episode_attribution() {
    let mut pending = crate::agent::usage_ledger::PendingContextRewrite::default();
    pending.record(ContextRewriteKind::Gc, 1_000);
    pending.record(ContextRewriteKind::Episode, 9_000);
    pending.record_episode_evictions(&[3]);
    assert_eq!(pending.kind, ContextRewriteKind::Episode);
    assert_eq!(pending.episode_seq, Some(3));
    assert_eq!(pending.saved_tokens, Some(10_000));

    // A same-turn compaction outranks the episode kind but keeps attribution.
    pending.record(ContextRewriteKind::Compaction, 2_000);
    assert_eq!(pending.kind, ContextRewriteKind::Compaction);
    assert_eq!(pending.episode_seq, Some(3));

    // A second eviction batch before the flush clears single-episode attribution.
    pending.record_episode_evictions(&[5]);
    assert_eq!(pending.episode_seq, None);

    let taken = pending.take();
    assert_eq!(taken.kind, ContextRewriteKind::Compaction);
    assert_eq!(
        pending,
        crate::agent::usage_ledger::PendingContextRewrite::default()
    );
}

#[tokio::test]
async fn deterministic_card_extracts_goal_outcome_files_and_gotchas() {
    let (mut agent, store, _fixture) = episode_agent_with_responses(vec![]);
    push_user_turn(&mut agent, "fix the storage flake before release");
    set_title(&mut agent, "call-ta", "Fix storage flake");
    push_bulky_tool_group(&mut agent, "call-a1", "src/storage/mod.rs", 9_000);
    agent.push_message(assistant_text_message_for_tests(
        "The flake came from an unawaited checkpoint; fixed and verified.",
    ));
    agent
        .episode_ledger()
        .expect("episode ledger wired")
        .mark_active_completable();
    push_user_turn(&mut agent, "next topic");
    set_title(&mut agent, "call-tb", "Next");

    let episodes = episodes_of(&store);
    let episode = episodes[0].clone();
    let start = agent
        .message_ids
        .iter()
        .position(|id| *id == episode.start_stable_id())
        .unwrap();
    let end = agent
        .message_ids
        .iter()
        .position(|id| Some(id.as_str()) == episode.end_stable_id())
        .unwrap();
    let span = agent.messages[start..=end].to_vec();
    let card = agent.deterministic_episode_card(&episode, &span).await;

    assert!(card.starts_with("## Episode card"), "{card}");
    assert!(
        card.contains("- Goal: fix the storage flake before release"),
        "{card}"
    );
    assert!(card.contains("unawaited checkpoint"), "{card}");
    assert!(card.contains("src/storage/mod.rs"), "{card}");
    assert!(card.contains("- Completion signal:"), "{card}");
    assert!(card.chars().count() <= 1_600);
}

// ─── P3: recall integration on the agent side ──────────────────────────────

/// Push a completed `recall` tool group with a registered context detail, the
/// shape `apply_tool_result` leaves behind for a real recall call.
fn push_recall_result(agent: &mut Agent, call_id: &str, arguments: &str, content: &str) {
    agent
        .messages
        .push(assistant_tool_call_message(call_id, "recall", arguments));
    agent.message_ids.push(format!("{call_id}-assistant"));
    agent.tool_context_details.insert(
        call_id.to_string(),
        ToolContextDetail {
            call_id: call_id.to_string(),
            name: "recall".to_string(),
            arguments: arguments.to_string(),
            read_evidence: None,
            result: ToolContextResult::Text {
                rendered: content.to_string(),
            },
            reuse_target_call_id: None,
        },
    );
    agent.messages.push(tool_result_message(call_id, content));
    agent.message_ids.push(format!("{call_id}-result"));
}

#[tokio::test]
async fn repeat_recall_collapses_to_pointer_only_while_prior_page_is_live() {
    let (mut agent, _store, _fixture) = episode_agent();
    push_user_turn(&mut agent, "task");
    let args = r#"{"episode":3}"#;
    let page =
        "<<<untrusted-content source=\"episode:3\">>>\narchived bytes\n<<<end-untrusted-content>>>";
    push_recall_result(&mut agent, "recall-1", args, page);

    // Identical args + identical live bytes → pointer.
    let pointer = agent
        .recall_reuse_pointer(&test_tool_call("recall-2", "recall", args), page)
        .expect("a live identical prior page collapses to a pointer");
    assert!(pointer.starts_with("[reused previous recall]"), "{pointer}");
    assert!(pointer.contains("recall-1"), "{pointer}");

    // Different args (another page) never collapse.
    assert!(
        agent
            .recall_reuse_pointer(
                &test_tool_call("recall-3", "recall", r#"{"episode":3,"cursor":"1.0"}"#),
                page,
            )
            .is_none(),
        "a different page is fresh content"
    );

    // Once the prior page is GC-stubbed, the recall re-executes with real
    // bytes — pointers must never point at a stub (annotate, not amputate).
    let stub = "[Compacted tool output]\ncall_id: recall-1";
    let index = agent
        .messages
        .iter()
        .position(|message| {
            crate::context_view::tool_message_call_id(message).as_deref() == Some("recall-1")
        })
        .unwrap();
    agent.messages[index] = tool_result_message("recall-1", stub);
    assert!(
        agent
            .recall_reuse_pointer(&test_tool_call("recall-4", "recall", args), page)
            .is_none(),
        "a stubbed prior page must not satisfy the dedup"
    );
}

#[tokio::test]
async fn recalled_output_is_gc_stubbable_under_pressure() {
    let (mut agent, _store, _fixture) = episode_agent();
    agent.set_context_budget_tokens(33_000);
    push_user_turn(&mut agent, "old work");
    // An old recalled page big enough for the old-output stub tier.
    let recalled = format!(
        "<<<untrusted-content source=\"episode:1\">>>\n{}RECALL_END\n<<<end-untrusted-content>>>",
        "archived line\n".repeat(4_000)
    );
    push_recall_result(&mut agent, "recall-old", r#"{"episode":1}"#, &recalled);
    // Enough newer groups to push the recall result out of the protected tail.
    for index in 0..4 {
        push_tool_group(&mut agent, &format!("call-r{index}"), "src/x.rs");
        push_user_turn(&mut agent, &format!("later turn {index}"));
    }

    assert!(
        agent.apply_context_gc(true) > 0,
        "the pressure pass reclaims the old recall page"
    );
    let stubbed = agent
        .context_controls()
        .get("tool-recall-old")
        .is_some_and(|state| state.stubbed);
    assert!(
        stubbed,
        "recall outputs must be GC-eligible or every recall becomes permanent dead weight: {:?}",
        agent.context_controls()
    );
}
