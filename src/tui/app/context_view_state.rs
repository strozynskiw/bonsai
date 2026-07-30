use std::collections::HashSet;

use super::*;
use crate::tui::event::ModalKind;

fn context_wire_row_ids(
    report: &crate::agent::ContextReport,
    expanded: &HashSet<String>,
) -> Vec<String> {
    let Some(preview) = &report.payload_preview else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    for section in &preview.wire_sections {
        collect_context_wire_row_ids(section, expanded, &mut ids);
    }
    ids.push(CONTEXT_WIRE_RAW_JSON_ID.to_string());
    ids
}

fn default_context_wire_expanded_ids(_report: &crate::agent::ContextReport) -> HashSet<String> {
    HashSet::new()
}

fn collect_context_wire_row_ids(
    section: &crate::provider::ProviderWireSection,
    expanded: &HashSet<String>,
    ids: &mut Vec<String>,
) {
    ids.push(section.id.clone());
    if expanded.contains(&section.id) {
        for child in &section.children {
            collect_context_wire_row_ids(child, expanded, ids);
        }
    }
}

impl AppState {
    pub(crate) fn visible_context_node_ids(&self) -> Vec<String> {
        let Some(report) = self.active_context_report() else {
            return Vec::new();
        };
        report.visible_node_ids(&self.context_state.expanded)
    }

    pub(crate) fn visible_context_wire_row_ids(&self) -> Vec<String> {
        let Some(report) = self.active_context_report() else {
            return Vec::new();
        };
        context_wire_row_ids(report, &self.context_state.wire_expanded)
    }

    /// Turn `seq`s selectable in the Turns view, oldest first, capped to the
    /// most recent [`CONTEXT_TURNS_ROW_LIMIT`]. The renderer must derive its
    /// turn rows from the same window or cursor and display drift apart.
    pub(crate) fn visible_context_turn_seqs(&self) -> Vec<usize> {
        let Some(report) = self.active_context_report() else {
            return Vec::new();
        };
        visible_context_turn_seqs(report)
    }

    fn active_context_report(&self) -> Option<&crate::agent::ContextReport> {
        if let Some(ModalKind::Detail(crate::tui::event::DetailModal::Context(report))) =
            self.modal.as_ref()
        {
            Some(report.as_ref())
        } else {
            self.latest_context_report.as_ref()
        }
    }

    pub(crate) fn selected_context_node_id(&self) -> Option<String> {
        let ids = self.visible_context_node_ids();
        ids.get(self.context_state.cursor.min(ids.len().saturating_sub(1)))
            .cloned()
    }

    pub(crate) fn selected_context_wire_row_id(&self) -> Option<String> {
        let ids = self.visible_context_wire_row_ids();
        ids.get(
            self.context_state
                .wire_cursor
                .min(ids.len().saturating_sub(1)),
        )
        .cloned()
    }

    pub(crate) fn selected_context_turn_seq(&self) -> Option<usize> {
        let seqs = self.visible_context_turn_seqs();
        seqs.get(
            self.context_state
                .turns_cursor
                .min(seqs.len().saturating_sub(1)),
        )
        .copied()
    }

    /// Reset the context inspector's per-view cursor/expansion state for a
    /// freshly opened or refreshed report. Shared by the three open paths so
    /// they cannot drift.
    pub(crate) fn init_context_view_state(&mut self, report: &crate::agent::ContextReport) {
        self.context_state.cursor = 0;
        self.context_state.expanded = report.default_expanded_node_ids();
        self.context_state.wire_cursor = 0;
        self.context_state.wire_expanded = default_context_wire_expanded_ids(report);
        self.context_state.turns_cursor = 0;
        self.context_state.turns_expanded = HashSet::new();
        self.context_state.manual_scroll = false;
        self.context_state.reveal_expanded = false;
        self.context_state.view_mode = ContextViewMode::Ledger;
    }

    pub(super) fn move_context_cursor(&mut self, delta: i16) {
        let max = self.visible_context_node_ids().len().saturating_sub(1);
        self.context_state.cursor = move_index(self.context_state.cursor, delta, max);
    }

    pub(super) fn move_context_wire_cursor(&mut self, delta: i16) {
        let max = self.visible_context_wire_row_ids().len().saturating_sub(1);
        self.context_state.wire_cursor = move_index(self.context_state.wire_cursor, delta, max);
    }

    pub(super) fn clamp_context_cursor(&mut self) {
        let max = self.visible_context_node_ids().len().saturating_sub(1);
        self.context_state.cursor = self.context_state.cursor.min(max);
    }

    pub(super) fn clamp_context_wire_cursor(&mut self) {
        let max = self.visible_context_wire_row_ids().len().saturating_sub(1);
        self.context_state.wire_cursor = self.context_state.wire_cursor.min(max);
    }

    pub(super) fn move_context_turns_cursor(&mut self, delta: i16) {
        let max = self.visible_context_turn_seqs().len().saturating_sub(1);
        self.context_state.turns_cursor = move_index(self.context_state.turns_cursor, delta, max);
    }

    pub(super) fn clamp_context_turns_cursor(&mut self) {
        let max = self.visible_context_turn_seqs().len().saturating_sub(1);
        self.context_state.turns_cursor = self.context_state.turns_cursor.min(max);
    }
}

/// Shared cap window for the Turns view: the seqs of the most recent
/// [`CONTEXT_TURNS_ROW_LIMIT`] turns, oldest first.
pub(crate) fn visible_context_turn_seqs(report: &crate::agent::ContextReport) -> Vec<usize> {
    let skip = report
        .usage_turns
        .len()
        .saturating_sub(CONTEXT_TURNS_ROW_LIMIT);
    report
        .usage_turns
        .iter()
        .skip(skip)
        .map(|turn| turn.seq)
        .collect()
}
