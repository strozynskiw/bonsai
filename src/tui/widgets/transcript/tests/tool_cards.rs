use super::*;
use crate::tui::widgets::transcript::tool_card::{
    execution_group_summary, tool_waits_for_permission,
};

#[test]
fn renders_headings_inline_code_and_lists() {
    let lines = render_markdown("# Title\n\n- item with `code`\n1. numbered\nplain text", 40);

    let rendered = rendered_text(lines);

    assert!(rendered.iter().any(|line| line.contains("Title")));
    assert!(rendered.iter().any(|line| line.contains("item with code")));
    assert!(rendered.iter().any(|line| line.contains("numbered")));
}

#[test]
fn queued_message_title_renders_cancel_label() {
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.transcript.push(TranscriptItem::QueuedUserMessage {
        id: 1,
        text: "queued text".to_string(),
        delivery: crate::tui::app::FollowUpDelivery::Queue,
    });

    let rendered = rendered_text(transcript_lines(&app, 80));

    assert!(
        rendered
            .iter()
            .any(|line| line.contains("queued") && line.trim_end().ends_with("Del")),
        "queued title should render a trailing cancel label: {rendered:?}"
    );
}

#[test]
fn diff_tool_activity_renders_comparison_view() {
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.transcript
        .push(TranscriptItem::ToolActivity(ToolActivity {
            id: "tool-1".to_string(),
            name: "write".to_string(),
            arguments: r#"{"file_path":"src/main.rs"}"#.to_string(),
            delegated_model: None,
            status: ToolStatus::Succeeded,
            result: Some("Wrote src/main.rs".to_string()),
            diff: Some(Box::new(FileDiff {
                path: "src/main.rs".to_string(),
                status: DiffStatus::Modified,
                hunks: vec![DiffHunk {
                    old_start: 1,
                    new_start: 1,
                    lines: vec![
                        DiffLine {
                            kind: DiffLineKind::Removed,
                            content: "old".to_string(),
                            old_line: Some(1),
                            new_line: None,
                        },
                        DiffLine {
                            kind: DiffLineKind::Added,
                            content: "new".to_string(),
                            old_line: None,
                            new_line: Some(1),
                        },
                    ],
                }],
                truncated: false,
                old_size: Some(3),
                new_size: 3,
                added_lines: 1,
                removed_lines: 1,
                additional_files: Box::default(),
            })),
            started_at: std::time::Instant::now(),
            finished_at: Some(std::time::Instant::now()),
        }));

    let rendered = rendered_text(transcript_lines(&app, 80));

    // Tool calls are one-liners: name, primary arg, +added -removed, and
    // duration on a single line; the diff body lives in the detail modal.
    assert!(
        rendered.iter().any(|line| line.contains("✓ write")
            && line.contains("src/main.rs")
            && line.contains("+1 -1")),
        "tool one-liner should carry name, path, and diff counts: {rendered:?}"
    );
    assert!(
        !rendered.iter().any(|line| line.contains("- old")),
        "diff rows must not render inline: {rendered:?}"
    );
    assert!(
        !rendered.iter().any(|line| line.contains("+ new")),
        "diff rows must not render inline: {rendered:?}"
    );
}

#[test]
fn wrapped_tool_card_paints_every_row_with_block_background() {
    let activity = ToolActivity {
        id: "tool-1".to_string(),
        name: "read".to_string(),
        arguments: r#"{"file_path":"/Users/wojtek/code/bonsai/src/tui/widgets/transcript.rs"}"#
            .to_string(),
        delegated_model: None,
        status: ToolStatus::Succeeded,
        result: Some("ok".to_string()),
        diff: None,
        started_at: std::time::Instant::now(),
        finished_at: Some(std::time::Instant::now()),
    };
    let block_bg = theme::palette().tool_block;

    let lines = transcript_lines_for_activity(&activity, 36);
    let card_rows: Vec<&Line<'static>> = lines
        .iter()
        .filter(|line| line.spans.iter().any(|s| s.style.bg == Some(block_bg)))
        .collect();
    assert!(
        card_rows.len() >= 2,
        "expected a wrapped card with >= 2 painted rows, got {}",
        card_rows.len()
    );
    for (index, row) in card_rows.iter().enumerate() {
        assert!(
            row.spans.iter().any(|s| s.style.bg == Some(block_bg)),
            "row {index}: missing block background, spans={:?}",
            row.spans
        );
    }
}

fn transcript_lines_for_activity(activity: &ToolActivity, width: usize) -> Vec<Line<'static>> {
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.transcript
        .push(TranscriptItem::ToolActivity(activity.clone()));
    transcript_lines(&app, width)
}

fn bash_result_with_summary(
    body: &str,
    exit_code: &str,
    timed_out: bool,
    duration: &str,
    stdout_bytes: usize,
    stderr_bytes: usize,
    saved_output: Option<&str>,
) -> String {
    bash_result_with_command_summary(
        "cargo test",
        body,
        exit_code,
        timed_out,
        duration,
        (stdout_bytes, stderr_bytes),
        saved_output,
    )
}

fn bash_result_with_command_summary(
    command: &str,
    body: &str,
    exit_code: &str,
    timed_out: bool,
    duration: &str,
    bytes: (usize, usize),
    saved_output: Option<&str>,
) -> String {
    let (stdout_bytes, stderr_bytes) = bytes;
    let mut result = String::new();
    if !body.is_empty() {
        result.push_str(body);
        result.push_str("\n\n");
    }
    result.push_str("[Command summary]\n");
    result.push_str(&format!("command: {command}\n"));
    result.push_str(&format!("exit_code: {exit_code}\n"));
    result.push_str(&format!("timed_out: {timed_out}\n"));
    if timed_out {
        result.push_str("timeout_seconds: 30\n");
    }
    result.push_str(&format!("duration: {duration}\n"));
    result.push_str(&format!("stdout_bytes: {stdout_bytes}\n"));
    result.push_str(&format!("stderr_bytes: {stderr_bytes}\n"));
    result.push_str(&format!(
        "combined_output_chars: {}\n",
        stdout_bytes + stderr_bytes
    ));
    if let Some(path) = saved_output {
        result.push_str(&format!("saved_output: {path}\n"));
    }
    result.push_str("last_output:\n");
    result.push_str(if body.is_empty() { "(no output)" } else { body });
    result
}

#[test]
fn lists_use_bullet_markers() {
    let lines = render_markdown("- one\n- two\n", 40);
    let rendered: Vec<String> = lines
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
        })
        .collect();

    assert!(
        rendered
            .iter()
            .any(|line| line.contains("•") && line.contains("one"))
    );
    assert!(
        rendered
            .iter()
            .any(|line| line.contains("•") && line.contains("two"))
    );
}

