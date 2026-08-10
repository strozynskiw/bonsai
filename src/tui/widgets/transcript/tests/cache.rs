//! Invalidation matrix and windowing parity for the transcript layout cache.
//! The golden rendering tests already exercise cached *content* (they call
//! `transcript_lines`, which reads the cache); these tests pin *when* the
//! cache re-renders — the whole point of the buffer is that unchanged frames
//! do zero layout work.

use super::*;

/// Render counts are only deterministic while no parallel test bumps the
/// process-global theme generation (every bump drops the cache). All theme
/// mutations in tests happen under [`theme::TEST_LOCK`], so holding it here
/// serializes these tests against them.
fn theme_guard() -> tokio::sync::MutexGuard<'static, ()> {
    theme::TEST_LOCK.blocking_lock()
}

fn test_app() -> AppState {
    AppState::new("opencode", "test-model".to_string(), ".".to_string(), None)
}

fn assistant(text: &str) -> TranscriptItem {
    TranscriptItem::AssistantMessage {
        text: text.to_string(),
    }
}

fn running_tool(id: &str) -> ToolActivity {
    ToolActivity {
        id: id.to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"sleep 1"}"#.to_string(),
        delegated_model: None,
        status: ToolStatus::Running,
        result: None,
        diff: None,
        started_at: std::time::Instant::now(),
        finished_at: None,
    }
}

fn finished_tool(id: &str) -> ToolActivity {
    ToolActivity {
        id: id.to_string(),
        name: "read".to_string(),
        arguments: r#"{"file_path":"src/main.rs"}"#.to_string(),
        delegated_model: None,
        status: ToolStatus::Succeeded,
        result: Some("ok".to_string()),
        diff: None,
        started_at: std::time::Instant::now(),
        finished_at: Some(std::time::Instant::now()),
    }
}

/// Fill the cache and return how many item renders the fill performed.
fn fill(app: &AppState, width: usize) -> usize {
    let before = app.transcript_layout.render_count();
    let _ = app.transcript_layout.total_rows(app, width);
    app.transcript_layout.render_count() - before
}

#[test]
fn unchanged_state_renders_nothing() {
    let _guard = theme_guard();
    let mut app = test_app();
    app.transcript.push(assistant("first message"));
    app.transcript.push(assistant("second message"));

    assert_eq!(fill(&app, 80), 2, "first pass renders every item");
    assert_eq!(fill(&app, 80), 0, "second pass is a pure cache hit");

    // A tick alone must not disturb finished content.
    app.tick += 1;
    assert_eq!(fill(&app, 80), 0, "tick with nothing animated is free");
}

#[test]
fn reduced_motion_keeps_running_items_stable_across_ticks() {
    let _guard = theme_guard();
    let mut app = test_app();
    app.reduced_motion = true;
    app.transcript
        .push(TranscriptItem::ToolActivity(running_tool("running")));

    assert_eq!(fill(&app, 80), 1);
    app.tick += 1;
    assert_eq!(
        fill(&app, 80),
        0,
        "decorative ticks must not invalidate reduced-motion output"
    );
}

#[test]
fn streaming_append_rerenders_only_the_tail() {
    let _guard = theme_guard();
    let mut app = test_app();
    app.transcript.push(assistant("finished reply"));
    app.transcript.push(assistant("streaming reply"));
    fill(&app, 80);

    if let Some(TranscriptItem::AssistantMessage { text }) = app.transcript.get_mut(1) {
        text.push_str(" plus a delta");
    }
    assert_eq!(fill(&app, 80), 1, "only the appended-to item re-renders");
}

#[test]
fn global_fingerprint_changes_rebuild_everything() {
    let _guard = theme_guard();
    let mut app = test_app();
    app.transcript.push(assistant("a"));
    app.transcript.push(assistant("b"));
    fill(&app, 80);

    assert_eq!(fill(&app, 60), 2, "width change drops the cache");
    assert_eq!(fill(&app, 60), 0);

    // Re-selecting the current theme still bumps the generation.
    assert!(crate::tui::theme::set_theme(
        crate::tui::theme::current_theme_name()
    ));
    assert_eq!(fill(&app, 60), 2, "theme switch drops the cache");

    app.serenity_mode = true;
    assert_eq!(fill(&app, 60), 2, "serenity flip drops the cache");

    app.screen_reader_mode = true;
    app.reduced_motion = true;
    assert_eq!(
        fill(&app, 60),
        2,
        "linear accessibility mode drops the cache"
    );
}

#[test]
fn focus_move_rerenders_exactly_both_items() {
    let _guard = theme_guard();
    let mut app = test_app();
    app.transcript.push(assistant("a"));
    app.transcript.push(assistant("b"));
    app.transcript.push(assistant("c"));
    app.transcript_focus = Some(0);
    fill(&app, 80);

    app.transcript_focus = Some(2);
    assert_eq!(fill(&app, 80), 2, "old and new focused items re-render");
}

