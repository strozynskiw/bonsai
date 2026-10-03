use super::context::*;
use super::detail::*;
use super::episodes::*;
use super::mcp::*;
use super::perf::*;
use super::providers::*;
use super::skill_manager::*;
use super::subtasks::*;
use super::tasks::*;
use super::usage::*;
use super::*;

/// Four modal width tiers, so a new modal picks an existing footprint instead
/// of inventing a twelfth width percentage. Heights stay per-modal.
const MODAL_WIDTH_PICKER: u16 = 62; // single-column pickers
const MODAL_WIDTH_PROMPT: u16 = 66; // confirms and small dialogs
const MODAL_WIDTH_FORM: u16 = 80; // wizards, forms, browsers, list tables
const MODAL_WIDTH_WIDE: u16 = 90; // reading surfaces and column tables

pub(crate) fn modal_area(area: Rect, kind: &ModalKind) -> Rect {
    match kind {
        ModalKind::Detail(crate::tui::event::DetailModal::ToolDetail { .. })
        | ModalKind::Detail(crate::tui::event::DetailModal::BlockDetail { .. })
        | ModalKind::Detail(crate::tui::event::DetailModal::PlanFindingDetail { .. })
        | ModalKind::Detail(crate::tui::event::DetailModal::DiffPreview { .. })
        | ModalKind::Detail(crate::tui::event::DetailModal::Context(_))
        | ModalKind::Detail(crate::tui::event::DetailModal::Episodes { .. }) => {
            centered_rect(area, MODAL_WIDTH_WIDE, 80)
        }
        ModalKind::Detail(crate::tui::event::DetailModal::PerfReport { .. }) => {
            centered_rect(area, MODAL_WIDTH_FORM, 62)
        }
        // First-run setup owns the whole screen: nothing behind it matters
        // yet, and the growing bonsai deserves the canvas.
        ModalKind::Wizard(crate::tui::event::WizardModal::Onboarding { .. }) => area,
        ModalKind::Detail(crate::tui::event::DetailModal::QuestionPrompt { .. }) => {
            question_prompt_area(area)
        }
        ModalKind::Detail(crate::tui::event::DetailModal::PermissionPrompt { .. })
        | ModalKind::Detail(crate::tui::event::DetailModal::SandboxEscalationPrompt { .. })
        | ModalKind::Detail(crate::tui::event::DetailModal::WebDomainPrompt { .. })
        | ModalKind::Detail(crate::tui::event::DetailModal::ExtensionToolPrompt { .. })
        | ModalKind::Detail(crate::tui::event::DetailModal::HookTrustPrompt { .. }) => {
            centered_rect_capped(area, MODAL_WIDTH_PROMPT, 90, 12)
        }
        ModalKind::Detail(crate::tui::event::DetailModal::CommandHelp) => {
            centered_rect(area, MODAL_WIDTH_WIDE, 82)
        }
        ModalKind::Picker(crate::tui::event::PickerModal::AuthorizeProviderPicker {
            providers,
            ..
        }) => {
            // 2 frame + 1 search header + N provider rows (+2 picker padding)
            // + 2 footer.
            let rows = (providers.len().max(1) as u16).saturating_add(7);
            centered_rect_capped(area, MODAL_WIDTH_PICKER, 58, rows)
        }
        ModalKind::Picker(crate::tui::event::PickerModal::UnauthorizeProviderPicker {
            providers,
            ..
        }) => {
            // Same footprint as the authorize picker.
            let rows = (providers.len().max(1) as u16).saturating_add(7);
            centered_rect_capped(area, MODAL_WIDTH_PICKER, 58, rows)
        }
        ModalKind::Confirm(crate::tui::event::ConfirmModal::Unauthorize { .. }) => {
            centered_rect_capped(area, MODAL_WIDTH_PROMPT, 36, 8)
        }
        ModalKind::Picker(crate::tui::event::PickerModal::ModelPicker { .. })
        | ModalKind::Manager(crate::tui::event::ManagerModal::ProviderManager { .. }) => {
            centered_rect(area, MODAL_WIDTH_WIDE, 76)
        }
        ModalKind::Manager(crate::tui::event::ManagerModal::McpServers { .. }) => {
            centered_rect(area, MODAL_WIDTH_WIDE, 76)
        }
        ModalKind::Picker(crate::tui::event::PickerModal::SessionPicker { .. }) => {
            centered_rect(area, MODAL_WIDTH_FORM, 60)
        }
        ModalKind::Picker(crate::tui::event::PickerModal::PlanPicker { .. }) => {
            centered_rect(area, MODAL_WIDTH_FORM, 60)
        }
        ModalKind::Picker(crate::tui::event::PickerModal::PlanOpenChoice { .. }) => {
            centered_rect_capped(area, MODAL_WIDTH_PROMPT, 38, 7)
        }
        ModalKind::Picker(crate::tui::event::PickerModal::StartPlanChoice { .. }) => {
            centered_rect_capped(area, MODAL_WIDTH_PROMPT, 38, 7)
        }
        ModalKind::Picker(crate::tui::event::PickerModal::BudgetWarning { .. }) => {
            centered_rect_capped(area, MODAL_WIDTH_FORM, 48, 13)
        }
        ModalKind::Confirm(crate::tui::event::ConfirmModal::PlanDelete { .. }) => {
            centered_rect_capped(area, MODAL_WIDTH_PROMPT, 36, 8)
        }
        ModalKind::Confirm(crate::tui::event::ConfirmModal::SessionDelete { .. }) => {
            centered_rect_capped(area, MODAL_WIDTH_PROMPT, 36, 8)
        }
        ModalKind::Confirm(crate::tui::event::ConfirmModal::PlanDiscard { .. }) => {
            centered_rect_capped(area, MODAL_WIDTH_PROMPT, 36, 8)
        }
        ModalKind::Picker(crate::tui::event::PickerModal::ReviewScopePicker { .. }) => {
            // 2 frame + N scope rows (+2 picker padding) + 2 footer.
            let rows = (crate::agent::ReviewScope::all().len() as u16).saturating_add(6);
            centered_rect_capped(area, MODAL_WIDTH_PICKER, 44, rows)
        }
        ModalKind::Wizard(crate::tui::event::WizardModal::LocalModelWizard { .. }) => {
            centered_rect(area, MODAL_WIDTH_FORM, 68)
        }
        ModalKind::Manager(crate::tui::event::ManagerModal::AgentBrowser { .. }) => {
            centered_rect(area, MODAL_WIDTH_FORM, 66)
        }
        ModalKind::Manager(crate::tui::event::ManagerModal::SkillManager { .. }) => {
            centered_rect(area, MODAL_WIDTH_WIDE, 80)
        }
        ModalKind::Manager(crate::tui::event::ManagerModal::ProviderDetail { .. }) => {
            centered_rect(area, MODAL_WIDTH_WIDE, 80)
        }
        ModalKind::Manager(crate::tui::event::ManagerModal::MemoryManager { .. }) => {
            centered_rect(area, MODAL_WIDTH_WIDE, 80)
        }
        ModalKind::Manager(crate::tui::event::ManagerModal::PermissionsManager { .. }) => {
            centered_rect(area, MODAL_WIDTH_WIDE, 80)
        }
        ModalKind::Wizard(crate::tui::event::WizardModal::MemoryAddWizard { .. }) => {
            centered_rect(area, MODAL_WIDTH_WIDE, 80)
        }
        ModalKind::Manager(crate::tui::event::ManagerModal::Settings { .. }) => area,
        ModalKind::Wizard(crate::tui::event::WizardModal::AgentComposer { .. }) => {
            centered_rect(area, MODAL_WIDTH_FORM, 72)
        }
        ModalKind::Confirm(crate::tui::event::ConfirmModal::AgentDelete { .. }) => {
            centered_rect_capped(area, MODAL_WIDTH_PROMPT, 34, 7)
        }
        ModalKind::Manager(crate::tui::event::ManagerModal::SandboxStatus { .. }) => {
            centered_rect_capped(area, MODAL_WIDTH_PROMPT, 52, 16)
        }
        ModalKind::Manager(crate::tui::event::ManagerModal::Doctor { report, .. }) => {
            // 2 frame + 2 header + N check rows + 2 footer, capped so a long
            // check list scrolls internally instead of filling the screen.
            let rows = (report.checks.len().max(1) as u16).saturating_add(6);
            centered_rect_capped(area, MODAL_WIDTH_WIDE, 82, rows)
        }
        ModalKind::Picker(crate::tui::event::PickerModal::ThemePicker { .. }) => {
            let rows_height = (theme::theme_count().max(1) as u16).saturating_add(8);
            centered_rect_capped(area, MODAL_WIDTH_PICKER, 42, rows_height)
        }
        ModalKind::Picker(crate::tui::event::PickerModal::ModePicker { rows, .. }) => {
            // 2 frame + N rows (+2 picker padding) + 2 footer.
            let rows_height = (rows.len().max(1) as u16).saturating_add(6);
            centered_rect_capped(area, MODAL_WIDTH_PICKER, 52, rows_height)
        }
        ModalKind::Detail(crate::tui::event::DetailModal::BusyCommand { rows, .. }) => {
            let rows_height = (rows.len().max(1) as u16).saturating_add(6);
            centered_rect_capped(area, MODAL_WIDTH_PROMPT, 44, rows_height)
        }
        ModalKind::Manager(crate::tui::event::ManagerModal::TaskList { .. })
        | ModalKind::Manager(crate::tui::event::ManagerModal::SubtaskList { .. })
        | ModalKind::Detail(crate::tui::event::DetailModal::Refresh { .. }) => {
            centered_rect(area, MODAL_WIDTH_WIDE, 82)
        }
        ModalKind::Detail(crate::tui::event::DetailModal::UsageDashboard { .. }) => {
            centered_rect(area, MODAL_WIDTH_WIDE, 82)
        }
        ModalKind::Manager(crate::tui::event::ManagerModal::PeerList { peers, .. }) => {
            // 2 frame + N peer rows + 2 footer, capped so a crowded project
            // scrolls internally instead of filling the screen.
            let rows = (peers.len().max(1) as u16).saturating_add(4);
            centered_rect_capped(area, MODAL_WIDTH_FORM, 66, rows)
        }
        // Small dialogs with no dedicated arm: API-key prompt, provider remove
        // confirm, and help.
        _ => centered_rect(area, MODAL_WIDTH_PROMPT, 48),
    }
}