#[test]
fn blocks_are_separated_by_a_blank_line() {
    // Adjacent blocks (list items, paragraphs) get one blank separator line so
    // the transcript breathes instead of packing every line edge-to-edge.
    let rendered = rendered_text(render_markdown("- one\n- two\n\npara", 40));
    let one = rendered.iter().position(|l| l.contains("one")).unwrap();
    let two = rendered.iter().position(|l| l.contains("two")).unwrap();
    let para = rendered.iter().position(|l| l.contains("para")).unwrap();

    assert_eq!(two, one + 2, "a blank line should sit between list items");
    assert!(
        rendered[one + 1].trim().is_empty(),
        "separator between items should be blank, got {:?}",
        rendered[one + 1]
    );
    assert_eq!(
        para,
        two + 2,
        "a blank line should sit between the list and the paragraph"
    );
    // The document neither starts nor ends on a blank separator.
    assert!(!rendered.first().unwrap().trim().is_empty());
    assert!(!rendered.last().unwrap().trim().is_empty());
}

#[test]
fn execution_group_summary_reports_running_count_only() {
    let group = execution_group(
        1,
        std::time::Instant::now(),
        vec![
            ToolActivity {
                id: "call-1".to_string(),
                name: "read".to_string(),
                arguments: "{}".to_string(),
                delegated_model: None,
                status: ToolStatus::Running,
                result: None,
                diff: None,
                started_at: std::time::Instant::now(),
                finished_at: None,
            },
            ToolActivity {
                id: "call-2".to_string(),
                name: "bash".to_string(),
                arguments: "{}".to_string(),
                delegated_model: None,
                status: ToolStatus::Running,
                result: None,
                diff: None,
                started_at: std::time::Instant::now(),
                finished_at: None,
            },
        ],
        None,
    );
    let (summary, _, _) = execution_group_summary(&group);
    assert_eq!(summary, "Inspect · 2 tools · 2 running");
}

#[test]
fn execution_group_summary_uses_tool_card_style() {
    let group = execution_group(
        1,
        std::time::Instant::now(),
        vec![
            ToolActivity {
                id: "call-1".to_string(),
                name: "write".to_string(),
                arguments: "{}".to_string(),
                delegated_model: None,
                status: ToolStatus::Succeeded,
                result: Some("ok".to_string()),
                diff: None,
                started_at: std::time::Instant::now(),
                finished_at: Some(std::time::Instant::now()),
            },
            ToolActivity {
                id: "call-2".to_string(),
                name: "read".to_string(),
                arguments: "{}".to_string(),
                delegated_model: None,
                status: ToolStatus::Failed,
                result: Some("nope".to_string()),
                diff: None,
                started_at: std::time::Instant::now(),
                finished_at: Some(std::time::Instant::now()),
            },
        ],
        None,
    );
    let (summary, accent, background) = execution_group_summary(&group);
    assert!(summary.contains("Edit"), "{summary}");
    assert!(summary.contains("2 tools"), "{summary}");
    assert!(summary.contains("1 ok / 1 failed"), "{summary}");
    assert!(!summary.contains("nope"), "{summary}");
    assert_eq!(accent, theme::palette().tool);
    assert_eq!(background, theme::palette().tool_block);
}

#[test]
fn execution_group_summary_reports_summarize_phase() {
    let now = std::time::Instant::now();
    let group = execution_group(
        1,
        now,
        vec![ToolActivity {
            id: "call-1".to_string(),
            name: "todo_write".to_string(),
            arguments: r#"{"todos":[]}"#.to_string(),
            delegated_model: None,
            status: ToolStatus::Succeeded,
            result: Some("updated".to_string()),
            diff: None,
            started_at: now,
            finished_at: Some(now),
        }],
        Some(now),
    );

    let (summary, _, _) = execution_group_summary(&group);

    assert!(summary.contains("Summarize · 1 tool"), "{summary}");
}

#[test]
fn execution_group_summary_reports_duration_and_edit_summary() {
    let started_at = std::time::Instant::now();
    let mut group = execution_group(
        1,
        started_at,
        vec![
            ToolActivity {
                id: "call-1".to_string(),
                name: "bash".to_string(),
                arguments: "{}".to_string(),
                delegated_model: None,
                status: ToolStatus::Succeeded,
                result: Some("ok".to_string()),
                diff: Some(Box::new(FileDiff {
                    path: "src/main.rs".to_string(),
                    status: DiffStatus::Modified,
                    hunks: vec![DiffHunk {
                        old_start: 1,
                        new_start: 1,
                        lines: vec![
                            DiffLine {
                                kind: DiffLineKind::Added,
                                content: "added".to_string(),
                                old_line: None,
                                new_line: Some(1),
                            },
                            DiffLine {
                                kind: DiffLineKind::Removed,
                                content: "removed".to_string(),
                                old_line: Some(1),
                                new_line: None,
                            },
                        ],
                    }],
                    truncated: false,
                    old_size: Some(1),
                    new_size: 2,
                    added_lines: 1,
                    removed_lines: 1,
                    additional_files: Box::default(),
                })),
                started_at,
                finished_at: Some(started_at + std::time::Duration::from_millis(750)),
            },
            ToolActivity {
                id: "call-2".to_string(),
                name: "read".to_string(),
                arguments: "{}".to_string(),
                delegated_model: None,
                status: ToolStatus::Succeeded,
                result: Some("ok".to_string()),
                diff: None,
                started_at,
                finished_at: Some(started_at + std::time::Duration::from_millis(750)),
            },
        ],
        Some(started_at + std::time::Duration::from_millis(750)),
    );
    group.finished_at = Some(started_at + std::time::Duration::from_millis(750));
    let (summary, _, _) = execution_group_summary(&group);
    assert!(summary.contains("Edit"), "{summary}");
    assert!(summary.contains("edited src/main.rs"), "{summary}");
    assert!(summary.contains("+1 -1"), "{summary}");
    assert!(summary.contains("750ms"), "{summary}");
}

