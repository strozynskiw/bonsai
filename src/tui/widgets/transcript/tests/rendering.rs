use super::*;
use crate::tui::widgets::transcript::diff_card::diff_line;
use crate::tui::widgets::transcript::tool_card::arg_summary_lines;

#[test]
fn render_block_stays_within_provided_width() {
    let body = "| Col 1 | Col 2 | Col 3 |\n| --- | --- | --- |\n| A | B | C |";
    let lines = render_block(
        "[test]",
        body,
        theme::palette().user,
        md(theme::palette().text),
        theme::palette().user_block,
        60,
        ItemSelection::None,
        false,
    );
    for line in &lines {
        let len: usize = line
            .spans
            .iter()
            .map(|span| span.content.chars().count())
            .sum();
        assert!(
            len <= 60,
            "render_block line exceeded width 60: len={len}, line={:?}",
            line.spans
        );
    }
}

#[test]
fn partial_selection_preserves_rendered_block_layout() {
    use unicode_segmentation::UnicodeSegmentation;

    let body = "bonsai is a Rust terminal coding agent: it edits files, runs tools, and talks to multiple model providers through a TUI. It supports agentic coding workflows, planning mode, permissions, and provider auth/import flows like `/authorize`.";
    let start = body[..body.find("UI. It").expect("selection start should exist")]
        .graphemes(true)
        .count();
    let end = body[..body.find("/authorize").expect("selection end should exist")]
        .graphemes(true)
        .count()
        + "/aut".graphemes(true).count();

    let plain = render_block(
        "● bonsai",
        body,
        theme::palette().assistant,
        md(theme::palette().text),
        theme::palette().assistant_block,
        120,
        ItemSelection::None,
        false,
    );
    let selected = render_block(
        "● bonsai",
        body,
        theme::palette().assistant,
        md(theme::palette().text),
        theme::palette().assistant_block,
        120,
        ItemSelection::Range { start, end },
        false,
    );
    let has_selection_bg = selected
        .iter()
        .flat_map(|line| line.spans.iter())
        .any(|span| span.style.bg == Some(theme::palette().selection_bg));

    assert_eq!(
        rendered_text(selected),
        rendered_text(plain),
        "selection highlighting must not reflow or split transcript text"
    );
    assert!(
        has_selection_bg,
        "partial selection should still paint selected spans"
    );
}

#[test]
fn render_block_handles_narrow_terminals() {
    let body = "A long paragraph of assistant text that should not overflow the card even on a narrow column width that is still wide enough to render.";
    for width in [20usize, 32, 48, 64, 96] {
        let lines = render_block(
            "[assistant]",
            body,
            theme::palette().assistant,
            md(theme::palette().text),
            theme::palette().assistant_block,
            width,
            ItemSelection::None,
            false,
        );
        for line in &lines {
            let len: usize = line
                .spans
                .iter()
                .map(|span| span.content.chars().count())
                .sum();
            assert!(
                len <= width,
                "render_block overflowed at width {width}: len={len}, line spans={:?}",
                line.spans
            );
        }
    }
}

#[test]
fn render_block_keeps_wide_chars_within_width() {
    // Mix of CJK (2 cells), emoji (2 cells), accents (1 cell each).
    let body = "Schrödinger's cat walks into a bar. And doesn't. 🐱🍺 Both states observed.";
    for width in [20usize, 40, 80] {
        let lines = render_block(
            "[assistant]",
            body,
            theme::palette().assistant,
            md(theme::palette().text),
            theme::palette().assistant_block,
            width,
            ItemSelection::None,
            false,
        );
        for line in &lines {
            let cells: usize = line
                .spans
                .iter()
                .map(|span| span.content.as_ref().width())
                .sum();
            assert!(
                cells <= width,
                "wide char overflow at width {width}: cells={cells}, line spans={:?}",
                line.spans
            );
        }
    }
}

#[test]
fn card_glyphs_are_single_cell_wide() {
    // ratatui resets the continuation cell behind a width-2 glyph, which
    // punches a terminal-default-background hole through the card.
    for glyph in [
        "❯", "●", "✦", "·", "✗", "±", "≡", "✓", "⋯", "┃", "→", "$", "○", "•", "×", "⌂", "⎇", "⏱",
    ] {
        assert_eq!(
            glyph.width(),
            1,
            "glyph {glyph} must be one cell wide to avoid reset continuation cells"
        );
    }
    for frame in theme::SPINNER {
        assert_eq!(frame.width(), 1, "spinner frame {frame} must be one cell");
    }
}

