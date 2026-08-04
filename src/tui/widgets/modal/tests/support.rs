use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn render_to_buffer(area: Rect, command: &str, scroll: u16) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    let app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
    terminal
        .draw(|frame| {
            render_confirm_prompt(frame, area, &app, &permission_prompt(command, None), scroll);
        })
        .expect("permission prompt should render");
    terminal.backend().buffer().clone()
}

pub(super) fn render_block_detail_to_buffer(
    area: Rect,
    text: &str,
    scroll: u16,
) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
    app.transcript.push(TranscriptItem::AssistantMessage {
        text: text.to_string(),
    });
    app.modal_scroll = scroll;
    terminal
        .draw(|frame| {
            render_block_detail(frame, area, &app, 0);
        })
        .expect("block detail should render");
    terminal.backend().buffer().clone()
}

pub(super) fn render_tool_detail_to_buffer(
    area: Rect,
    result: &str,
    scroll: u16,
) -> ratatui::buffer::Buffer {
    render_tool_detail_with_arguments_to_buffer(area, r#"{"path":"src/main.rs"}"#, result, scroll)
}

pub(super) fn render_tool_detail_with_arguments_to_buffer(
    area: Rect,
    arguments: &str,
    result: &str,
    scroll: u16,
) -> ratatui::buffer::Buffer {
    render_tool_detail_activity_to_buffer(area, "read", arguments, result, None, scroll)
}

pub(super) fn render_tool_detail_activity_to_buffer(
    area: Rect,
    name: &str,
    arguments: &str,
    result: &str,
    diff: Option<crate::diff::FileDiff>,
    scroll: u16,
) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
    let now = std::time::Instant::now();
    app.transcript
        .push(TranscriptItem::ToolActivity(ToolActivity {
            id: "call-1".to_string(),
            name: name.to_string(),
            arguments: arguments.to_string(),
            status: ToolStatus::Succeeded,
            result: Some(result.to_string()),
            diff,
            started_at: now,
            finished_at: Some(now),
        }));
    app.modal_scroll = scroll;
    terminal
        .draw(|frame| {
            render_tool_detail(frame, area, &app, "call-1");
        })
        .expect("tool detail should render");
    terminal.backend().buffer().clone()
}

pub(super) fn render_diff_preview_to_buffer(
    area: Rect,
    diff: crate::diff::FileDiff,
    scroll: u16,
) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
    let now = std::time::Instant::now();
    app.transcript
        .push(TranscriptItem::ToolActivity(ToolActivity {
            id: "call-1".to_string(),
            name: "edit".to_string(),
            arguments: "{}".to_string(),
            status: ToolStatus::Succeeded,
            result: None,
            diff: Some(diff),
            started_at: now,
            finished_at: Some(now),
        }));
    app.modal_scroll = scroll;
    terminal
        .draw(|frame| {
            render_diff_preview(frame, area, &app, "call-1");
        })
        .expect("diff preview should render");
    terminal.backend().buffer().clone()
}

/// A modified-file diff whose single added line carries `tail` at its end after
/// `pad` filler columns — wide enough to wrap in a narrow modal so the tail only
/// shows once the scroll clamp accounts for wrapped rows.
pub(super) fn file_diff_with_wide_line(tail: &str, pad: usize) -> crate::diff::FileDiff {
    use crate::diff::{DiffHunk, DiffLine, DiffLineKind, DiffStatus, FileDiff};
    FileDiff {
        path: "src/main.rs".to_string(),
        status: DiffStatus::Modified,
        hunks: vec![DiffHunk {
            old_start: 1,
            new_start: 1,
            lines: vec![DiffLine {
                kind: DiffLineKind::Added,
                content: format!("{} {tail}", "x".repeat(pad)),
                old_line: None,
                new_line: Some(1),
            }],
        }],
        truncated: false,
        old_size: Some(10),
        new_size: 20,
        added_lines: 1,
        removed_lines: 0,
        additional_files: Box::default(),
    }
}