pub(crate) fn max_modal_scroll(app: &AppState, area: Rect) -> Option<u16> {
    let kind = app.modal.as_ref()?;
    let modal = modal_area(area, kind);
    match kind {
        ModalKind::Detail(crate::tui::event::DetailModal::Context(_)) => {
            context_modal_metrics(app, area).map(|metrics| metrics.max_scroll)
        }
        ModalKind::Detail(crate::tui::event::DetailModal::Episodes { report, cursor }) => {
            Some(max_episode_detail_scroll(
                modal,
                &report.episodes,
                (*cursor).min(report.episodes.len().saturating_sub(1)),
            ))
        }
        ModalKind::Detail(crate::tui::event::DetailModal::PerfReport { lines, .. }) => {
            Some(perf_report_max_scroll(modal, lines))
        }
        ModalKind::Detail(crate::tui::event::DetailModal::ToolDetail { tool_id }) => {
            app.tool_activity(tool_id).map(|activity| {
                max_tool_detail_scroll(modal, activity, app.subagent_model_for(tool_id))
            })
        }
        ModalKind::Detail(crate::tui::event::DetailModal::DiffPreview { tool_id }) => app
            .tool_activity(tool_id)
            .and_then(|activity| activity.diff.as_ref())
            .map(|diff| max_diff_preview_scroll(modal, diff)),
        ModalKind::Detail(crate::tui::event::DetailModal::QuestionPrompt {
            prompt,
            origin,
            options,
            multiple,
            cursor,
            ..
        }) => Some(
            question_prompt_metrics(
                modal,
                prompt,
                origin.as_deref(),
                options,
                *multiple,
                *cursor,
            )
            .max_scroll,
        ),
        ModalKind::Detail(crate::tui::event::DetailModal::PermissionPrompt {
            command,
            origin,
            ..
        }) => Some(confirm_prompt_max_scroll(
            modal,
            &permission_prompt(command, origin.as_deref()),
        )),
        ModalKind::Detail(crate::tui::event::DetailModal::SandboxEscalationPrompt {
            command,
            origin,
            kind,
            ..
        }) => Some(confirm_prompt_max_scroll(
            modal,
            &sandbox_escalation_prompt(command, origin.as_deref(), *kind),
        )),
        ModalKind::Detail(crate::tui::event::DetailModal::WebDomainPrompt {
            url,
            host,
            redirected_from,
            origin,
            ..
        }) => Some(confirm_prompt_max_scroll(
            modal,
            &web_domain_prompt(host, url, redirected_from.as_deref(), origin.as_deref()),
        )),
        ModalKind::Detail(crate::tui::event::DetailModal::ExtensionToolPrompt {
            id,
            server,
            capabilities,
            args_preview,
            ..
        }) => Some(confirm_prompt_max_scroll(
            modal,
            &extension_tool_prompt(id, server, capabilities, args_preview),
        )),
        ModalKind::Detail(crate::tui::event::DetailModal::HookTrustPrompt {
            name,
            event,
            action_kind,
            action_preview,
            ..
        }) => Some(confirm_prompt_max_scroll(
            modal,
            &hook_trust_prompt(name, event, action_kind, action_preview),
        )),
        ModalKind::Manager(crate::tui::event::ManagerModal::TaskList { tasks, cursor }) => Some(
            max_task_detail_scroll(modal, tasks, (*cursor).min(tasks.len().saturating_sub(1))),
        ),
        ModalKind::Detail(crate::tui::event::DetailModal::Refresh {
            sources, cursor, ..
        }) => Some(max_refresh_detail_scroll(
            modal,
            sources,
            (*cursor).min(sources.len().saturating_sub(1)),
        )),
        ModalKind::Manager(crate::tui::event::ManagerModal::SubtaskList {
            subtasks,
            cursor,
            ..
        }) => Some(max_subtask_detail_scroll(
            app,
            modal,
            subtasks,
            (*cursor).min(subtasks.len().saturating_sub(1)),
        )),
        ModalKind::Manager(crate::tui::event::ManagerModal::SkillManager { rows, cursor }) => Some(
            skill_detail_max_scroll(modal, rows, (*cursor).min(rows.len().saturating_sub(1))),
        ),
        ModalKind::Manager(crate::tui::event::ManagerModal::ProviderDetail { detail }) => {
            Some(provider_detail_max_scroll(modal, detail))
        }
        ModalKind::Manager(crate::tui::event::ManagerModal::MemoryManager { rows, cursor }) => {
            Some(memory_detail_max_scroll(
                modal,
                rows,
                (*cursor).min(rows.len().saturating_sub(1)),
            ))
        }
        ModalKind::Wizard(crate::tui::event::WizardModal::MemoryAddWizard { state }) => {
            Some(memory_wizard_max_scroll(modal, state))
        }
        ModalKind::Manager(crate::tui::event::ManagerModal::McpServers { rows, cursor }) => Some(
            mcp_detail_max_scroll(modal, rows, (*cursor).min(rows.len().saturating_sub(1))),
        ),
        ModalKind::Detail(crate::tui::event::DetailModal::UsageDashboard { dashboard, tab }) => {
            Some(usage_dashboard_max_scroll(modal, dashboard, *tab))
        }
        ModalKind::Manager(crate::tui::event::ManagerModal::SandboxStatus { .. }) => None,
        _ => None,
    }
}

