use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Margin, Rect};

use crate::tui::app::{AppState, MouseArea, SelectionKind, next_mouse_click};
use crate::tui::event::{AppAction, Focus, ModalKind, PromptDecision, PromptFamily};
use crate::tui::layout;
use crate::tui::widgets::{input, modal, plan, sidebar, transcript};

/// The action a click in the scrim (outside the modal frame) should fire —
/// the same dismissal Esc performs for that modal kind. Permission prompts
/// deny and question prompts cancel so the underlying request is answered
/// rather than left dangling; every other modal just closes.
fn modal_scrim_dismiss(kind: &ModalKind) -> AppAction {
    if let Some(family) = PromptFamily::of_modal(kind) {
        return AppAction::PromptDecision {
            family,
            decision: PromptDecision::Deny,
        };
    }
    match kind {
        ModalKind::Detail(crate::tui::event::DetailModal::QuestionPrompt { .. }) => {
            AppAction::QuestionCancel
        }
        _ => AppAction::CloseModal,
    }
}

pub fn map_mouse(mouse: MouseEvent, app: &AppState, area: Rect) -> Option<AppAction> {
    if let Some(kind) = app.modal.as_ref() {
        return match mouse.kind {
            MouseEventKind::ScrollUp => Some(AppAction::ScrollModal(-3)),
            MouseEventKind::ScrollDown => Some(AppAction::ScrollModal(3)),
            MouseEventKind::Down(MouseButton::Left) => {
                // A click outside the modal frame dismisses it like Esc.
                let modal_rect = modal::modal_area(area, kind);
                if !modal_rect.contains((mouse.column, mouse.row).into()) {
                    return Some(modal_scrim_dismiss(kind));
                }
                // Try body text selection first (double-click word, triple-click line).
                if let Some(offset) = modal_body_hit(app, mouse.column, mouse.row) {
                    let click = next_mouse_click(
                        app.last_mouse_click,
                        MouseArea::Modal,
                        mouse.column,
                        mouse.row,
                    );
                    let selection_kind = selection_kind_from_count(click.count);
                    return Some(AppAction::ModalClick {
                        offset,
                        kind: selection_kind,
                        column: mouse.column,
                        row: mouse.row,
                    });
                }
                modal::context_row_index_at(app, area, mouse.column, mouse.row)
                    .map(|row_index| AppAction::ContextClick { row_index })
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(offset) = modal_body_hit(app, mouse.column, mouse.row) {
                    return Some(AppAction::ModalDrag { offset });
                }
                None
            }
            MouseEventKind::Up(MouseButton::Left) => Some(AppAction::PointerSelectionEnd),
            _ => None,
        };
    }
    let regions = layout::split(area, app.surface());
    match mouse.kind {
        MouseEventKind::ScrollUp => scroll_action(mouse.column, mouse.row, app, &regions, -3),
        MouseEventKind::ScrollDown => scroll_action(mouse.column, mouse.row, app, &regions, 3),
        MouseEventKind::Down(MouseButton::Left) => {
            down_action(mouse.column, mouse.row, app, &regions)
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            drag_action(mouse.column, mouse.row, app, &regions)
        }
        // Releasing the button finalizes a pointer selection: the copy (and its
        // notice) happens here, once, rather than on every drag tick.
        MouseEventKind::Up(MouseButton::Left) => Some(AppAction::PointerSelectionEnd),
        _ => None,
    }
}

fn scroll_action(
    column: u16,
    row: u16,
    app: &AppState,
    regions: &layout::AppLayout,
    delta: i16,
) -> Option<AppAction> {
    if input::active_completion_area(regions.input, app)
        .is_some_and(|area| area.contains((column, row).into()))
    {
        return None;
    }
    // Wheel over the composer drives the composer's own page scroll when
    // the input has focus. Otherwise fall through to the
    // usual plan / sidebar / transcript routing.
    if regions.input.contains((column, row).into()) && app.focus == Focus::Input {
        return Some(AppAction::ComposerPage(if delta < 0 { -1 } else { 1 }));
    }
    if regions.input_footer.contains((column, row).into()) {
        return None;
    }
    if let Some(plan_area) = regions.plan
        && plan_area.contains((column, row).into())
    {
        return Some(AppAction::ScrollPlan(delta));
    }
    if let Some(sidebar) = regions.sidebar
        && sidebar.contains((column, row).into())
    {
        // The sidebar is the todo card in full now, so any point inside it
        // scrolls the todo list.
        return Some(AppAction::ScrollSidebar(delta));
    }
    Some(AppAction::ScrollCurrent(delta))
}