pub(super) fn render_context_to_buffer(
    area: Rect,
    report: crate::agent::ContextReport,
    scroll: u16,
    view_mode: ContextViewMode,
) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    let mut app = AppState::new("codex", "gpt-5".to_string(), ".".to_string(), None);
    app.modal_scroll = scroll;
    app.context_state.view_mode = view_mode;
    terminal
        .draw(|frame| {
            render_context(frame, area, &app, &report);
        })
        .expect("context modal should render");
    terminal.backend().buffer().clone()
}

pub(super) fn render_episodes_to_buffer(
    area: Rect,
    report: crate::agent::ContextReport,
    cursor: usize,
    scroll: u16,
) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    let mut app = AppState::new("codex", "gpt-5".to_string(), ".".to_string(), None);
    app.modal_scroll = scroll;
    terminal
        .draw(|frame| {
            render_episodes(frame, area, &app, &report, cursor);
        })
        .expect("episodes modal should render");
    terminal.backend().buffer().clone()
}

pub(super) fn render_context_wire_expanded_to_buffer(
    area: Rect,
    report: crate::agent::ContextReport,
    expanded: &[&str],
) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    let mut app = AppState::new("codex", "gpt-5".to_string(), ".".to_string(), None);
    app.context_state.view_mode = ContextViewMode::Wire;
    app.context_state.wire_expanded = expanded.iter().map(|id| (*id).to_string()).collect();
    terminal
        .draw(|frame| {
            render_context(frame, area, &app, &report);
        })
        .expect("context modal should render");
    terminal.backend().buffer().clone()
}

pub(super) fn render_context_turns_expanded_to_buffer(
    area: Rect,
    report: crate::agent::ContextReport,
    expanded: &[usize],
) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    let mut app = AppState::new("codex", "gpt-5".to_string(), ".".to_string(), None);
    app.context_state.view_mode = ContextViewMode::Turns;
    app.context_state.turns_expanded = expanded.iter().copied().collect();
    terminal
        .draw(|frame| {
            render_context(frame, area, &app, &report);
        })
        .expect("context modal should render");
    terminal.backend().buffer().clone()
}

/// Three turns telling one cache story: warm, then cold with a matching
/// compaction event (hash aaaa→bbbb), then partial with unexplained churn
/// (bbbb→cccc).
pub(super) fn context_report_with_usage_turns() -> crate::agent::ContextReport {
    let base_ms = 1_700_000_000_000_i64;
    let turn = |seq: usize| crate::agent::UsageTurnReport {
        seq,
        lane_kind: crate::agent::ExecutionLaneKind::Parent,
        lane_id: "parent-42".to_string(),
        lane_seq: seq,
        parent_tool_call_id: None,
        launch_group_id: None,
        status: crate::agent::UsageTurnStatus::Reported,
        finish_reason: Some(crate::provider::FinishReason::ToolCalls),
        reasoning_chars: 4_800,
        provider_attempts: Vec::new(),
        provider_id: None,
        model: None,
        effective_reasoning: None,
        prompt_tokens: Some(42_000),
        completion_tokens: Some(1_200),
        cache_read_input_tokens: Some(38_600),
        cache_creation_input_tokens: Some(0),
        cache_measured_input_tokens: Some(42_000),
        turn_cost_micros: Some(21_000),
        no_cache_cost_micros: Some(130_000),
        estimated_prompt_tokens: Some(41_800),
        estimate_source: Some(crate::provider::TokenCounterKind::Tiktoken),
        estimate_confidence: Some(crate::provider::EstimateConfidence::High),
        tool_schema_tokens: Some(7_300),
        tool_schema_hash: Some("schema1111".to_string()),
        tool_schema_names: vec!["read".to_string(), "bash".to_string()],
        request_body_bytes: Some(82_000),
        request_body_hash: Some("request1111".to_string()),
        cache_mechanism: Some("cache_control".to_string()),
        cache_route_fingerprint: None,
        expected_cacheable_percent: Some(91),
        actual_cache_read_percent: Some(92),
        local_reusable_prefix_tokens: Some(37_000),
        local_reusable_prefix_percent: Some(90),
        cacheable_prefix_tokens: Some(38_000),
        volatile_tail_tokens: Some(4_200),
        context_window_tokens: Some(200_000),
        rewrite_kind: crate::agent::ContextRewriteKind::None,
        rewrite_saved_tokens: None,
        episode_seq: None,
        created_at_ms: base_ms + seq as i64 * 60_000,
        latency_ms: Some(3_400),
        ttft_ms: Some(600),
        prefix_hash: Some("aaaa1111".to_string()),
        inspection_executed: 1,
        inspection_reused: 2,
        inspection_rejected: 0,
        inspection_returned_chars: 8_000,
        inspection_avoided_chars: 12_000,
        delegated_parent_overlap: 0,
    };
    let warm = turn(1);
    let mut cold = turn(2);
    cold.cache_read_input_tokens = Some(0);
    cold.actual_cache_read_percent = Some(0);
    cold.cache_creation_input_tokens = Some(43_000);
    cold.rewrite_kind = crate::agent::ContextRewriteKind::Compaction;
    cold.prefix_hash = Some("bbbb2222".to_string());
    let mut churned = turn(3);
    churned.cache_read_input_tokens = Some(29_800);
    churned.actual_cache_read_percent = Some(70);
    churned.prefix_hash = Some("cccc3333".to_string());
    let compaction = crate::agent::CompactionEvent {
        seq: 1,
        occurred_at_ms: base_ms + 90_000,
        before_tokens: 162_200,
        after_tokens: 101_000,
        messages_omitted: 5,
        tool_outputs_stubbed: 12,
        summary_available: true,
        prefix_hash_before: Some("aaaa1111".to_string()),
        prefix_hash_after: Some("bbbb2222".to_string()),
        cacheable_prefix_tokens_before: Some(140_000),
        cacheable_prefix_tokens_after: Some(95_000),
        ..Default::default()
    };
    crate::agent::ContextReport {
        budget_tokens: 200_000,
        usage_turns: vec![warm, cold, churned],
        compaction_events: vec![compaction],
        session_prompt_tokens: 126_000,
        session_completion_tokens: 3_600,
        ..Default::default()
    }
}