fn memory_detail_max_scroll(
    area: Rect,
    rows: &[crate::memory::entry::MemoryEntry],
    cursor: usize,
) -> u16 {
    let Some(row) = rows.get(cursor.min(rows.len().saturating_sub(1))) else {
        return 0;
    };
    let (_, detail_area, _) = super::list_detail::list_detail_regions(
        area,
        super::list_detail::ListDetailSplit::Horizontal,
    );
    if detail_area.width == 0 || detail_area.height == 0 {
        return 0;
    }
    detail_max_scroll(
        super::list_detail::detail_pane_inner(detail_area),
        &super::memory::memory_detail_lines(row),
    )
}

fn memory_wizard_max_scroll(
    area: Rect,
    state: &crate::tui::memory_manager::MemoryAddWizardState,
) -> u16 {
    let (_, detail_area, _) = super::list_detail::list_detail_regions(
        area,
        super::list_detail::ListDetailSplit::Horizontal,
    );
    if detail_area.width == 0 || detail_area.height == 0 {
        return 0;
    }
    detail_max_scroll(
        super::list_detail::detail_pane_inner(detail_area),
        &super::memory::wizard_preview_lines(state),
    )
}

pub(crate) fn context_modal_metrics(app: &AppState, area: Rect) -> Option<ContextModalMetrics> {
    let Some(ModalKind::Detail(crate::tui::event::DetailModal::Context(report))) =
        app.modal.as_ref()
    else {
        return None;
    };
    let modal = modal_area(area, app.modal.as_ref()?);
    let (body_area, _footer_area) = context_modal_regions(modal);
    let body_height = body_area.height.max(1);
    // All metrics are in *visual* (wrapped) rows — the same space the body
    // paragraph scrolls in. The row walkers count logical lines; the shared
    // offsets table converts, so a wrapped preview line can no longer make
    // auto-follow under-scroll and lose the cursor below the fold.
    let bar_width = (modal.width.saturating_sub(4) as usize).max(10);
    let lines = context_modal_lines(app, report, bar_width);
    let offsets = visual_line_offsets(&lines, body_area.width.max(1) as usize);
    let total_visual = offsets.last().copied().unwrap_or(0);
    let max_scroll = saturating_u16(total_visual.saturating_sub(body_height as usize));
    let visual_at = |logical: usize| offsets.get(logical).copied().unwrap_or(total_visual);
    let span = context_selected_span(app, report);
    Some(ContextModalMetrics {
        body_height,
        max_scroll,
        selected_line: span.map(|(start, _)| saturating_u16(visual_at(start))),
        selected_block_end: span.map(|(_, end)| saturating_u16(visual_at(end))),
    })
}

