use super::*;

#[test]
fn context_modal_control_legend_stays_on_bottom_row() {
    let area = Rect::new(0, 0, 88, 14);
    let report = context_report_with_rows(48);
    let buffer = render_context_to_buffer(area, report, 24, ContextViewMode::Ledger);
    let footer = row_text(&buffer, area.height.saturating_sub(2));

    assert!(
        footer.contains("p pin") && footer.contains("Tab wire"),
        "context control legend should stay in the bottom modal row: {footer}"
    );
}

#[test]
fn context_modal_shows_stub_retention_reason() {
    let area = Rect::new(0, 0, 120, 40);
    let mut report = context_report_with_rows(1);
    report.ledger[0].id = crate::agent::ContextNodeId::raw("tool-call-1");
    report.controls.insert(
        "tool-call-1".to_string(),
        crate::agent::ContextControlState {
            stubbed: true,
            stub_reason: Some(crate::agent::ContextStubReason::SupersededRead),
            ..Default::default()
        },
    );

    let text = buffer_text(&render_context_to_buffer(
        area,
        report,
        0,
        ContextViewMode::Ledger,
    ));

    assert!(
        text.contains("superseded read"),
        "/ctx should render automatic retention reasons, got {text:?}"
    );
}

#[test]
fn wire_raw_json_row_aligns_with_section_rows() {
    let area = Rect::new(0, 0, 120, 36);
    let report = context_report_with_wire_preview();
    let buffer = render_context_to_buffer(area, report, 0, ContextViewMode::Wire);
    // Both the section rows and the Raw JSON row share the prefix + label width,
    // so their path columns (`$…`) begin at the same screen column.
    let path_col = |needle: &str| -> u16 {
        for y in 0..area.height {
            let row = row_text(&buffer, y);
            if row.contains(needle) {
                let byte = row.find('$').expect("row has a path column");
                return row[..byte].chars().count() as u16;
            }
        }
        panic!("no row containing {needle}");
    };
    assert_eq!(
        path_col("$.messages"),
        path_col("Raw JSON"),
        "the Raw JSON path column should line up with the section path columns"
    );
}

#[test]
fn context_wire_view_renders_serialized_body_order_separately_from_ledger_order() {
    let area = Rect::new(0, 0, 120, 36);
    let report = context_report_with_wire_preview();
    let ledger = buffer_text(&render_context_to_buffer(
        area,
        report.clone(),
        0,
        ContextViewMode::Ledger,
    ));
    let wire = buffer_text(&render_context_to_buffer(
        area,
        report.clone(),
        0,
        ContextViewMode::Wire,
    ));

    let ledger_system = ledger.find("System group").expect("ledger system row");
    let ledger_tools = ledger.find("Tools group").expect("ledger tools row");
    assert!(
        ledger_system < ledger_tools,
        "ledger groups should keep grouped order"
    );

    let wire_messages = wire.find("$.messages").expect("wire messages row");
    let wire_system = wire.find("$.system").expect("wire system row");
    let wire_tools = wire.find("$.tools").expect("wire tools row");
    assert!(
        wire_messages < wire_system && wire_system < wire_tools,
        "wire rows should keep serialized JSON body order"
    );
    assert!(wire.contains("chars"));
    assert!(wire.contains("bytes"));
    assert!(wire.contains("p/d/s/r disabled"));
    assert!(
        !wire.contains("\"name\": \"read\""),
        "wire sections should be collapsed by default"
    );
    assert!(
        !wire.contains("tool 1: read"),
        "wire child rows should also be collapsed by default"
    );
    assert!(
        !wire.contains("Persona"),
        "wire system parts should also be collapsed by default"
    );

    let expanded_system = buffer_text(&render_context_wire_expanded_to_buffer(
        area,
        report.clone(),
        &["wire-system"],
    ));
    assert!(expanded_system.contains("Persona"));
    assert!(expanded_system.contains("Project context"));

    let expanded = buffer_text(&render_context_wire_expanded_to_buffer(
        area,
        report,
        &["wire-tools"],
    ));
    assert!(expanded.contains("tool 1: read"));
}

#[test]
fn context_turns_view_renders_cache_story_with_interleaved_compaction() {
    let area = Rect::new(0, 0, 120, 44);
    let report = context_report_with_usage_turns();
    let text = buffer_text(&render_context_to_buffer(
        area,
        report,
        0,
        ContextViewMode::Turns,
    ));

    assert!(text.contains("TURNS"), "{text}");
    assert!(text.contains("session"), "{text}");
    // Turn 1 read 38.6k of 42k → warm; turn 2 cold, attributed to the
    // hash-matched compaction event; turn 3 partial with unexplained churn.
    assert!(text.contains("warm"), "{text}");
    assert!(text.contains("COLD · compaction #1"), "{text}");
    assert!(text.contains("partial · prefix changed"), "{text}");
    // The compaction event row sits between the warm turn and the cold turn.
    let event_row = text.find("── compaction #1").expect("event row");
    let warm_note = text.find("warm").expect("warm note");
    let cold_note = text.find("COLD ·").expect("cold note");
    assert!(
        warm_note < event_row && event_row < cold_note,
        "compaction row should interleave chronologically: {text}"
    );
    assert!(text.contains("162.2k→101k"), "{text}");
    assert!(text.contains("prefix 140k→95k"), "{text}");
    // Turns is a read-only view; controls stay ledger-only.
    assert!(text.contains("Tab ledger"), "{text}");
}

