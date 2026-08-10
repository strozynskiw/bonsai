// The production paths read rendered rows from the layout cache; only the
// row-drift regression tests still render items directly.
#[cfg(test)]
use super::message::*;
use super::selection::resolve_position_in_item;
use super::tool_card::*;
use super::*;

/// Resolve a screen `row` to the transcript item painted there: `(item index,
/// item, row offset within the item)`. Reads the shared
/// [`TranscriptLayoutCache`](super::TranscriptLayoutCache) — the same rows the
/// renderer painted — so one mouse event costs an integer walk, not a
/// transcript layout, and hit-testing can never disagree with the paint. The
/// scroll is resolved through the same [`resolved_scroll`] rule as
/// [`render`](super::render).
fn locate(app: &AppState, area: Rect, row: u16) -> Option<(usize, &TranscriptItem, usize)> {
    if row < area.y || row >= area.bottom() {
        return None;
    }
    let content_width = area.width.saturating_sub(4).max(20) as usize;
    let viewport_height = transcript_viewport_height(area);
    let total_rows = app.transcript_layout.total_rows(app, content_width);
    let (scroll, _) = resolved_scroll(app, total_rows, viewport_height);
    let target = scroll as usize + row.saturating_sub(area.y + 1) as usize;
    let (index, row_in_item) = app
        .transcript_layout
        .locate_row(app, content_width, target)?;
    Some((index, &app.transcript[index], row_in_item))
}

/// What a left click on the transcript body lands on. One resolved hit answers
/// every question the mouse handler used to ask through separate per-question
/// walks (queued-cancel badge, group row, tool card, text position).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TranscriptHit {
    /// The `Del` badge on a queued user message.
    QueuedCancel(u64),
    /// An execution group's summary or nested tool row.
    Group(ExecutionGroupRowHit),
    /// A standalone tool activity card.
    Tool(String),
    /// Selectable text (or the whole-item fallback) anywhere else.
    Position(TranscriptPosition),
}

pub(crate) fn hit_at(app: &AppState, area: Rect, column: u16, row: u16) -> Option<TranscriptHit> {
    let (index, item, row_in_item) = locate(app, area, row)?;
    let content_width = area.width.saturating_sub(4).max(20) as usize;
    match item {
        TranscriptItem::QueuedUserMessage { id, .. }
            if row_in_item == 1 && queued_cancel_band(area, content_width).contains(&column) =>
        {
            Some(TranscriptHit::QueuedCancel(*id))
        }
        TranscriptItem::ToolActivity(activity) => Some(TranscriptHit::Tool(activity.id.clone())),
        TranscriptItem::ExecutionGroup(group) => {
            let hit = execution_group_row_hit(
                group,
                content_width,
                row_in_item,
                app.serenity_mode,
                app.screen_reader_mode || app.execution_group_is_expanded(group.id),
                pending_permission_command(app),
                app.animation_tick(),
            )
            .unwrap_or(ExecutionGroupRowHit::Summary { group_id: group.id });
            Some(TranscriptHit::Group(hit))
        }
        _ => Some(TranscriptHit::Position(resolve_position_in_item(
            item,
            index,
            row_in_item,
            column,
            area,
        ))),
    }
}