pub(super) fn context_report_with_rows(row_count: usize) -> crate::agent::ContextReport {
    let metadata_source = crate::provider::TokenCounterKind::Heuristic;
    let metadata_confidence = crate::provider::EstimateConfidence::Low;
    let tokens = row_count.saturating_mul(10);
    crate::agent::ContextReport {
        budget_tokens: 120_000,
        entries: vec![crate::agent::ContextEntry {
            role: crate::agent::ContextRole::User,
            tokens,
            text: "context rows".to_string(),
        }],
        ledger: (0..row_count)
            .map(|index| crate::agent::ContextNode {
                id: format!("msg-{index}").into(),
                kind: crate::agent::ContextNodeKind::ChatMessage,
                inclusion: crate::agent::ContextInclusion::Included,
                role: Some(crate::agent::ContextRole::User),
                label: format!("Message {index}"),
                tokens: 10,
                chars: 8,
                bytes: 8,
                source: metadata_source,
                confidence: metadata_confidence,
                preview: String::new(),
                sources: Vec::new(),
                children: Vec::new(),
            })
            .collect(),
        estimate_source: metadata_source,
        estimate_confidence: metadata_confidence,
        prompt_estimate_tokens: tokens,
        ..Default::default()
    }
}

pub(super) fn context_report_with_wire_preview() -> crate::agent::ContextReport {
    let metadata_source = crate::provider::TokenCounterKind::Heuristic;
    let metadata_confidence = crate::provider::EstimateConfidence::Low;
    let system_text = "You are a coding agent.\n\nStyle:\n- Direct.\n\n# Project context\n\n## Environment\n- cwd: /repo";
    let ledger_node =
        |id: &str, label: &str, kind: crate::agent::ContextNodeKind| crate::agent::ContextNode {
            id: id.into(),
            kind,
            inclusion: crate::agent::ContextInclusion::Included,
            role: Some(crate::agent::ContextRole::System),
            label: label.to_string(),
            tokens: 10,
            chars: label.len(),
            bytes: label.len(),
            source: metadata_source,
            confidence: metadata_confidence,
            preview: String::new(),
            sources: Vec::new(),
            children: Vec::new(),
        };
    crate::agent::ContextReport {
        budget_tokens: 120_000,
        entries: vec![crate::agent::ContextEntry {
            role: crate::agent::ContextRole::System,
            tokens: 30,
            text: "ledger rows".to_string(),
        }],
        ledger: vec![
            ledger_node(
                "ledger-system",
                "System group",
                crate::agent::ContextNodeKind::SystemRoot,
            ),
            ledger_node(
                "ledger-chat",
                "Chat group",
                crate::agent::ContextNodeKind::ChatRoot,
            ),
            ledger_node(
                "ledger-tools",
                "Tools group",
                crate::agent::ContextNodeKind::ToolsRoot,
            ),
        ],
        estimate_source: metadata_source,
        estimate_confidence: metadata_confidence,
        prompt_estimate_tokens: 30,
        payload_preview: Some(crate::provider::ProviderRequestPreview::with_wire_sections(
            "POST",
            "/v1/messages",
            serde_json::json!({
                "tools": [{"name": "read"}],
                "system": system_text,
                "messages": [{"role": "user", "content": "hello"}],
            }),
            vec![
                crate::provider::ProviderWireSection::from_value(
                    "wire-messages",
                    "Messages",
                    "$.messages",
                    &serde_json::json!([{"role": "user", "content": "hello"}]),
                    None,
                ),
                crate::provider::ProviderWireSection::from_value(
                    "wire-system",
                    "System",
                    "$.system",
                    &serde_json::json!(system_text),
                    None,
                ),
                crate::provider::ProviderWireSection::from_value(
                    "wire-tools",
                    "Tools",
                    "$.tools",
                    &serde_json::json!([{"name": "read"}]),
                    None,
                ),
            ],
        )),
        ..Default::default()
    }
}