#[test]
fn group_expansion_and_inline_selection_are_item_precise() {
    let _guard = theme_guard();
    let mut app = test_app();
    // Expansion only gates rendering in serenity mode — outside it every
    // group reports expanded (`execution_group_is_expanded`), so the toggle
    // below would be a visual no-op and, correctly, also a cache no-op.
    app.serenity_mode = true;
    app.transcript
        .push(TranscriptItem::ExecutionGroup(execution_group(
            1,
            std::time::Instant::now(),
            vec![finished_tool("t1")],
            Some(std::time::Instant::now()),
        )));
    app.transcript
        .push(TranscriptItem::ExecutionGroup(execution_group(
            2,
            std::time::Instant::now(),
            vec![finished_tool("t2")],
            Some(std::time::Instant::now()),
        )));
    app.transcript.push(assistant("done"));
    fill(&app, 80);

    app.expanded_execution_groups.insert(1);
    assert_eq!(fill(&app, 80), 1, "expansion re-renders only that group");

    app.active_group_tool_selection = Some(InlineToolSelection {
        group_id: 1,
        selected_tool: 0,
    });
    assert_eq!(fill(&app, 80), 1, "selecting into group 1 re-renders it");

    app.active_group_tool_selection = Some(InlineToolSelection {
        group_id: 2,
        selected_tool: 0,
    });
    assert_eq!(
        fill(&app, 80),
        2,
        "moving the selection re-renders exactly the two groups involved"
    );
}

#[test]
fn middle_insert_salvages_unchanged_items() {
    let _guard = theme_guard();
    let mut app = test_app();
    app.transcript.push(assistant("history"));
    app.transcript.push(TranscriptItem::QueuedUserMessage {
        id: 7,
        text: "queued".to_string(),
        delivery: crate::tui::app::FollowUpDelivery::Queue,
    });
    fill(&app, 80);

    // `push_item` inserts before the trailing queued message — the structural
    // path a busy session exercises on every new item.
    let mut focus = None;
    let mut selection = None;
    app.transcript
        .push_item(assistant("inserted"), &mut focus, &mut selection);

    assert_eq!(
        fill(&app, 80),
        1,
        "index shift alone must not re-render salvaged items"
    );
}

#[test]
fn removal_renders_nothing_and_shifts_spans() {
    let _guard = theme_guard();
    let mut app = test_app();
    app.transcript.push(assistant("a"));
    app.transcript.push(assistant("b"));
    app.transcript.push(assistant("c"));
    fill(&app, 80);
    let last_span = app
        .transcript_layout
        .item_row_span(&app, 80, 2)
        .expect("span for item 2");
    let total_before = app.transcript_layout.total_rows(&app, 80);

    app.transcript.remove(1);

    assert_eq!(fill(&app, 80), 0, "removal re-renders nothing");
    let shifted = app
        .transcript_layout
        .item_row_span(&app, 80, 1)
        .expect("span for shifted item");
    let removed_rows = last_span.0
        - app
            .transcript_layout
            .item_row_span(&app, 80, 0)
            .expect("span for item 0")
            .1;
    assert_eq!(
        shifted,
        (last_span.0 - removed_rows, last_span.1 - removed_rows),
        "the surviving item's span shifts up by the removed item's rows"
    );
    assert_eq!(
        app.transcript_layout.total_rows(&app, 80),
        total_before - removed_rows
    );
}

#[test]
fn running_tool_re_renders_every_tick_finished_group_does_not() {
    let _guard = theme_guard();
    let mut app = test_app();
    app.transcript
        .push(TranscriptItem::ToolActivity(running_tool("live")));
    // All tools finished but the group not yet closed (`finished_at: None`):
    // visually stable, so it must NOT count as animated — the group's own
    // `finished_at` is not a render input, only its tools' timestamps are.
    app.transcript
        .push(TranscriptItem::ExecutionGroup(execution_group(
            9,
            std::time::Instant::now(),
            vec![finished_tool("done")],
            None,
        )));
    fill(&app, 80);

    app.tick += 1;
    assert_eq!(
        fill(&app, 80),
        1,
        "only the running tool follows the tick; the unclosed-but-finished group stays cached"
    );
}