/// Screen columns occupied by the `Del` badge on a queued message's title row.
fn queued_cancel_band(area: Rect, content_width: usize) -> std::ops::Range<u16> {
    let start = area
        .x
        .saturating_add(2)
        .saturating_add(content_width.saturating_sub(QUEUED_CANCEL_LABEL.width() + 4) as u16);
    start..start.saturating_add(QUEUED_CANCEL_LABEL.width() as u16)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExecutionGroupRowHit {
    Summary {
        group_id: u64,
    },
    Tool {
        group_id: u64,
        selected_tool: usize,
        tool_id: String,
    },
}

pub(super) fn execution_group_row_hit(
    group: &ExecutionGroup,
    width: usize,
    row_in_item: usize,
    serenity_mode: bool,
    execution_group_expanded: bool,
    permission_command: Option<&str>,
    tick: u64,
) -> Option<ExecutionGroupRowHit> {
    let mut cursor = 1usize;
    if row_in_item == 0 {
        return Some(ExecutionGroupRowHit::Summary { group_id: group.id });
    }
    let summary_rows = wrapped_rows_for_lines(
        &render_execution_group_summary_row(
            group,
            width,
            ExecutionGroupSummaryRenderOptions {
                selected: ItemSelection::None,
                focused: false,
                serenity_mode,
                execution_group_expanded,
                permission_command,
                tick,
            },
        ),
        width,
    );
    if row_in_item < cursor + summary_rows {
        return Some(ExecutionGroupRowHit::Summary { group_id: group.id });
    }
    cursor += summary_rows;

    for index in visible_tool_indices(group, serenity_mode, execution_group_expanded) {
        let Some(activity) = group.tools.get(index) else {
            continue;
        };
        let tool_rows = wrapped_rows_for_lines(
            &render_nested_tool_activity(activity, width, ItemSelection::None, false, None, tick),
            width,
        );
        if row_in_item < cursor + tool_rows {
            return Some(ExecutionGroupRowHit::Tool {
                group_id: group.id,
                selected_tool: index,
                tool_id: activity.id.clone(),
            });
        }
        cursor += tool_rows;
    }

    Some(ExecutionGroupRowHit::Summary { group_id: group.id })
}

/// Resolve a screen point to a `TranscriptPosition` for drag selection. The
/// hit-test returns:
/// - `WHOLE` for tool cards (B1: tool cards stay item-granular),
/// - `(item, 0)` for the card's title row,
/// - `(item, grapheme)` for body rows, where `grapheme` is the offset
///   into `body_text_for(item)`.
pub(crate) fn position_at(
    app: &AppState,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<TranscriptPosition> {
    let (index, item, row_in_item) = locate(app, area, row)?;
    Some(resolve_position_in_item(
        item,
        index,
        row_in_item,
        column,
        area,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::ToolStatus;
    use crate::tui::transcript::TranscriptSelection;

    fn running_tool(id: &str) -> ToolActivity {
        ToolActivity {
            id: id.to_string(),
            name: "read".to_string(),
            arguments: "{}".to_string(),
            delegated_model: None,
            status: ToolStatus::Running,
            result: None,
            diff: None,
            started_at: std::time::Instant::now(),
            finished_at: None,
        }
    }

    fn finished_read_tool(id: &str, path: &str, status: ToolStatus) -> ToolActivity {
        ToolActivity {
            id: id.to_string(),
            name: "read".to_string(),
            arguments: format!(r#"{{"file_path":"{path}"}}"#),
            delegated_model: None,
            status,
            result: Some(
                if matches!(status, ToolStatus::Failed) {
                    "missing file"
                } else {
                    "ok"
                }
                .to_string(),
            ),
            diff: None,
            started_at: std::time::Instant::now(),
            finished_at: Some(std::time::Instant::now()),
        }
    }

    fn item_wrapped_rows(app: &AppState, index: usize, content_width: usize) -> usize {
        let lines = render_transcript_item(
            &app.transcript[index],
            content_width,
            TranscriptRenderOptions {
                selected: ItemSelection::None,
                focused: false,
                active_group_tool_selection: None,
                serenity_mode: app.serenity_mode,
                linear_output: app.screen_reader_mode,
                execution_group_expanded: execution_group_expanded_for_item(
                    app,
                    &app.transcript[index],
                ),
                reasoning_active: serenity_reasoning_is_active(app, index, &app.transcript[index]),
                permission_command: None,
                tick: app.animation_tick(),
            },
        );
        wrapped_rows_for_lines(&lines, content_width)
    }

    /// The tool id a click at `row` resolves to, if the hit is a tool card.
    fn tool_id_at_row(app: &AppState, area: Rect, row: u16) -> Option<String> {
        match hit_at(app, area, area.x + 2, row) {
            Some(TranscriptHit::Tool(id)) => Some(id),
            _ => None,
        }
    }

    /// The hit-test walk must place a tool card exactly across the wrapped rows
    /// the renderer paints it on, so a click resolves to the right item. This
    /// guards `item_at_target`/`target_row` against drift relative to
    /// `render` (both now share the post-wrap row space).
    #[test]
    fn tool_card_hit_boundary_matches_rendered_rows() {
        let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
        app.transcript.push(TranscriptItem::AssistantMessage {
            text: "some assistant reply that occupies a few rows in a card".to_string(),
        });
        app.transcript
            .push(TranscriptItem::ToolActivity(running_tool("call-1")));

        // Tall enough that nothing scrolls (scroll == 0 in both branches).
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 200,
        };
        let content_width = area.width.saturating_sub(4).max(20) as usize;

        let above = item_wrapped_rows(&app, 0, content_width);
        let tool_rows = item_wrapped_rows(&app, 1, content_width);

        let first_row = area.y + 1; // row 0 is the frame border
        let last_above_row = first_row + above as u16 - 1;
        let first_tool_row = first_row + above as u16;
        let last_tool_row = first_row + above as u16 + tool_rows as u16 - 1;

        assert_eq!(tool_id_at_row(&app, area, last_above_row), None);
        assert_eq!(
            tool_id_at_row(&app, area, first_tool_row),
            Some("call-1".to_string())
        );
        assert_eq!(
            tool_id_at_row(&app, area, last_tool_row),
            Some("call-1".to_string())
        );
        assert_eq!(tool_id_at_row(&app, area, last_tool_row + 1), None);
    }

    /// A click must resolve to the grapheme drawn directly under the cursor.
    /// The transcript frame insets content by 1 border + 1 padding, and every
    /// card prefixes a 2-cell `┃ ` rail, so the first body glyph is painted at
    /// `area.x + 4`. Regression for the hit-test that landed a column or more to
    /// the right (making the last character unreachable).
    #[test]
    fn click_resolves_to_grapheme_under_cursor() {
        let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
        app.transcript.push(TranscriptItem::AssistantMessage {
            text: "hello world".to_string(),
        });
        // Tall enough that nothing scrolls (scroll == 0).
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 200,
        };

        // Item rows: spacer(0), title(1), body(2). With the top border at row 0,
        // the first body line is painted at screen row area.y + 1 + 2.
        let body_row = area.y + 1 + 2;
        // Body glyph 0 ('h') is drawn at area.x + 4 (border + padding + rail).
        let body_col0 = area.x + 4;

        // "hello world": h0 e1 l2 l3 o4 ' '5 w6 o7 r8 l9 d10.
        for (offset, expected) in [(0u16, 0usize), (1, 1), (6, 6), (10, 10)] {
            let pos = position_at(&app, area, body_col0 + offset, body_row)
                .expect("a body cell should hit the message item");
            assert_eq!(pos.item, 0);
            assert_eq!(
                pos.grapheme, expected,
                "click at body column +{offset} should select grapheme {expected}"
            );
        }
    }

    #[test]
    fn markdown_selection_copies_the_rendered_text_under_the_cursor() {
        let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
        app.transcript.push(TranscriptItem::AssistantMessage {
            text: "`alpha` and `omega`".to_string(),
        });
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 20,
        };
        let body_row = area.y + 3;
        let body_col0 = area.x + 4;

        // The rendered line is "alpha and omega". Raw Markdown contains four
        // additional backticks, so slicing it with these display offsets used
        // to copy unrelated characters from earlier in the source.
        let anchor =
            position_at(&app, area, body_col0 + 10, body_row).expect("omega should be selectable");
        let caret = position_at(&app, area, body_col0 + 15, body_row)
            .expect("the end of omega should be selectable");
        let selected = app
            .transcript
            .selected_text(Some(TranscriptSelection { anchor, caret }));

        assert_eq!(selected.as_deref(), Some("omega"));
    }

    #[test]
    fn wrapped_markdown_drag_reaches_the_visible_end() {
        use unicode_segmentation::UnicodeSegmentation;

        let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
        app.transcript.push(TranscriptItem::AssistantMessage {
            text: "Or save a scene file and pass `--scene file.scene`. Use `--max-depth 0` to disable reflections (faster), or `--max-depth 5` for deeper reflections. A wider terminal with smaller font gives better results — try `--width 120 --height 60.`".to_string(),
        });
        let area = Rect {
            x: 0,
            y: 0,
            width: 96,
            height: 30,
        };
        let content_width = area.width.saturating_sub(4).max(20) as usize;
        let inner_width = content_width.saturating_sub(4).max(8);
        let lines = render_transcript_item(
            &app.transcript[0],
            content_width,
            TranscriptRenderOptions {
                selected: ItemSelection::None,
                focused: false,
                active_group_tool_selection: None,
                serenity_mode: false,
                linear_output: false,
                execution_group_expanded: false,
                reasoning_active: false,
                permission_command: None,
                tick: 0,
            },
        );
        let target = "--height 60.";
        let (row_in_item, body_line) = lines
            .iter()
            .enumerate()
            .find_map(|(row, line)| {
                let rendered: String = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect();
                let body = rendered.strip_prefix("┃ ")?;
                body.find(target)
                    .map(|start| (row, body[..start + target.len()].to_string()))
            })
            .expect("the final option should be visible on a wrapped row");
        let anchor = position_at(&app, area, area.x + 4, area.y + 3)
            .expect("the first body grapheme should be selectable");
        let caret = position_at(
            &app,
            area,
            area.x + 4 + body_line.width() as u16,
            area.y + 1 + row_in_item as u16,
        )
        .expect("the final body grapheme should be selectable");
        let selected = app
            .transcript
            .selected_text(Some(TranscriptSelection { anchor, caret }));
        let visible = selection_text_for(&app.transcript[0], inner_width);

        assert_eq!(caret.grapheme, visible.graphemes(true).count());
        assert_eq!(selected.as_deref(), Some(visible.as_str()));
    }

    #[test]
    fn large_successful_execution_group_hits_visible_tool_rows() {
        let group = ExecutionGroup {
            id: 1,
            finished_at: Some(std::time::Instant::now()),
            tools: (0..8)
                .map(|index| {
                    finished_read_tool(
                        &format!("call-{index}"),
                        "src/main.rs",
                        ToolStatus::Succeeded,
                    )
                })
                .collect(),
        };

        let hit = execution_group_row_hit(&group, 240, 2, false, true, None, 0);

        assert!(
            matches!(
                hit,
                Some(ExecutionGroupRowHit::Tool {
                    group_id: 1,
                    selected_tool: 0,
                    ref tool_id,
                }) if tool_id == "call-0"
            ),
            "the first nested row should map to the first visible tool: {hit:?}"
        );
    }

    #[test]
    fn collapsed_serenity_execution_group_hits_summary_only() {
        let group = ExecutionGroup {
            id: 1,
            finished_at: Some(std::time::Instant::now()),
            tools: (0..3)
                .map(|index| {
                    finished_read_tool(
                        &format!("call-{index}"),
                        "src/main.rs",
                        ToolStatus::Succeeded,
                    )
                })
                .collect(),
        };

        let hit = execution_group_row_hit(&group, 240, 2, true, false, None, 0);

        assert_eq!(hit, Some(ExecutionGroupRowHit::Summary { group_id: 1 }));
    }

    #[test]
    fn large_failed_execution_group_hits_later_failed_row() {
        let group = ExecutionGroup {
            id: 1,
            finished_at: Some(std::time::Instant::now()),
            tools: (0..8)
                .map(|index| {
                    let status = if index == 5 {
                        ToolStatus::Failed
                    } else {
                        ToolStatus::Succeeded
                    };
                    finished_read_tool(&format!("call-{index}"), "src/main.rs", status)
                })
                .collect(),
        };

        let hit = execution_group_row_hit(&group, 240, 7, false, true, None, 0);

        assert!(
            matches!(
                hit,
                Some(ExecutionGroupRowHit::Tool {
                    group_id: 1,
                    selected_tool: 5,
                    ref tool_id,
                }) if tool_id == "call-5"
            ),
            "the first visible nested row should be the failed tool: {hit:?}"
        );
    }

    #[test]
    fn expanded_execution_group_hits_all_visible_nested_rows() {
        let now = std::time::Instant::now();
        let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
        app.transcript
            .push(TranscriptItem::ExecutionGroup(ExecutionGroup {
                id: 9,
                finished_at: Some(now),
                tools: (0..8)
                    .map(|index| {
                        finished_read_tool(
                            &format!("call-{index}"),
                            &format!("src/file_{index}.rs"),
                            ToolStatus::Succeeded,
                        )
                    })
                    .collect(),
            }));
        app.active_group_tool_selection = Some(InlineToolSelection {
            group_id: 9,
            selected_tool: 0,
        });

        let area = Rect {
            x: 0,
            y: 0,
            width: 240,
            height: 80,
        };
        let first_content_row = area.y + 1;
        let first_tool_row = first_content_row + 2;
        let last_tool_row = first_tool_row + 7;

        assert_eq!(
            hit_at(&app, area, area.x + 2, first_tool_row),
            Some(TranscriptHit::Group(ExecutionGroupRowHit::Tool {
                group_id: 9,
                selected_tool: 0,
                tool_id: "call-0".to_string(),
            }))
        );
        assert_eq!(
            hit_at(&app, area, area.x + 2, last_tool_row),
            Some(TranscriptHit::Group(ExecutionGroupRowHit::Tool {
                group_id: 9,
                selected_tool: 7,
                tool_id: "call-7".to_string(),
            }))
        );
    }
}
