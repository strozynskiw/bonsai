//! Seeding helpers for the /mode picker. Lives in its own module so the
//! reducer file stays compact and the seeding rules can be unit-tested
//! independently of the picker UI.

use crate::tui::app::AppState;
use crate::tui::event::{MODE_AXES, ModeAxisId, ModeRow, ModeValueDef};

/// Build a fresh rows list for the /mode picker from the current app state.
/// Re-running on every open keeps the picker's view of the world in sync with
/// the shared holders, so closing and reopening /mode reflects the latest
/// values without persisting intermediate cycle state across picker sessions.
pub(super) fn seed_mode_picker_rows(app: &AppState) -> Vec<ModeRow> {
    let value_rows = MODE_AXES
        .iter()
        .map(|axis| axis.values.len())
        .sum::<usize>();
    let mut rows = Vec::with_capacity(MODE_AXES.len() + value_rows);
    for axis in MODE_AXES {
        rows.push(ModeRow::Header(axis.header));
        for value_def in axis.values {
            rows.push(ModeRow::Value {
                axis: value_def.axis,
                key: value_def.key,
                values: value_def.values,
                current: mode_picker_current_index(app, value_def),
                note: mode_picker_note_for(app, value_def.axis),
            });
        }
    }
    rows
}

/// Resolve the current display label for a posture axis into an index into
/// the axis's cycle-order values array. The labels used here match the
/// strings declared in MODE_AXES (ask, conservative, ...; off/on; allow/deny);
/// when none matches (e.g. app.sandbox is None on a fresh test app) we fall
/// back to the first entry so the picker always highlights a valid choice.
fn mode_picker_current_index(app: &AppState, value_def: &ModeValueDef) -> usize {
    let needle = mode_picker_current_label(app, value_def.axis);
    value_def
        .values
        .iter()
        .position(|value| *value == needle)
        .unwrap_or(0)
}

/// Current display label for a posture axis, read from app (which already
/// mirrors the shared holders). Returning a static str keeps the
/// ModeRow::Value::values array as a slice of static strs without needing
/// per-row owned strings.
fn mode_picker_current_label(app: &AppState, axis: ModeAxisId) -> &'static str {
    match axis {
        ModeAxisId::Autonomy => app.approval_level.label(),
        ModeAxisId::SelfReview => app.self_review_mode.label(),
        ModeAxisId::SandboxConfinement => match app.sandbox.as_ref() {
            Some(sandbox) if sandbox.is_enabled() => "on",
            _ => "off",
        },
        ModeAxisId::SandboxNetwork => match app.sandbox.as_ref() {
            Some(sandbox) if sandbox.deny_network() => "deny",
            _ => "allow",
        },
    }
}

/// Optional inline note shown after a value row, calling out edge cases
/// (no backend, unenforced network deny, network a no-op while confinement
/// is off). Returning None for healthy states keeps the row clean.
fn mode_picker_note_for(app: &AppState, axis: ModeAxisId) -> Option<&'static str> {
    match axis {
        ModeAxisId::Autonomy => None,
        ModeAxisId::SelfReview => match app.self_review_mode {
            crate::self_review::SelfReviewMode::Auto
                if app.approval_level >= crate::tool::ApprovalLevel::Balanced =>
            {
                Some("on at current autonomy")
            }
            crate::self_review::SelfReviewMode::Auto => Some("off at current autonomy"),
            _ => None,
        },
        ModeAxisId::SandboxConfinement => match app.sandbox.as_ref() {
            Some(sandbox) if !sandbox.backend().is_available() => Some("no backend"),
            _ => None,
        },
        ModeAxisId::SandboxNetwork => match app.sandbox.as_ref() {
            Some(sandbox)
                if sandbox.deny_network() && !sandbox.backend().supports_network_deny() =>
            {
                Some("unenforced")
            }
            Some(sandbox) if !sandbox.is_enabled() => Some("no effect while off"),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::event::{ModeAxisId, ModeRow};
    use crate::tui::test_utils::app;

    #[test]
    fn seed_rows_match_mode_axes_layout() {
        let application = app();
        let rows = seed_mode_picker_rows(&application);
        let expected_value_rows = MODE_AXES.iter().map(|a| a.values.len()).sum::<usize>();
        assert_eq!(rows.len(), MODE_AXES.len() + expected_value_rows);

        let mut value_seen = 0;
        let mut header_seen = 0;
        for row in &rows {
            match row {
                ModeRow::Header(_) => header_seen += 1,
                ModeRow::Value { .. } => value_seen += 1,
            }
        }
        assert_eq!(header_seen, MODE_AXES.len());
        assert_eq!(value_seen, expected_value_rows);
    }

    #[test]
    fn autonomy_current_index_tracks_approval_level_label() {
        let mut application = app();
        for level in [
            crate::tool::ApprovalLevel::Ask,
            crate::tool::ApprovalLevel::Conservative,
            crate::tool::ApprovalLevel::Balanced,
            crate::tool::ApprovalLevel::AutoAccept,
            crate::tool::ApprovalLevel::Yolo,
        ] {
            application.approval_level = level;
            let rows = seed_mode_picker_rows(&application);
            let autonomy_value = rows.iter().find_map(|row| match row {
                ModeRow::Value {
                    axis: ModeAxisId::Autonomy,
                    current,
                    ..
                } => Some(*current),
                _ => None,
            });
            let def = &MODE_AXES[0].values[0];
            let expected = def
                .values
                .iter()
                .position(|v| *v == level.label())
                .unwrap_or(0);
            assert_eq!(autonomy_value, Some(expected), "label: {}", level.label());
        }
    }

    #[test]
    fn self_review_current_index_tracks_mode_label() {
        let mut application = app();
        for mode in [
            crate::self_review::SelfReviewMode::Auto,
            crate::self_review::SelfReviewMode::Off,
            crate::self_review::SelfReviewMode::Ask,
            crate::self_review::SelfReviewMode::On,
        ] {
            application.self_review_mode = mode;
            let rows = seed_mode_picker_rows(&application);
            let self_review_value = rows.iter().find_map(|row| match row {
                ModeRow::Value {
                    axis: ModeAxisId::SelfReview,
                    current,
                    note,
                    ..
                } => Some((*current, *note)),
                _ => None,
            });
            let def = MODE_AXES
                .iter()
                .flat_map(|axis| axis.values.iter())
                .find(|value| value.axis == ModeAxisId::SelfReview)
                .expect("self-review axis should exist");
            let expected = def
                .values
                .iter()
                .position(|value| *value == mode.label())
                .unwrap_or(0);
            assert_eq!(
                self_review_value,
                Some((
                    expected,
                    mode_picker_note_for(&application, ModeAxisId::SelfReview)
                )),
                "label: {}",
                mode.label()
            );
        }
    }
}