pub(super) fn render_help_to_buffer(area: Rect) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
    app.modal = Some(ModalKind::Detail(crate::tui::event::DetailModal::Help));
    terminal
        .draw(|frame| {
            render(frame, area, &app);
        })
        .expect("help modal should render");
    terminal.backend().buffer().clone()
}

pub(super) fn render_command_help_to_buffer(area: Rect) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
    app.modal = Some(ModalKind::Detail(
        crate::tui::event::DetailModal::CommandHelp,
    ));
    terminal
        .draw(|frame| {
            render(frame, area, &app);
        })
        .expect("command help modal should render");
    terminal.backend().buffer().clone()
}

pub(super) fn render_question_to_buffer(area: Rect, scroll: u16) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    let options = (0..12)
        .map(|index| crate::interaction::QuestionOption {
            label: format!("Option {index} has a label that wraps in narrow modals"),
            description: format!("Description {index} wraps as well"),
            preselected: false,
        })
        .collect::<Vec<_>>();
    terminal
        .draw(|frame| {
            render_question_prompt(
                frame,
                area,
                "Pick one",
                Some("Question header"),
                None,
                &options,
                false,
                8,
                &vec![false; options.len()],
                scroll,
            );
        })
        .expect("question prompt should render");
    terminal.backend().buffer().clone()
}

pub(super) fn render_sessions_to_buffer(
    area: Rect,
    sessions: &[crate::storage::SessionSummary],
    cursor: usize,
) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    terminal
        .draw(|frame| {
            render_session_picker(frame, area, sessions, cursor);
        })
        .expect("session picker should render");
    terminal.backend().buffer().clone()
}

pub(super) fn render_plans_to_buffer(
    area: Rect,
    plans: &[crate::storage::SavedPlanSummary],
    query: &str,
    cursor: usize,
) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    terminal
        .draw(|frame| {
            render_plan_picker(frame, area, plans, query, cursor);
        })
        .expect("plan picker should render");
    terminal.backend().buffer().clone()
}