#[test]
fn episodes_view_owns_tracked_episode_details() {
    let area = Rect::new(0, 0, 120, 44);
    let mut report = context_report_with_usage_turns();
    // Episode diagnostics have a dedicated view and do not crowd the turn table.
    let without = buffer_text(&render_context_to_buffer(
        area,
        report.clone(),
        0,
        ContextViewMode::Turns,
    ));
    assert!(!without.contains("Episodes"), "{without}");

    report.episodes = vec![
        crate::context_view::EpisodeReport {
            seq: 1,
            title: "Fix resume flake".to_string(),
            status_label: "evicted".to_string(),
            close_reason_label: Some("title_change".to_string()),
            goal: "fix the flake".to_string(),
            live_span_messages: None,
            evicted_tokens: Some(21_400),
            recall_count: 2,
            archived_messages: 14,
            completable: false,
            repaired: false,
        },
        crate::context_view::EpisodeReport {
            seq: 2,
            title: "Build exporter".to_string(),
            status_label: "active".to_string(),
            close_reason_label: None,
            goal: "build it".to_string(),
            live_span_messages: Some(9),
            evicted_tokens: None,
            recall_count: 0,
            archived_messages: 0,
            completable: false,
            repaired: false,
        },
    ];
    let turns = buffer_text(&render_context_to_buffer(
        area,
        report.clone(),
        0,
        ContextViewMode::Turns,
    ));
    assert!(!turns.contains("Fix resume flake"), "{turns}");

    let text = buffer_text(&render_episodes_to_buffer(area, report, 0, 0));
    assert!(text.contains("Episodes"), "{text}");
    assert!(text.contains("evicted"), "{text}");
    assert!(text.contains("Fix resume flake"), "{text}");
    assert!(text.contains("title_change"), "{text}");
    assert!(text.contains("21400"), "{text}");
    assert!(text.contains("Archived messages 14"), "{text}");
    assert!(text.contains("Recall count      2"), "{text}");
}

#[test]
fn context_header_shows_cache_verdict_in_every_mode_and_keeps_extras_scoped() {
    let area = Rect::new(0, 0, 120, 44);
    let report = context_report_with_usage_turns();
    let render = |mode| buffer_text(&render_context_to_buffer(area, report.clone(), 0, mode));
    let ledger = render(ContextViewMode::Ledger);
    let wire = render(ContextViewMode::Wire);
    let turns = render(ContextViewMode::Turns);

    // The last turn read 29.8k of 42k (70%) after unexplained prefix churn —
    // the verdict is pinned to the header in every mode.
    for (mode, text) in [("ledger", &ledger), ("wire", &wire), ("turns", &turns)] {
        assert!(
            text.contains("cache partial (70%)"),
            "{mode} header should carry the cache verdict: {text}"
        );
    }
    // Drivers and legend are ledger-only; session totals live in Turns.
    assert!(!wire.contains("drivers "), "{wire}");
    assert!(!turns.contains("drivers "), "{turns}");
    for text in [&ledger, &wire, &turns] {
        assert!(
            !text.contains("session total"),
            "the old session-total header line should be gone: {text}"
        );
    }
    assert!(turns.contains("session  "), "{turns}");
}

#[test]
fn context_turns_expanded_row_shows_diagnosis_and_reconciliation() {
    let area = Rect::new(0, 0, 120, 44);
    let report = context_report_with_usage_turns();
    let collapsed = buffer_text(&render_context_to_buffer(
        area,
        report.clone(),
        0,
        ContextViewMode::Turns,
    ));
    assert!(
        !collapsed.contains("prefix hash"),
        "detail lines should be collapsed by default: {collapsed}"
    );

    let expanded = buffer_text(&render_context_turns_expanded_to_buffer(area, report, &[2]));
    assert!(
        expanded.contains("compaction #1 rewrote prefix"),
        "{expanded}"
    );
    assert!(expanded.contains("wrote 43k to cache"), "{expanded}");
    assert!(
        expanded.contains("est 41.8k (tiktoken/high confidence) vs actual 42k"),
        "{expanded}"
    );
    assert!(expanded.contains("prefix hash bbbb2222"), "{expanded}");
    assert!(expanded.contains("cacheable ~38k"), "{expanded}");
    assert!(expanded.contains("request 80.1 KiB request"), "{expanded}");
    assert!(expanded.contains("cache hint cache_control"), "{expanded}");
    assert!(
        expanded.contains("cache expected 91% vs read 0%"),
        "{expanded}"
    );
    assert!(expanded.contains("tools read, bash"), "{expanded}");
}

#[test]
fn context_wire_rows_render_cache_tags_and_plan_line() {
    use crate::provider::WireCacheHint;
    let area = Rect::new(0, 0, 120, 36);
    let mut report = context_report_with_wire_preview();
    report.cacheable_prefix_tokens = 38_000;
    report.volatile_tail_tokens = 4_200;
    {
        let sections = &mut report
            .payload_preview
            .as_mut()
            .expect("wire preview")
            .wire_sections;
        sections[0].cache = Some(WireCacheHint::CachedPrefix);
        sections[1].cache = Some(WireCacheHint::Breakpoint);
        sections[2].cache = Some(WireCacheHint::Volatile);
    }

    let text = buffer_text(&render_context_to_buffer(
        area,
        report,
        0,
        ContextViewMode::Wire,
    ));

    assert!(text.contains("▐ cached"), "{text}");
    assert!(text.contains("◆ cache-bp"), "{text}");
    assert!(text.contains("~ volatile"), "{text}");
    assert!(
        text.contains("cache plan  prefix ~38,000 tok · volatile tail ~4,200 tok · 1 breakpoints"),
        "{text}"
    );
}

#[test]
fn context_wire_mouse_hit_testing_counts_wrapped_rows() {
    let area = Rect::new(0, 0, 72, 32);
    let report = context_report_with_wire_preview();
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    let mut app = AppState::new("codex", "gpt-5".to_string(), ".".to_string(), None);
    app.modal = Some(ModalKind::Context(Box::new(report)));
    app.context_state.view_mode = ContextViewMode::Wire;
    terminal
        .draw(|frame| {
            render(frame, area, &app);
        })
        .expect("context modal should render");
    let buffer = terminal.backend().buffer().clone();
    let system_row = (0..area.height)
        .find(|row| {
            let text = row_text(&buffer, *row);
            text.contains("System") && text.contains("$.system")
        })
        .expect("system wire row should be visible");

    let modal = modal_area(
        area,
        app.modal.as_ref().expect("context modal should be open"),
    );
    let (content, _footer) = context_modal_regions(modal);
    let click_column = content.x.saturating_add(2);

    assert_eq!(
        context_row_index_at(&app, area, click_column, system_row),
        Some(1),
        "clicking the visible + on System should target the System wire row"
    );
}

