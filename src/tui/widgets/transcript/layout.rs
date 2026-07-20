//! Per-item layout cache for the chat transcript — the buffer that keeps long
//! sessions cheap to draw.
//!
//! Without it, every frame re-rendered every transcript item (markdown parse,
//! syntax highlight, wrap, owned `Line` allocations) three times over — scroll
//! clamp, paint, jump pill — and every mouse click or drag once more. The
//! cache renders an item when it first appears or when one of its render
//! inputs changes; every read path (paint, scroll math, hit-testing, focus
//! spans) shares the result and only the viewport's rows are cloned per frame.
//!
//! Correctness rests on three legs:
//!
//! - **Item stamps** ([`ItemStamp`]): every mutation path through
//!   [`crate::tui::transcript::TranscriptModel`] bumps the touched item's
//!   revision — mutable accessors bump pessimistically, so "borrowed mutably"
//!   counts as "changed".
//! - **Render-input keys** ([`ItemKey`]): everything else that can change an
//!   item's rendered lines (selection, focus, group expansion, serenity
//!   reasoning state, inline tool selection) is recomputed per item per frame
//!   and compared against the cached key.
//! - **Animated items**: items whose output depends on time — running tool
//!   spinners and live durations, active serenity reasoning — carry the
//!   current animation tick in their key unless reduced motion is active.
//!   `permission_command` is deliberately NOT a key input:
//!   it only alters items hosting a `Running` tool (`tool_waits_for_permission`
//!   requires `Running`), and those are animated, hence tick-keyed, hence
//!   refreshed next frame anyway. A test pins that finished tools render
//!   identically with and without a pending permission.
//!
//! Global inputs — content width, presentation/accessibility modes, and theme
//! generation — form a fingerprint; when any changes the whole cache drops
//! and rebuilds on the next read.
//!
//! Windowed painting is exact because every line is pre-wrapped to at most
//! `content_width` cells (card mode also pads to exactly that width), so an
//! item's post-wrap row count equals `lines.len()`. [`enforce_line_width`]
//! guards that invariant at fill time.

use std::cell::RefCell;
use std::collections::HashMap;

use super::markdown::rendered_line_width;
use super::message::wrap_card_line;
use super::*;
use crate::tui::transcript::ItemStamp;

/// Interior-mutable cache handle stored on [`AppState`]. Interior mutability
/// is required because the draw path only holds `&AppState`; it is sound
/// because the TUI renders and reduces on a single task, and no borrow
/// escapes a method (see [`TranscriptLayoutCache::with`]).
#[derive(Default)]
pub(crate) struct TranscriptLayoutCache {
    state: RefCell<LayoutState>,
}

#[derive(Default)]
struct LayoutState {
    fingerprint: Option<Fingerprint>,
    /// Index-parallel to `AppState::transcript`.
    items: Vec<ItemLayout>,
    /// Prefix sums of `items[..].rows`; `len == items.len() + 1`, reused
    /// buffer rebuilt on every `ensure`.
    row_offsets: Vec<usize>,
    /// How many per-item renders the cache has performed, for tests asserting
    /// that unchanged frames do zero layout work.
    #[cfg(test)]
    renders: usize,
}

/// Global render inputs; a change to any of them invalidates every item.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Fingerprint {
    width: usize,
    serenity_mode: bool,
    reduced_motion: bool,
    linear_output: bool,
    theme_generation: u64,
}

/// Per-item render inputs. Change-detection only — when a mismatch triggers a
/// re-render, the [`TranscriptRenderOptions`] are rebuilt from `app`, never
/// reconstructed from the key, so the key can never drift from what the
/// renderer actually consumed.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ItemKey {
    rev: u32,
    selected: ItemSelection,
    focused: bool,
    expanded: bool,
    reasoning_active: bool,
    /// The inline tool selection resolved against this item's group id, so
    /// moving the selection from group A to group B invalidates exactly A
    /// and B.
    group_tool_selected: Option<usize>,
    /// The animation tick iff the item renders live state and motion is enabled.
    animated_tick: Option<u64>,
}

struct ItemLayout {
    id: u64,
    /// `None` marks a slot that has never been rendered (fresh or salvaged
    /// placeholder), forcing the first key comparison to miss.
    key: Option<ItemKey>,
    lines: Vec<Line<'static>>,
    rows: usize,
}

impl ItemLayout {
    fn vacant(id: u64) -> Self {
        Self {
            id,
            key: None,
            lines: Vec::new(),
            rows: 0,
        }
    }
}

/// The per-frame answer `render` needs, resolved under a single cache pass.
pub(super) struct ViewWindow {
    pub(super) total_rows: usize,
    pub(super) scroll: u16,
    pub(super) max_scroll: u16,
    pub(super) lines: Vec<Line<'static>>,
}

impl TranscriptLayoutCache {
    /// Card-body width from the most recent layout pass, if one has run.
    pub(crate) fn selection_width(&self) -> Option<usize> {
        self.state.borrow().fingerprint.map(|fingerprint| {
            if fingerprint.linear_output {
                fingerprint.width
            } else {
                fingerprint.width.saturating_sub(4).max(8)
            }
        })
    }