/// Pins the invariant that lets the cache omit `permission_command` from its
/// keys: a pending permission can only alter items hosting a `Running` tool
/// (which are tick-keyed and re-render every frame anyway). If a finished
/// tool ever starts rendering differently under a pending permission, this
/// fails and the key must learn about permissions.
#[test]
fn finished_tool_ignores_pending_permission() {
    let _guard = theme_guard();
    let item = TranscriptItem::ToolActivity(finished_tool("t"));
    let options = |permission_command| TranscriptRenderOptions {
        selected: ItemSelection::None,
        focused: false,
        active_group_tool_selection: None,
        serenity_mode: false,
        linear_output: false,
        execution_group_expanded: false,
        reasoning_active: false,
        permission_command,
        tick: 0,
    };
    assert_eq!(
        render_transcript_item(&item, 60, options(None)),
        render_transcript_item(&item, 60, options(Some("cargo test"))),
        "permission prompts must not affect finished tools"
    );
}

#[test]
fn window_slices_match_the_full_transcript() {
    let _guard = theme_guard();
    let mut app = test_app();
    app.transcript.push(TranscriptItem::UserMessage {
        text: "please refactor\n\n```rust\nfn main() {}\n```".to_string(),
    });
    app.transcript.push(assistant(
        "A longer reply that wraps across several rows at width sixty to make \
         the window math earn its keep. **Bold**, `code`, and a list:\n- one\n- two",
    ));
    app.transcript
        .push(TranscriptItem::ToolActivity(finished_tool("t")));
    app.transcript.push(assistant("tail"));

    let width = 60;
    let viewport = 5;
    let full = transcript_lines(&app, width);
    let total = app.transcript_layout.total_rows(&app, width);
    assert_eq!(full.len(), total, "cached rows == cached lines");

    app.transcript_autoscroll = false;
    for scroll in 0..=total.saturating_sub(viewport) {
        app.transcript_scroll = scroll as u16;
        let window = app.transcript_layout.view_window(&app, width, viewport);
        assert_eq!(window.scroll as usize, scroll);
        assert_eq!(
            window.lines,
            full[scroll..(scroll + viewport).min(total)],
            "window at scroll {scroll} must equal the full-layout slice"
        );
    }
}

#[test]
fn spans_partition_the_rows_and_locate_agrees() {
    let _guard = theme_guard();
    let mut app = test_app();
    app.transcript.push(assistant("first"));
    app.transcript.push(assistant("second, a bit longer"));
    app.transcript
        .push(TranscriptItem::ToolActivity(finished_tool("t")));

    let width = 60;
    let total = app.transcript_layout.total_rows(&app, width);
    let mut expected_start = 0;
    for index in 0..app.transcript.len() {
        let (start, end) = app
            .transcript_layout
            .item_row_span(&app, width, index)
            .expect("span");
        assert_eq!(start, expected_start, "spans must be contiguous");
        for row in start..end {
            assert_eq!(
                app.transcript_layout.locate_row(&app, width, row),
                Some((index, row - start))
            );
        }
        expected_start = end;
    }
    assert_eq!(expected_start, total, "spans must cover every row");
    assert_eq!(
        app.transcript_layout.locate_row(&app, width, total),
        None,
        "past-the-end rows resolve to nothing"
    );
}

#[test]
#[ignore = "manual perf probe"]
fn perf_probe_cold_vs_warm() {
    let _guard = theme_guard();
    let mut app = test_app();
    for i in 0..400 {
        app.transcript.push(assistant(&format!(
            "Reply {i} with **markdown**, `inline code`, a list:\n- alpha\n- beta\n\n\
             ```rust\nfn demo_{i}() -> usize {{ {i} }}\n```\nand a wrapping paragraph that \
             goes on long enough to span multiple rows at typical widths."
        )));
        app.transcript
            .push(TranscriptItem::ToolActivity(finished_tool(&format!(
                "t{i}"
            ))));
    }

    let width = 100;
    let cold_start = std::time::Instant::now();
    let _ = app.transcript_layout.total_rows(&app, width);
    let cold = cold_start.elapsed();

    let warm_start = std::time::Instant::now();
    let frames = 100;
    for _ in 0..frames {
        app.tick += 1;
        let _ = app
            .transcript_layout
            .view_window(&app, width, 40)
            .lines
            .len();
    }
    let warm = warm_start.elapsed() / frames;

    // Streaming: append to the tail each frame.
    let tail = app.transcript.len() - 2;
    let stream_start = std::time::Instant::now();
    for _ in 0..frames {
        app.tick += 1;
        if let Some(TranscriptItem::AssistantMessage { text }) = app.transcript.get_mut(tail) {
            text.push_str(" delta");
        }
        let _ = app
            .transcript_layout
            .view_window(&app, width, 40)
            .lines
            .len();
    }
    let stream = stream_start.elapsed() / frames;

    println!(
        "800 items · cold fill {cold:?} · warm frame {warm:?} · streaming frame {stream:?} \
         (old cost ≈ 3 × cold per frame)"
    );
}