pub(crate) fn context_row_index_at(
    app: &AppState,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<usize> {
    let Some(ModalKind::Detail(crate::tui::event::DetailModal::Context(report))) =
        app.modal.as_ref()
    else {
        return None;
    };
    let modal = modal_area(area, app.modal.as_ref()?);
    let (content, _footer_area) = context_modal_regions(modal);
    if !content.contains((column, row).into()) {
        return None;
    }

    // The click lands in visual (wrapped) rows; convert to the logical line
    // the row walkers count in. Any segment of a wrapped line maps back to
    // that line's row.
    let target_visual =
        usize::from(app.modal_scroll).saturating_add(usize::from(row.saturating_sub(content.y)));
    let bar_width = (modal.width.saturating_sub(4) as usize).max(10);
    let lines = context_modal_lines(app, report, bar_width);
    let offsets = visual_line_offsets(&lines, content.width.max(1) as usize);
    let total_visual = offsets.last().copied().unwrap_or(0);
    if target_visual >= total_visual {
        return None;
    }
    let target_line = match offsets.binary_search(&target_visual) {
        Ok(index) => index.min(lines.len().saturating_sub(1)),
        Err(index) => index.saturating_sub(1),
    };
    if app.context_state.view_mode.is_wire() {
        return context_wire_row_index_at(app, report, target_line);
    }
    if app.context_state.view_mode.is_turns() {
        return context_turns_row_index_at(app, report, target_line);
    }
    let header = context_header_line_count(app, report);
    target_line
        .checked_sub(header)
        .and_then(|line| context_ledger_row_index_at(app, report, line))
}

/// A single dim message inside a titled frame — the "no longer available" /
/// "empty" placeholder every detail modal shows when its subject is gone.
pub(super) fn render_modal_notice(f: &mut Frame, area: Rect, title: &str, message: &str) {
    f.render_widget(
        Paragraph::new(vec![Line::from(Span::styled(
            message.to_string(),
            theme::dim(),
        ))])
        .block(theme::frame(title, true))
        .style(theme::panel()),
        area,
    );
}

/// The header / body / footer split of a scrollable modal. The header is
/// dropped (a zero rect) when the modal is too short to fit both it and the
/// footer, so the body keeps everything above the pinned footer.
pub(crate) struct ScrollableRegions {
    pub header: Rect,
    pub body: Rect,
    pub footer: Rect,
}

/// The single source of the footer-height and 3-vs-2-region math. Both
/// [`render_scrollable_modal`] and the scroll-metric helpers derive their
/// geometry here so the clamp can never disagree with what is drawn.
pub(crate) fn split_scrollable_modal(
    area: Rect,
    header_line_count: usize,
    footer_line_count: usize,
) -> ScrollableRegions {
    let inner = theme::frame(String::new(), true).inner(area);
    // Footer height is the number of footer lines plus a blank spacer.
    let footer_height = (footer_line_count as u16 + 1).max(1);
    if inner.height > footer_height + 1 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(header_line_count as u16),
                Constraint::Min(1),
                Constraint::Length(footer_height),
            ])
            .split(inner);
        ScrollableRegions {
            header: chunks[0],
            body: chunks[1],
            footer: chunks[2],
        }
    } else {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(footer_height)])
            .split(inner);
        ScrollableRegions {
            header: Rect::default(),
            body: chunks[0],
            footer: chunks[1],
        }
    }
}