fn down_action(
    column: u16,
    row: u16,
    app: &AppState,
    regions: &layout::AppLayout,
) -> Option<AppAction> {
    if input::active_completion_area(regions.input, app)
        .is_some_and(|area| area.contains((column, row).into()))
    {
        return None;
    }
    if let Some(rect) =
        crate::tui::view::header_context_chip_rect(regions.header, app, regions.show_input)
        && rect.contains((column, row).into())
    {
        return Some(AppAction::OpenContextModal);
    }

    // The jump-to-latest pill floats over the bottom of the transcript, so it
    // must be tested before the transcript body claims the click.
    if let Some((rect, _)) = transcript::jump_to_bottom_pill(app, regions.main)
        && rect.contains((column, row).into())
    {
        return Some(AppAction::ScrollBottom);
    }

    if let Some(action) = scrollbar_action(column, row, app, regions) {
        return Some(action);
    }

    if regions.input.contains((column, row).into()) {
        // Hit-test against the chip-expanded display text, then map the display
        // index back to a buffer index (snapping chip-interior clicks to an
        // edge) so the cursor never lands inside a chip.
        let display = app.composer.display();
        if let Some(display_index) = input::composer_position_at(
            regions.input,
            &display.text,
            display.to_display(app.composer.cursor),
            app.composer_scroll,
            app.composer_follow,
            column,
            row,
        ) {
            let last = next_mouse_click(app.last_mouse_click, MouseArea::Composer, column, row);
            let kind = selection_kind_from_count(last.count);
            return Some(AppAction::ComposerClick {
                char_index: display.to_buffer(display_index),
                kind,
                column,
                row,
            });
        }
        return Some(AppAction::SetFocus(Focus::Input));
    }
    if regions.input_footer.contains((column, row).into()) {
        return Some(AppAction::SetFocus(Focus::Input));
    }
    if let Some(plan_area) = regions.plan
        && plan_area.contains((column, row).into())
    {
        if let Some(position) = plan::position_at(app, plan_area, column, row) {
            let last = next_mouse_click(app.last_mouse_click, MouseArea::Plan, column, row);
            let kind = selection_kind_from_count(last.count);
            return Some(AppAction::PlanClick {
                position,
                kind,
                column,
                row,
            });
        }
        // Focusing the plan canvas makes keyboard scrolling drive the
        // canvas while the plan view is open. The plan canvas only
        // exists in plan view, so this is a no-op outside it.
        return Some(AppAction::SetFocus(Focus::Plan));
    }
    if let Some(sidebar) = regions.sidebar
        && sidebar.contains((column, row).into())
    {
        // The sidebar is the todo card in full; clicking anywhere in it focuses
        // the todo list. (The context readout moved to the header chip above.)
        return Some(AppAction::SetFocus(Focus::Todo));
    }
    if regions.main.contains((column, row).into()) {
        // One resolved hit answers every transcript-click question in a single
        // wrapped-row walk, instead of one full transcript layout per question.
        if let Some(hit) = transcript::hit_at(app, regions.main, column, row) {
            return Some(match hit {
                transcript::TranscriptHit::QueuedCancel(id) => AppAction::CancelQueuedInput { id },
                transcript::TranscriptHit::Group(transcript::ExecutionGroupRowHit::Summary {
                    group_id,
                }) => {
                    if app.serenity_mode {
                        AppAction::ToggleExecutionGroup { group_id }
                    } else {
                        AppAction::OpenExecutionGroup { group_id }
                    }
                }
                transcript::TranscriptHit::Group(transcript::ExecutionGroupRowHit::Tool {
                    tool_id,
                    ..
                })
                | transcript::TranscriptHit::Tool(tool_id) => AppAction::OpenToolDetail(tool_id),
                transcript::TranscriptHit::Position(position) => {
                    let last =
                        next_mouse_click(app.last_mouse_click, MouseArea::Transcript, column, row);
                    AppAction::TranscriptClick {
                        position,
                        kind: selection_kind_from_count(last.count),
                        extend: false,
                        column,
                        row,
                    }
                }
            });
        }
        return Some(AppAction::SetFocus(Focus::Transcript));
    }
    None
}