#[test]
fn arg_summary_renders_command_without_json() {
    let lines = arg_summary_lines(r#"{"command":"cargo test --all","timeout":120}"#);

    assert_eq!(lines.len(), 1);
    let text: String = lines[0]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(text.starts_with("$ cargo test --all"), "got: {text}");
    assert!(text.contains("timeout 120"), "got: {text}");
    assert!(!text.contains('{'), "raw JSON must not leak: {text}");
}

#[test]
fn tool_card_summarizes_todowrite_in_progress_item() {
    let activity = ToolActivity {
            id: "t-1".to_string(),
            name: "todowrite".to_string(),
            arguments: r#"{"todos":[{"content":"Inspect uncommitted changes","status":"in_progress"},{"content":"Run tests","status":"pending"}]}"#
                .to_string(),
            delegated_model: None,
            status: ToolStatus::Succeeded,
            result: Some("ok".to_string()),
            diff: None,
            started_at: std::time::Instant::now(),
            finished_at: Some(std::time::Instant::now()),
        };
    let lines = transcript_lines_for_activity(&activity, 120);
    let text: String = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        text.contains("Inspect uncommitted changes"),
        "in_progress item content should appear, got: {text}"
    );
    assert!(!text.contains('{'), "raw JSON must not leak: {text}");
}

#[test]
fn tool_card_summarizes_todowrite_count_when_no_in_progress() {
    let activity = ToolActivity {
            id: "t-2".to_string(),
            name: "todowrite".to_string(),
            arguments: r#"{"todos":[{"content":"a","status":"completed"},{"content":"b","status":"completed"}]}"#
                .to_string(),
            delegated_model: None,
            status: ToolStatus::Succeeded,
            result: Some("ok".to_string()),
            diff: None,
            started_at: std::time::Instant::now(),
            finished_at: Some(std::time::Instant::now()),
        };
    let lines = transcript_lines_for_activity(&activity, 120);
    let text: String = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        text.contains("2 todos"),
        "expected count summary, got: {text}"
    );
    assert!(!text.contains('{'), "raw JSON must not leak: {text}");
}

#[test]
fn tool_card_summarizes_question_options_without_raw_json() {
    let activity = ToolActivity {
        id: "q-1".to_string(),
        name: "question".to_string(),
        arguments: serde_json::json!({
            "id": "json-call-id",
            "question": "Which implementation path should I take?",
            "options": [
                {"label": "A"},
                {"label": "B"},
                {"label": "C"},
                {"label": "D"}
            ]
        })
        .to_string(),
        delegated_model: None,
        status: ToolStatus::Running,
        result: None,
        diff: None,
        started_at: std::time::Instant::now(),
        finished_at: None,
    };
    let lines = transcript_lines_for_activity(&activity, 120);
    let text: String = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();

    assert!(text.contains("question (4 options)"), "got: {text}");
    assert!(
        text.contains("Which implementation path should I take?"),
        "got: {text}"
    );
    assert!(
        !text.contains("json-call-id") && !text.contains('{'),
        "raw JSON must not leak: {text}"
    );
}

#[test]
fn arg_summary_colors_file_paths() {
    let lines = arg_summary_lines(r#"{"file_path":"src/main.rs","limit":40}"#);

    let path_span = lines[0]
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "src/main.rs")
        .expect("path span should exist");
    assert_eq!(path_span.style.fg, Some(theme::palette().path));
}

#[test]
fn diff_rows_carry_their_own_backgrounds() {
    let added = diff_line(&DiffLine {
        kind: DiffLineKind::Added,
        content: "new line".to_string(),
        old_line: None,
        new_line: Some(7),
    });
    let removed = diff_line(&DiffLine {
        kind: DiffLineKind::Removed,
        content: "old line".to_string(),
        old_line: Some(7),
        new_line: None,
    });

    assert!(
        added
            .spans
            .iter()
            .any(|span| span.style.bg == Some(theme::palette().added_bg))
    );
    assert!(
        removed
            .spans
            .iter()
            .any(|span| span.style.bg == Some(theme::palette().removed_bg))
    );
    let number: String = added.spans[0].content.as_ref().to_string();
    assert_eq!(number, "   7 ", "added rows use the new-file line number");
}

