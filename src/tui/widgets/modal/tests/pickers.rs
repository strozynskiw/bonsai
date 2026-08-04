use super::*;
use crate::tui::event::{ModeAxisId, ModeRow, SettingId, SettingsRow};
use crate::tui::widgets::modal::picker_common::{
    picker_line, visible_picker_rows, visible_weighted_rows,
};

fn smol_off() -> crate::smol::SmolProfile {
    crate::smol::SmolProfile::resolve(crate::smol::SmolPreference::Off, 128_000)
}

#[test]
fn model_picker_visible_rows_keep_cursor_in_view() {
    // The cursor is centred in the viewport, so early rows keep it near the
    // top, mid-list rows stay roughly centred, and the last page pins to
    // the bottom edge of the list.
    assert_eq!(visible_picker_rows(20, 0, 5), 0..5);
    assert_eq!(visible_picker_rows(20, 4, 5), 2..7);
    assert_eq!(visible_picker_rows(20, 5, 5), 3..8);
    assert_eq!(visible_picker_rows(20, 19, 5), 15..20);
    assert_eq!(visible_picker_rows(3, 2, 12), 0..3);
}

fn provider_manager_row(index: usize) -> crate::tui::provider_manager::ProviderManagerRow {
    crate::tui::provider_manager::ProviderManagerRow {
        connection_id: format!("prov-{index}"),
        display_name: format!("Provider {index:02}"),
        origin: crate::tui::provider_manager::ProviderOrigin::BuiltIn,
        enabled: true,
        authorized: false,
        current: false,
        model_count: 0,
        discovery: crate::model_catalog::DiscoveryKind::Generic,
        base_url: String::new(),
        credential_label: None,
        auth_hint: None,
    }
}

fn render_provider_manager_lines(app: &AppState, area: Rect, cursor: usize) -> String {
    let rows: Vec<_> = (0..12).map(provider_manager_row).collect();
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    terminal
        .draw(|frame| render_provider_manager(frame, area, app, &rows, "", false, cursor))
        .expect("provider manager should render");
    buffer_text(terminal.backend().buffer())
}

#[test]
fn provider_manager_scrolls_only_at_window_edges() {
    // A body that shows fewer rows than the list; the cursor should travel
    // inside the window without shifting it (model-picker semantics), and the
    // window advances only when the cursor crosses the bottom edge.
    let app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
    let area = Rect::new(0, 0, 80, 10);

    let top = render_provider_manager_lines(&app, area, 0);
    assert!(top.contains("> Provider 00"));

    // Move the cursor within the first window: the top row must stay visible
    // (the old behavior pinned the cursor to the bottom and scrolled here).
    let mid = render_provider_manager_lines(&app, area, 1);
    assert!(
        mid.contains("Provider 00"),
        "window must not shift while the cursor moves inside it"
    );
    assert!(mid.contains("> Provider 01"));

    // Walk the cursor past the bottom edge: the window advances and the top
    // row scrolls out.
    let mut past_edge = String::new();
    for cursor in 0..=8 {
        past_edge = render_provider_manager_lines(&app, area, cursor);
    }
    assert!(past_edge.contains("> Provider 08"));
    assert!(
        !past_edge.contains("Provider 00"),
        "window must advance once the cursor crosses the bottom edge"
    );

    // Walking back up: the window stays until the cursor crosses the top edge.
    let back_inside = render_provider_manager_lines(&app, area, 7);
    assert!(
        back_inside.contains("Provider 08"),
        "window must not shift while moving up inside it"
    );
    let mut back_top = String::new();
    for cursor in (0..=7).rev() {
        back_top = render_provider_manager_lines(&app, area, cursor);
    }
    assert!(back_top.contains("> Provider 00"));
}

#[test]
fn model_picker_provider_column_uses_names_and_scrolls_rows() {
    let area = Rect::new(0, 0, 82, 11);
    let entries = vec![
        model_picker_entry("provider-one-id", "Provider One", "model-one"),
        model_picker_entry("provider-two-id", "Provider Two", "model-two"),
        model_picker_entry("provider-three-id", "Provider Three", "model-three"),
        model_picker_entry("provider-four-id", "Provider Four", "model-four"),
        model_picker_entry("provider-five-id", "Provider Five", "model-five"),
    ];
    let top = render_model_picker_to_buffer(area, &entries, 0);
    let scrolled = render_model_picker_to_buffer(area, &entries, 4);
    let top_text = buffer_text(&top);
    let scrolled_text = buffer_text(&scrolled);

    assert!(top_text.contains("> Provider One"));
    assert!(top_text.contains("Provider Three"));
    assert!(!top_text.contains("provider-one-id"));
    assert!(!top_text.contains("provider-two-id"));
    assert!(scrolled_text.contains("> Provider Five"));
    assert!(!scrolled_text.contains("Provider One"));
    assert!(!scrolled_text.contains("provider-five-id"));
}

#[test]
fn model_picker_uses_short_model_names_when_canonical_id_is_available() {
    let area = Rect::new(0, 0, 100, 12);
    let mut entry = model_picker_entry("codex", "Codex", "gpt-5.5");
    entry.connection_id = "codex".to_string();
    entry.model_id = Some("openai/gpt-5.5".to_string());

    let buffer = render_model_picker_to_buffer(area, &[entry], 0);
    let text = buffer_text(&buffer);

    assert!(text.contains("gpt-5.5"));
    assert!(!text.contains("openai/gpt-5.5"));
    assert!(!text.contains("remote: gpt-5.5"));
}