fn drag_action(
    column: u16,
    row: u16,
    app: &AppState,
    regions: &layout::AppLayout,
) -> Option<AppAction> {
    if input::active_completion_area(regions.input, app)
        .is_some_and(|area| area.contains((column, row).into()))
    {
        return None;
    }
    if app.pointer_selecting && app.focus == Focus::Transcript {
        return transcript_drag_action(column, row, app, regions.main);
    }
    if app.pointer_selecting
        && app.focus == Focus::Plan
        && let Some(plan_area) = regions.plan
    {
        return plan_drag_action(column, row, app, plan_area);
    }
    if regions.input.contains((column, row).into()) {
        let display = app.composer.display();
        if let Some(display_index) = input::composer_position_at(
            regions.input,
            &display.text,
            display.to_display(app.composer.cursor),
            app.composer_scroll,
            app.composer_follow,
            column,
            row,
        ) {
            return Some(AppAction::ComposerDrag {
                char_index: display.to_buffer(display_index),
            });
        }
    }
    if let Some(action) = transcript_drag_action(column, row, app, regions.main) {
        return Some(action);
    }
    if let Some(plan_area) = regions.plan
        && let Some(action) = plan_drag_action(column, row, app, plan_area)
    {
        return Some(action);
    }
    None
}

fn transcript_drag_action(column: u16, row: u16, app: &AppState, area: Rect) -> Option<AppAction> {
    // Check the inner viewport before its full frame. Terminal mouse coordinates
    // cannot travel beyond the screen, and the frame itself is still inside
    // `area`; treating its top and bottom rows as edge zones keeps selection
    // scrolling available when the viewport abuts another pane or screen edge.
    if horizontal_contains(area, column)
        && let Some((position, scroll_delta)) = transcript_edge_drag(app, area, column, row)
    {
        return Some(AppAction::TranscriptDrag {
            position,
            scroll_delta,
        });
    }
    if area.contains((column, row).into())
        && let Some(position) = transcript::position_at(app, area, column, row)
    {
        return Some(AppAction::TranscriptDrag {
            position,
            scroll_delta: 0,
        });
    }
    None
}

fn plan_drag_action(column: u16, row: u16, app: &AppState, area: Rect) -> Option<AppAction> {
    // See `transcript_drag_action`: the frame rows must be edge zones too so
    // selection scrolling works at a screen or adjacent-pane boundary.
    if horizontal_contains(area, column)
        && let Some((position, scroll_delta)) = plan_edge_drag(app, area, column, row)
    {
        return Some(AppAction::PlanDrag {
            position,
            scroll_delta,
        });
    }
    if area.contains((column, row).into())
        && let Some(position) = plan::position_at(app, area, column, row)
    {
        return Some(AppAction::PlanDrag {
            position,
            scroll_delta: 0,
        });
    }
    None
}

fn horizontal_contains(area: Rect, column: u16) -> bool {
    column >= area.x && column < area.right()
}