#[test]
fn markdown_inside_cards_never_leaks_the_panel_background() {
    let body = "# Title\n\nplain **bold** *italic* `code` [link](https://x.dev)\n\n> quote\n\n- item\n\n```rust\nfn x() {}\n```\n\n| a | b |\n| - | - |\n| 1 | 2 |";
    let lines = render_block(
        "● bonsai",
        body,
        theme::palette().assistant,
        theme::block(theme::palette().text, theme::palette().assistant_block),
        theme::palette().assistant_block,
        80,
        ItemSelection::None,
        false,
    );

    for line in &lines {
        // Skip the accent rail, which intentionally sits on the panel.
        for span in line.spans.iter().skip(1) {
            assert_ne!(
                span.style.bg,
                Some(theme::palette().panel),
                "markdown span leaked the panel background: {:?}",
                span.content
            );
        }
    }
}

#[test]
fn card_line_preserves_span_backgrounds() {
    let line = Line::from(vec![Span::styled(
        "+ new".to_string(),
        Style::default()
            .fg(theme::palette().added)
            .bg(theme::palette().added_bg),
    )]);

    let rendered = card_line(
        &line,
        40,
        theme::block(theme::palette().text, theme::palette().result_block),
        theme::palette().result_block,
        theme::palette().success,
        false,
    );

    assert!(
        rendered
            .spans
            .iter()
            .any(|span| span.style.bg == Some(theme::palette().added_bg)),
        "diff background must survive card composition"
    );
}

#[test]
fn wrap_does_not_split_a_fitting_remainder_at_its_last_space() {
    let line = Line::from("aaaa bbbb cccc dddd");

    let wrapped = wrap_card_line(&line, 10);

    let texts: Vec<String> = wrapped
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect();
    assert_eq!(
        texts,
        vec!["aaaa bbbb ".to_string(), "cccc dddd".to_string()],
        "the remainder fits in one line and must not be broken again"
    );
}

#[test]
fn render_block_applies_selection_background() {
    let body = "A short message.";
    let lines = render_block(
        "[user]",
        body,
        theme::palette().user,
        md(theme::palette().text),
        theme::palette().user_block,
        60,
        ItemSelection::Whole,
        false,
    );
    let has_selection_bg = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .any(|span| span.style.bg == Some(theme::palette().selection_bg));
    assert!(
        has_selection_bg,
        "selected card should paint at least one span with the selection background"
    );
}

#[test]
fn focused_block_uses_distinct_gutter_without_selection_background() {
    let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
    app.transcript.push(TranscriptItem::UserMessage {
        text: "hi".to_string(),
    });
    app.transcript_focus = Some(0);

    let lines = transcript_lines(&app, 80);
    let has_focus_gutter = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .any(|span| span.content.as_ref() == "▌ ");
    let has_selection_bg = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .any(|span| span.style.bg == Some(theme::palette().selection_bg));

    assert!(has_focus_gutter, "focused block should render a gutter");
    assert!(
        !has_selection_bg,
        "focus must not reuse text selection background"
    );
}

#[test]
fn transcript_lines_skip_unselected_items() {
    let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
    app.transcript.push(TranscriptItem::UserMessage {
        text: "hi".to_string(),
    });
    app.transcript.push(TranscriptItem::AssistantMessage {
        text: "hello".to_string(),
    });
    app.transcript_selection = Some(TranscriptSelection {
        anchor: TranscriptPosition {
            item: 1,
            grapheme: 0,
            width: 76,
        },
        caret: TranscriptPosition {
            item: 1,
            grapheme: 5,
            width: 76,
        },
    });

    let lines = transcript_lines(&app, 80);

    let find_card = |label: &str| -> Vec<ratatui::text::Line<'static>> {
        let mut out = Vec::new();
        let mut inside = false;
        for line in &lines {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            if text.contains(label) {
                inside = true;
                out.push(line.clone());
                continue;
            }
            if inside && text.trim().is_empty() {
                break;
            }
            if inside {
                out.push(line.clone());
            }
        }
        out
    };

    let selected_card = find_card("● bonsai");
    let unselected_card = find_card("❯ you");

    let selected_has_bg = selected_card
        .iter()
        .flat_map(|line| line.spans.iter())
        .any(|span| span.style.bg == Some(theme::palette().selection_bg));
    let unselected_has_bg = unselected_card
        .iter()
        .flat_map(|line| line.spans.iter())
        .any(|span| span.style.bg == Some(theme::palette().selection_bg));

    assert!(
        selected_has_bg,
        "selected item card should include the selection background"
    );
    assert!(
        !unselected_has_bg,
        "unselected item card should not include the selection background"
    );
}