#[test]
fn model_picker_shows_input_cached_and_output_pricing() {
    let area = Rect::new(0, 0, 160, 12);
    let mut entry = model_picker_entry("anthropic", "Anthropic", "claude-sonnet-4-5");
    entry.pricing = Some(
        crate::provider::ModelPricing::new(3_000_000, 15_000_000)
            .with_cache_rates(Some(300_000), None),
    );

    let buffer = render_model_picker_to_buffer(area, &[entry], 0);
    let rows = (0..area.height)
        .map(|y| row_text(&buffer, y))
        .collect::<Vec<_>>();
    let text = rows.join("\n");
    let model_row = rows
        .iter()
        .find(|row| row.contains("claude-sonnet-4-5"))
        .expect("model row should render");
    let price_row = rows
        .iter()
        .find(|row| row.contains("price:"))
        .expect("price footer row should render");

    assert!(
        !model_row.contains("$3/M"),
        "price moved out of model row: {model_row}"
    );
    assert!(price_row.contains("in $3/M"));
    assert!(price_row.contains("cached $0.30/M"));
    assert!(price_row.contains("out $15/M"));
    // The compact model row still carries the context window.
    assert!(text.contains("131k ctx"), "{text}");
}

#[test]
fn model_picker_shows_refreshed_metadata_source_and_catalog_drift() {
    let area = Rect::new(0, 0, 180, 12);
    let mut entry = model_picker_entry("opencode", "OpenCode Go", "qwen3.7-max");
    entry.metadata_sources.pricing = Some(crate::model_catalog::ModelMetadataSource::ModelsDev);
    entry.catalog_drift = vec!["pricing differs from models.dev".to_string()];

    let buffer = render_model_picker_to_buffer(area, &[entry], 0);
    let text = buffer_text(&buffer);

    assert!(text.contains("price: n/a"), "{text}");
    assert!(text.contains("sources: price:models.dev"), "{text}");
    assert!(text.contains("⚠ catalog drift"), "{text}");
}

#[test]
fn model_picker_shows_capability_icons_and_assumed_context() {
    let area = Rect::new(0, 0, 160, 12);
    let mut entry = model_picker_entry("lm-local", "LM Studio", "qwen3-coder");
    entry.context_window = None;
    entry.unverified = true;
    entry.features = vec![
        crate::model_catalog::ModelFeature::ToolCall,
        crate::model_catalog::ModelFeature::Attachment,
        crate::model_catalog::ModelFeature::Reasoning,
    ];

    let buffer = render_model_picker_to_buffer(area, &[entry], 0);
    let text = buffer_text(&buffer);

    assert!(text.contains("~120k ctx (assumed)"), "{text}");
    assert!(text.contains("unverified"), "{text}");
    assert!(text.contains("⚒︎"), "{text}");
    assert!(text.contains("◉"), "{text}");
    assert!(text.contains("∴"), "{text}");
    assert!(text.contains("icons:"), "{text}");
    assert!(text.contains("⚒︎ tools"), "{text}");
    assert!(text.contains("◉ vision"), "{text}");
    assert!(text.contains("∴ thinking"), "{text}");
    assert!(!text.contains("🔨"), "{text}");
    assert!(!text.contains("👁"), "{text}");
    assert!(!text.contains("🧠"), "{text}");
    assert!(!text.contains("tools · vision"), "{text}");
}

#[test]
fn model_picker_shows_shortcut_badges_for_assigned_letters() {
    let area = Rect::new(0, 0, 160, 12);
    let mut entry = model_picker_entry("codex", "Codex", "gpt-5.5");
    entry.shortcut_bindings = vec![
        (
            crate::model_role::ModelShortcutKey::new('a').unwrap(),
            crate::provider::ReasoningSelection::Default,
        ),
        (
            crate::model_role::ModelShortcutKey::new('m').unwrap(),
            crate::provider::ReasoningSelection::Default,
        ),
        (
            crate::model_role::ModelShortcutKey::new('z').unwrap(),
            crate::provider::ReasoningSelection::Default,
        ),
    ];

    let buffer = render_model_picker_to_buffer(area, &[entry], 0);
    let text = buffer_text(&buffer);

    assert!(text.contains("a/m/z"), "{text}");
    // Reasoning pane's assignment cue plus the footer's invocation hint.
    assert!(text.contains("press "), "{text}");
    assert!(text.contains("letter"), "{text}");
    assert!(text.contains("shortcut usage: /a /m /z"), "{text}");
}

#[test]
fn command_help_renders_command_board_with_provider_manager() {
    // Tall enough for every command in both columns (52 as of M5.3's /hooks) —
    // a fixed test viewport, not a real terminal size; bump this if the
    // roster grows enough to clip the shorter column again.
    let area = Rect::new(0, 0, 160, 40);
    let buffer = render_command_help_to_buffer(area);
    let text = buffer_text(&buffer);

    assert!(text.contains("Commands"));
    assert!(text.contains("/authorize [provider]"));
    assert!(text.contains("Choose a provider to authorize"));
    assert!(text.contains("/model <number|"));
    // `/providers` (the manager) is registered; the old singular `/provider`
    // command stays removed.
    assert!(text.contains("/providers"));
    assert!(!text.contains("/provider "));
}