/// Wrapped-row variant used by prompt renderers whose header/footer text can
/// span multiple rows on narrow terminals.
pub(crate) fn split_scrollable_modal_lines(
    area: Rect,
    header_lines: &[Line<'static>],
    footer_lines: &[Line<'static>],
) -> ScrollableRegions {
    let inner = theme::frame(String::new(), true).inner(area);
    let width = inner.width.max(1) as usize;
    let header_rows = wrapped_lines_height(header_lines, width);
    let footer_rows = wrapped_lines_height(footer_lines, width);
    split_scrollable_modal(area, header_rows, footer_rows)
}

fn wrapped_lines_height(lines: &[Line<'static>], width: usize) -> usize {
    lines
        .iter()
        .map(|line| usize::from(wrapped_line_count(line, width)))
        .sum()
}

/// Max scroll of a detail pane: wrapped rows of `lines` at `inner.width` minus
/// `inner.height`. Callers pass the *same* lines they render, so the clamp
/// tracks the drawn content exactly.
pub(super) fn detail_max_scroll(inner: Rect, lines: &[Line<'static>]) -> u16 {
    let wrap_width = inner.width.max(1) as usize;
    let body_height = inner.height.max(1) as usize;
    let total_rows = lines
        .iter()
        .map(|line| wrapped_line_count(line, wrap_width) as usize)
        .sum::<usize>();
    total_rows.saturating_sub(body_height) as u16
}

/// Render a modal split into a fixed header, a scrollable body, and a fixed
/// footer (always visible). Long content scrolls; the action hints stay
/// pinned to the bottom so they can't be pushed off-screen by a tall command
/// or a long question.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_scrollable_modal(
    f: &mut Frame,
    area: Rect,
    title: &str,
    header_lines: &[Line<'static>],
    body_lines: &[Line<'static>],
    footer_lines: &[Line<'static>],
    modal_scroll: u16,
    selection: Option<(usize, usize)>,
    body_cache: &std::cell::RefCell<Vec<Line<'static>>>,
    body_rect_cache: &std::cell::Cell<Option<Rect>>,
) {
    let frame = theme::frame(title.to_string(), true);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(Vec::new())
            .block(frame)
            .style(theme::panel()),
        area,
    );

    let regions = split_scrollable_modal_lines(area, header_lines, footer_lines);
    let body_area = regions.body;

    if regions.header.height > 0 {
        let header = Paragraph::new(header_lines.to_vec())
            .style(theme::panel())
            .wrap(Wrap { trim: false });
        f.render_widget(header, regions.header);
    }

    // Cache the unwrapped body lines and body rect so the mouse handler can
    // resolve screen coordinates to grapheme offsets without re-generating
    // content or recomputing the layout.
    *body_cache.borrow_mut() = body_lines.to_vec();
    body_rect_cache.set(Some(body_area));

    // The body viewport accounts for wrap width so we can compute the real
    // line count after wrapping and clamp the scroll.
    let wrap_width = body_area.width.max(1) as usize;
    let body_height = body_area.height.max(1) as usize;
    // Apply selection highlighting before wrapping.
    let highlighted: Vec<Line<'static>>;
    let render_lines = if let Some((start, end)) = selection.filter(|(s, e)| s != e) {
        highlighted = modal_selection_highlight(body_lines, start, end);
        &highlighted
    } else {
        body_lines
    };
    let mut wrapped: Vec<Line<'static>> = Vec::new();
    for line in render_lines {
        if line.width() <= wrap_width {
            wrapped.push(line.clone());
        } else {
            wrapped.extend(wrap_line(line, wrap_width));
        }
    }
    let max_scroll = wrapped.len().saturating_sub(body_height) as u16;
    let scroll = modal_scroll.min(max_scroll);

    let body = Paragraph::new(wrapped)
        .style(theme::panel())
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(body, body_area);

    if max_scroll > 0 {
        let scrollbar_area = body_area.inner(Margin {
            vertical: 0,
            horizontal: 0,
        });
        let mut state = ScrollbarState::new(max_scroll as usize + 1)
            .position(scroll as usize)
            .viewport_content_length(body_height);
        let scrollbar = theme::scrollbar(ScrollbarOrientation::VerticalRight);
        f.render_stateful_widget(scrollbar, scrollbar_area, &mut state);
    }

    let footer = Paragraph::new(footer_lines.to_vec())
        .style(theme::panel())
        .wrap(Wrap { trim: false });
    f.render_widget(footer, regions.footer);
}

pub(super) fn scrollable_modal_max_scroll(
    area: Rect,
    header_line_count: usize,
    footer_line_count: usize,
    body_lines: &[Line<'static>],
) -> u16 {
    let body_area = split_scrollable_modal(area, header_line_count, footer_line_count).body;
    detail_max_scroll(body_area, body_lines)
}

/// One modal footer hint line from `(key, label)` pairs, in the shared style:
/// key names in body text, labels dimmed, pairs separated by two spaces —
/// `Enter select  Esc close`. Every modal footer should render through this so
/// the hint chrome cannot drift between modals.
pub(super) fn footer_hint_line(pairs: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::with_capacity(pairs.len() * 2);
    for (index, (key, label)) in pairs.iter().enumerate() {
        spans.push(Span::styled(
            key.to_string(),
            theme::body(theme::palette().text),
        ));
        let separator = if index + 1 < pairs.len() { "  " } else { "" };
        spans.push(Span::styled(format!(" {label}{separator}"), theme::dim()));
    }
    Line::from(spans)
}

/// The shared styled section label for modal reading surfaces. The accent bar
/// keeps section boundaries scannable when a modal stacks several sections
/// (arguments, authorization, changes, result) above dense body text.
pub(super) fn section_header(text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("▎ ", theme::body(theme::palette().tool)),
        Span::styled(text.to_string(), theme::label(theme::palette().tool)),
    ])
}

pub(super) fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![line.clone()];
    }
    use unicode_width::UnicodeWidthChar;
    let mut out = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_width = 0usize;
    for span in &line.spans {
        let text = span.content.as_ref();
        let mut remaining: &str = text;
        while !remaining.is_empty() {
            let available = width.saturating_sub(current_width);
            if available == 0 {
                out.push(Line::from(std::mem::take(&mut current_spans)));
                current_width = 0;
                continue;
            }
            let mut split_byte = remaining.len();
            let mut split_width = 0usize;
            for (byte_idx, ch) in remaining.char_indices() {
                let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
                if split_width + ch_width > available {
                    break;
                }
                split_width += ch_width;
                split_byte = byte_idx + ch.len_utf8();
                if split_width == available {
                    break;
                }
            }
            if split_byte == 0 {
                break;
            }
            let (chunk, rest) = remaining.split_at(split_byte);
            current_spans.push(Span::styled(chunk.to_string(), span.style));
            current_width += split_width;
            remaining = rest;
            if !remaining.is_empty() {
                out.push(Line::from(std::mem::take(&mut current_spans)));
                current_width = 0;
            }
        }
    }
    if !current_spans.is_empty() {
        out.push(Line::from(current_spans));
    }
    if out.is_empty() {
        out.push(Line::from(""));
    }
    out
}

pub(crate) fn question_prompt_metrics(
    area: Rect,
    prompt: &str,
    origin: Option<&str>,
    options: &[crate::interaction::QuestionOption],
    multiple: bool,
    cursor: usize,
) -> QuestionPromptMetrics {
    let header_lines = question_prompt_header_lines(prompt, origin);
    let footer_lines = question_prompt_footer_lines(multiple);
    let body_area = split_scrollable_modal_lines(area, &header_lines, &footer_lines).body;
    let wrap_width = body_area.width.max(1) as usize;
    let body_height = body_area.height.max(1);

    // Measure the exact lines the render draws. The checkbox glyph is a
    // constant four cells whether checked or not, so an empty `selected` yields
    // identical wrapped-row geometry — no need to thread selection through here.
    let body = question_prompt_body(options, multiple, cursor, &[]);
    // Cumulative wrapped-row offset at the start of each logical line, plus the
    // grand total as the last entry.
    let mut wrapped_at = Vec::with_capacity(body.lines.len() + 1);
    let mut total_rows = 0u16;
    for line in &body.lines {
        wrapped_at.push(total_rows);
        total_rows = total_rows.saturating_add(wrapped_line_count(line, wrap_width));
    }
    wrapped_at.push(total_rows);

    let start_logical = body.option_offsets.get(cursor).copied().unwrap_or(0);
    let end_logical = body
        .option_offsets
        .get(cursor + 1)
        .copied()
        .unwrap_or(body.lines.len());
    let selected_start = wrapped_at.get(start_logical).copied().unwrap_or(0);
    let selected_end = wrapped_at
        .get(end_logical)
        .copied()
        .unwrap_or(total_rows)
        .max(selected_start.saturating_add(1));

    QuestionPromptMetrics {
        body_height,
        max_scroll: total_rows.saturating_sub(body_height),
        selected_start,
        selected_end,
    }
}

pub(super) fn wrapped_line_count(line: &Line<'static>, width: usize) -> u16 {
    if line.width() <= width {
        1
    } else {
        wrap_line(line, width).len().max(1) as u16
    }
}

pub(super) fn centered_rect(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

/// Floor so a content-sized modal never collapses to an unreadable sliver.
pub(super) const MIN_MODAL_HEIGHT: u16 = 5;
pub(super) const MIN_MODAL_WIDTH: u16 = 36;

/// Like [`centered_rect`], but the height tracks the content instead of the
/// terminal: it grows to fit `content_rows` yet never exceeds `percent_y` of
/// the area (the cap). Short dialogs stop getting tall vertical padding while
/// long ones stay capped and scroll internally. Width keeps the percentage
/// behaviour so the horizontal floor tuning still applies.
pub(super) fn centered_rect_capped(
    area: Rect,
    percent_x: u16,
    percent_y: u16,
    content_rows: u16,
) -> Rect {
    let cap = ((u32::from(area.height) * u32::from(percent_y)) / 100) as u16;
    let cap = cap.min(area.height).max(MIN_MODAL_HEIGHT.min(area.height));
    let height = content_rows.clamp(MIN_MODAL_HEIGHT.min(cap), cap);

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(area.height.saturating_sub(height) / 2),
            Constraint::Length(height),
            Constraint::Min(0),
        ])
        .split(area);

    let percentage_width = ((u32::from(area.width) * u32::from(percent_x)) / 100) as u16;
    let width = percentage_width
        .max(MIN_MODAL_WIDTH.min(area.width))
        .min(area.width);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(area.width.saturating_sub(width) / 2),
            Constraint::Length(width),
            Constraint::Min(0),
        ])
        .split(vertical[1]);
    horizontal[1]
}