#[test]
fn transcript_lines_skip_suppressed_items() {
    let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
    app.transcript.push(TranscriptItem::AssistantMessage {
        text: String::new(),
    });
    app.transcript.push(TranscriptItem::UserMessage {
        text: "visible".to_string(),
    });

    let body = transcript_lines(&app, 80)
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(!body.contains("● bonsai"), "{body}");
    assert!(!body.contains("(empty)"), "{body}");
    assert!(body.contains("visible"), "{body}");
}

#[test]
fn screen_reader_mode_uses_linear_labels_and_expands_tool_groups() {
    let now = std::time::Instant::now();
    let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
    app.screen_reader_mode = true;
    app.reduced_motion = true;
    app.transcript.push(TranscriptItem::UserMessage {
        text: "please inspect".to_string(),
    });
    app.transcript
        .push(TranscriptItem::ExecutionGroup(execution_group(
            1,
            now,
            vec![
                ToolActivity {
                    id: "read-1".to_string(),
                    name: "read".to_string(),
                    arguments: r#"{"file_path":"src/main.rs"}"#.to_string(),
                    delegated_model: None,
                    status: ToolStatus::Succeeded,
                    result: Some("file contents".to_string()),
                    diff: None,
                    started_at: now,
                    finished_at: Some(now),
                },
                ToolActivity {
                    id: "bash-1".to_string(),
                    name: "bash".to_string(),
                    arguments: r#"{"command":"cargo test"}"#.to_string(),
                    delegated_model: None,
                    status: ToolStatus::Running,
                    result: None,
                    diff: None,
                    started_at: now,
                    finished_at: None,
                },
            ],
            None,
        )));

    let rendered = rendered_text(transcript_lines(&app, 80)).join("\n");
    assert!(rendered.contains("You:\nplease inspect"), "{rendered}");
    assert!(
        rendered.contains("Tool read (done): file_path=src/main.rs"),
        "{rendered}"
    );
    assert!(rendered.contains("file contents"), "{rendered}");
    assert!(
        rendered.contains("Tool test (running): command=cargo test"),
        "{rendered}"
    );
    assert!(!rendered.contains('┃'), "{rendered}");
    assert!(!rendered.contains('✓'), "{rendered}");
}

#[test]
fn screen_reader_mode_does_not_change_active_theme() {
    let original = theme::current_theme_name();
    let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
    app.screen_reader_mode = true;
    app.reduced_motion = true;
    app.transcript.push(TranscriptItem::AssistantMessage {
        text: "accessible output".to_string(),
    });

    let rendered = rendered_text(transcript_lines(&app, 80)).join("\n");
    let active_theme = theme::current_theme_name();

    assert_eq!(active_theme, original);
    assert!(
        rendered.contains("Bonsai:\naccessible output"),
        "{rendered}"
    );
}

#[test]
fn zero_length_text_selection_does_not_render_selection_background() {
    let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
    app.transcript.push(TranscriptItem::AssistantMessage {
        text: "hello".to_string(),
    });
    app.transcript_selection = Some(TranscriptSelection {
        anchor: TranscriptPosition {
            item: 0,
            grapheme: 2,
            width: 76,
        },
        caret: TranscriptPosition {
            item: 0,
            grapheme: 2,
            width: 76,
        },
    });

    let lines = transcript_lines(&app, 80);
    let has_selection_bg = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .any(|span| span.style.bg == Some(theme::palette().selection_bg));

    assert!(
        !has_selection_bg,
        "caret-only text clicks must not paint the card as selected"
    );
}

fn welcome_text(view: crate::tui::event::View) -> Vec<String> {
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.view = view;
    welcome_text_for(&app)
}