#[test]
fn tool_detail_summarizes_json_arguments_as_key_value_rows() {
    let area = Rect::new(0, 0, 100, 24);
    let arguments =
        r#"{"todos":[{"content":"Spawn three sleep 5 tasks in parallel","status":"completed"}]}"#;
    let buffer = render_tool_detail_with_arguments_to_buffer(area, arguments, "done", 0);
    let text = buffer_text(&buffer);

    assert!(text.contains("todos"));
    assert!(text.contains("1 todo"));
    assert!(text.contains("Spawn three sleep 5 tasks in parallel"));
    assert!(text.contains("completed"));
    assert!(!text.contains(r#""todos""#), "raw JSON key leaked: {text}");
    assert!(!text.contains('{'), "raw JSON object leaked: {text}");
}

#[test]
fn tool_detail_summarizes_json_array_arguments_without_dumping_json() {
    let area = Rect::new(0, 0, 100, 24);
    let arguments = r#"[{"content":"Spawn three sleep 5 tasks in parallel","status":"completed"}]"#;
    let buffer = render_tool_detail_with_arguments_to_buffer(area, arguments, "done", 0);
    let text = buffer_text(&buffer);

    assert!(text.contains("items"));
    assert!(text.contains("1 item"));
    assert!(text.contains("content, status"));
    assert!(!text.contains("│ ["), "array JSON leaked: {text}");
    assert!(!text.contains('{'), "raw JSON object leaked: {text}");
}

#[test]
fn bash_tool_detail_summarizes_command_arguments() {
    let area = Rect::new(0, 0, 100, 24);
    let arguments = serde_json::json!({
        "command": "cargo test --locked",
        "timeout_secs": 120,
        "run_in_background": true
    })
    .to_string();
    let buffer = render_tool_detail_activity_to_buffer(area, "bash", &arguments, "done", None, 0);
    let text = buffer_text(&buffer);

    assert!(text.contains("command"));
    assert!(text.contains("cargo test --locked"));
    assert!(text.contains("timeout"));
    assert!(text.contains("120"));
    assert!(text.contains("background"));
    assert!(text.contains("true"));
    assert!(
        !text.contains(r#""command""#),
        "raw JSON key leaked: {text}"
    );
    assert!(!text.contains('{'), "raw JSON object leaked: {text}");
}

#[test]
fn question_tool_detail_summarizes_options() {
    let area = Rect::new(0, 0, 110, 24);
    let arguments = serde_json::json!({
        "question": "Which path should we use?",
        "options": [
            {"label": "Primary", "description": "Use the main implementation path"},
            {"label": "Fallback", "description": "Use the fallback implementation path"}
        ],
        "multi_select": false
    })
    .to_string();
    let buffer =
        render_tool_detail_activity_to_buffer(area, "question", &arguments, "done", None, 0);
    let text = buffer_text(&buffer);

    assert!(text.contains("question"));
    assert!(text.contains("Which path should we use?"));
    assert!(text.contains("options"));
    assert!(text.contains("2 options: Primary, Fallback"));
    assert!(text.contains("multi select"));
    assert!(
        !text.contains(r#""options""#),
        "raw JSON key leaked: {text}"
    );
    assert!(!text.contains('{'), "raw JSON object leaked: {text}");
}

#[test]
fn plan_tool_detail_summarizes_patch_arguments() {
    let area = Rect::new(0, 0, 110, 24);
    let arguments = serde_json::json!({
        "heading": "Implementation",
        "operation": "replace_text",
        "old_text": "old plan text",
        "new_text": "new plan text"
    })
    .to_string();
    let buffer = render_tool_detail_activity_to_buffer(
        area,
        "plan_patch_section",
        &arguments,
        "done",
        None,
        0,
    );
    let text = buffer_text(&buffer);

    assert!(text.contains("heading"));
    assert!(text.contains("Implementation"));
    assert!(text.contains("operation"));
    assert!(text.contains("replace_text"));
    assert!(text.contains("old text"));
    assert!(text.contains("new plan text"));
    assert!(
        !text.contains(r#""heading""#),
        "raw JSON key leaked: {text}"
    );
    assert!(!text.contains('{'), "raw JSON object leaked: {text}");
}

#[test]
fn write_tool_detail_summarizes_arguments_without_content_json() {
    let area = Rect::new(0, 0, 100, 24);
    let arguments = serde_json::json!({
        "path": "src/main.rs",
        "content": "secret\npayload\n"
    })
    .to_string();
    let buffer = render_tool_detail_activity_to_buffer(area, "write", &arguments, "done", None, 0);
    let text = buffer_text(&buffer);

    assert!(text.contains("src/main.rs"));
    assert!(text.contains("write"));
    assert!(text.contains("15 bytes, 2 lines"));
    assert!(text.contains("new"));
    assert!(text.contains("\"secret payload\""));
    assert!(!text.contains("\"content\""), "raw JSON key leaked: {text}");
    assert!(!text.contains('{'), "raw JSON object leaked: {text}");
}

#[test]
fn edit_tool_detail_summarizes_replacement_arguments() {
    let area = Rect::new(0, 0, 100, 24);
    let old = "this old parameter value is intentionally long enough to truncate in the modal";
    let new = "this new parameter value is also intentionally long enough to truncate cleanly";
    let arguments = serde_json::json!({
        "path": "src/lib.rs",
        "old_string": old,
        "new_string": new,
        "replace_all": true
    })
    .to_string();
    let buffer = render_tool_detail_activity_to_buffer(area, "edit", &arguments, "done", None, 0);
    let text = buffer_text(&buffer);

    assert!(text.contains("src/lib.rs"));
    assert!(text.contains("replace all"));
    assert!(text.contains("..."));
    assert!(
        !text.contains("\"old_string\""),
        "raw JSON key leaked: {text}"
    );
    assert!(
        !text.contains("\"new_string\""),
        "raw JSON key leaked: {text}"
    );
    assert!(!text.contains(old), "full old_string leaked: {text}");
    assert!(!text.contains(new), "full new_string leaked: {text}");
}

#[test]
fn edit_tool_detail_summarizes_multi_region_arguments() {
    let area = Rect::new(0, 0, 120, 24);
    let old = "this old multi-region parameter value is intentionally long enough to truncate";
    let new = "this new multi-region parameter value is also intentionally long enough to truncate";
    let arguments = serde_json::json!({
        "path": "src/lib.rs",
        "edits": [
            {"old_string": old, "new_string": new},
            {"old_string": "alpha", "new_string": "beta", "replace_all": true}
        ]
    })
    .to_string();
    let buffer = render_tool_detail_activity_to_buffer(area, "edit", &arguments, "done", None, 0);
    let text = buffer_text(&buffer);

    assert!(text.contains("src/lib.rs"));
    assert!(text.contains("2 edits"));
    assert!(text.contains("old #1"));
    assert!(text.contains("new #1"));
    assert!(text.contains("old #2 (all)"));
    assert!(text.contains("new #2"));
    assert!(text.contains("alpha"));
    assert!(text.contains("beta"));
    assert!(text.contains("..."));
    assert!(
        !text.contains("1 more edit"),
        "multi-edit should render every region: {text}"
    );
    assert!(!text.contains("\"edits\""), "raw JSON key leaked: {text}");
    assert!(
        !text.contains("\"old_string\""),
        "raw JSON key leaked: {text}"
    );
    assert!(!text.contains(old), "full old_string leaked: {text}");
    assert!(!text.contains(new), "full new_string leaked: {text}");
}

#[test]
fn edit_argument_summary_names_create_and_delete_variants() {
    let create = edit_argument_lines(
        &serde_json::json!({
            "path": "created.txt",
            "old_string": "",
            "new_string": "created content"
        })
        .to_string(),
    )
    .expect("create args should summarize");
    let delete = edit_argument_lines(
        &serde_json::json!({
            "path": "deleted.txt",
            "old_string": "remove me",
            "new_string": ""
        })
        .to_string(),
    )
    .expect("delete args should summarize");

    let create_text = rendered_lines_text(&create);
    let delete_text = rendered_lines_text(&delete);
    assert!(create_text.contains("create"), "got: {create_text}");
    assert!(
        create_text.contains("created content"),
        "got: {create_text}"
    );
    assert!(delete_text.contains("delete"), "got: {delete_text}");
    assert!(delete_text.contains("remove me"), "got: {delete_text}");
}

#[test]
fn tool_detail_caps_rendered_diff_rows() {
    let now = std::time::Instant::now();
    let new = (1..=200)
        .map(|line| format!("new-{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let activity = ToolActivity {
        id: "call-1".to_string(),
        name: "write".to_string(),
        arguments: serde_json::json!({
            "path": "large.txt",
            "content": new
        })
        .to_string(),
        status: ToolStatus::Succeeded,
        result: Some("done".to_string()),
        diff: Some(crate::diff::build_file_diff(
            "large.txt".to_string(),
            None,
            &(1..=200)
                .map(|line| format!("new-{line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )),
        started_at: now,
        finished_at: Some(now),
    };

    let text = rendered_lines_text(&tool_detail_lines(&activity, None, 80));

    assert!(
        text.contains("more"),
        "hidden-row note should render: {text}"
    );
    assert!(
        !text.contains("new-200"),
        "tool detail should not render the entire diff"
    );
}

#[test]
fn tool_detail_preserves_raw_result_line_breaks() {
    let area = Rect::new(0, 0, 80, 18);
    let buffer = render_tool_detail_to_buffer(area, "1: first line\n2: second line\n", 0);
    let rows = (0..area.height)
        .map(|y| row_text(&buffer, y))
        .collect::<Vec<_>>();
    let first = rows
        .iter()
        .position(|row| row.contains("1: first line"))
        .expect("first read output line should render");
    let second = rows
        .iter()
        .position(|row| row.contains("2: second line"))
        .expect("second read output line should render");

    assert!(
        first < second,
        "read output lines should stay on separate rows: {rows:?}"
    );
    assert!(
        !rows
            .iter()
            .any(|row| row.contains("1: first line") && row.contains("2: second line")),
        "tool detail must not collapse result newlines into one row: {rows:?}"
    );
}

#[test]
fn ten_char_argument_label_keeps_space_before_value() {
    let area = Rect::new(0, 0, 100, 24);
    let arguments = serde_json::json!({
        "command": "sleep 5",
        "run_in_background": true
    })
    .to_string();
    let buffer = render_tool_detail_activity_to_buffer(area, "bash", &arguments, "done", None, 0);
    let text = buffer_text(&buffer);

    assert!(
        !text.contains("backgroundtrue"),
        "a 10-char label must not fuse with its value: {text}"
    );
    assert!(text.contains("background true"), "got: {text}");
}

#[test]
fn tool_detail_extracts_authorization_into_its_own_section() {
    let now = std::time::Instant::now();
    let activity = ToolActivity {
        id: "call-1".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"cargo test"}"#.to_string(),
        status: ToolStatus::Succeeded,
        result: Some(
            "[authorization] allow · high · network,code-execution · built-in-default · \
             autonomy=ask · sandbox=none:off,network:allow · delegated tools retain their own effect gates\n\n\
             test result: ok"
                .to_string(),
        ),
        diff: None,
        started_at: now,
        finished_at: Some(now),
    };

    let text = rendered_lines_text(&tool_detail_lines(&activity, None, 100));

    assert!(text.contains("authorization"), "{text}");
    assert!(text.contains("decision   allow"), "{text}");
    assert!(text.contains("risk       high"), "{text}");
    assert!(
        text.contains("effects    network, code-execution"),
        "{text}"
    );
    assert!(text.contains("rule       built-in-default"), "{text}");
    assert!(text.contains("autonomy   ask"), "{text}");
    assert!(text.contains("sandbox    none:off,network:allow"), "{text}");
    assert!(text.contains("note       delegated tools"), "{text}");
    assert!(text.contains("test result: ok"), "{text}");
    // The marker line must not leak into the result section.
    assert!(!text.contains("[authorization]"), "{text}");
    // The authorization section renders above the result section.
    let auth_at = text.find("decision").expect("decision row");
    let result_at = text.find("test result: ok").expect("result body");
    assert!(auth_at < result_at, "{text}");
}

#[test]
fn agent_tool_detail_sections_subagent_report() {
    let now = std::time::Instant::now();
    let result = "[authorization] allow · high · network,code-execution · built-in-default · \
                  autonomy=ask · sandbox=none:off,network:allow · delegated tools retain their own effect gates\n\n\
                  sub-2 succeeded as 'review'\n\n\
                  ID: sub-2\n\
                  Agent: review\n\
                  Mode: background\n\
                  Model: codex:openai/gpt-5.6-terra\n\
                  Launch group: group-1\n\
                  Status: succeeded\n\
                  Elapsed: 27s\n\n\
                  Prompt:\n\
                  Review uncommitted changes limited to eval files.\n\n\
                  Activity:\n\
                  ▸ git {\"op\":\"diff\"} ↳ ok\n\
                  ▸ git {\"op\":\"diff\",\"path\":\"eval/README.md\"} ↳ ok\n\n\
                  Result:\n\
                  No substantive findings.";
    let activity = ToolActivity {
        id: "call-1".to_string(),
        name: "agent".to_string(),
        arguments: r#"{"agent":"review","background":true}"#.to_string(),
        status: ToolStatus::Succeeded,
        result: Some(result.to_string()),
        diff: None,
        started_at: now,
        finished_at: Some(now),
    };

    let lines = tool_detail_lines(&activity, None, 100);
    let text = rendered_lines_text(&lines);

    // Every report part lands in its own section, in reading order.
    let order = [
        "arguments",
        "authorization",
        "subagent run",
        "prompt",
        "activity",
        "subagent response",
    ];
    let mut last = 0;
    for section in order {
        let at = text.find(section).unwrap_or_else(|| {
            panic!("section {section} missing: {text}");
        });
        assert!(at >= last, "section {section} out of order: {text}");
        last = at;
    }

    // Run metadata renders as one kv row per field, not one soft-wrapped
    // paragraph.
    assert!(text.contains("id         sub-2"), "{text}");
    assert!(
        text.contains("model      codex:openai/gpt-5.6-terra"),
        "{text}"
    );
    assert!(text.contains("status     succeeded"), "{text}");
    assert!(text.contains("elapsed    27s"), "{text}");
    let model_row = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .find(|row| row.contains("codex:openai/gpt-5.6-terra"))
        .expect("model row");
    assert!(
        !model_row.contains("succeeded"),
        "meta fields must not share a row: {model_row}"
    );

    // The redundant headline is dropped in favor of the structured run rows.
    assert!(!text.contains("succeeded as"), "{text}");
    // Activity entries stay one per line.
    assert!(text.contains("▸ git {\"op\":\"diff\"} ↳ ok"), "{text}");
    // The final response renders under its own header.
    assert!(text.contains("No substantive findings."), "{text}");
}

#[test]
fn agent_tool_detail_shows_adopted_subagent_model_while_running() {
    let now = std::time::Instant::now();
    let activity = ToolActivity {
        id: "call-1".to_string(),
        name: "agent".to_string(),
        arguments: r#"{"agent":"research"}"#.to_string(),
        status: ToolStatus::Running,
        result: None,
        diff: None,
        started_at: now,
        finished_at: None,
    };

    let text = rendered_lines_text(&tool_detail_lines(
        &activity,
        Some("openrouter:z-ai/glm-4.7"),
        100,
    ));

    // The row must carry the subagent run's own model — a running call has no
    // completion report yet, and the parent turn's model must never stand in.
    assert!(
        text.contains("model      openrouter:z-ai/glm-4.7"),
        "{text}"
    );
}

#[test]
fn tool_detail_has_no_model_row_without_a_subagent_run() {
    let now = std::time::Instant::now();
    let activity = ToolActivity {
        id: "call-1".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"cargo test"}"#.to_string(),
        status: ToolStatus::Running,
        result: None,
        diff: None,
        started_at: now,
        finished_at: None,
    };

    let text = rendered_lines_text(&tool_detail_lines(&activity, None, 100));

    assert!(
        !text.contains("model"),
        "a plain tool call must not claim a model: {text}"
    );
}

#[test]
fn agent_tool_detail_shows_placeholder_while_subagent_runs() {
    let now = std::time::Instant::now();
    let activity = ToolActivity {
        id: "call-1".to_string(),
        name: "agent".to_string(),
        arguments: r#"{"agent":"review"}"#.to_string(),
        status: ToolStatus::Running,
        result: Some("[authorization] allow · high · network · built-in-default".to_string()),
        diff: None,
        started_at: now,
        finished_at: None,
    };

    let text = rendered_lines_text(&tool_detail_lines(&activity, None, 100));

    assert!(text.contains("authorization"), "{text}");
    assert!(
        text.contains("subagent still running"),
        "an authorization-only result must still show the running placeholder: {text}"
    );
}

#[test]
fn agent_tool_detail_renders_markdown_result() {
    let area = Rect::new(0, 0, 96, 24);
    let result = "# Agent Report\n\n- Read `src/bootstrap.rs`\n- Mapped provider flow";
    let buffer = render_tool_detail_activity_to_buffer(
        area,
        "agent",
        r#"{"prompt":"map"}"#,
        result,
        None,
        0,
    );
    let text = buffer_text(&buffer);

    assert!(
        text.contains("subagent response"),
        "result label missing: {text}"
    );
    assert!(text.contains("Agent Report"), "heading missing: {text}");
    assert!(text.contains("Read"), "list item missing: {text}");
    assert!(
        text.contains("src/bootstrap.rs"),
        "inline code text missing: {text}"
    );
    assert!(
        !text.contains("# Agent Report"),
        "agent result should not render raw Markdown heading markers: {text}"
    );
}

#[test]
fn tool_detail_scroll_changes_visible_result_rows() {
    let area = Rect::new(0, 0, 70, 12);
    let result = (1..=30)
        .map(|line| format!("{line}: {}", "abcdefghij ".repeat(8)))
        .collect::<Vec<_>>()
        .join("\n");

    let top = render_tool_detail_to_buffer(area, &result, 0);
    let scrolled = render_tool_detail_to_buffer(area, &result, 8);

    let mut diffs = 0;
    for y in 0..area.height {
        if row_text(&top, y) != row_text(&scrolled, y) {
            diffs += 1;
        }
    }
    assert!(
        diffs > 0,
        "scrolling tool detail should change visible result rows"
    );
}

#[test]
fn permission_prompt_scrolls_long_command() {
    let area = Rect::new(0, 0, 60, 12);
    let long_command = "echo ".to_string() + &"abcdefghij ".repeat(40);

    let unscrolled = render_to_buffer(area, &long_command, 0);
    let scrolled = render_to_buffer(area, &long_command, 5);

    // At least one row should differ between the two renders, proving the
    // scroll actually changes what's visible in the body.
    let mut diffs = 0;
    for y in 0..area.height {
        if row_text(&unscrolled, y) != row_text(&scrolled, y) {
            diffs += 1;
        }
    }
    assert!(
        diffs > 0,
        "scrolling the body should change what the user sees in the body"
    );
}

#[test]
fn question_prompt_scroll_changes_visible_options() {
    let area = Rect::new(0, 0, 60, 12);
    let top = render_question_to_buffer(area, 0);
    let scrolled = render_question_to_buffer(area, 12);
    let top_text = (0..area.height)
        .map(|y| row_text(&top, y))
        .collect::<Vec<_>>()
        .join("\n");
    let scrolled_text = (0..area.height)
        .map(|y| row_text(&scrolled, y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(top_text.contains("Option 0"));
    assert!(!top_text.contains("Option 8"));
    assert!(scrolled_text.contains("Option 8"));
    assert!(!scrolled_text.contains("Option 0"));
}

#[test]
fn question_prompt_last_option_visible_at_max_scroll() {
    // With the cursor on the last option, rendering at the metric's reported
    // max scroll must reveal it — the class of clamp bug the shared body
    // builder makes unrepresentable.
    let area = Rect::new(0, 0, 60, 12);
    let options = (0..12)
        .map(|index| crate::interaction::QuestionOption {
            label: format!("Option {index} has a label that wraps in narrow modals"),
            description: format!("Description {index} wraps as well"),
            preselected: false,
        })
        .collect::<Vec<_>>();
    let selected = vec![false; options.len()];
    let last = options.len() - 1;
    let metrics = question_prompt_metrics(area, "Pick one", None, &options, false, last);

    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
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
                last,
                &selected,
                metrics.max_scroll,
            );
        })
        .expect("question prompt should render");
    let text = buffer_text(&terminal.backend().buffer().clone());
    assert!(
        text.contains("Option 11"),
        "last option should be visible at max scroll:\n{text}"
    );
}

#[test]
fn question_prompt_metrics_counts_origin_line() {
    let area = Rect::new(0, 0, 60, 12);
    let options = (0..6)
        .map(|index| crate::interaction::QuestionOption {
            label: format!("Option {index}"),
            description: format!("Description {index}"),
            preselected: false,
        })
        .collect::<Vec<_>>();

    let without_origin = question_prompt_metrics(area, "Pick one", None, &options, false, 0);
    let with_origin = question_prompt_metrics(
        area,
        "Pick one",
        Some("subagent explore (sub-1)"),
        &options,
        false,
        0,
    );

    assert_eq!(
        with_origin.body_height.saturating_add(1),
        without_origin.body_height
    );
}

#[test]
fn permission_prompt_terminal_size_matrix_keeps_footer_and_tail_reachable() {
    for (width, height) in [(40, 8), (60, 12), (80, 24), (120, 40)] {
        let area = Rect::new(0, 0, width, height);
        let command = format!("cargo test {} TAIL-MARKER", "--workspace ".repeat(48));
        let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
        app.modal = Some(ModalKind::PermissionPrompt {
            request_id: 1,
            command,
            origin: Some("subagent explore with a deliberately long origin".to_string()),
        });
        let modal = modal_area(area, app.modal.as_ref().expect("modal"));
        assert!(modal.width <= area.width && modal.height <= area.height);
        assert!(modal.width >= MIN_MODAL_WIDTH.min(area.width));
        assert!(modal.height >= MIN_MODAL_HEIGHT.min(area.height));

        let max_scroll = max_modal_scroll(&app, area).expect("permission scroll metrics");
        assert!(max_scroll > 0, "{width}x{height} should scroll");
        app.modal_scroll = max_scroll;
        let buffer = render_full_modal(area, &app);
        let text = buffer_text(&buffer);
        assert!(text.contains("Permission"), "{width}x{height}:\n{text}");
        assert!(text.contains("RKER"), "{width}x{height}:\n{text}");
        assert!(
            text.contains("D/Esc") && text.contains("deny"),
            "{width}x{height}:\n{text}"
        );
    }
}

#[test]
fn question_prompt_terminal_size_matrix_keeps_selection_and_footer_visible() {
    for (width, height) in [(40, 8), (60, 12), (80, 24), (120, 40)] {
        let area = Rect::new(0, 0, width, height);
        let prompt = "Choose the safest next action after reviewing all available evidence";
        let options = (0..20)
            .map(|index| crate::interaction::QuestionOption {
                label: format!("Option {index}"),
                description: String::new(),
                preselected: false,
            })
            .collect::<Vec<_>>();
        let last = options.len() - 1;
        let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
        app.modal = Some(ModalKind::QuestionPrompt {
            request_id: 1,
            prompt: prompt.to_string(),
            header: Some("Matrix question".to_string()),
            options,
            multiple: false,
            origin: Some("subagent explore with a deliberately long origin".to_string()),
            cursor: last,
            selected: vec![false; 20],
        });
        let modal = modal_area(area, app.modal.as_ref().expect("modal"));
        assert!(modal.width <= area.width && modal.height <= area.height);
        assert!(modal.width >= MIN_MODAL_WIDTH.min(area.width));
        assert!(modal.height >= MIN_MODAL_HEIGHT.min(area.height));
        let max_scroll = max_modal_scroll(&app, area).expect("question scroll metrics");
        assert!(max_scroll > 0, "{width}x{height} should scroll");
        app.modal_scroll = max_scroll;

        let buffer = render_full_modal(area, &app);
        let text = buffer_text(&buffer);
        assert!(
            text.contains("Matrix question"),
            "{width}x{height}:\n{text}"
        );
        assert!(text.contains("> Option 19"), "{width}x{height}:\n{text}");
        assert!(
            text.contains("Esc") && text.contains("cancel"),
            "{width}x{height}:\n{text}"
        );
    }
}

fn render_full_modal(area: Rect, app: &AppState) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    terminal
        .draw(|frame| render(frame, area, app))
        .expect("modal should render");
    terminal.backend().buffer().clone()
}

#[test]
fn question_prompt_uses_its_header_as_the_title_and_aligns_option_details() {
    let area = Rect::new(0, 0, 80, 10);
    let options = vec![
        crate::interaction::QuestionOption {
            label: "Trust & restart".to_string(),
            description: "Enable workspace resources next launch.".to_string(),
            preselected: false,
        },
        crate::interaction::QuestionOption {
            label: "Keep restricted".to_string(),
            description: "Leave workspace resources disabled.".to_string(),
            preselected: false,
        },
    ];
    let selected = vec![false; options.len()];
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");

    terminal
        .draw(|frame| {
            render_question_prompt(
                frame,
                area,
                "Project-owned configuration is disabled until trusted.",
                Some("Workspace trust"),
                None,
                &options,
                false,
                0,
                &selected,
                0,
            );
        })
        .expect("question prompt should render");

    let buffer = terminal.backend().buffer().clone();
    let first_row = (0..area.height)
        .map(|row| row_text(&buffer, row))
        .find(|row| row.contains("Trust & restart"))
        .expect("first option should render");
    let second_row = (0..area.height)
        .map(|row| row_text(&buffer, row))
        .find(|row| row.contains("Keep restricted"))
        .expect("second option should render");

    assert!(buffer_text(&buffer).contains("Workspace trust"));
    assert_eq!(
        first_row.find("Enable"),
        second_row.find("Leave"),
        "option descriptions should share an aligned detail column"
    );
}

#[test]
fn permission_prompt_has_a_visible_frame() {
    let area = Rect::new(0, 0, 60, 12);
    let buffer = render_to_buffer(area, "ls -la", 0);

    // The frame uses a rounded border, so the top-left corner is a '╭'
    // and the top-right corner is a '╮'. The vertical edges should be
    // '│' (not blank, not the scrollbar glyph).
    assert_eq!(
        buffer[(0, 0)].symbol(),
        "╭",
        "top-left corner should be a rounded frame border"
    );
    assert_eq!(
        buffer[(area.width - 1, 0)].symbol(),
        "╮",
        "top-right corner should be a rounded frame border"
    );
    assert_eq!(
        buffer[(0, area.height - 1)].symbol(),
        "╰",
        "bottom-left corner should be a rounded frame border"
    );
    assert_eq!(
        buffer[(area.width - 1, area.height - 1)].symbol(),
        "╯",
        "bottom-right corner should be a rounded frame border"
    );
}

#[test]
fn block_detail_renders_markdown_and_scrolls() {
    let area = Rect::new(0, 0, 60, 12);
    let text = "# Title\n\n".to_string()
        + &(0..30)
            .map(|index| format!("- body line {index}\n"))
            .collect::<String>();

    let top = render_block_detail_to_buffer(area, &text, 0);
    let scrolled = render_block_detail_to_buffer(area, &text, 8);

    let top_rows = (0..area.height)
        .map(|y| row_text(&top, y))
        .collect::<Vec<_>>();
    assert!(
        top_rows.iter().any(|row| row.contains("Title")),
        "markdown heading should render in block detail"
    );

    let mut diffs = 0;
    for y in 0..area.height {
        if row_text(&top, y) != row_text(&scrolled, y) {
            diffs += 1;
        }
    }
    assert!(diffs > 0, "modal scroll should change visible block body");
}

#[test]
fn review_scope_picker_renders_all_options_and_marks_selected() {
    let area = Rect::new(0, 0, 60, 12);
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    terminal
        .draw(|frame| {
            render_review_scope_picker(frame, area, 1);
        })
        .expect("review scope picker should render");
    let buffer = terminal.backend().buffer().clone();
    let rows = (0..area.height)
        .map(|y| row_text(&buffer, y))
        .collect::<Vec<_>>();
    let text = rows.join("\n");

    assert!(text.contains("Review"));
    assert!(text.contains("Uncommitted changes"));
    assert!(text.contains("Last commit"));
    assert!(text.contains("Diff vs main branch"));
    assert!(text.contains("git diff HEAD"));
    assert!(text.contains("git diff master/main...HEAD"));

    // The selected row (cursor 1 = "Last commit") carries the arrow marker.
    let selected = rows
        .iter()
        .find(|row| row.contains("Last commit"))
        .expect("selected scope should render");
    assert!(
        selected.contains("> Last commit"),
        "selected row should use the arrow marker, got: {selected:?}"
    );
    // A non-selected row does not.
    let unselected = rows
        .iter()
        .find(|row| row.contains("Uncommitted changes"))
        .expect("first scope should render");
    assert!(
        !unselected.contains("> Uncommitted"),
        "non-selected row should not use the arrow marker, got: {unselected:?}"
    );
}

#[test]
fn diff_preview_wraps_wide_line_and_reaches_the_tail() {
    // A single very wide added line has few logical lines but many wrapped
    // rows. The old logical-line clamp could not scroll to the wrapped tail;
    // render_scrollable_modal clamps on wrapped rows, so it becomes reachable.
    let area = Rect::new(0, 0, 40, 12);
    let diff = file_diff_with_wide_line("ENDMARKER", 300);
    // Scroll past the end; the modal clamps to the real (wrapped) max.
    let buffer = render_diff_preview_to_buffer(area, diff, u16::MAX);
    let text = buffer_text(&buffer);
    assert!(
        text.contains("ENDMARKER"),
        "wrapped tail should be reachable at max scroll:\n{text}"
    );
}

#[test]
fn diff_preview_reports_wrapped_max_scroll() {
    // The wrapped rows of a wide line exceed the viewport even though the
    // logical line count does not, so max scroll must be positive.
    let area = Rect::new(0, 0, 40, 12);
    let diff = file_diff_with_wide_line("ENDMARKER", 300);
    assert!(
        max_diff_preview_scroll(area, &diff) > 0,
        "a wide wrapped diff line should produce a positive max scroll"
    );
}

#[test]
fn diff_preview_pins_footer_hints() {
    let area = Rect::new(0, 0, 60, 12);
    let diff = file_diff_with_wide_line("tail", 10);
    let buffer = render_diff_preview_to_buffer(area, diff, 0);
    let (_, hint_row) =
        find_cell(&buffer, "close").expect("diff preview should render a close hint");
    assert!(
        hint_row >= area.height.saturating_sub(3),
        "the scroll/close footer should be pinned near the bottom, got row {hint_row}"
    );
    assert!(
        row_text(&buffer, hint_row).contains("Esc"),
        "footer hint row should name the Esc key"
    );
}

#[test]
fn plan_finding_detail_lines_render_header_and_sections() {
    let finding = crate::plan::Finding {
        severity: crate::plan::Severity::Major,
        file: Some("src/main.rs".to_string()),
        line: Some(42),
        issue: "Off-by-one in the loop bound".to_string(),
        required_fix: "Use `<=` instead of `<`".to_string(),
        acceptance_tests: vec!["covers the last element".to_string()],
        source_ids: vec!["call-1".to_string()],
        task: None,
        resolved: false,
    };
    let text = rendered_lines_text(&plan_finding_detail_lines(&finding, 60));
    assert!(text.contains("severity"), "header shows severity: {text}");
    assert!(text.contains("MAJOR"));
    assert!(
        text.contains("src/main.rs:42"),
        "location row shows file:line: {text}"
    );
    assert!(text.contains("location"));
    assert!(text.contains("issue"));
    assert!(text.contains("required fix"));
    assert!(text.contains("acceptance"));
    assert!(text.contains("evidence"));
    // An unresolved finding shows no status row.
    assert!(
        !text.contains("resolved"),
        "unresolved finding omits status: {text}"
    );
}

#[test]
fn confirm_prompt_specs_render_expected_content() {
    // Permission: offers the project scope; the command is the body.
    let p = permission_prompt("ls -la", None);
    assert!(p.scopes.project);
    assert_eq!(rendered_lines_text(&p.body), "ls -la");
    let footer = rendered_lines_text(&approval_footer(&p.scopes));
    assert!(
        footer.contains("P project"),
        "permission offers project: {footer}"
    );

    // Sandbox escalation: never offers project; subtitle warns about confinement.
    let s = sandbox_escalation_prompt("rm -rf /", None);
    assert!(!s.scopes.project);
    let s_footer = rendered_lines_text(&approval_footer(&s.scopes));
    assert!(
        !s_footer.contains("P project"),
        "sandbox escape never offers a persistent project rule: {s_footer}"
    );
    assert!(rendered_lines_text(&s.subtitle).contains("confinement"));

    // Web domain: the redirect subtitle appears only when redirected.
    let direct = web_domain_prompt("example.com", "https://example.com/x", None, None);
    assert_eq!(direct.title, "Web access");
    assert!(
        rendered_lines_text(std::slice::from_ref(&direct.heading)).contains("Allow web access")
    );
    assert!(direct.subtitle.is_empty());
    let redirected = web_domain_prompt("evil.com", "https://evil.com/x", Some("good.com"), None);
    assert!(
        rendered_lines_text(&redirected.subtitle).contains("Redirected from good.com"),
        "redirect warning names the origin"
    );

    // Extension tool: undeclared caps, and the args line is dropped when empty.
    let no_caps = extension_tool_prompt("srv.tool", "srv", &[], "");
    assert_eq!(no_caps.body.len(), 2, "id + caps, no args line");
    assert!(rendered_lines_text(&no_caps.body).contains("capabilities: undeclared"));
    let with = extension_tool_prompt("srv.tool", "srv", &["read".to_string()], "{\"path\":\"x\"}");
    assert_eq!(with.body.len(), 3, "id + caps + args line");
    let with_body = rendered_lines_text(&with.body);
    assert!(with_body.contains("capabilities: read"));
    assert!(with_body.contains("path"));

    // Hook trust: the subtitle names the event and action kind.
    let h = hook_trust_prompt("format", "PostToolUse", "shell", "prettier --write");
    assert!(rendered_lines_text(&h.subtitle).contains("fires on PostToolUse · shell"));
    assert!(rendered_lines_text(&h.body).contains("prettier --write"));
}