pub(super) fn render_tasks_to_buffer(
    area: Rect,
    tasks: &[BackgroundTaskSnapshot],
    cursor: usize,
) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
    app.modal = Some(ModalKind::Manager(
        crate::tui::event::ManagerModal::TaskList {
            tasks: tasks.to_vec(),
            cursor,
        },
    ));
    terminal
        .draw(|frame| {
            render(frame, area, &app);
        })
        .expect("task list should render");
    terminal.backend().buffer().clone()
}

pub(super) fn render_authorize_providers_to_buffer(area: Rect) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    let app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
    let providers = vec![
        ProviderOption {
            provider_id: "opencode".to_string(),
            provider_label: "OpenCode Go".to_string(),
            authorized: true,
            current: true,
            uses_endpoint_auth_form: false,
        },
        ProviderOption {
            provider_id: "codex".to_string(),
            provider_label: "Codex".to_string(),
            authorized: false,
            current: false,
            uses_endpoint_auth_form: false,
        },
    ];
    terminal
        .draw(|frame| {
            render_authorize_provider_picker(frame, area, &app, &providers, "", 1);
        })
        .expect("authorize provider picker should render");
    terminal.backend().buffer().clone()
}

pub(super) fn render_model_picker_to_buffer(
    area: Rect,
    entries: &[ModelOption],
    provider_cursor: usize,
) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
    app.model_picker.active_pane = ModelPickerPane::Provider;
    app.model_picker.provider_cursor = provider_cursor;
    terminal
        .draw(|frame| {
            render_model_picker(frame, area, &app, entries);
        })
        .expect("model picker should render");
    terminal.backend().buffer().clone()
}

pub(super) fn model_picker_entry(
    provider_id: &str,
    provider_label: &str,
    model: &str,
) -> ModelOption {
    ModelOption {
        provider_id: provider_id.to_string(),
        connection_id: provider_id.to_string(),
        provider_label: provider_label.to_string(),
        model_id: None,
        model: model.to_string(),
        display_name: crate::tui::pickers::short_model_label(model).to_string(),
        reasoning: ReasoningSelection::default(),
        recommended_reasoning: None,
        discouraged_reasoning: Vec::new(),
        supported_reasoning: Vec::new(),
        shortcut_bindings: Vec::new(),
        parameter_preview: "default parameters".to_string(),
        pricing: None,
        context_window: Some(131_072),
        features: Vec::new(),
        metadata_sources: crate::model_catalog::ResolvedModelMetadataSources::default(),
        catalog_drift: Vec::new(),
        unverified: false,
    }
}

pub(super) fn session_summary(
    id: i64,
    summary: &str,
    provider_id: &str,
    model: &str,
    status: &str,
    message_count: i64,
) -> crate::storage::SessionSummary {
    crate::storage::SessionSummary {
        id: crate::storage::SessionId::from_raw(id),
        project_path: "/tmp/project".to_string(),
        name: "/tmp/project".to_string(),
        summary: summary.to_string(),
        provider_id: provider_id.to_string(),
        model: model.to_string(),
        reasoning: ReasoningSelection::default(),
        status: crate::storage::SessionStatus::from_db_str(status),
        terminal_reason: None,
        latest_task: None,
        updated_at_ms: current_time_ms(),
        message_count,
        prompt_token_count: 0,
        completion_token_count: 0,
        cache_read_input_token_count: 0,
        cache_creation_input_token_count: 0,
        cache_measured_input_token_count: 0,
        cost_micros: 0,
        no_cache_cost_micros: 0,
        source_plan_id: None,
    }
}

pub(super) fn saved_plan(
    id: i64,
    title: &str,
    branch: Option<&str>,
    status: &str,
) -> crate::storage::SavedPlanSummary {
    crate::storage::SavedPlanSummary {
        id: crate::storage::SavedPlanId::from_raw(id),
        project_path: "/tmp/project".to_string(),
        title: title.to_string(),
        source_session_id: Some(crate::storage::SessionId::from_raw(id)),
        branch: branch.map(str::to_string),
        status: crate::storage::SavedPlanStatus::from_db_str(status),
        execution_session_id: None,
        saved_at_ms: current_time_ms(),
        updated_at_ms: current_time_ms(),
        section_count: 2,
        task_count: 3,
    }
}