fn welcome_text_for(app: &AppState) -> Vec<String> {
    empty_state(app)
        .iter()
        .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

#[test]
fn no_provider_welcome_focuses_on_getting_a_provider() {
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.has_authorized_provider = false;
    let text = welcome_text_for(&app);

    assert!(
        text.iter()
            .any(|line| line.contains("No provider is authorized")),
        "the blurb must say why nothing works yet"
    );
    for command in ["/authorize", "/wizard", "/model"] {
        assert!(
            text.iter().any(|line| line.contains(command)),
            "no-provider welcome should list {command}"
        );
    }
    assert!(
        !text.iter().any(|line| line.contains("Controls")),
        "controls belong to the authorized welcome, not the empty state"
    );

    // Once a provider is authorized the regular welcome returns.
    app.has_authorized_provider = true;
    let text = welcome_text_for(&app);
    assert!(text.iter().any(|line| line.contains("token-efficient")));
    assert!(!text.iter().any(|line| line.contains("/wizard")));
}

#[test]
fn agent_welcome_shows_bonsai_blurb_and_critical_commands_only() {
    use crate::tui::event::View;
    let text = welcome_text(View::Agent);

    assert!(
        text.iter().any(|line| line.contains("Welcome to bonsai")),
        "welcome heading should be present"
    );
    assert!(
        text.iter().any(|line| line.contains("token-efficient")),
        "a blurb about bonsai should be present"
    );

    // `/mode ` keeps its trailing space so the check can't pass on `/model`.
    for command in ["/authorize", "/model", "/mode ", "/review", "/new"] {
        assert!(
            text.iter().any(|line| line.contains(command)),
            "agent welcome should list {command}"
        );
    }

    assert!(
        !text.iter().any(|line| line.contains("/c            ")
            || line.contains("/s            ")
            || line.contains("/f            ")
            || line.contains("cheap model")
            || line.contains("smart model")
            || line.contains("fast model")),
        "agent welcome should not list dynamic one-letter model shortcuts"
    );
    assert!(
        !text.iter().any(|line| line.contains("/help")),
        "agent welcome should not spend main-screen space on /help"
    );
    assert!(
        !text.iter().any(|line| line.contains("/keys")),
        "agent welcome should not spend main-screen space on /keys"
    );
    assert!(
        !text.iter().any(|line| line.contains("Keys")),
        "agent welcome should remove the Keys section"
    );

    // Plan-authoring commands belong only to the Plan welcome.
    for plan_command in ["/start", "/save", "/export"] {
        assert!(
            !text.iter().any(|line| line.contains(plan_command)),
            "agent welcome must not list the plan command {plan_command}"
        );
    }

    // No raw keybinding noise leaks into the welcome.
    assert!(!text.iter().any(|line| line.contains("Alt+I")));
    assert!(!text.iter().any(|line| line.contains("F5")));
    assert!(!text.iter().any(|line| line.contains("Ctrl+O")));
}

#[test]
fn plan_welcome_shows_plan_commands_not_agent_quick_start() {
    use crate::tui::event::View;
    use unicode_width::UnicodeWidthStr;

    let text = welcome_text(View::Plan);

    assert!(
        text.iter().any(|line| line.contains("Plan mode")),
        "plan welcome heading should be present"
    );
    for command in ["/start", "/save", "/export"] {
        assert!(
            text.iter().any(|line| line.contains(command)),
            "plan welcome should list {command}"
        );
    }
    for label in ["build", "save", "export file", "coding agent"] {
        assert!(
            text.iter().any(|line| line.contains(label)),
            "plan welcome should use compact label `{label}`"
        );
    }
    assert!(
        text.iter().any(|line| line.contains("Shift+Tab")),
        "plan welcome should keep the agent-switch shortcut"
    );
    assert!(
        text.iter()
            .all(|line| UnicodeWidthStr::width(line.as_str()) <= 32),
        "plan welcome rows should fit the typical narrow split: {text:?}"
    );
    assert!(
        !text.iter().any(|line| line.contains("/keys")),
        "plan welcome should not spend space on /keys"
    );
    assert!(
        !text.iter().any(|line| line.contains("keyboard shortcuts")),
        "plan welcome should avoid long generic key help"
    );

    // Provider/session plumbing is the coding agent's concern, not the plan view's.
    for agent_command in ["/authorize", "/model", "/sessions"] {
        assert!(
            !text.iter().any(|line| line.contains(agent_command)),
            "plan welcome must not list {agent_command}"
        );
    }
}