pub(crate) fn question_prompt_area(area: Rect) -> Rect {
    centered_rect_capped(area, 64, 90, 14)
}

// ── Modal text selection helpers ──────────────────────────────────────────

/// Extract the plain text content of a `Vec<Line>`, joining lines with `\n`.
pub(crate) fn modal_body_plain_text(lines: &[Line<'static>]) -> String {
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        for span in &line.spans {
            out.push_str(span.content.as_ref());
        }
    }
    out
}

/// Resolve a screen `(column, row)` to a flat grapheme offset into the plain
/// text of the unwrapped `body_lines`, accounting for the modal frame chrome,
/// scroll offset, and soft-wrapping at `body_width`.
///
/// Returns `None` when the point is outside the body text area.
pub(crate) fn modal_resolve_position(
    column: u16,
    row: u16,
    body_lines: &[Line<'static>],
    scroll: u16,
    body_area: Rect,
) -> Option<usize> {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    if body_area.width == 0 || body_area.height == 0 {
        return None;
    }
    if column < body_area.x || row < body_area.y || row >= body_area.bottom() {
        return None;
    }
    let wrap_width = body_area.width as usize;
    if wrap_width == 0 {
        return None;
    }
    let click_row_in_viewport = (row - body_area.y) as usize;
    let target_row = click_row_in_viewport + scroll as usize;
    let col_in_body = (column - body_area.x) as usize;

    let mut flat_offset: usize = 0;
    let mut wrapped_row = 0usize;
    for (line_idx, line) in body_lines.iter().enumerate() {
        if line_idx > 0 {
            // The newline between logical lines is a single grapheme in
            // the plain text but does NOT occupy a visual row in ratatui's
            // Paragraph.  Count it in flat_offset only.
            flat_offset += 1;
        }
        let line_text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        let line_graphemes: Vec<&str> = line_text.graphemes(true).collect();
        if line_graphemes.is_empty() {
            // An empty logical line still occupies one visual row on screen,
            // so it must consume a wrapped row here or every offset below a
            // blank line lands one row off.
            if wrapped_row == target_row {
                return Some(flat_offset);
            }
            wrapped_row += 1;
            continue;
        }
        // Walk the line in wrap_width-wide rows.
        let mut col = 0usize;
        let mut seg_start = 0usize;
        for (gi, g) in line_graphemes.iter().enumerate() {
            let gw = g.width();
            if col > 0 && col + gw > wrap_width {
                // This grapheme starts a new wrapped row.
                if wrapped_row == target_row {
                    let local_col = col_in_body.min(col);
                    return Some(flat_offset + seg_start + local_col);
                }
                wrapped_row += 1;
                seg_start = gi;
                col = 0;
            }
            col += gw;
        }
        if wrapped_row == target_row {
            let local_col = col_in_body.min(col);
            return Some(flat_offset + seg_start + local_col);
        }
        wrapped_row += 1;
        flat_offset += line_graphemes.len();
    }
    None
}

/// Build a selection-highlighted copy of `body_lines` where graphemes in
/// `[sel_start, sel_end)` are painted with the selection background.
pub(crate) fn modal_selection_highlight(
    body_lines: &[Line<'static>],
    sel_start: usize,
    sel_end: usize,
) -> Vec<Line<'static>> {
    use unicode_segmentation::UnicodeSegmentation;

    let sel_bg = crate::tui::theme::palette().selection_bg;
    let base_style = crate::tui::theme::panel();

    let mut flat_offset: usize = 0;
    let mut result = Vec::with_capacity(body_lines.len());

    for (line_idx, line) in body_lines.iter().enumerate() {
        if line_idx > 0 {
            flat_offset += 1; // '\n' grapheme between lines
        }
        // Collect all graphemes in this line with their span style.
        let mut graphemes: Vec<(String, ratatui::style::Style)> = Vec::new();
        for span in &line.spans {
            let text: &str = span.content.as_ref();
            for g in text.graphemes(true) {
                graphemes.push((g.to_string(), span.style));
            }
        }
        if graphemes.is_empty() {
            result.push(line.clone());
            continue;
        }

        // Build highlighted spans.
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut current_text = String::new();
        let mut current_style: Option<ratatui::style::Style> = None;
        for (gi, (g, base)) in graphemes.iter().enumerate() {
            let g_offset = flat_offset + gi;
            let highlighted = g_offset >= sel_start && g_offset < sel_end;
            // Selection swaps the background only, keeping the span's fg so
            // colored content (diff rows, syntax accents) stays readable.
            let style = if highlighted { base.bg(sel_bg) } else { *base };
            if current_style.as_ref() != Some(&style) {
                if !current_text.is_empty() {
                    spans.push(Span::styled(
                        std::mem::take(&mut current_text),
                        current_style.unwrap_or(base_style),
                    ));
                }
                current_text.push_str(g);
                current_style = Some(style);
            } else {
                current_text.push_str(g);
            }
        }
        if !current_text.is_empty() {
            spans.push(Span::styled(
                current_text,
                current_style.unwrap_or(base_style),
            ));
        }
        result.push(Line::from(spans));
        flat_offset += graphemes.len();
    }
    result
}

/// Extract the substring of `plain_text` between grapheme offsets `[start, end)`.
pub(crate) fn modal_text_range(plain_text: &str, start: usize, end: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    plain_text
        .graphemes(true)
        .skip(start)
        .take(end - start)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_scrollable_modal_tiles_header_body_footer_on_a_tall_modal() {
        let area = Rect::new(0, 0, 40, 20);
        let regions = split_scrollable_modal(area, 2, 1);
        assert_eq!(regions.header.height, 2);
        assert_eq!(regions.footer.height, 2, "1 footer line + a blank spacer");
        assert!(regions.body.height >= 1);
        // The three regions tile the inner rect top to bottom with no gap.
        assert_eq!(regions.header.y + regions.header.height, regions.body.y);
        assert_eq!(regions.body.y + regions.body.height, regions.footer.y);
    }

    #[test]
    fn split_scrollable_modal_drops_header_when_too_short() {
        // Inner height barely fits the footer, so the header collapses and the
        // body keeps everything above the pinned footer.
        let area = Rect::new(0, 0, 40, 5);
        let regions = split_scrollable_modal(area, 2, 1);
        assert_eq!(regions.header.height, 0);
        assert_eq!(regions.footer.height, 2);
        assert!(regions.body.height >= 1);
    }

    #[test]
    fn detail_max_scroll_counts_wrapped_rows_not_logical_lines() {
        let inner = Rect::new(0, 0, 10, 3);
        // One 25-cell line wraps to 3 rows at width 10; 3 rows, 3 tall -> 0.
        let short = vec![Line::from("x".repeat(25))];
        assert_eq!(detail_max_scroll(inner, &short), 0);
        // Two such lines wrap to 6 rows; 6 - 3 = 3.
        let long = vec![Line::from("x".repeat(25)), Line::from("y".repeat(25))];
        assert_eq!(detail_max_scroll(inner, &long), 3);
    }

    #[test]
    fn footer_hint_line_emphasizes_keys_and_dims_labels() {
        let line = footer_hint_line(&[("Enter", "select"), ("Esc", "close")]);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Enter select  Esc close");
        // Keys and labels alternate: key span, then " label" (with a trailing
        // two-space gap on every pair but the last).
        assert_eq!(line.spans[0].content.as_ref(), "Enter");
        assert_eq!(line.spans[1].content.as_ref(), " select  ");
        assert_eq!(line.spans[2].content.as_ref(), "Esc");
        assert_eq!(line.spans[3].content.as_ref(), " close");
    }

    #[test]
    fn capped_modal_shrinks_to_content_on_a_tall_terminal() {
        // On a tall terminal the old percentage height (36% of 100 = 36 rows)
        // left a short confirm swimming in padding. Content sizing pins it to
        // the content height instead.
        let area = Rect::new(0, 0, 120, 100);
        let modal = centered_rect_capped(area, 64, 36, 8);
        assert_eq!(modal.height, 8, "height should track content, not the cap");
        // Still vertically centered.
        assert_eq!(modal.y, (100 - 8) / 2);
    }

    #[test]
    fn capped_modal_never_exceeds_the_percentage_cap() {
        // A picker with many rows must not grow past the cap; it scrolls
        // internally instead.
        let area = Rect::new(0, 0, 120, 40);
        let cap = 40 * 44 / 100; // percent_y = 44
        let modal = centered_rect_capped(area, 62, 44, 200);
        assert_eq!(modal.height, cap);
    }

    #[test]
    fn capped_modal_keeps_a_readable_floor_on_a_tiny_terminal() {
        let area = Rect::new(0, 0, 80, 8);
        let modal = centered_rect_capped(area, 64, 36, 2);
        assert!(modal.height >= MIN_MODAL_HEIGHT.min(area.height));
    }

    #[test]
    fn mode_modal_fits_all_rows_at_normal_terminal_height() {
        let area = Rect::new(0, 0, 100, 30);
        let rows = (0..7).map(|_| ModeRow::Header("axis")).collect();
        let kind =
            ModalKind::Picker(crate::tui::event::PickerModal::ModePicker { rows, cursor: 0 });

        assert_eq!(modal_area(area, &kind).height, 13);
    }

    #[test]
    fn settings_modal_uses_full_terminal_area() {
        let area = Rect::new(3, 4, 120, 30);
        let kind = ModalKind::Manager(crate::tui::event::ManagerModal::Settings {
            rows: Vec::new(),
            cursor: 0,
        });

        assert_eq!(modal_area(area, &kind), area);
    }

    #[test]
    fn provider_manager_uses_wide_picker_footprint() {
        let area = Rect::new(0, 0, 120, 50);
        let kind = ModalKind::Manager(crate::tui::event::ManagerModal::ProviderManager {
            rows: Vec::new(),
            filter: String::new(),
            searching: false,
            cursor: 0,
        });

        assert_eq!(modal_area(area, &kind), Rect::new(6, 6, 108, 38));
    }

    // ── Modal text selection tests ────────────────────────────────────────

    #[test]
    fn modal_body_plain_text_joins_lines_with_newline() {
        let lines = vec![Line::from("hello"), Line::from("world")];
        assert_eq!(modal_body_plain_text(&lines), "hello\nworld");
    }

    #[test]
    fn modal_body_plain_text_empty_for_no_lines() {
        assert_eq!(modal_body_plain_text(&[]), "");
    }

    #[test]
    fn modal_body_plain_text_concatenates_spans() {
        let line = Line::from(vec![Span::raw("foo"), Span::raw("bar")]);
        assert_eq!(modal_body_plain_text(&[line]), "foobar");
    }

    #[test]
    fn modal_text_range_extracts_grapheme_slice() {
        let text = "hello world";
        assert_eq!(modal_text_range(text, 0, 5), "hello");
        assert_eq!(modal_text_range(text, 6, 11), "world");
        assert_eq!(modal_text_range(text, 0, 0), "");
    }

    #[test]
    fn modal_resolve_position_returns_none_outside_body() {
        let lines = vec![Line::from("hello")];
        let body = Rect::new(10, 10, 20, 5);
        // Above body
        assert_eq!(modal_resolve_position(15, 9, &lines, 0, body), None);
        // Below body
        assert_eq!(modal_resolve_position(15, 15, &lines, 0, body), None);
        // Left of body
        assert_eq!(modal_resolve_position(9, 12, &lines, 0, body), None);
    }

    #[test]
    fn modal_resolve_position_maps_first_line() {
        let lines = vec![Line::from("abcde")];
        let body = Rect::new(0, 0, 20, 5);
        // Click at column 2 of the first row → grapheme offset 2.
        assert_eq!(modal_resolve_position(2, 0, &lines, 0, body), Some(2));
        // Click at column 0 → offset 0.
        assert_eq!(modal_resolve_position(0, 0, &lines, 0, body), Some(0));
    }

    #[test]
    fn modal_resolve_position_maps_second_line_with_newline() {
        let lines = vec![Line::from("abc"), Line::from("de")];
        let body = Rect::new(0, 0, 20, 5);
        // First line has 3 graphemes + 1 newline = offset 4 is 'd'.
        // Click at row 1, col 0 → flat offset 4.
        assert_eq!(modal_resolve_position(0, 1, &lines, 0, body), Some(4));
        // Click at row 1, col 1 → flat offset 5.
        assert_eq!(modal_resolve_position(1, 1, &lines, 0, body), Some(5));
    }

    #[test]
    fn modal_resolve_position_accounts_for_scroll() {
        let lines = vec![Line::from("first"), Line::from("second")];
        let body = Rect::new(0, 0, 20, 1);
        // With scroll=1, viewport row 0 shows the second line.
        // "first" = 5 graphemes + 1 newline = offset 6 is 's'.
        assert_eq!(modal_resolve_position(0, 0, &lines, 1, body), Some(6));
    }

    #[test]
    fn modal_resolve_position_handles_wrapping() {
        // A 10-char line in a width-5 body wraps to 2 rows.
        let lines = vec![Line::from("0123456789")];
        let body = Rect::new(0, 0, 5, 10);
        // Row 0, col 3 → offset 3.
        assert_eq!(modal_resolve_position(3, 0, &lines, 0, body), Some(3));
        // Row 1, col 0 → offset 5 (start of second wrapped row).
        assert_eq!(modal_resolve_position(0, 1, &lines, 0, body), Some(5));
        // Row 1, col 4 → offset 9.
        assert_eq!(modal_resolve_position(4, 1, &lines, 0, body), Some(9));
    }

    #[test]
    fn modal_resolve_position_counts_blank_lines_as_rows() {
        // Regression: a blank line occupies one visual row, so text below it
        // must not resolve one row early.
        let lines = vec![Line::from("ab"), Line::from(""), Line::from("cd")];
        let body = Rect::new(0, 0, 20, 5);
        // Row 1 is the blank line: resolves to its flat offset (the position
        // right after "ab\n" = 3).
        assert_eq!(modal_resolve_position(0, 1, &lines, 0, body), Some(3));
        // Row 2 col 0 is 'c': "ab" (2) + '\n' + "" (0) + '\n' = offset 4.
        assert_eq!(modal_resolve_position(0, 2, &lines, 0, body), Some(4));
        // Row 2 col 1 is 'd' = offset 5.
        assert_eq!(modal_resolve_position(1, 2, &lines, 0, body), Some(5));
    }

    #[test]
    fn modal_selection_highlight_applies_bg_to_selected_range() {
        let lines = vec![Line::from("hello world")];
        let highlighted = modal_selection_highlight(&lines, 0, 5);
        // The highlighted line should have spans; the first 5 graphemes
        // should carry the selection background.
        assert_eq!(highlighted.len(), 1);
        assert!(!highlighted[0].spans.is_empty());
        // The selection bg should be applied to the first span.
        let sel_bg = crate::tui::theme::palette().selection_bg;
        assert_eq!(highlighted[0].spans[0].style.bg, Some(sel_bg));
    }

    #[test]
    fn modal_selection_highlight_noop_when_range_empty() {
        let lines = vec![Line::from("hello")];
        let highlighted = modal_selection_highlight(&lines, 3, 3);
        // Empty range → no highlighting, spans carry base style.
        let sel_bg = crate::tui::theme::palette().selection_bg;
        for span in &highlighted[0].spans {
            assert_ne!(span.style.bg, Some(sel_bg));
        }
    }

    #[test]
    fn modal_selection_range_orders_anchor_and_caret() {
        let sel = crate::tui::app::ModalSelection {
            anchor: 10,
            caret: 5,
        };
        assert_eq!(sel.range(), (5, 10));
        let sel2 = crate::tui::app::ModalSelection {
            anchor: 3,
            caret: 7,
        };
        assert_eq!(sel2.range(), (3, 7));
    }
}