#[test]
fn execution_group_renders_summary_and_nested_tool_rows() {
    let now = std::time::Instant::now();
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.transcript
        .push(TranscriptItem::ExecutionGroup(execution_group(
            1,
            now,
            vec![
                ToolActivity {
                    id: "call-success".to_string(),
                    name: "read".to_string(),
                    arguments: r#"{"file_path":"src/main.rs"}"#.to_string(),
                    delegated_model: None,
                    status: ToolStatus::Succeeded,
                    result: Some("ok".to_string()),
                    diff: None,
                    started_at: now,
                    finished_at: Some(now),
                },
                ToolActivity {
                    id: "call-failed".to_string(),
                    name: "bash".to_string(),
                    arguments: r#"{"command":"cargo test"}"#.to_string(),
                    delegated_model: None,
                    status: ToolStatus::Failed,
                    result: Some("tests failed\nnext line".to_string()),
                    diff: None,
                    started_at: now,
                    finished_at: Some(now),
                },
            ],
            Some(now),
        )));

    let rendered = rendered_text(transcript_lines(&app, 120));
    let bash_row = rendered
        .iter()
        .position(|line| line.contains("✗ test") && line.contains("$ cargo test"))
        .expect("failed bash tool row should render with command");
    let read_row = rendered
        .iter()
        .position(|line| line.contains("✓ read") && line.contains("src/main.rs"))
        .expect("read tool row should render with path");

    assert!(
        rendered.iter().any(|line| line.contains("Verify")
            && line.contains("2 tools")
            && line.contains("read src/main.rs")
            && line.contains("cmds: test")
            && !line.contains("tests failed")),
        "group summary should render semantic details above nested rows: {rendered:?}"
    );
    assert!(
        read_row < bash_row,
        "nested rows should preserve start order: {rendered:?}"
    );
    assert!(
        !rendered
            .iter()
            .any(|line| line.contains("call-success") || line.contains("call-failed")),
        "nested rows should prefer useful args over raw call ids: {rendered:?}"
    );
}

#[test]
fn serenity_execution_group_keeps_authorization_out_of_the_transcript() {
    let now = std::time::Instant::now();
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.serenity_mode = true;
    app.transcript
        .push(TranscriptItem::ExecutionGroup(execution_group(
            1,
            now,
            vec![
                ToolActivity {
                    id: "call-read-1".to_string(),
                    name: "read".to_string(),
                    arguments: r#"{"file_path":"src/main.rs"}"#.to_string(),
            delegated_model: None,
                    status: ToolStatus::Succeeded,
                    result: Some("ok".to_string()),
                    diff: None,
                    started_at: now,
                    finished_at: Some(now),
                },
                ToolActivity {
                    id: "call-bash".to_string(),
                    name: "bash".to_string(),
                    arguments: r#"{"command":"bash check_task_ray.sh"}"#.to_string(),
            delegated_model: None,
                    status: ToolStatus::Failed,
                    result: Some(
                        "[authorization] allow · read-only · code-execution · fallback\n\nprocess exited with status 1"
                            .to_string(),
                    ),
                    diff: None,
                    started_at: now,
                    finished_at: Some(now),
                },
                ToolActivity {
                    id: "call-read-2".to_string(),
                    name: "read".to_string(),
                    arguments: r#"{"file_path":"Cargo.toml"}"#.to_string(),
            delegated_model: None,
                    status: ToolStatus::Succeeded,
                    result: Some("ok".to_string()),
                    diff: None,
                    started_at: now,
                    finished_at: Some(now),
                },
            ],
            Some(now),
        )));

    let rendered = rendered_text(transcript_lines(&app, 180));
    let summary_row = rendered
        .iter()
        .find(|line| line.contains("Recover · 3 tools"))
        .expect("summary row should render");

    assert!(!summary_row.contains("process exited"), "{rendered:?}");
    // Authorization decisions are audit context; they live in the tool detail
    // modal's authorization section, not in the transcript.
    assert!(
        !rendered.iter().any(|line| line.contains("authorization")),
        "authorization details must not render inline: {rendered:?}"
    );
}

#[test]
fn failed_bash_row_omits_warning_suffix() {
    let now = std::time::Instant::now();
    let activity = ToolActivity {
        id: "call-bash".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"cargo test"}"#.to_string(),
        delegated_model: None,
        status: ToolStatus::Failed,
        result: Some(bash_result_with_summary(
            "warning: unused import: Foo\ntest failed",
            "101",
            false,
            "8.4s",
            64,
            32,
            None,
        )),
        diff: None,
        started_at: now,
        finished_at: Some(now),
    };

    let rendered = rendered_text(transcript_lines_for_activity(&activity, 220)).join("\n");
    assert!(!rendered.contains("exit 101"), "{rendered}");
    assert!(!rendered.contains("out 64B err 32B"), "{rendered}");
    assert!(
        !rendered.contains("warning: unused import"),
        "a failed command's row must not stack a warning on the failure: {rendered}"
    );
}

#[test]
fn failed_tool_row_hides_reason_and_authorization() {
    let now = std::time::Instant::now();
    let long_reason = "Error: this failure explanation is deliberately far longer than the \
                       inline budget so the row must compact it";
    let activity = ToolActivity {
        id: "call-edit".to_string(),
        name: "edit".to_string(),
        arguments: r#"{"path":"src/lib.rs"}"#.to_string(),
        delegated_model: None,
        status: ToolStatus::Failed,
        result: Some(format!(
            "[authorization] allow · medium · workspace-write · fallback\n\n{long_reason}"
        )),
        diff: None,
        started_at: now,
        finished_at: Some(now),
    };

    let rendered = rendered_text(transcript_lines_for_activity(&activity, 220)).join("\n");
    assert!(
        !rendered.contains("[authorization]") && !rendered.contains("workspace-write"),
        "the inline reason must skip authorization lines: {rendered}"
    );
    assert!(!rendered.contains("failure explanation"), "{rendered}");
}

