use super::*;

pub(super) fn clamp_scrolls(app: &mut AppState, area: Rect) {
    let regions = crate::tui::layout::split(area, app.surface());
    let transcript_max = crate::tui::widgets::transcript::max_transcript_scroll(app, regions.main);
    app.clamp_current_scroll(transcript_max);
    scroll_transcript_focus_into_view(app, regions.main, transcript_max);

    if let Some(plan) = regions.plan {
        let plan_max = crate::tui::widgets::plan::max_plan_scroll(app, plan);
        app.clamp_plan_scroll(plan_max);
    }

    // Take the request unconditionally so it never lingers when the sidebar is
    // hidden (it would otherwise fire on a later frame against a stale list).
    let scroll_todo = std::mem::take(&mut app.scroll_todo_in_progress_into_view);
    app.todo_focus_available = regions.sidebar.is_some();
    if let Some(sidebar) = regions.sidebar {
        let sidebar_max = crate::tui::widgets::sidebar::max_todo_scroll(app, sidebar);
        app.clamp_sidebar_scroll(sidebar_max);
        if scroll_todo {
            app.sidebar_scroll =
                crate::tui::widgets::sidebar::todo_in_progress_scroll(app, sidebar, sidebar_max);
        }
    } else if app.focus == Focus::Todo {
        app.focus = Focus::Input;
    }

    resolve_question_scroll(app, area);
    reconcile_model_picker_offsets(app, area);
    if let Some(max_scroll) = crate::tui::widgets::modal::max_modal_scroll(app, area) {
        app.modal_scroll = app.modal_scroll.min(max_scroll);
    }
    resolve_context_scroll(app, area);
    resolve_composer_scroll(app, regions.input);
}

fn reconcile_model_picker_offsets(app: &mut AppState, area: Rect) {
    let Some(kind) = app.modal.as_ref() else {
        return;
    };
    let ModalKind::ModelPicker { entries } = kind else {
        return;
    };
    let modal_area = crate::tui::widgets::modal::modal_area(area, kind);
    let capacities = crate::tui::widgets::modal::model_picker_capacities(modal_area);
    let (provider_len, model_len, reasoning_len) = {
        let view = app.model_picker_view(entries);
        let reasoning_len = view
            .selected_model
            .map(AppState::model_picker_reasoning_choices)
            .map_or(0, |choices| choices.len());
        (
            view.provider_rows.len(),
            view.filtered_models.len() + usize::from(view.reset_label.is_some()),
            reasoning_len,
        )
    };
    app.model_picker.reconcile_offsets(
        provider_len,
        capacities.0,
        model_len,
        capacities.1,
        reasoning_len,
        capacities.2,
    );
}

fn resolve_context_scroll(app: &mut AppState, area: Rect) {
    // One-shot; taken unconditionally so it can't linger and fire against a
    // stale selection on a later frame.
    let reveal_expanded = std::mem::take(&mut app.context_state.reveal_expanded);
    let Some(metrics) = crate::tui::widgets::modal::context_modal_metrics(app, area) else {
        return;
    };
    app.modal_scroll = app.modal_scroll.min(metrics.max_scroll);
    if app.context_state.manual_scroll {
        return;
    }
    let Some(selected_line) = metrics.selected_line else {
        return;
    };
    if selected_line < app.modal_scroll {
        app.modal_scroll = selected_line;
    } else {
        let bottom = app
            .modal_scroll
            .saturating_add(metrics.body_height.saturating_sub(1));
        if selected_line > bottom {
            app.modal_scroll = selected_line.saturating_sub(metrics.body_height.saturating_sub(1));
        }
    }
    // A row was just expanded: also pull its opened block (sources, preview,
    // child rows) into view — capped so the selected row itself never scrolls
    // off the top when the block is taller than the viewport.
    if reveal_expanded && let Some(block_end) = metrics.selected_block_end {
        let last_block_line = block_end.saturating_sub(1);
        let bottom = app
            .modal_scroll
            .saturating_add(metrics.body_height.saturating_sub(1));
        if last_block_line > bottom {
            app.modal_scroll = last_block_line
                .saturating_sub(metrics.body_height.saturating_sub(1))
                .min(selected_line);
        }
    }
    app.modal_scroll = app.modal_scroll.min(metrics.max_scroll);
}