#[test]
fn picker_selection_is_arrow_only_without_row_highlight() {
    // Selection is signalled by the `> ` marker alone; the label carries no
    // reversed-row highlight and its style is identical whether selected or not.
    let selected = picker_line(true, "row".to_string(), None, true);
    let unselected = picker_line(false, "row".to_string(), None, true);
    assert_eq!(selected.spans[0].content.as_ref(), "> ");
    assert_eq!(unselected.spans[0].content.as_ref(), "  ");
    assert_eq!(selected.spans[1].style, unselected.spans[1].style);
    assert!(
        !selected.spans[1]
            .style
            .add_modifier
            .contains(Modifier::REVERSED)
    );
}

#[test]
fn theme_picker_renders_themes_with_arrow_selection() {
    let _guard = theme::TEST_LOCK.blocking_lock();
    theme::reset_registry_for_tests();
    let area = Rect::new(0, 0, 96, 14);
    let buffer = render_theme_picker_to_buffer(area, 1);
    let text = buffer_text(&buffer);

    assert!(text.contains("Themes"));
    assert!(text.contains("forest"));
    assert!(text.contains("warm mossy dark"));
    assert!(text.contains("> ocean"));
    assert!(text.contains("paper"));
    assert!(text.contains("Enter save"));
    assert!(text.contains("Esc cancel"));
    assert!(!text.contains(">>"));
}

#[test]
fn theme_picker_labels_custom_theme_provenance() {
    let _guard = theme::TEST_LOCK.blocking_lock();
    theme::reset_registry_for_tests();
    theme::install_custom_theme_for_tests("mytheme", theme::ThemeSource::Project);
    // The custom theme is appended last; put the cursor on it so it scrolls into
    // the visible window (there are more built-ins than fit at once).
    let cursor = theme::theme_count() - 1;
    let area = Rect::new(0, 0, 96, 22);
    let buffer = render_theme_picker_to_buffer(area, cursor);
    let text = buffer_text(&buffer);

    assert!(text.contains("mytheme"), "custom theme listed: {text}");
    assert!(
        text.contains("project theme"),
        "provenance tag shown: {text}"
    );
    theme::reset_registry_for_tests();
}

#[test]
fn authorize_provider_picker_authorized_name_is_green() {
    let provider = ProviderOption {
        provider_id: "opencode".to_string(),
        provider_label: "OpenCode Go".to_string(),
        authorized: true,
        current: false,
        uses_endpoint_auth_form: false,
    };
    let columns = AuthorizeProviderColumns {
        provider: 16,
        details: 40,
    };
    let line = authorize_provider_line(&provider, &columns, false);

    let name = line
        .spans
        .iter()
        .find(|span| span.content.as_ref().contains("OpenCode Go"))
        .expect("provider name should render");
    assert_eq!(name.style.fg, Some(theme::palette().success));
}

#[test]
fn authorize_provider_picker_keeps_auth_text_out_of_rows() {
    let provider = ProviderOption {
        provider_id: "opencode".to_string(),
        provider_label: "OpenCode Go".to_string(),
        authorized: true,
        current: false,
        uses_endpoint_auth_form: false,
    };
    let columns = AuthorizeProviderColumns {
        provider: 16,
        details: 40,
    };
    let line = authorize_provider_line(&provider, &columns, false);
    let row = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();

    assert!(row.contains("opencode"));
    assert!(!row.contains("authorized"));
}

#[test]
fn authorize_provider_picker_renders_supported_providers() {
    let area = Rect::new(0, 0, 80, 16);
    let buffer = render_authorize_providers_to_buffer(area);
    let text = buffer_text(&buffer);

    assert!(text.contains("Authorize Provider"));
    assert!(text.contains("OpenCode Go"));
    assert!(text.contains("opencode"));
    assert!(text.contains("Legend"));
    assert!(text.contains("authorize"));
}

#[test]
fn session_picker_renders_dense_header_and_single_row_sessions() {
    let area = Rect::new(0, 0, 100, 18);
    let mut sessions = vec![
        session_summary(
            1,
            "Inspect startup crash",
            "codex",
            "gpt-5.5",
            "completed",
            14,
        ),
        session_summary(
            2,
            "Polish sessions",
            "anthropic",
            "claude-sonnet-4",
            "active",
            9,
        ),
    ];
    let first_session_id = sessions[0].id;
    sessions[0].latest_task = Some(Box::new(crate::storage::TaskRun {
        id: crate::storage::TaskRunId::from_raw(10),
        session_id: first_session_id,
        episode_seq: Some(1),
        goal_id: "goal-1".to_string(),
        goal: "Inspect startup crash".to_string(),
        outcome: Some(crate::storage::TaskOutcome::Blocked),
        terminal_reason: Some(crate::storage::TaskTerminalReason::new(
            crate::storage::TaskTerminalReasonCode::BudgetExhausted,
            "Run budget exhausted.",
        )),
        started_at_ms: 1,
        ended_at_ms: Some(2),
    }));
    let buffer = render_sessions_to_buffer(area, &sessions, 1);
    let rows = (0..area.height)
        .map(|y| row_text(&buffer, y))
        .collect::<Vec<_>>();
    let text = rows.join("\n");

    assert!(text.contains("ID"));
    assert!(text.contains("Session"));
    assert!(text.contains("Updated"));
    assert!(text.contains("Lifecycle"));
    assert!(text.contains("Task"));
    assert!(text.contains("Msgs"));

    let first = rows
        .iter()
        .find(|row| row.contains("Inspect startup crash"))
        .expect("first session should render");
    assert!(first.contains("completed"));
    assert!(first.contains("blocked"));
    assert!(first.contains("14"));

    let selected = rows
        .iter()
        .find(|row| row.contains("Polish sessions"))
        .expect("selected session should render");
    assert!(selected.contains(">"));
    assert!(selected.contains("active"));
    assert!(selected.contains("9"));
}