#[test]
fn large_successful_execution_group_shows_nested_rows() {
    let now = std::time::Instant::now();
    let tools = (0..8)
        .map(|index| ToolActivity {
            id: format!("call-{index}"),
            name: "read".to_string(),
            arguments: format!(r#"{{"file_path":"src/file_{index}.rs"}}"#),
            delegated_model: None,
            status: ToolStatus::Succeeded,
            result: Some("ok".to_string()),
            diff: None,
            started_at: now,
            finished_at: Some(now),
        })
        .collect::<Vec<_>>();
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.transcript
        .push(TranscriptItem::ExecutionGroup(execution_group(
            1,
            now,
            tools,
            Some(now),
        )));

    let rendered = rendered_text(transcript_lines(&app, 180)).join("\n");

    assert!(
        rendered.contains("Inspect · 8 tools") && rendered.contains("read 8 files"),
        "group summary should carry high-level semantics: {rendered}"
    );
    assert!(
        !rendered.contains("+8 more tools"),
        "group should not hide nested rows behind a more-tools footer: {rendered}"
    );
    assert!(
        rendered.matches("✓ read").count() >= 8,
        "successful nested rows should render by default: {rendered}"
    );
}

#[test]
fn serenity_execution_group_collapses_nested_rows_by_default() {
    let now = std::time::Instant::now();
    let tools = (0..3)
        .map(|index| ToolActivity {
            id: format!("call-{index}"),
            name: "read".to_string(),
            arguments: format!(r#"{{"file_path":"src/file_{index}.rs"}}"#),
            delegated_model: None,
            status: ToolStatus::Succeeded,
            result: Some("ok".to_string()),
            diff: None,
            started_at: now,
            finished_at: Some(now),
        })
        .collect::<Vec<_>>();
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.serenity_mode = true;
    app.transcript
        .push(TranscriptItem::ExecutionGroup(execution_group(
            1,
            now,
            tools,
            Some(now),
        )));

    let rendered = rendered_text(transcript_lines(&app, 180)).join("\n");

    assert!(rendered.contains("Inspect · 3 tools"), "{rendered}");
    assert!(rendered.contains("+3 more tools"), "{rendered}");
    assert!(
        !rendered.contains("✓ read"),
        "collapsed serenity group should hide nested rows: {rendered}"
    );
}

#[test]
fn serenity_expanded_execution_group_shows_nested_rows() {
    let now = std::time::Instant::now();
    let tools = (0..3)
        .map(|index| ToolActivity {
            id: format!("call-{index}"),
            name: "read".to_string(),
            arguments: format!(r#"{{"file_path":"src/file_{index}.rs"}}"#),
            delegated_model: None,
            status: ToolStatus::Succeeded,
            result: Some("ok".to_string()),
            diff: None,
            started_at: now,
            finished_at: Some(now),
        })
        .collect::<Vec<_>>();
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.serenity_mode = true;
    app.expanded_execution_groups.insert(1);
    app.transcript
        .push(TranscriptItem::ExecutionGroup(execution_group(
            1,
            now,
            tools,
            Some(now),
        )));

    let rendered = rendered_text(transcript_lines(&app, 180)).join("\n");

    assert!(
        !rendered.contains("+3 more tools"),
        "expanded serenity group should not report hidden rows: {rendered}"
    );
    assert!(
        rendered.matches("✓ read").count() >= 3,
        "expanded serenity group should render nested rows: {rendered}"
    );
}

#[test]
fn serenity_reasoning_summary_renders_thought_label_without_content() {
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.serenity_mode = true;
    app.transcript.push(TranscriptItem::ReasoningSummary {
        text: "private chain".to_string(),
    });
    app.transcript.push(TranscriptItem::AssistantMessage {
        text: "visible answer".to_string(),
    });

    let rendered = rendered_text(transcript_lines(&app, 80)).join("\n");

    assert!(!rendered.contains("private chain"), "{rendered}");
    assert!(rendered.contains("Quick thought"), "{rendered}");
    assert!(rendered.contains("visible answer"), "{rendered}");
}

#[test]
fn serenity_thought_label_buckets_by_reasoning_volume() {
    // Boundary pairs around the bucket thresholds (250 / 900 / 1800 / 3200 chars).
    for (chars, label) in [
        (249, "Quick thought"),
        (250, "Chewed on it"),
        (899, "Chewed on it"),
        (900, "Went deep"),
        (1799, "Went deep"),
        (1800, "Ran the numbers"),
        (3199, "Ran the numbers"),
        (3200, "Took the scenic route"),
    ] {
        let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
        app.serenity_mode = true;
        app.transcript.push(TranscriptItem::ReasoningSummary {
            text: "x".repeat(chars),
        });
        app.transcript.push(TranscriptItem::AssistantMessage {
            text: "answer".to_string(),
        });

        let rendered = rendered_text(transcript_lines(&app, 80)).join("\n");

        assert!(
            rendered.contains(label),
            "{chars} chars should render '{label}': {rendered}"
        );
    }
}

#[test]
fn serenity_only_trailing_reasoning_cell_animates_while_busy() {
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.serenity_mode = true;
    app.task_state = crate::tui::event::TaskState::Running;
    app.tick = 12;
    // Two calls in one turn: the earlier cell's thinking is finished even
    // though the task is still busy; only the trailing cell streams.
    app.transcript.push(TranscriptItem::ReasoningSummary {
        text: "earlier finished thought".to_string(),
    });
    app.transcript.push(TranscriptItem::ReasoningSummary {
        text: "still streaming".to_string(),
    });

    let rendered = rendered_text(transcript_lines(&app, 80)).join("\n");

    assert!(!rendered.contains("earlier finished thought"), "{rendered}");
    assert!(!rendered.contains("still streaming"), "{rendered}");
    assert!(rendered.contains("Quick thought"), "{rendered}");
    assert!(rendered.contains("Thinking"), "{rendered}");
}

#[test]
fn serenity_active_reasoning_summary_renders_thinking_animation() {
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.serenity_mode = true;
    app.task_state = crate::tui::event::TaskState::Running;
    app.tick = 12;
    app.transcript.push(TranscriptItem::ReasoningSummary {
        text: "private chain".to_string(),
    });

    let rendered = rendered_text(transcript_lines(&app, 80)).join("\n");

    assert!(!rendered.contains("private chain"), "{rendered}");
    assert!(rendered.contains("Thinking"), "{rendered}");
    assert!(!rendered.contains("Thinking done"), "{rendered}");
}

#[test]
fn active_thinking_indicator_is_separated_from_the_previous_card() {
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.serenity_mode = true;
    app.task_state = crate::tui::event::TaskState::Running;
    app.tick = 12;
    // A preceding card, then the streaming reasoning indicator.
    app.transcript.push(TranscriptItem::AssistantMessage {
        text: "done writing history".to_string(),
    });
    app.transcript.push(TranscriptItem::ReasoningSummary {
        text: "private chain".to_string(),
    });

    let lines = rendered_text(transcript_lines(&app, 80));
    let thinking = lines
        .iter()
        .position(|line| line.contains("Thinking"))
        .expect("thinking indicator should render");
    // The line directly above the indicator is a blank spacer, not the tail of
    // the previous card — the two items must not sit flush.
    assert!(thinking > 0, "indicator should not be the first line");
    assert!(
        lines[thinking - 1].trim().is_empty(),
        "expected a blank line before the thinking indicator, got {:?}",
        lines[thinking - 1]
    );
}

#[test]
fn selected_large_execution_group_expands_nested_rows() {
    let now = std::time::Instant::now();
    let tools = (0..8)
        .map(|index| ToolActivity {
            id: format!("call-{index}"),
            name: "read".to_string(),
            arguments: format!(r#"{{"file_path":"src/file_{index}.rs"}}"#),
            delegated_model: None,
            status: ToolStatus::Succeeded,
            result: Some("ok".to_string()),
            diff: None,
            started_at: now,
            finished_at: Some(now),
        })
        .collect::<Vec<_>>();
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.transcript
        .push(TranscriptItem::ExecutionGroup(execution_group(
            1,
            now,
            tools,
            Some(now),
        )));
    app.transcript_focus = Some(0);
    app.active_group_tool_selection = Some(InlineToolSelection {
        group_id: 1,
        selected_tool: 0,
    });

    let rendered = rendered_text(transcript_lines(&app, 180)).join("\n");

    assert!(
        !rendered.contains("+8 more tools"),
        "expanded group should not report hidden rows: {rendered}"
    );
    assert!(
        rendered.matches("✓ read").count() >= 8,
        "expanded group should render every nested row: {rendered}"
    );
}

#[test]
fn large_failed_execution_group_keeps_failed_row_visible() {
    let now = std::time::Instant::now();
    let tools = (0..8)
        .map(|index| ToolActivity {
            id: format!("call-{index}"),
            name: "read".to_string(),
            arguments: format!(r#"{{"file_path":"src/file_{index}.rs"}}"#),
            delegated_model: None,
            status: if index == 5 {
                ToolStatus::Failed
            } else {
                ToolStatus::Succeeded
            },
            result: Some(if index == 5 { "missing file" } else { "ok" }.to_string()),
            diff: None,
            started_at: now,
            finished_at: Some(now),
        })
        .collect::<Vec<_>>();
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.transcript
        .push(TranscriptItem::ExecutionGroup(execution_group(
            1,
            now,
            tools,
            Some(now),
        )));

    let rendered = rendered_text(transcript_lines(&app, 180)).join("\n");

    assert!(
        rendered.contains("Recover · 8 tools")
            && rendered.contains("7 ok / 1 failed")
            && !rendered.contains("missing file"),
        "failed group summary should keep details out of the transcript: {rendered}"
    );
    assert!(
        rendered.contains("✗ read") && rendered.contains("src/file_5.rs"),
        "failed nested row should stay visible: {rendered}"
    );
    assert!(
        rendered.matches("✓ read").count() >= 7,
        "successful nested rows should stay visible alongside the failure: {rendered}"
    );
}

#[test]
fn execution_group_running_tool_renders_after_existing_rows() {
    let now = std::time::Instant::now();
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.transcript
        .push(TranscriptItem::ExecutionGroup(execution_group(
            1,
            now,
            vec![
                ToolActivity {
                    id: "call-read".to_string(),
                    name: "read".to_string(),
                    arguments: r#"{"file_path":"src/main.rs"}"#.to_string(),
                    delegated_model: None,
                    status: ToolStatus::Succeeded,
                    result: Some("ok".to_string()),
                    diff: None,
                    started_at: now,
                    finished_at: Some(now),
                },
                ToolActivity {
                    id: "call-bash".to_string(),
                    name: "bash".to_string(),
                    arguments: r#"{"command":"sleep 5"}"#.to_string(),
                    delegated_model: None,
                    status: ToolStatus::Running,
                    result: None,
                    diff: None,
                    started_at: now + std::time::Duration::from_millis(1),
                    finished_at: None,
                },
            ],
            None,
        )));

    let rendered = rendered_text(transcript_lines(&app, 120));
    let read_row = rendered
        .iter()
        .position(|line| line.contains("✓ read") && line.contains("src/main.rs"))
        .expect("completed read row should render");
    let bash_row = rendered
        .iter()
        .position(|line| line.contains("bash") && line.contains("$ sleep 5"))
        .expect("running bash row should render");

    assert!(
        read_row < bash_row,
        "new running tool rows should append below existing rows: {rendered:?}"
    );
}

#[test]
fn running_bash_row_surfaces_live_output_preview() {
    let now = std::time::Instant::now();
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.transcript
        .push(TranscriptItem::ExecutionGroup(execution_group(
            1,
            now,
            vec![ToolActivity {
                id: "call-bash".to_string(),
                name: "bash".to_string(),
                arguments: r#"{"command":"gh auth login"}"#.to_string(),
                delegated_model: None,
                status: ToolStatus::Running,
                result: Some("warning: browser auth unavailable\ncopy this code".to_string()),
                diff: None,
                started_at: now,
                finished_at: None,
            }],
            None,
        )));

    let rendered = rendered_text(transcript_lines(&app, 120));
    assert!(
        rendered.iter().any(|line| line.contains("copy this code")),
        "running bash row should surface live output: {rendered:?}"
    );
}

#[test]
fn running_bash_row_skips_live_truncation_footer() {
    let now = std::time::Instant::now();
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.transcript
        .push(TranscriptItem::ExecutionGroup(execution_group(
            1,
            now,
            vec![ToolActivity {
                id: "call-bash".to_string(),
                name: "bash".to_string(),
                arguments: r#"{"command":"long-running"}"#.to_string(),
            delegated_model: None,
                status: ToolStatus::Running,
                result: Some(
                    "first line\n\n[Live output truncated: 12000 chars shown, 14000 chars total so far]"
                        .to_string(),
                ),
                diff: None,
                started_at: now,
                finished_at: None,
            }],
            None,
        )));

    let rendered = rendered_text(transcript_lines(&app, 120));
    assert!(
        rendered.iter().any(|line| line.contains("first line")),
        "running bash row should fall back to real output: {rendered:?}"
    );
    assert!(
        !rendered
            .iter()
            .any(|line| line.contains("Live output truncated")),
        "running bash row should not summarize the truncation footer: {rendered:?}"
    );
}

#[test]
fn finished_successful_bash_row_shows_command_summary() {
    let now = std::time::Instant::now();
    let activity = ToolActivity {
        id: "call-bash".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"cargo test"}"#.to_string(),
        delegated_model: None,
        status: ToolStatus::Succeeded,
        result: Some(bash_result_with_summary(
            "ok",
            "0",
            false,
            "1.2s",
            12 * 1024,
            0,
            None,
        )),
        diff: None,
        started_at: now,
        finished_at: Some(now),
    };

    let rendered = rendered_text(transcript_lines_for_activity(&activity, 180)).join("\n");
    assert!(
        rendered.contains("✓ test"),
        "finished bash row should use a typed workflow label: {rendered}"
    );
    assert!(
        rendered.contains("passed · 1.2s · out 12KB err 0B"),
        "finished bash row should use the footer summary: {rendered}"
    );
}

#[test]
fn cargo_fmt_bash_row_shows_format_workflow() {
    let now = std::time::Instant::now();
    let command = "cargo fmt --all -- --check";
    let activity = ToolActivity {
        id: "call-bash".to_string(),
        name: "bash".to_string(),
        arguments: format!(r#"{{"command":"{command}"}}"#),
        delegated_model: None,
        status: ToolStatus::Succeeded,
        result: Some(bash_result_with_command_summary(
            command,
            "",
            "0",
            false,
            "420ms",
            (0, 0),
            None,
        )),
        diff: None,
        started_at: now,
        finished_at: Some(now),
    };

    let rendered = rendered_text(transcript_lines_for_activity(&activity, 220)).join("\n");
    assert!(
        rendered.contains("✓ format") && rendered.contains("$ cargo fmt --all -- --check"),
        "format row should carry a typed workflow label and command: {rendered}"
    );
    assert!(
        rendered.contains("passed · 420ms · out 0B err 0B"),
        "format row should carry a compact status summary: {rendered}"
    );
}

#[test]
fn cargo_clippy_bash_row_shows_lint_failure() {
    let now = std::time::Instant::now();
    let command = "cargo clippy --all-targets --all-features -- -D warnings";
    let activity = ToolActivity {
        id: "call-bash".to_string(),
        name: "bash".to_string(),
        arguments: format!(r#"{{"command":"{command}"}}"#),
        delegated_model: None,
        status: ToolStatus::Failed,
        result: Some(bash_result_with_command_summary(
            command,
            "error: lint failed",
            "101",
            false,
            "8.4s",
            (4 * 1024, 2 * 1024),
            None,
        )),
        diff: None,
        started_at: now,
        finished_at: Some(now),
    };

    let rendered = rendered_text(transcript_lines_for_activity(&activity, 260)).join("\n");
    assert!(
        rendered.contains("✗ lint"),
        "lint row should use a typed workflow label: {rendered}"
    );
    assert!(!rendered.contains("exit 101"), "{rendered}");
    assert!(
        !rendered.contains("error: lint failed"),
        "typed command rows should keep raw output secondary: {rendered}"
    );
}

#[test]
fn failed_bash_row_shows_exit_summary_instead_of_first_output_line() {
    let now = std::time::Instant::now();
    let activity = ToolActivity {
        id: "call-bash".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"cargo test"}"#.to_string(),
        delegated_model: None,
        status: ToolStatus::Failed,
        result: Some(bash_result_with_summary(
            "compiler failed loudly",
            "42",
            false,
            "230ms",
            0,
            86,
            None,
        )),
        diff: None,
        started_at: now,
        finished_at: Some(now),
    };

    let rendered = rendered_text(transcript_lines_for_activity(&activity, 180)).join("\n");
    assert!(
        rendered.contains("✗ test"),
        "failed bash row should use a typed workflow label: {rendered}"
    );
    assert!(!rendered.contains("exit 42"), "{rendered}");
    assert!(
        !rendered.contains("compiler failed loudly"),
        "failed bash row should not fall back to the first output line: {rendered}"
    );
}

#[test]
fn running_cargo_test_row_keeps_live_output_with_typed_label() {
    let now = std::time::Instant::now();
    let activity = ToolActivity {
        id: "call-bash".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"cargo test --locked"}"#.to_string(),
        delegated_model: None,
        status: ToolStatus::Running,
        result: Some("running 14 tests\nlatest test still running".to_string()),
        diff: None,
        started_at: now,
        finished_at: None,
    };

    let rendered = rendered_text(transcript_lines_for_activity(&activity, 180)).join("\n");
    assert!(
        rendered.contains("$ cargo test --locked"),
        "running cargo test should keep the command: {rendered}"
    );
    // The row must use the typed 'test' workflow label, not the generic tool
    // name. Anchoring on "test" alone is vacuous (the echoed command contains
    // it); the discriminator is that a typed row never shows "bash".
    assert!(
        !rendered.contains("bash"),
        "running cargo test should use the typed label, not generic 'bash': {rendered}"
    );
    assert!(
        rendered.contains("latest test still running"),
        "running cargo test should still surface live output: {rendered}"
    );
}

#[test]
fn generic_bash_command_keeps_bash_label() {
    let now = std::time::Instant::now();
    let activity = ToolActivity {
        id: "call-bash".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"printf ok"}"#.to_string(),
        delegated_model: None,
        status: ToolStatus::Succeeded,
        result: Some(bash_result_with_command_summary(
            "printf ok",
            "ok",
            "0",
            false,
            "120ms",
            (2, 0),
            None,
        )),
        diff: None,
        started_at: now,
        finished_at: Some(now),
    };

    let rendered = rendered_text(transcript_lines_for_activity(&activity, 180)).join("\n");
    assert!(
        rendered.contains("✓ bash") && rendered.contains("$ printf ok"),
        "generic shell commands should keep the bash label: {rendered}"
    );
    assert!(
        rendered.contains("exit 0 · 120ms · out 2B err 0B"),
        "generic shell commands should keep the generic exit summary: {rendered}"
    );
}

#[test]
fn compound_raw_bash_command_overrides_compact_footer_workflow() {
    let now = std::time::Instant::now();
    let activity = ToolActivity {
        id: "call-bash".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"cargo test && cargo fmt"}"#.to_string(),
        delegated_model: None,
        status: ToolStatus::Succeeded,
        result: Some(bash_result_with_command_summary(
            "cargo test",
            "ok",
            "0",
            false,
            "1.2s",
            (12, 0),
            None,
        )),
        diff: None,
        started_at: now,
        finished_at: Some(now),
    };

    let rendered = rendered_text(transcript_lines_for_activity(&activity, 220)).join("\n");
    assert!(
        rendered.contains("✓ bash") && rendered.contains("$ cargo test && cargo fmt"),
        "compound raw bash commands should keep the generic bash label: {rendered}"
    );
    assert!(
        rendered.contains("exit 0 · 1.2s · out 12B err 0B"),
        "compound raw bash commands should keep the generic exit summary: {rendered}"
    );
    assert!(
        !rendered.contains("passed · 1.2s"),
        "compacted footer command must not force workflow pass/fail wording: {rendered}"
    );
}

#[test]
fn timed_out_saved_bash_row_shows_distinct_summary() {
    let now = std::time::Instant::now();
    let activity = ToolActivity {
        id: "call-bash".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"cargo test"}"#.to_string(),
        delegated_model: None,
        status: ToolStatus::Failed,
        result: Some(bash_result_with_summary(
            "",
            "none",
            true,
            "30.0s",
            4 * 1024,
            0,
            Some(".bonsai/tool-output/bash_1.txt"),
        )),
        diff: None,
        started_at: now,
        finished_at: Some(now),
    };

    let rendered = rendered_text(transcript_lines_for_activity(&activity, 180)).join("\n");
    assert!(!rendered.contains("timeout"), "{rendered}");
    assert!(!rendered.contains("saved"), "{rendered}");
}

#[test]
fn git_tool_row_promotes_operation_argument() {
    let now = std::time::Instant::now();
    let activity = ToolActivity {
        id: "call-git".to_string(),
        name: "git".to_string(),
        arguments: r#"{"op":"status","path":"src/main.rs"}"#.to_string(),
        delegated_model: None,
        status: ToolStatus::Succeeded,
        result: Some("## master".to_string()),
        diff: None,
        started_at: now,
        finished_at: Some(now),
    };

    let rendered = rendered_text(transcript_lines_for_activity(&activity, 180)).join("\n");
    assert!(
        rendered.contains("✓ git") && rendered.contains("status"),
        "git rows should promote the op argument: {rendered}"
    );
}

#[test]
fn signal_bash_row_shows_signal_summary() {
    let now = std::time::Instant::now();
    let mut result = bash_result_with_summary("", "none", false, "120ms", 0, 0, None);
    result = result.replacen("timed_out: false\n", "signal: 9\ntimed_out: false\n", 1);
    let activity = ToolActivity {
        id: "call-bash".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"cargo test"}"#.to_string(),
        delegated_model: None,
        status: ToolStatus::Failed,
        result: Some(result),
        diff: None,
        started_at: now,
        finished_at: Some(now),
    };

    let rendered = rendered_text(transcript_lines_for_activity(&activity, 180)).join("\n");
    assert!(!rendered.contains("signal 9"), "{rendered}");
}