pub(super) fn saved_plan_with_times(
    id: i64,
    title: &str,
    saved_at_ms: i64,
    updated_at_ms: i64,
) -> crate::storage::SavedPlanSummary {
    crate::storage::SavedPlanSummary {
        saved_at_ms,
        updated_at_ms,
        ..saved_plan(id, title, Some("main"), "started")
    }
}

pub(super) fn task_snapshot(
    id: &str,
    command: &str,
    status: BackgroundTaskStatus,
) -> BackgroundTaskSnapshot {
    BackgroundTaskSnapshot {
        id: id.to_string(),
        incarnation: "test-task".to_string(),
        command: command.to_string(),
        cwd: PathBuf::from("/tmp/project"),
        status,
        started_at: SystemTime::now(),
        finished_at: Some(SystemTime::now()),
        exit_code: Some(0),
        timeout_secs: 30,
        timed_out: false,
        tail: "first line\nlast line".to_string(),
        tail_truncated: false,
        total_output_chars: 20,
        version: 1,
        tool_call_id: None,
    }
}

pub(super) fn subagent_snapshot(id: &str, agent: &str, activity_lines: usize) -> SubagentSnapshot {
    let activity = (0..activity_lines)
        .map(|index| format!("step {index} running"))
        .collect::<Vec<_>>()
        .join("\n");
    SubagentSnapshot {
        id: id.into(),
        agent: agent.into(),
        prompt: "Investigate the failing test".into(),
        detached: false,
        status: SubagentStatus::Running,
        started_at: SystemTime::now(),
        finished_at: None,
        activity: activity.into(),
        result: None,
        model: Some("gpt-5".into()),
        tool_call_id: None,
        launch_group_id: None,
    }
}

pub(super) fn render_subtasks_to_buffer(
    area: Rect,
    subtasks: &[SubagentSnapshot],
    cursor: usize,
    scroll: u16,
    override_for: Option<(&str, crate::subagent::SubagentModelOverride)>,
) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
    app.modal_scroll = scroll;
    if let Some((agent, over)) = override_for {
        app.subagent_model_overrides
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(agent.to_string(), over);
    }
    terminal
        .draw(|frame| {
            render_subtask_list(frame, area, &app, subtasks, cursor, SubtaskListPane::Detail);
        })
        .expect("subtask list should render");
    terminal.backend().buffer().clone()
}

pub(super) fn render_mode_picker_to_buffer(
    area: Rect,
    rows: &[ModeRow],
    cursor: usize,
) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    terminal
        .draw(|frame| {
            render_mode_picker(frame, area, rows, cursor);
        })
        .expect("mode picker should render");
    terminal.backend().buffer().clone()
}

pub(super) fn render_settings_to_buffer(
    area: Rect,
    rows: &[crate::tui::event::SettingsRow],
    cursor: usize,
) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    terminal
        .draw(|frame| {
            render_settings(frame, area, rows, cursor);
        })
        .expect("settings should render");
    terminal.backend().buffer().clone()
}

pub(super) fn render_theme_picker_to_buffer(area: Rect, cursor: usize) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    terminal
        .draw(|frame| {
            render_theme_picker(frame, area, cursor);
        })
        .expect("theme picker should render");
    terminal.backend().buffer().clone()
}

pub(super) fn current_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// Coordinates of the first cell of `needle` in the rendered buffer, scanning
/// rows top to bottom. Assumes ASCII content so the char index maps to cell x.
pub(super) fn find_cell(buffer: &ratatui::buffer::Buffer, needle: &str) -> Option<(u16, u16)> {
    for y in 0..buffer.area.height {
        let row = row_text(buffer, y);
        if let Some(byte_idx) = row.find(needle) {
            let x = row[..byte_idx].chars().count() as u16;
            return Some((x, y));
        }
    }
    None
}

pub(super) fn row_text(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

pub(super) fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
    let mut text = String::new();
    for y in 0..buffer.area.height {
        text.push_str(&row_text(buffer, y));
        text.push('\n');
    }
    text
}

pub(crate) fn rendered_lines_text(lines: &[Line<'static>]) -> String {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