#[test]
fn session_picker_truncates_long_columns_with_ascii_ellipsis() {
    let area = Rect::new(0, 0, 60, 12);
    let sessions = vec![session_summary(
        42,
        "This session title is intentionally long enough to truncate",
        "minimax-coding-plan",
        "a-very-long-model-name-that-will-not-fit",
        "completed",
        123,
    )];
    let buffer = render_sessions_to_buffer(area, &sessions, 0);
    let text = (0..area.height)
        .map(|y| row_text(&buffer, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(text.contains("..."));
    assert!(!text.contains("intentionally long enough to truncate"));
}

#[test]
fn session_picker_uses_project_name_when_summary_is_empty() {
    let area = Rect::new(0, 0, 100, 12);
    let sessions = vec![session_summary(77, "", "codex", "gpt-5.5", "completed", 9)];
    let buffer = render_sessions_to_buffer(area, &sessions, 0);
    let text = (0..area.height)
        .map(|y| row_text(&buffer, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(text.contains("/tmp/project"));
    assert!(!text.contains("No prior sessions"));
}

#[test]
fn session_picker_empty_state_keeps_table_header() {
    let area = Rect::new(0, 0, 80, 12);
    let buffer = render_sessions_to_buffer(area, &[], 0);
    let text = (0..area.height)
        .map(|y| row_text(&buffer, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(text.contains("No prior sessions"));
}

#[test]
fn plan_picker_renders_search_columns_and_selected_marker() {
    let area = Rect::new(0, 0, 100, 18);
    let plans = vec![
        saved_plan(1, "Parser cleanup", Some("main"), "draft"),
        saved_plan(2, "Plan library", Some("feature/plans"), "started"),
    ];
    let buffer = render_plans_to_buffer(area, &plans, "plan", 0);
    let rows = (0..area.height)
        .map(|y| row_text(&buffer, y))
        .collect::<Vec<_>>();
    let text = rows.join("\n");

    assert!(text.contains("Search: plan"));
    assert!(text.contains("Plan"));
    assert!(text.contains("Branch"));
    assert!(text.contains("Updated"));
    assert!(text.contains("Status"));
    assert!(text.contains("Items"));
    assert!(!text.contains("Parser cleanup"));

    let selected = rows
        .iter()
        .find(|row| row.contains("Plan library"))
        .expect("filtered plan should render");
    assert!(selected.contains(">"));
    assert!(selected.contains("feature/plans"));
    assert!(selected.contains("started"));
    assert!(selected.contains("2s/3t"));
}

#[test]
fn plan_picker_empty_state_keeps_table_header() {
    let area = Rect::new(0, 0, 80, 12);
    let buffer = render_plans_to_buffer(area, &[], "", 0);
    let text = (0..area.height)
        .map(|y| row_text(&buffer, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(text.contains("Branch"));
    assert!(text.contains("No saved plans"));
}

#[test]
fn plan_picker_updated_column_uses_latest_update_time() {
    let area = Rect::new(0, 0, 100, 12);
    let now = current_time_ms();
    let three_days_ms = 3 * 24 * 60 * 60 * 1000;
    let plans = vec![saved_plan_with_times(
        9,
        "Started plan",
        now - three_days_ms,
        now,
    )];

    let buffer = render_plans_to_buffer(area, &plans, "", 0);
    let text = (0..area.height)
        .map(|y| row_text(&buffer, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(text.contains("<1m ago"));
    assert!(!text.contains("3d ago"));
}

#[test]
fn task_list_preserves_full_task_ids() {
    let area = Rect::new(0, 0, 100, 24);
    let tasks = vec![task_snapshot(
        "bg-1",
        "printf task-output",
        BackgroundTaskStatus::Succeeded,
    )];
    let buffer = render_tasks_to_buffer(area, &tasks, 0);
    let text = buffer_text(&buffer);

    assert!(text.contains("bg-1"));
    assert!(text.contains("Background Tasks"));
    assert!(text.contains("Selected Task"));
}

#[test]
fn task_list_truncates_long_commands_in_rows() {
    let area = Rect::new(0, 0, 70, 20);
    let long_command = format!("printf {}", "very-long-command ".repeat(20));
    let tasks = vec![task_snapshot(
        "bg-1",
        &long_command,
        BackgroundTaskStatus::Running,
    )];
    let buffer = render_tasks_to_buffer(area, &tasks, 0);
    let rows = (0..area.height)
        .map(|y| row_text(&buffer, y))
        .collect::<Vec<_>>();

    let row = rows
        .iter()
        .find(|row| row.contains("bg-1") && row.contains("running"))
        .expect("task row should render");
    assert!(row.contains("..."));
    assert!(row.chars().count() <= area.width as usize);
}

#[test]
fn task_list_detail_shows_command_cwd_status_exit_and_tail() {
    let area = Rect::new(0, 0, 110, 50);
    let tasks = vec![task_snapshot(
        "bg-1",
        "printf task-output",
        BackgroundTaskStatus::Succeeded,
    )];
    let buffer = render_tasks_to_buffer(area, &tasks, 0);
    let text = buffer_text(&buffer);

    assert!(text.contains("Status"));
    assert!(text.contains("succeeded"));
    assert!(text.contains("Exit/timeout"));
    assert!(text.contains("exit 0"));
    assert!(text.contains("Command"));
    assert!(text.contains("printf task-output"));
    assert!(text.contains("Cwd"));
    assert!(text.contains("/tmp/project"));
    assert!(text.contains("Output tail"));
    assert!(text.contains("first line"));
}

#[test]
fn permission_prompt_footer_is_always_visible() {
    let area = Rect::new(0, 0, 60, 12);
    let buffer = render_to_buffer(area, "ls -la", 0);

    // Find the bottom row that contains the footer hint.
    let mut found_footer = false;
    for y in 0..area.height {
        let text = row_text(&buffer, y);
        if text.contains("once") && text.contains("project") && text.contains("deny") {
            found_footer = true;
            assert!(
                y >= area.height.saturating_sub(3),
                "footer should be near the bottom of the modal, got y={y}"
            );
        }
    }
    assert!(
        found_footer,
        "permission prompt must always show the action hints"
    );
}

#[test]
fn permission_prompt_long_command_still_shows_footer() {
    let area = Rect::new(0, 0, 60, 12);
    let long_command = "echo this is a very long command that should wrap across many \
            lines and push any non-scrolling content off the bottom of the modal \
            unless the footer is pinned into its own row like it should be \
            because we do not want to lose the approve/deny hints";
    let buffer = render_to_buffer(area, long_command, 0);

    let mut found_footer = false;
    for y in 0..area.height {
        let text = row_text(&buffer, y);
        if text.contains("once") && text.contains("project") && text.contains("deny") {
            found_footer = true;
        }
    }
    assert!(
        found_footer,
        "footer hints must remain visible even when the command wraps many lines"
    );
}

#[test]
fn combined_command_and_sandbox_prompt_shows_one_explicit_warning() {
    let area = Rect::new(0, 0, 70, 14);
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    let app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
    terminal
        .draw(|frame| {
            render_confirm_prompt(
                frame,
                area,
                &app,
                &sandbox_escalation_prompt(
                    "cargo test",
                    None,
                    crate::interaction::SandboxEscalationKind::CommandAndSandbox,
                ),
                0,
            );
        })
        .expect("combined prompt should render");
    let text = buffer_text(terminal.backend().buffer());

    assert!(text.contains("Allow this command to run OUTSIDE the sandbox?"));
    assert!(text.contains("single decision approves both the command"));
    assert!(text.contains("once"));
    assert!(text.contains("session"));
    assert!(text.contains("deny"));
    assert!(
        !text.contains("project"),
        "combined sandbox approval must never become a persistent project rule"
    );
}

#[test]
fn help_modal_advertises_start_and_save() {
    let area = Rect::new(0, 0, 100, 40);
    let buffer = render_help_to_buffer(area);
    let text = (0..area.height)
        .map(|y| row_text(&buffer, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(text.contains("/start"));
    assert!(text.contains("/save"));
    assert!(text.contains("/export"));
    assert!(text.contains("/mode"));
    assert!(!text.contains("Alt+I"));
    assert!(!text.contains("F5"));
    assert!(!text.contains("Ctrl+O"));
}

#[test]
fn mode_picker_renders_axes_with_headers_and_value_rows() {
    let area = Rect::new(0, 0, 60, 16);
    let rows = vec![
        ModeRow::Header("Autonomy"),
        ModeRow::Value {
            axis: ModeAxisId::Autonomy,
            key: "level",
            values: &["ask", "conservative", "balanced", "auto-accept", "yolo"],
            current: 2,
            note: None,
        },
        ModeRow::Header("Self-review"),
        ModeRow::Value {
            axis: ModeAxisId::SelfReview,
            key: "mode",
            values: &["auto", "off", "ask", "on"],
            current: 0,
            note: Some("off at current autonomy"),
        },
        ModeRow::Header("Sandbox"),
        ModeRow::Value {
            axis: ModeAxisId::SandboxConfinement,
            key: "confinement",
            values: &["off", "on"],
            current: 0,
            note: None,
        },
        ModeRow::Value {
            axis: ModeAxisId::SandboxNetwork,
            key: "network",
            values: &["allow", "deny"],
            current: 1,
            note: None,
        },
    ];
    let buffer = render_mode_picker_to_buffer(area, &rows, 1);
    let text = buffer_text(&buffer);
    assert!(text.contains("Mode"), "missing title: {text}");
    assert!(text.contains("Autonomy"), "missing header: {text}");
    assert!(
        text.contains("Self-review"),
        "missing self-review header: {text}"
    );
    assert!(text.contains("Sandbox"), "missing sandbox header: {text}");
    assert!(text.contains("level"), "missing key: {text}");
    assert!(text.contains("mode"), "missing self-review key: {text}");
    assert!(text.contains("balanced"), "missing current value: {text}");
    assert!(text.contains("auto"), "missing self-review value: {text}");
    assert!(text.contains("deny"), "missing network value: {text}");
    assert!(text.contains("cycle"), "missing cycle hint: {text}");
    assert!(text.contains("move"), "missing move hint: {text}");
}

#[test]
fn short_mode_picker_keeps_focused_sandbox_row_and_header_visible() {
    let area = Rect::new(0, 0, 60, 9);
    let rows = vec![
        ModeRow::Header("Autonomy"),
        ModeRow::Value {
            axis: ModeAxisId::Autonomy,
            key: "level",
            values: &["ask", "balanced"],
            current: 0,
            note: None,
        },
        ModeRow::Header("Self-review"),
        ModeRow::Value {
            axis: ModeAxisId::SelfReview,
            key: "mode",
            values: &["auto", "off"],
            current: 0,
            note: None,
        },
        ModeRow::Header("Sandbox"),
        ModeRow::Value {
            axis: ModeAxisId::SandboxConfinement,
            key: "confinement",
            values: &["off", "on"],
            current: 0,
            note: None,
        },
        ModeRow::Value {
            axis: ModeAxisId::SandboxNetwork,
            key: "network",
            values: &["allow", "deny"],
            current: 1,
            note: None,
        },
    ];
    let buffer = render_mode_picker_to_buffer(area, &rows, 6);
    let text = buffer_text(&buffer);

    assert!(text.contains("Sandbox"), "missing focused section: {text}");
    let focused = text
        .lines()
        .find(|line| line.contains("network"))
        .expect("focused Sandbox row should be visible");
    assert!(
        focused.contains('>'),
        "focused row missing marker: {focused}"
    );
}

#[test]
fn mode_picker_marks_focused_value_with_arrow_and_keeps_header_dim() {
    let area = Rect::new(0, 0, 60, 12);
    let rows = vec![
        ModeRow::Header("Autonomy"),
        ModeRow::Value {
            axis: ModeAxisId::Autonomy,
            key: "level",
            values: &["ask", "conservative", "balanced"],
            current: 1,
            note: None,
        },
    ];
    let buffer = render_mode_picker_to_buffer(area, &rows, 1);
    let text = buffer_text(&buffer);
    // The focused value row carries a leading ">"; the header still has a
    // blank marker. We can't read the styles directly without a buffer
    // scanner, so we assert the marker is present on the row that includes
    // the current value.
    let lines: Vec<&str> = text.lines().collect();
    let value_line = lines
        .iter()
        .find(|line| line.contains("conservative"))
        .expect("value line missing");
    assert!(
        value_line.contains(">"),
        "focused row missing marker: {value_line}"
    );
}

#[test]
fn mode_picker_shows_every_option_with_selected_highlighted() {
    let area = Rect::new(0, 0, 80, 12);
    let rows = vec![
        ModeRow::Header("Autonomy"),
        ModeRow::Value {
            axis: ModeAxisId::Autonomy,
            key: "level",
            values: &["ask", "conservative", "balanced", "auto-accept", "yolo"],
            current: 2,
            note: None,
        },
    ];
    let buffer = render_mode_picker_to_buffer(area, &rows, 1);
    let text = buffer_text(&buffer);

    // A wide panel shows the full option set, not just the current value.
    for option in ["ask", "conservative", "balanced", "auto-accept", "yolo"] {
        assert!(text.contains(option), "missing option {option}: {text}");
    }

    // The current value is painted with the active accent; a neighbor is not.
    let (sx, sy) = find_cell(&buffer, "balanced").expect("selected option missing");
    let selected_fg = buffer[(sx, sy)].fg;
    assert!(
        (0..theme::theme_count())
            .filter_map(theme::theme_at)
            .any(|palette| palette.border_active == selected_fg),
        "selected option should use the active accent color"
    );
    let (ux, uy) = find_cell(&buffer, "yolo").expect("unselected option missing");
    assert_ne!(
        buffer[(ux, uy)].fg,
        selected_fg,
        "unselected option should not use the active accent color"
    );
}

#[test]
fn mode_picker_collapses_to_selected_value_when_too_narrow() {
    // Too little width for the full option set: the row falls back to just the
    // selected value rather than wrapping mid-row.
    let area = Rect::new(0, 0, 30, 10);
    let rows = vec![
        ModeRow::Header("Autonomy"),
        ModeRow::Value {
            axis: ModeAxisId::Autonomy,
            key: "level",
            values: &["ask", "conservative", "balanced", "auto-accept", "yolo"],
            current: 2,
            note: None,
        },
    ];
    let buffer = render_mode_picker_to_buffer(area, &rows, 1);
    let text = buffer_text(&buffer);

    assert!(text.contains("balanced"), "selected value missing: {text}");
    for hidden in ["conservative", "auto-accept", "yolo"] {
        assert!(
            !text.contains(hidden),
            "narrow row should hide non-selected option {hidden}: {text}"
        );
    }
}

#[test]
fn settings_wide_layout_shows_every_section_in_two_columns() {
    let app = crate::tui::test_utils::app();
    let rows = crate::tui::settings::seed_settings_rows(&app, smol_off());
    let area = Rect::new(0, 0, 220, 18);
    let buffer = render_settings_to_buffer(area, &rows, 1);
    let text = buffer_text(&buffer);
    let (model_x, _) = find_cell(&buffer, "Model").expect("model section should render");
    let (budgets_x, budgets_y) =
        find_cell(&buffer, "Budgets").expect("budgets section should render");
    let (_, model_y) = find_cell(&buffer, "Model").expect("model section should render");
    let (appearance_x, _) =
        find_cell(&buffer, "Appearance").expect("appearance section should render");

    assert!(
        model_x < area.width / 2,
        "model should be in the left column"
    );
    assert!(
        budgets_x > area.width / 2,
        "budgets should be in the right column"
    );
    assert_eq!(
        model_y, budgets_y,
        "balanced panes should start their first sections together"
    );
    assert!(
        appearance_x < area.width / 2,
        "the balanced split should keep Appearance in the left pane"
    );
    assert!(
        text.contains("Sandbox"),
        "bottom section should render: {text}"
    );
    assert!(
        text.contains("session output") && text.contains("session time"),
        "all budget rows should render: {text}"
    );
}

#[test]
fn settings_wide_table_aligns_controls_and_descriptions() {
    let app = crate::tui::test_utils::app();
    let rows = crate::tui::settings::seed_settings_rows(&app, smol_off());
    let area = Rect::new(0, 0, 220, 20);
    let buffer = render_settings_to_buffer(area, &rows, 1);
    let text = buffer_text(&buffer);
    let (_, max_turns_y) = find_cell(&buffer, "max turns").expect("max turns should render");
    let (_, run_time_y) = find_cell(&buffer, "run time").expect("run time should render");
    let max_turns = row_text(&buffer, max_turns_y);
    let run_time = row_text(&buffer, run_time_y);

    assert!(
        max_turns.contains("off  25  50  100  250"),
        "wide rows should show the full option axis: {max_turns}"
    );
    assert_eq!(
        max_turns.find("off"),
        run_time.find("off"),
        "control columns should align"
    );
    assert_eq!(
        max_turns.find("per run"),
        run_time.find("per run"),
        "description columns should align"
    );
    assert!(
        text.contains("foreground, across resumes"),
        "wide layout should preserve notes: {text}"
    );
}

#[test]
fn settings_medium_table_collapses_options_but_keeps_notes() {
    let app = crate::tui::test_utils::app();
    let rows = crate::tui::settings::seed_settings_rows(&app, smol_off());
    let area = Rect::new(0, 0, 80, 30);
    let buffer = render_settings_to_buffer(area, &rows, 1);
    let text = buffer_text(&buffer);

    assert!(text.contains("balanced"), "selected value missing: {text}");
    assert!(
        !text.contains("auto-accept"),
        "medium rows should collapse unselected options: {text}"
    );
    assert!(
        text.contains("foreground, across resumes"),
        "medium rows should retain descriptions when they fit: {text}"
    );
}

#[test]
fn settings_narrow_table_prioritizes_key_and_selected_value() {
    let app = crate::tui::test_utils::app();
    let rows = crate::tui::settings::seed_settings_rows(&app, smol_off());
    let area = Rect::new(0, 0, 38, 30);
    let buffer = render_settings_to_buffer(area, &rows, 1);
    let text = buffer_text(&buffer);

    assert!(text.contains("level"), "setting key missing: {text}");
    assert!(text.contains("balanced"), "selected value missing: {text}");
    assert!(
        !text.contains("auto-accept") && !text.contains("foreground, across resumes"),
        "narrow rows should omit options and descriptions: {text}"
    );
}

#[test]
fn settings_two_pane_breakpoint_is_measured_and_monotonic() {
    let app = crate::tui::test_utils::app();
    let rows = crate::tui::settings::seed_settings_rows(&app, smol_off());
    let is_two_pane = |width| {
        let buffer = render_settings_to_buffer(Rect::new(0, 0, width, 30), &rows, 1);
        find_cell(&buffer, "Budgets").is_some_and(|(x, _)| x > width / 2)
    };
    let threshold = (100..=240)
        .find(|width| is_two_pane(*width))
        .expect("a wide terminal should activate two panes");

    assert!(
        threshold > 100,
        "test range should include the single-pane tier"
    );
    assert!(!is_two_pane(threshold - 1));
    assert!((threshold..=240).all(is_two_pane));

    let threshold_text = buffer_text(&render_settings_to_buffer(
        Rect::new(0, 0, threshold, 30),
        &rows,
        1,
    ));
    assert!(
        threshold_text.contains("auto-accept"),
        "two panes must retain full option axes: {threshold_text}"
    );
    assert!(
        threshold_text.contains("foreground, across resumes"),
        "two panes must retain descriptions: {threshold_text}"
    );
}

#[test]
fn settings_ultra_narrow_table_truncates_key_and_keeps_selected_value() {
    let rows = vec![
        SettingsRow::Header("Narrow"),
        SettingsRow::Choice {
            id: SettingId::Autonomy,
            key: "an exceptionally long setting key",
            values: &["disabled", "enabled"],
            current: 1,
            note: None,
        },
    ];
    let area = Rect::new(0, 0, 20, 8);
    let buffer = render_settings_to_buffer(area, &rows, 1);
    let text = buffer_text(&buffer);
    let (_, value_y) = find_cell(&buffer, "enabled").expect("selected value should be reserved");
    let value_row = row_text(&buffer, value_y);

    assert!(value_row.contains('>'), "focus marker missing: {value_row}");
    assert!(
        value_row.contains("..."),
        "long key should truncate: {value_row}"
    );
    assert!(
        !text.contains("exceptionally"),
        "the full oversized key should not consume the row: {text}"
    );
}

#[test]
fn settings_narrow_layout_keeps_last_selection_visible() {
    let app = crate::tui::test_utils::app();
    let rows = crate::tui::settings::seed_settings_rows(&app, smol_off());
    let cursor = rows.len() - 1;
    let area = Rect::new(0, 0, 72, 12);
    let buffer = render_settings_to_buffer(area, &rows, cursor);
    let text = buffer_text(&buffer);
    let (_, network_y) = find_cell(&buffer, "network").expect("last setting should render");
    let network_row = row_text(&buffer, network_y);

    assert!(
        network_row.contains('>'),
        "last setting should retain the focus marker: {network_row}"
    );
    assert!(
        text.contains("Sandbox"),
        "last section should render: {text}"
    );
    assert!(
        !text.contains("Autonomy"),
        "narrow list should window away from the top: {text}"
    );
}

#[test]
fn weighted_rows_window_by_line_count_and_keep_cursor_visible() {
    // Uniform 3-line rows, 9-line budget: three rows fit, centred on the
    // cursor, pinning to the list edges at either end.
    assert_eq!(visible_weighted_rows(&[3, 3, 3, 3, 3, 3], 0, 9), 0..3);
    assert_eq!(visible_weighted_rows(&[3, 3, 3, 3, 3, 3], 2, 9), 1..4);
    assert_eq!(visible_weighted_rows(&[3, 3, 3, 3, 3, 3], 5, 9), 3..6);
    // Everything fits → no windowing.
    assert_eq!(visible_weighted_rows(&[1, 1, 1], 2, 9), 0..3);
    // A cursor row taller than the budget still renders alone (clipped).
    assert_eq!(visible_weighted_rows(&[2, 5, 2], 1, 3), 1..2);
    assert_eq!(visible_weighted_rows(&[], 0, 5), 0..0);
}

#[test]
fn subtask_modal_places_list_beside_selected_detail() {
    let area = Rect::new(0, 0, 110, 30);
    let subtasks = vec![subagent_snapshot("sub-1", "research", 3)];
    let buffer = render_subtasks_to_buffer(area, &subtasks, 0, 0, None);
    let (list_area, detail_area, footer_area) =
        list_detail_regions(area, ListDetailSplit::Horizontal);

    assert_eq!(
        list_area.y, detail_area.y,
        "list and detail panes should begin side-by-side"
    );
    let list_row = row_text(&buffer, list_area.y);
    let list_title = list_row.find("ID").expect("subtask list header");
    assert!(
        list_title >= list_area.x as usize && list_title < list_area.right() as usize,
        "list header should render in the left pane: {list_row}"
    );
    let detail_row = row_text(&buffer, detail_area.y);
    let detail_title = detail_row.find("Selected Subagent").expect("detail title");
    assert!(
        detail_title >= detail_area.x as usize,
        "detail title should render in the right pane: {detail_row}"
    );
    assert!(
        row_text(&buffer, footer_area.y).contains("Read-only subagents"),
        "footer should remain beneath both panes"
    );
}

#[test]
fn subtask_list_windows_compact_rows_to_keep_cursor_visible() {
    // The horizontal list reserves the prompt and activity for the selected
    // detail pane. Its compact rows must still window so the cursor reaches a
    // long tail without clipping the list body.
    let area = Rect::new(0, 0, 84, 24);
    let long_prompt = "I need a comprehensive understanding of all growth concepts \
                       across the entire codebase including every module"
        .to_string();
    let subtasks = (1..=24)
        .map(|index| {
            let mut snapshot = subagent_snapshot(&format!("sub-{index}"), "research", 1);
            snapshot.prompt = long_prompt.clone().into();
            snapshot
        })
        .collect::<Vec<_>>();

    let top = buffer_text(&render_subtasks_to_buffer(area, &subtasks, 0, 0, None));
    assert!(
        top.contains("sub-1"),
        "cursor at top shows the head:\n{top}"
    );
    assert!(
        !top.contains("sub-24"),
        "twenty-four compact rows cannot all fit; the tail should be windowed out:\n{top}"
    );

    let bottom = buffer_text(&render_subtasks_to_buffer(area, &subtasks, 23, 0, None));
    assert!(
        bottom.contains("sub-24"),
        "moving the cursor to the last row must scroll it into view:\n{bottom}"
    );
    assert!(
        !bottom.contains("sub-5"),
        "the head scrolls out once the cursor reaches the tail:\n{bottom}"
    );
}

#[test]
fn subtask_override_block_is_reachable_at_max_scroll() {
    // A subagent with a long activity overflows the short detail pane; the
    // pending override is appended below it. The scroll clamp must count the
    // override lines too, or scrolling to the bottom still can't reveal them.
    let area = Rect::new(0, 0, 60, 16);
    let subtasks = vec![subagent_snapshot("sub-1", "explorer", 40)];
    let over = crate::subagent::SubagentModelOverride::selector(
        "opus".to_string(),
        ReasoningSelection::default(),
    );
    // Scroll past the end; the render clamps to the real max.
    let buffer = render_subtasks_to_buffer(area, &subtasks, 0, u16::MAX, Some(("explorer", over)));
    let text = buffer_text(&buffer);
    assert!(
        text.contains("Override (all explorer runs") && text.contains("opus [default]"),
        "wrapped override should be reachable at max scroll:\n{text}"
    );
}