#[test]
fn warning_bash_row_keeps_warning_suffix_with_command_summary() {
    let now = std::time::Instant::now();
    let activity = ToolActivity {
        id: "call-bash".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"cargo test"}"#.to_string(),
        delegated_model: None,
        status: ToolStatus::Succeeded,
        result: Some(bash_result_with_summary(
            "warning: unused import: Foo\nfinished",
            "0",
            false,
            "1.2s",
            64,
            0,
            None,
        )),
        diff: None,
        started_at: now,
        finished_at: Some(now),
    };

    let rendered = rendered_text(transcript_lines_for_activity(&activity, 220)).join("\n");
    assert!(
        rendered.contains("passed · 1.2s · out 64B err 0B · warning: unused import: Foo"),
        "warning should remain visible beside the footer summary: {rendered}"
    );
}

#[test]
fn bash_row_ignores_command_summary_marker_in_last_output() {
    let now = std::time::Instant::now();
    let activity = ToolActivity {
        id: "call-bash".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"printf marker"}"#.to_string(),
        delegated_model: None,
        status: ToolStatus::Succeeded,
        result: Some(bash_result_with_command_summary(
            "printf marker",
            "tail line\n[Command summary]",
            "0",
            false,
            "120ms",
            (128, 0),
            None,
        )),
        diff: None,
        started_at: now,
        finished_at: Some(now),
    };

    let rendered = rendered_text(transcript_lines_for_activity(&activity, 180)).join("\n");
    assert!(
        rendered.contains("exit 0 · 120ms · out 128B err 0B"),
        "literal footer markers in command output should not hide the real summary: {rendered}"
    );
}