    /// The only borrow point: takes the `RefCell` mutably, refreshes stale
    /// entries, runs `read` against the up-to-date state, and releases the
    /// borrow before returning. Public queries never nest (nothing reachable
    /// from `ensure` or `read` touches the cache again), and every query
    /// returns owned data, so no borrow can leak or panic.
    fn with<R>(&self, app: &AppState, width: usize, read: impl FnOnce(&LayoutState) -> R) -> R {
        let mut state = self.state.borrow_mut();
        ensure(&mut state, app, width);
        read(&state)
    }

    /// Total post-wrap rows of the whole transcript. `0` means "no visible
    /// items" (empty or all suppressed) — callers fall back to the welcome
    /// screen exactly like `transcript_lines` used to.
    pub(super) fn total_rows(&self, app: &AppState, width: usize) -> usize {
        self.with(app, width, |state| state.total_rows())
    }

    /// Scroll resolution plus the visible window's lines in one pass.
    pub(super) fn view_window(&self, app: &AppState, width: usize, viewport: usize) -> ViewWindow {
        self.with(app, width, |state| {
            let total_rows = state.total_rows();
            let (scroll, max_scroll) = resolved_scroll(app, total_rows, viewport);
            ViewWindow {
                total_rows,
                scroll,
                max_scroll,
                lines: state.window(scroll as usize, viewport),
            }
        })
    }

    /// `(item index, row offset within the item)` for an absolute transcript
    /// row, or `None` past the end. Zero-row (suppressed) items are never
    /// returned; the row belongs to the item that actually paints it.
    pub(super) fn locate_row(
        &self,
        app: &AppState,
        width: usize,
        row: usize,
    ) -> Option<(usize, usize)> {
        self.with(app, width, |state| state.locate(row))
    }

    /// `[start, end)` rows occupied by `item_index`, or `None` out of range.
    pub(super) fn item_row_span(
        &self,
        app: &AppState,
        width: usize,
        item_index: usize,
    ) -> Option<(usize, usize)> {
        self.with(app, width, |state| {
            let start = *state.row_offsets.get(item_index)?;
            let end = *state.row_offsets.get(item_index + 1)?;
            Some((start, end))
        })
    }

    /// Every cached line in transcript order. Test/compat surface for
    /// `transcript_lines`; production paints go through [`Self::view_window`]
    /// and clone only the viewport.
    #[cfg(test)]
    pub(super) fn all_lines(&self, app: &AppState, width: usize) -> Vec<Line<'static>> {
        self.with(app, width, |state| {
            state
                .items
                .iter()
                .flat_map(|entry| entry.lines.iter().cloned())
                .collect()
        })
    }

    /// Cumulative per-item render count, to assert cache hits do zero work.
    #[cfg(test)]
    pub(crate) fn render_count(&self) -> usize {
        self.state.borrow().renders
    }
}

impl LayoutState {
    fn total_rows(&self) -> usize {
        self.row_offsets.last().copied().unwrap_or(0)
    }

    fn locate(&self, row: usize) -> Option<(usize, usize)> {
        if row >= self.total_rows() {
            return None;
        }
        // First offset strictly past `row`, minus one, lands on the owning
        // item; zero-row items share their successor's offset and are skipped
        // by the strict comparison.
        let index = self.row_offsets.partition_point(|&offset| offset <= row) - 1;
        Some((index, row - self.row_offsets[index]))
    }

    /// Clones the lines for rows `[first_row, first_row + count)`.
    fn window(&self, first_row: usize, count: usize) -> Vec<Line<'static>> {
        let total = self.total_rows();
        if count == 0 || first_row >= total {
            return Vec::new();
        }
        let end_row = (first_row + count).min(total);
        let mut out = Vec::with_capacity(end_row - first_row);
        let Some((mut index, offset)) = self.locate(first_row) else {
            return out;
        };
        let mut row = first_row;
        let mut local = offset;
        while row < end_row && index < self.items.len() {
            for line in &self.items[index].lines[local..] {
                if row >= end_row {
                    break;
                }
                out.push(line.clone());
                row += 1;
            }
            index += 1;
            local = 0;
        }
        out
    }
}

/// Today's autoscroll/clamp rule, shared by paint and hit-testing so a click
/// while streaming resolves against exactly the rows on screen.
pub(super) fn resolved_scroll(app: &AppState, total_rows: usize, viewport: usize) -> (u16, u16) {
    let max_scroll = total_rows.saturating_sub(viewport).min(u16::MAX as usize) as u16;
    let scroll = if app.transcript_autoscroll_enabled() {
        max_scroll
    } else {
        app.current_scroll().min(max_scroll)
    };
    (scroll, max_scroll)
}

fn ensure(state: &mut LayoutState, app: &AppState, width: usize) {
    let fingerprint = Fingerprint {
        width,
        serenity_mode: app.serenity_mode,
        reduced_motion: app.reduced_motion,
        linear_output: app.screen_reader_mode,
        theme_generation: theme::generation(),
    };
    if state.fingerprint != Some(fingerprint) {
        state.fingerprint = Some(fingerprint);
        state.items.clear();
    }

    sync_structure(state, app.transcript.stamps());
    refresh_items(state, app, width);

    state.row_offsets.clear();
    state.row_offsets.push(0);
    let mut total = 0usize;
    for entry in &state.items {
        total += entry.rows;
        state.row_offsets.push(total);
    }
}