fn transcript_edge_drag(
    app: &AppState,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<(crate::tui::app::TranscriptPosition, i16)> {
    let inner = area.inner(Margin {
        vertical: 1,
        horizontal: 1,
    });
    edge_drag_row(inner, row).and_then(|(hit_row, scroll_delta)| {
        transcript::position_at(app, area, column, hit_row).map(|position| (position, scroll_delta))
    })
}

fn plan_edge_drag(
    app: &AppState,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<(crate::tui::app::PlanPosition, i16)> {
    let canvas_area = if area.height < 3 {
        area
    } else {
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: area.height - 1,
        }
    };
    let inner = canvas_area.inner(Margin {
        vertical: 1,
        horizontal: 1,
    });
    edge_drag_row(inner, row).and_then(|(hit_row, scroll_delta)| {
        plan::position_at(app, area, column, hit_row).map(|position| (position, scroll_delta))
    })
}

fn edge_drag_row(inner: Rect, row: u16) -> Option<(u16, i16)> {
    if inner.is_empty() {
        return None;
    }
    if row < inner.y {
        return Some((inner.y, -1));
    }
    if row >= inner.bottom() {
        return Some((inner.bottom().saturating_sub(1), 1));
    }
    None
}

fn selection_kind_from_count(count: u8) -> SelectionKind {
    match count {
        1 => SelectionKind::Position,
        2 => SelectionKind::Word,
        _ => SelectionKind::Line,
    }
}

/// Hit-test a modal body click: returns the flat grapheme offset if the
/// `(column, row)` falls inside the body text area of a scrollable modal.
fn modal_body_hit(app: &AppState, column: u16, row: u16) -> Option<usize> {
    let body_lines = app.modal_body_lines.borrow();
    let body_area = app.modal_body_rect.get()?;
    if body_lines.is_empty() {
        return None;
    }
    modal::modal_resolve_position(column, row, &body_lines, app.modal_scroll, body_area)
}

fn scrollbar_action(
    column: u16,
    row: u16,
    app: &AppState,
    regions: &layout::AppLayout,
) -> Option<AppAction> {
    if let Some(scroll) = vertical_scroll_from_point(
        column,
        row,
        regions.main.inner(Margin {
            vertical: 1,
            horizontal: 1,
        }),
        transcript::max_transcript_scroll(app, regions.main),
    ) {
        return Some(AppAction::SetCurrentScroll(scroll));
    }

    if let Some(plan_area) = regions.plan
        && let Some(scroll) = vertical_scroll_from_point(
            column,
            row,
            plan_area.inner(Margin {
                vertical: 1,
                horizontal: 1,
            }),
            plan::max_plan_scroll(app, plan_area),
        )
    {
        return Some(AppAction::SetPlanScroll(scroll));
    }

    let sidebar_area = regions.sidebar?;
    vertical_scroll_from_point(
        column,
        row,
        sidebar_area.inner(Margin {
            vertical: 1,
            horizontal: 1,
        }),
        sidebar::max_todo_scroll(app, sidebar_area),
    )
    .map(AppAction::SetSidebarScroll)
}

fn vertical_scroll_from_point(column: u16, row: u16, area: Rect, max_scroll: u16) -> Option<u16> {
    if max_scroll == 0 || area.is_empty() || column != area.right().saturating_sub(1) {
        return None;
    }
    if row < area.y || row >= area.bottom() {
        return None;
    }
    if row == area.y {
        return Some(0);
    }
    if row == area.bottom().saturating_sub(1) {
        return Some(max_scroll);
    }

    let track_len = area.height.saturating_sub(2).max(1);
    let rel = row
        .saturating_sub(area.y + 1)
        .min(track_len.saturating_sub(1));
    Some(((rel as u32 * max_scroll as u32) / track_len.saturating_sub(1).max(1) as u32) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{ExecutionGroup, ToolActivity, ToolStatus, TranscriptItem};
    use crate::tui::event::ModalKind;
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;

    use crate::tui::test_utils::input_app;

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    fn app_with_focus() -> AppState {
        input_app()
    }

    fn context_report() -> crate::agent::ContextReport {
        crate::agent::ContextReport {
            budget_tokens: 120_000,
            entries: vec![crate::agent::ContextEntry {
                role: crate::agent::ContextRole::User,
                tokens: 12_000,
                text: "hello".to_string(),
            }],
            last_prompt_tokens: None,
            last_completion_tokens: None,
            session_prompt_tokens: 0,
            session_completion_tokens: 0,
            ..Default::default()
        }
    }

    fn expandable_context_report() -> crate::agent::ContextReport {
        let metadata_source = crate::provider::TokenCounterKind::Heuristic;
        let metadata_confidence = crate::provider::EstimateConfidence::Low;
        crate::agent::ContextReport {
            budget_tokens: 120_000,
            entries: vec![crate::agent::ContextEntry {
                role: crate::agent::ContextRole::User,
                tokens: 12_000,
                text: "hello".to_string(),
            }],
            ledger: vec![crate::agent::ContextNode {
                id: "chat".into(),
                kind: crate::agent::ContextNodeKind::ChatRoot,
                inclusion: crate::agent::ContextInclusion::Included,
                role: None,
                label: "Chat".to_string(),
                tokens: 12_000,
                chars: 5,
                bytes: 5,
                source: metadata_source,
                confidence: metadata_confidence,
                preview: String::new(),
                sources: Vec::new(),
                children: vec![crate::agent::ContextNode {
                    id: "chat-message".into(),
                    kind: crate::agent::ContextNodeKind::ChatMessage,
                    inclusion: crate::agent::ContextInclusion::Included,
                    role: Some(crate::agent::ContextRole::User),
                    label: "Message text".to_string(),
                    tokens: 12_000,
                    chars: 5,
                    bytes: 5,
                    source: metadata_source,
                    confidence: metadata_confidence,
                    preview: "hello".to_string(),
                    sources: Vec::new(),
                    children: Vec::new(),
                }],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn wheel_in_composer_emits_composer_page() {
        let app = app_with_focus();
        // 80x24 area; the input region from layout::split will be a band
        // at the bottom. We just need *some* point inside the input area.
        let area = Rect::new(0, 0, 80, 24);
        let regions = crate::tui::layout::split(area, app.surface());
        let (col, row) = (regions.input.x + 2, regions.input.y + 1);

        let up = map_mouse(mouse(MouseEventKind::ScrollUp, col, row), &app, area);
        assert!(matches!(up, Some(AppAction::ComposerPage(-1))));

        let down = map_mouse(mouse(MouseEventKind::ScrollDown, col, row), &app, area);
        assert!(matches!(down, Some(AppAction::ComposerPage(1))));
    }

    #[test]
    fn wheel_in_composer_outside_focus_still_scrolls_transcript() {
        let mut app = app_with_focus();
        app.focus = Focus::Transcript;
        let area = Rect::new(0, 0, 80, 24);
        let regions = crate::tui::layout::split(area, app.surface());
        let (col, row) = (regions.input.x + 2, regions.input.y + 1);

        let down = map_mouse(mouse(MouseEventKind::ScrollDown, col, row), &app, area);
        assert!(matches!(down, Some(AppAction::ScrollCurrent(3))));
    }

    #[test]
    fn wheel_outside_composer_scrolls_transcript() {
        let app = app_with_focus();
        let area = Rect::new(0, 0, 80, 24);
        let regions = crate::tui::layout::split(area, app.surface());
        let (col, row) = (regions.main.x + 2, regions.main.y + 1);

        let down = map_mouse(mouse(MouseEventKind::ScrollDown, col, row), &app, area);
        assert!(matches!(down, Some(AppAction::ScrollCurrent(3))));
    }

    #[test]
    fn completion_overlay_shields_transcript_from_mouse_events() {
        let mut app = app_with_focus();
        app.reduce(AppAction::InputChar('/'));
        let area = Rect::new(0, 0, 80, 24);
        let regions = crate::tui::layout::split(area, app.surface());
        let completion = input::active_completion_area(regions.input, &app)
            .expect("slash completion should have an overlay");

        for row in [completion.y, completion.bottom() - 1] {
            let wheel = map_mouse(
                mouse(MouseEventKind::ScrollDown, completion.x + 1, row),
                &app,
                area,
            );
            let click = map_mouse(
                mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    completion.x + 1,
                    row,
                ),
                &app,
                area,
            );
            assert!(wheel.is_none(), "overlay wheel leaked at row {row}");
            assert!(click.is_none(), "overlay click leaked at row {row}");
        }
    }

    #[test]
    fn wheel_over_sidebar_scrolls_sidebar() {
        let app = app_with_focus();
        let area = Rect::new(0, 0, 120, 32);
        let regions = crate::tui::layout::split(area, app.surface());
        let sidebar = regions.sidebar.expect("wide layout should show sidebar");
        let (col, row) = (sidebar.x + 2, sidebar.y.saturating_add(2));

        let down = map_mouse(mouse(MouseEventKind::ScrollDown, col, row), &app, area);
        assert!(matches!(down, Some(AppAction::ScrollSidebar(3))));
    }

    #[test]
    fn wheel_in_composer_does_not_leak_to_modal_scroll() {
        let mut app = app_with_focus();
        app.modal = Some(ModalKind::Detail(crate::tui::event::DetailModal::Help));
        let area = Rect::new(0, 0, 80, 24);
        let regions = crate::tui::layout::split(area, app.surface());
        let (col, row) = (regions.input.x + 2, regions.input.y + 1);

        let down = map_mouse(mouse(MouseEventKind::ScrollDown, col, row), &app, area);
        assert!(
            matches!(down, Some(AppAction::ScrollModal(3))),
            "modal-open wheel must always scroll the modal body"
        );
    }

    #[test]
    fn left_click_context_modal_row_toggles_context_node() {
        let mut app = app_with_focus();
        app.modal = Some(ModalKind::Detail(crate::tui::event::DetailModal::Context(
            Box::new(expandable_context_report()),
        )));
        app.context_state.expanded.clear();
        let area = Rect::new(0, 0, 100, 40);
        let Some((col, row)) = first_context_row_point(&app, area) else {
            panic!("context row should be hittable");
        };

        let action = map_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), col, row),
            &app,
            area,
        );

        assert!(matches!(
            action,
            Some(AppAction::ContextClick { row_index: 0 })
        ));
    }

    fn first_context_row_point(app: &AppState, area: Rect) -> Option<(u16, u16)> {
        let modal_area = modal::modal_area(area, app.modal.as_ref()?);
        let content = modal_area.inner(ratatui::layout::Margin {
            vertical: 1,
            horizontal: 1,
        });
        for row in content.y..content.bottom() {
            for col in content.x..content.right() {
                if modal::context_row_index_at(app, area, col, row).is_some() {
                    return Some((col, row));
                }
            }
        }
        None
    }

    #[test]
    fn left_button_release_finalizes_pointer_selection() {
        // Releasing the left button maps to PointerSelectionEnd regardless of
        // where the cursor is — the reducer decides whether anything is copied.
        let app = app_with_focus();
        let area = Rect::new(0, 0, 80, 24);

        let action = map_mouse(
            mouse(MouseEventKind::Up(MouseButton::Left), 5, 5),
            &app,
            area,
        );
        assert!(matches!(action, Some(AppAction::PointerSelectionEnd)));
    }

    #[test]
    fn left_click_in_composer_still_works() {
        // Regression: the wheel-routing change must not break click
        // hit-testing for the composer.
        let mut app = app_with_focus();
        app.composer.set_text("hello".to_string());
        app.composer.cursor = 5;
        let area = Rect::new(0, 0, 80, 24);
        let regions = crate::tui::layout::split(area, app.surface());
        let (col, row) = (regions.input.x + 4, regions.input.y + 1);

        let action = map_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), col, row),
            &app,
            area,
        );
        assert!(matches!(action, Some(AppAction::ComposerClick { .. })));
    }

    #[test]
    fn left_click_in_plan_pane_starts_plan_selection() {
        let mut app = app_with_focus();
        // Drive view + active_mode together so the canvas surface is rendered.
        app.reduce(crate::tui::event::AppAction::SetView(
            crate::tui::event::View::Plan,
        ));
        app.focus = crate::tui::event::Focus::Transcript;
        let area = Rect::new(0, 0, 120, 32);
        let regions = crate::tui::layout::split(area, app.surface());
        let plan_area = regions.plan.expect("plan view should have a canvas");
        let (col, row) = (plan_area.x + 2, plan_area.y + 2);

        let action = map_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), col, row),
            &app,
            area,
        );
        assert!(matches!(action, Some(AppAction::PlanClick { .. })));
    }

    #[test]
    fn drag_in_plan_pane_extends_plan_selection() {
        let mut app = app_with_focus();
        app.reduce(crate::tui::event::AppAction::SetView(
            crate::tui::event::View::Plan,
        ));
        app.plan.edit().add_task("copy this task");
        let area = Rect::new(0, 0, 120, 32);
        let regions = crate::tui::layout::split(area, app.surface());
        let plan_area = regions.plan.expect("plan view should have a canvas");
        let (col, row) = (plan_area.x + 10, plan_area.y + 2);

        let action = map_mouse(
            mouse(MouseEventKind::Drag(MouseButton::Left), col, row),
            &app,
            area,
        );
        assert!(matches!(action, Some(AppAction::PlanDrag { .. })));
    }

    #[test]
    fn drag_below_transcript_edge_extends_and_scrolls() {
        let mut app = app_with_focus();
        app.transcript.push(TranscriptItem::AssistantMessage {
            text: (0..80)
                .map(|index| format!("copy this text line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        });
        app.focus = crate::tui::event::Focus::Transcript;
        app.pointer_selecting = true;
        let area = Rect::new(0, 0, 80, 24);
        let regions = crate::tui::layout::split(area, app.surface());
        let col = regions.main.x + 10;
        let row = regions.main.bottom();

        let action = map_mouse(
            mouse(MouseEventKind::Drag(MouseButton::Left), col, row),
            &app,
            area,
        );
        assert!(
            matches!(
                action,
                Some(AppAction::TranscriptDrag {
                    scroll_delta: 1,
                    ..
                })
            ),
            "unexpected action: {action:?}"
        );
    }

    #[test]
    fn drag_below_plan_edge_extends_and_scrolls() {
        let mut app = app_with_focus();
        app.reduce(crate::tui::event::AppAction::SetView(
            crate::tui::event::View::Plan,
        ));
        for index in 0..80 {
            app.plan.edit().add_task(&format!("copy this task {index}"));
        }
        app.focus = crate::tui::event::Focus::Plan;
        app.pointer_selecting = true;
        let area = Rect::new(0, 0, 120, 32);
        let regions = crate::tui::layout::split(area, app.surface());
        let plan_area = regions.plan.expect("plan view should have a canvas");
        let col = plan_area.x + 10;
        let row = plan_area.bottom();

        let action = map_mouse(
            mouse(MouseEventKind::Drag(MouseButton::Left), col, row),
            &app,
            area,
        );
        assert!(
            matches!(
                action,
                Some(AppAction::PlanDrag {
                    scroll_delta: 1,
                    ..
                })
            ),
            "unexpected action: {action:?}"
        );
    }

    #[test]
    fn left_click_in_todo_sets_todo_focus() {
        let app = app_with_focus();
        let area = Rect::new(0, 0, 120, 32);
        let regions = crate::tui::layout::split(area, app.surface());
        let sidebar = regions.sidebar.expect("wide layout should show sidebar");
        let (col, row) = (sidebar.x + 2, sidebar.y.saturating_add(2));

        let action = map_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), col, row),
            &app,
            area,
        );
        assert!(matches!(
            action,
            Some(AppAction::SetFocus(crate::tui::event::Focus::Todo))
        ));
    }

    #[test]
    fn left_click_header_context_chip_opens_context_modal() {
        let mut app = app_with_focus();
        app.latest_context_report = Some(context_report());
        let area = Rect::new(0, 0, 80, 24);
        let regions = crate::tui::layout::split(area, app.surface());
        let chip =
            crate::tui::view::header_context_chip_rect(regions.header, &app, regions.show_input)
                .expect("narrow layout should expose header context chip");
        let (col, row) = (chip.x + 1, chip.y);

        let action = map_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), col, row),
            &app,
            area,
        );

        assert!(matches!(action, Some(AppAction::OpenContextModal)));
    }

    #[test]
    fn left_click_group_summary_focuses_execution_group_item() {
        let mut app = app_with_focus();
        app.transcript
            .push(TranscriptItem::ExecutionGroup(ExecutionGroup {
                id: 1,
                finished_at: None,
                tools: vec![ToolActivity {
                    id: "call-1".to_string(),
                    name: "bash".to_string(),
                    arguments: "{}".to_string(),
                    status: ToolStatus::Succeeded,
                    result: Some("ok".to_string()),
                    diff: None,
                    started_at: std::time::Instant::now(),
                    finished_at: Some(std::time::Instant::now()),
                }],
            }));
        let area = Rect::new(0, 0, 80, 24);
        let regions = crate::tui::layout::split(area, app.surface());
        let (col, row) = (regions.main.x + 2, regions.main.y + 1);

        let action = map_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), col, row),
            &app,
            area,
        );
        assert!(matches!(
            action,
            Some(AppAction::OpenExecutionGroup { group_id: 1 })
        ));
    }

    #[test]
    fn serenity_left_click_group_summary_toggles_execution_group() {
        let mut app = app_with_focus();
        app.serenity_mode = true;
        app.transcript
            .push(TranscriptItem::ExecutionGroup(ExecutionGroup {
                id: 1,
                finished_at: None,
                tools: vec![ToolActivity {
                    id: "call-1".to_string(),
                    name: "bash".to_string(),
                    arguments: "{}".to_string(),
                    status: ToolStatus::Succeeded,
                    result: Some("ok".to_string()),
                    diff: None,
                    started_at: std::time::Instant::now(),
                    finished_at: Some(std::time::Instant::now()),
                }],
            }));
        let area = Rect::new(0, 0, 80, 24);
        let regions = crate::tui::layout::split(area, app.surface());
        let (col, row) = (regions.main.x + 2, regions.main.y + 1);

        let action = map_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), col, row),
            &app,
            area,
        );

        assert!(matches!(
            action,
            Some(AppAction::ToggleExecutionGroup { group_id: 1 })
        ));
    }

    #[test]
    fn left_click_group_tool_row_opens_tool_detail() {
        let mut app = app_with_focus();
        app.transcript
            .push(TranscriptItem::ExecutionGroup(ExecutionGroup {
                id: 1,
                finished_at: None,
                tools: vec![ToolActivity {
                    id: "call-1".to_string(),
                    name: "bash".to_string(),
                    arguments: "{\"command\":\"echo hi\"}".to_string(),
                    status: ToolStatus::Succeeded,
                    result: Some("ok".to_string()),
                    diff: None,
                    started_at: std::time::Instant::now(),
                    finished_at: Some(std::time::Instant::now()),
                }],
            }));
        let area = Rect::new(0, 0, 80, 24);
        let regions = crate::tui::layout::split(area, app.surface());
        let (col, row) = (regions.main.x + 2, regions.main.y + 3);

        let action = map_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), col, row),
            &app,
            area,
        );
        assert!(matches!(
            action,
            Some(AppAction::OpenToolDetail(tool_id)) if tool_id == "call-1"
        ));
    }

    #[test]
    fn left_click_jump_to_latest_pill_scrolls_to_bottom() {
        let mut app = app_with_focus();
        // Overflow the transcript, then scroll the reader away from the bottom
        // so the floating jump-to-latest pill is rendered.
        for i in 0..40 {
            app.transcript.push(TranscriptItem::AssistantMessage {
                text: format!("reply {i}"),
            });
        }
        app.transcript_autoscroll = false;
        app.transcript_scroll = 0;
        let area = Rect::new(0, 0, 80, 24);
        let regions = crate::tui::layout::split(area, app.surface());
        let (rect, _) = transcript::jump_to_bottom_pill(&app, regions.main)
            .expect("a scrolled-up transcript should expose the jump-to-latest pill");
        let (col, row) = (rect.x + rect.width / 2, rect.y);

        let action = map_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), col, row),
            &app,
            area,
        );
        assert!(matches!(action, Some(AppAction::ScrollBottom)));
    }

    #[test]
    fn modal_double_click_selects_word_through_full_event_sequence() {
        // Simulate the real terminal event sequence of a double-click
        // (press, release, press, release) against an open Help modal and
        // assert the word selection survives the final release.
        let mut app = app_with_focus();
        app.modal = Some(ModalKind::Detail(crate::tui::event::DetailModal::Help));
        let area = Rect::new(0, 0, 140, 40);
        let modal_rect = modal::modal_area(area, app.modal.as_ref().unwrap());
        *app.modal_body_lines.borrow_mut() = vec![ratatui::text::Line::from("hello world again")];
        let body = Rect::new(modal_rect.x + 1, modal_rect.y + 1, 40, 5);
        app.modal_body_rect.set(Some(body));

        // Click on 'w' of "world": offset 6.
        let (col, row) = (body.x + 6, body.y);
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            if let Some(action) = map_mouse(mouse(kind, col, row), &app, area) {
                app.reduce(action);
            }
        }

        assert_eq!(
            app.modal_selection,
            Some(crate::tui::app::ModalSelection {
                anchor: 6,
                caret: 11
            }),
            "double-click word selection must survive the trailing release"
        );
    }

    #[test]
    fn left_click_queued_message_del_cancels_message() {
        let mut app = app_with_focus();
        app.transcript.push(TranscriptItem::QueuedUserMessage {
            id: 9,
            text: "queued".to_string(),
            delivery: crate::tui::app::FollowUpDelivery::Queue,
        });
        let area = Rect::new(0, 0, 80, 24);
        let regions = crate::tui::layout::split(area, app.surface());
        let content_width = regions.main.width.saturating_sub(4).max(20);
        let col = regions
            .main
            .x
            .saturating_add(content_width.saturating_sub(5));
        let row = regions.main.y + 2;

        let action = map_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), col, row),
            &app,
            area,
        );

        assert!(matches!(
            action,
            Some(AppAction::CancelQueuedInput { id: 9 })
        ));
    }
}