#[test]
fn successful_bash_row_surfaces_warning_output() {
    let now = std::time::Instant::now();
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.transcript
        .push(TranscriptItem::ExecutionGroup(execution_group(
            1,
            now,
            vec![ToolActivity {
                id: "call-bash".to_string(),
                name: "bash".to_string(),
                arguments: r#"{"command":"cargo check"}"#.to_string(),
                delegated_model: None,
                status: ToolStatus::Succeeded,
                result: Some("warning: unused import: Foo\nfinished".to_string()),
                diff: None,
                started_at: now,
                finished_at: Some(now),
            }],
            Some(now),
        )));

    let rendered = rendered_text(transcript_lines(&app, 120));
    assert!(
        rendered
            .iter()
            .any(|line| line.contains("warning: unused import: Foo")),
        "successful bash row should surface warning output: {rendered:?}"
    );
}

#[test]
fn successful_bash_row_ignores_warning_substrings() {
    let now = std::time::Instant::now();
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.transcript
        .push(TranscriptItem::ExecutionGroup(execution_group(
            1,
            now,
            vec![ToolActivity {
                id: "call-bash".to_string(),
                name: "bash".to_string(),
                arguments: r#"{"command":"echo"}"#.to_string(),
                delegated_model: None,
                status: ToolStatus::Succeeded,
                result: Some("completed without warning: all good\nfinished".to_string()),
                diff: None,
                started_at: now,
                finished_at: Some(now),
            }],
            Some(now),
        )));

    let rendered = rendered_text(transcript_lines(&app, 120));
    assert!(
        !rendered
            .iter()
            .any(|line| line.contains("completed without warning")),
        "successful bash row should not surface warning substrings: {rendered:?}"
    );
}