/// Realigns cache slots with the transcript's items by id. Appends (the
/// steady state while streaming) just push vacant slots; inserts, removals,
/// and wholesale replacement salvage surviving entries by id so an index
/// shift alone — e.g. a new item inserted before trailing queued messages —
/// never re-renders untouched items. The salvage map is the only allocation
/// and only happens on structural change.
fn sync_structure(state: &mut LayoutState, stamps: &[ItemStamp]) {
    let prefix_intact = state.items.len() <= stamps.len()
        && state
            .items
            .iter()
            .zip(stamps)
            .all(|(entry, stamp)| entry.id == stamp.id);
    if prefix_intact {
        for stamp in &stamps[state.items.len()..] {
            state.items.push(ItemLayout::vacant(stamp.id));
        }
        return;
    }

    let mut salvage: HashMap<u64, ItemLayout> = state
        .items
        .drain(..)
        .map(|entry| (entry.id, entry))
        .collect();
    state.items.extend(stamps.iter().map(|stamp| {
        salvage
            .remove(&stamp.id)
            .unwrap_or_else(|| ItemLayout::vacant(stamp.id))
    }));
}

/// Re-renders every item whose key no longer matches. The common frame
/// re-renders nothing (idle) or one item (streaming tail / animated card).
fn refresh_items(state: &mut LayoutState, app: &AppState, width: usize) {
    let stamps = app.transcript.stamps();
    let permission_command = pending_permission_command(app);
    for (index, item) in app.transcript.iter().enumerate() {
        let selected = item_selection_state(app, index, width);
        let focused = app.transcript_focus == Some(index);
        let expanded = execution_group_expanded_for_item(app, item);
        let reasoning_active = serenity_reasoning_is_active(app, index, item);
        let key = ItemKey {
            rev: stamps[index].rev,
            selected,
            focused,
            expanded,
            reasoning_active,
            group_tool_selected: group_tool_selection(app, item),
            animated_tick: (!app.reduced_motion && item_is_animated(item, reasoning_active))
                .then_some(app.animation_tick()),
        };
        if state.items[index].key == Some(key) {
            continue;
        }

        #[cfg(test)]
        {
            state.renders += 1;
        }
        let lines = enforce_line_width(
            render_transcript_item(
                item,
                width,
                TranscriptRenderOptions {
                    selected,
                    focused,
                    active_group_tool_selection: app.active_group_tool_selection.as_ref(),
                    serenity_mode: app.serenity_mode,
                    linear_output: app.screen_reader_mode,
                    execution_group_expanded: expanded,
                    reasoning_active,
                    permission_command,
                    tick: app.animation_tick(),
                },
            ),
            width,
        );
        let entry = &mut state.items[index];
        entry.rows = lines.len();
        entry.lines = lines;
        entry.key = Some(key);
    }
}

/// Whether the item's rendered output depends on time. Decides per-tool
/// liveness from the **tools'** `status`/`finished_at`, never the group's own
/// `finished_at`: group durations are computed from per-tool timestamps
/// (`group_semantics::group_duration`), and a group whose tools all finished
/// but that hasn't been closed yet (close happens on the next delta) is
/// visually stable — keying it on the group field would re-render it every
/// frame forever.
fn item_is_animated(item: &TranscriptItem, reasoning_active: bool) -> bool {
    fn tool_is_live(activity: &ToolActivity) -> bool {
        matches!(activity.status, ToolStatus::Running) || activity.finished_at.is_none()
    }
    match item {
        TranscriptItem::ToolActivity(activity) => tool_is_live(activity),
        TranscriptItem::ExecutionGroup(group) => group.tools.iter().any(tool_is_live),
        TranscriptItem::ReasoningSummary { .. } => reasoning_active,
        _ => false,
    }
}

fn group_tool_selection(app: &AppState, item: &TranscriptItem) -> Option<usize> {
    let TranscriptItem::ExecutionGroup(group) = item else {
        return None;
    };
    app.active_group_tool_selection
        .as_ref()
        .filter(|selection| selection.group_id == group.id)
        .map(|selection| selection.selected_tool)
}

/// Pins the invariant windowed painting relies on: every transcript line is
/// at most `width` cells (card lines are exactly `width`). If a future card
/// leaks an over-width line, degrade to an extra wrap so `rows == lines.len()`
/// stays exact instead of silently desynchronizing scroll and hit-test math.
fn enforce_line_width(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    if lines.iter().all(|line| rendered_line_width(line) <= width) {
        return lines;
    }
    debug_assert!(false, "transcript emitted a line wider than {width} cells");
    lines
        .into_iter()
        .flat_map(|line| {
            if rendered_line_width(&line) <= width {
                vec![line]
            } else {
                wrap_card_line(&line, width)
            }
        })
        .collect()
}