fn scroll_transcript_focus_into_view(app: &mut AppState, area: Rect, max_scroll: u16) {
    if !app.scroll_transcript_focus_into_view {
        return;
    }
    app.scroll_transcript_focus_into_view = false;
    app.transcript_autoscroll = false;
    let Some(index) = app.transcript_focus else {
        return;
    };
    let Some((start, end)) =
        crate::tui::widgets::transcript::item_line_span(app, area.width, index)
    else {
        return;
    };
    let viewport = area.height.saturating_sub(2) as usize;
    if viewport == 0 {
        return;
    }
    let current = app.transcript_scroll as usize;
    let next = if start < current {
        start
    } else if end > current.saturating_add(viewport) {
        end.saturating_sub(viewport)
    } else {
        current
    };
    app.transcript_scroll = next.min(max_scroll as usize) as u16;
}

fn resolve_question_scroll(app: &mut AppState, area: Rect) {
    let Some(ModalKind::QuestionPrompt {
        prompt,
        origin,
        options,
        multiple,
        cursor,
        ..
    }) = app.modal.as_ref()
    else {
        app.pending_question_visibility = false;
        return;
    };

    let modal_area = crate::tui::widgets::modal::question_prompt_area(area);
    let metrics = crate::tui::widgets::modal::question_prompt_metrics(
        modal_area,
        prompt,
        origin.as_deref(),
        options,
        *multiple,
        *cursor,
    );
    if app.pending_question_visibility {
        app.pending_question_visibility = false;
        let current = app.modal_scroll;
        app.modal_scroll = if metrics.selected_start < current {
            metrics.selected_start
        } else if metrics.selected_end > current.saturating_add(metrics.body_height) {
            metrics.selected_end.saturating_sub(metrics.body_height)
        } else {
            current
        };
    }
    app.modal_scroll = app.modal_scroll.min(metrics.max_scroll);
}

/// Resolve any pending composer scroll/extend actions using the live
/// input region. The reducer queues page scrolls and selection-extend
/// page moves (whose char step depends on the body size) into pending
/// fields; we finalize them here once the layout is known. The manual
/// `composer_scroll` is clamped to the valid range.
fn resolve_composer_scroll(app: &mut AppState, input_area: Rect) {
    use crate::tui::widgets::input;

    let body_visible = input::body_visible_rows(input_area) as usize;
    let content_width = input::body_content_width(input_area) as usize;

    if let Some(delta) = app.pending_composer_page.take() {
        let row_delta = delta * body_visible as i16;
        app.composer_scroll = clamped_scroll(app.composer_scroll, row_delta);
    }

    if let Some(delta) = app.pending_composer_extend.take() {
        let char_step = (delta.unsigned_abs() as usize) * body_visible * content_width;
        let signed = if delta < 0 {
            -(char_step as i32)
        } else {
            char_step as i32
        };
        app.composer.extend_by_chars(signed);
    }

    let max_scroll = composer_max_scroll(app, body_visible, content_width);
    app.clamp_composer_scroll(max_scroll);
}

/// Largest valid `composer_scroll` offset for the current text. Computed from
/// the same visual row count used by the renderer, including the cursor-only
/// continuation row at an exact wrap boundary.
fn composer_max_scroll(app: &AppState, body_visible: usize, content_width: usize) -> u16 {
    // Count rows against the chip-expanded display text: a chip sentinel is one
    // buffer grapheme but renders as its full multi-column label, so the buffer
    // would undercount wrapped rows.
    let display = app.composer.display();
    let total = crate::tui::widgets::input::composer_visual_row_count(
        &display.text,
        display.to_display(app.composer.cursor),
        content_width,
    );
    if total <= body_visible {
        0
    } else {
        (total - body_visible) as u16
    }
}