#[test]
fn inline_group_selection_highlights_only_selected_nested_tool_row() {
    let now = std::time::Instant::now();
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.transcript
        .push(TranscriptItem::ExecutionGroup(execution_group(
            1,
            now,
            vec![
                ToolActivity {
                    id: "call-1".to_string(),
                    name: "read".to_string(),
                    arguments: r#"{"file_path":"src/main.rs"}"#.to_string(),
                    delegated_model: None,
                    status: ToolStatus::Succeeded,
                    result: Some("ok".to_string()),
                    diff: None,
                    started_at: now,
                    finished_at: Some(now),
                },
                ToolActivity {
                    id: "call-2".to_string(),
                    name: "bash".to_string(),
                    arguments: r#"{"command":"cargo test"}"#.to_string(),
                    delegated_model: None,
                    status: ToolStatus::Succeeded,
                    result: Some("ok".to_string()),
                    diff: None,
                    started_at: now,
                    finished_at: Some(now),
                },
            ],
            Some(now),
        )));
    app.transcript_focus = Some(0);
    app.active_group_tool_selection = Some(InlineToolSelection {
        group_id: 1,
        selected_tool: 0,
    });

    let lines = transcript_lines(&app, 120);
    let line_text = |line: &Line<'static>| -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    };
    let summary = lines
        .iter()
        .find(|line| line_text(line).contains("Verify · 2 tools"))
        .expect("summary row should render");
    let selected = lines
        .iter()
        .find(|line| line_text(line).contains("✓ read"))
        .expect("selected nested row should render");
    let unselected = lines
        .iter()
        .find(|line| line_text(line).contains("✓ test"))
        .expect("unselected nested row should render");

    assert!(
        !summary
            .spans
            .iter()
            .any(|span| span.style.bg == Some(theme::palette().selection_bg)),
        "summary should not take the inline tool selection background"
    );
    assert!(
        selected
            .spans
            .iter()
            .any(|span| span.style.bg == Some(theme::palette().selection_bg)),
        "selected nested row should use selection background"
    );
    assert!(
        !unselected
            .spans
            .iter()
            .any(|span| span.style.bg == Some(theme::palette().selection_bg)),
        "unselected nested rows should not use selection background"
    );
}

#[test]
fn running_bash_waiting_for_permission_uses_stable_pending_text() {
    let now = std::time::Instant::now();
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.modal = Some(crate::tui::event::ModalKind::Detail(
        crate::tui::event::DetailModal::PermissionPrompt {
            request_id: 1,
            command: "sleep 5".to_string(),
            origin: None,
        },
    ));
    app.transcript
        .push(TranscriptItem::ExecutionGroup(execution_group(
            1,
            now,
            vec![ToolActivity {
                id: "call-1".to_string(),
                name: "bash".to_string(),
                arguments: r#"{"command":"sleep 5"}"#.to_string(),
                delegated_model: None,
                status: ToolStatus::Running,
                result: None,
                diff: None,
                started_at: now,
                finished_at: None,
            }],
            None,
        )));

    let rendered = rendered_text(transcript_lines(&app, 120)).join("\n");
    assert!(rendered.contains("? bash"), "got: {rendered}");
    assert!(rendered.contains("awaiting permission"), "got: {rendered}");
    assert!(
        !rendered.contains("⠋ bash"),
        "permission wait should not use animated running spinner: {rendered}"
    );
}

#[test]
fn running_websearch_marks_only_an_active_domain_prompt_as_pending() {
    let now = std::time::Instant::now();
    let activity = ToolActivity {
        id: "call-search".to_string(),
        name: "websearch".to_string(),
        arguments: r#"{"query":"official Rust docs"}"#.to_string(),
        delegated_model: None,
        status: ToolStatus::Running,
        result: None,
        diff: None,
        started_at: now,
        finished_at: None,
    };
    assert!(!tool_waits_for_permission(&activity, None));
    assert!(tool_waits_for_permission(
        &activity,
        Some("https://api.search.brave.com/res/v1/web/search")
    ));

    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.modal = Some(crate::tui::event::ModalKind::Detail(
        crate::tui::event::DetailModal::WebDomainPrompt {
            request_id: 2,
            url: "https://api.search.brave.com/res/v1/web/search".to_string(),
            host: "api.search.brave.com".to_string(),
            redirected_from: None,
            origin: None,
        },
    ));
    app.transcript
        .push(TranscriptItem::ExecutionGroup(execution_group(
            1,
            now,
            vec![activity],
            None,
        )));

    let rendered = rendered_text(transcript_lines(&app, 120)).join("\n");
    assert!(rendered.contains("? websearch"), "got: {rendered}");
    assert!(rendered.contains("awaiting permission"), "got: {rendered}");
}

#[test]
fn denied_permission_renders_as_failed_tool_row() {
    let now = std::time::Instant::now();
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.transcript
        .push(TranscriptItem::ExecutionGroup(execution_group(
            1,
            now,
            vec![ToolActivity {
                id: "call-1".to_string(),
                name: "bash".to_string(),
                arguments: r#"{"command":"sleep 5"}"#.to_string(),
                delegated_model: None,
                status: ToolStatus::Failed,
                result: Some("Error: Permission denied by user for command: sleep 5".to_string()),
                diff: None,
                started_at: now,
                finished_at: Some(now),
            }],
            Some(now),
        )));

    let rendered = rendered_text(transcript_lines(&app, 120)).join("\n");
    assert!(rendered.contains("✗ bash"), "got: {rendered}");
    assert!(!rendered.contains("Permission denied"), "got: {rendered}");
    assert!(
        !rendered.contains("[permission]"),
        "permission decision should not render as a separate status line: {rendered}"
    );
}

#[test]
fn agent_tool_activity_shows_subagent_type() {
    let now = std::time::Instant::now();
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.transcript
        .push(TranscriptItem::ToolActivity(ToolActivity {
            id: "tool-1".to_string(),
            name: "agent".to_string(),
            arguments: r#"{"agent":"explore","prompt":"Explore the agent module"}"#.to_string(),
            delegated_model: Some("openrouter:z-ai/glm-4.7".to_string()),
            status: ToolStatus::Running,
            result: None,
            diff: None,
            started_at: now,
            finished_at: None,
        }));

    let rendered = rendered_text(transcript_lines(&app, 80)).join("\n");
    assert!(
        rendered.contains("agent:explore"),
        "agent row should show the subagent type: {rendered}"
    );
    assert!(rendered.contains("glm-4.7"), "got: {rendered}");
    assert!(!rendered.contains("openrouter"), "got: {rendered}");
    assert!(!rendered.contains("z-ai/"), "got: {rendered}");
    assert!(
        rendered.contains("Explore the agent module"),
        "agent row should still show the prompt summary: {rendered}"
    );
}

#[test]
fn delegated_agent_model_renders_in_nested_collapsed_and_linear_rows() {
    let now = std::time::Instant::now();
    let activity = ToolActivity {
        id: "agent-1".to_string(),
        name: "agent".to_string(),
        arguments: r#"{"agent":"self-review"}"#.to_string(),
        delegated_model: Some("codex:openai/gpt-5.6".to_string()),
        status: ToolStatus::Succeeded,
        result: Some("No findings".to_string()),
        diff: None,
        started_at: now,
        finished_at: Some(now),
    };
    let item = TranscriptItem::ExecutionGroup(execution_group(1, now, vec![activity], Some(now)));
    let options = |serenity_mode, linear_output| TranscriptRenderOptions {
        selected: ItemSelection::None,
        focused: false,
        active_group_tool_selection: None,
        serenity_mode,
        linear_output,
        execution_group_expanded: false,
        reasoning_active: false,
        permission_command: None,
        tick: 0,
    };

    for (serenity_mode, linear_output, label) in [
        (false, false, "nested"),
        (true, false, "collapsed"),
        (false, true, "linear"),
    ] {
        let text = rendered_text(render_transcript_item(
            &item,
            30,
            options(serenity_mode, linear_output),
        ))
        .join("\n");
        assert!(text.contains("agent:self-review"), "{label}: {text}");
        assert!(text.contains("gpt-5.6"), "{label}: {text}");
        assert!(!text.contains("codex"), "{label}: {text}");
        assert!(!text.contains("openai/"), "{label}: {text}");
    }
}
