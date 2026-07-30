//! Seeding for the `/settings` screen (`ModalKind::Manager(crate::tui::event::ManagerModal::Settings)`): read the live
//! posture holders and the current model/theme/SMOL/presentation state into a
//! grouped list
//! of [`SettingsRow`]s. Rebuilt fresh on open and after each change, so the
//! screen always mirrors reality. Rendering lives in
//! `crate::tui::widgets::modal::settings`; applying a change lives in
//! `crate::tui::runtime_actions`.
//!
//! SMOL preference and effective state live on the `Agent`, so the caller reads
//! the resolved profile once under the agent lock and passes it in.

use crate::tui::app::AppState;
use crate::tui::event::{SettingId, SettingsRow};

const AUTONOMY_VALUES: &[&str] = &["ask", "conservative", "balanced", "auto-accept", "yolo"];
const SELF_REVIEW_VALUES: &[&str] = &["auto", "off", "ask", "on"];
const SMOL_VALUES: &[&str] = &["off", "on"];
const TOGGLE_VALUES: &[&str] = &["off", "on"];
const NETWORK_VALUES: &[&str] = &["allow", "deny"];
const CREDENTIAL_VALUES: &[&str] = &["file", "keyring", "session"];
const BUDGET_TURN_VALUES: &[&str] = &["off", "25", "50", "100", "250"];
const BUDGET_RUN_TIME_VALUES: &[&str] = &["off", "5m", "15m", "30m", "60m"];
const BUDGET_GENERATION_TIME_VALUES: &[&str] = &["off", "1m", "3m", "5m", "10m"];
const BUDGET_OUTPUT_VALUES: &[&str] = &["off", "32k", "64k", "128k", "256k"];
const BUDGET_TOOL_TIME_VALUES: &[&str] = &["off", "30s", "2m", "5m", "10m"];
const BUDGET_SESSION_TURN_VALUES: &[&str] = &["off", "100", "250", "500", "1000"];
const BUDGET_SESSION_OUTPUT_VALUES: &[&str] = &["off", "256k", "512k", "1000k", "2000k"];
const BUDGET_SESSION_TIME_VALUES: &[&str] = &["off", "30m", "60m", "120m", "240m"];
const BUDGET_SESSION_COST_VALUES: &[&str] = &["off", "$1", "$5", "$10", "$25", "$50"];

/// Build grouped settings rows from the app state and the live SMOL profile.
pub(crate) fn seed_settings_rows(
    app: &AppState,
    smol_profile: crate::smol::SmolProfile,
) -> Vec<SettingsRow> {
    let mut rows = Vec::new();

    rows.push(SettingsRow::Header("Model"));
    rows.push(SettingsRow::Action {
        id: SettingId::Model,
        key: "model",
        value: crate::tui::pickers::short_model_label(&app.model).to_string(),
    });

    rows.push(SettingsRow::Header("Autonomy"));
    rows.push(SettingsRow::Choice {
        id: SettingId::Autonomy,
        key: "level",
        values: AUTONOMY_VALUES,
        current: index_of(AUTONOMY_VALUES, app.approval_level.label()),
        note: None,
    });

    rows.push(SettingsRow::Header("Self-review"));
    rows.push(SettingsRow::Choice {
        id: SettingId::SelfReview,
        key: "mode",
        values: SELF_REVIEW_VALUES,
        current: index_of(SELF_REVIEW_VALUES, app.self_review_mode.label()),
        note: self_review_note(app),
    });

    rows.push(SettingsRow::Header("Context"));
    rows.push(SettingsRow::Choice {
        id: SettingId::Smol,
        key: "smol",
        values: SMOL_VALUES,
        current: index_of(SMOL_VALUES, smol_profile.preference.as_str()),
        note: Some(format!(
            "effective: {} · {} context",
            smol_profile.effective_label(),
            crate::util::format::format_tokens_k(smol_profile.context_window_tokens)
        )),
    });

    rows.push(SettingsRow::Header("Credentials"));
    rows.push(SettingsRow::Choice {
        id: SettingId::CredentialStorage,
        key: "default store",
        values: CREDENTIAL_VALUES,
        current: index_of(CREDENTIAL_VALUES, app.credential_persistence.as_str()),
        note: Some("each authorization can override".to_string()),
    });

    rows.push(SettingsRow::Header("Appearance"));
    rows.push(SettingsRow::Choice {
        id: SettingId::Serenity,
        key: "serenity",
        values: TOGGLE_VALUES,
        current: usize::from(app.serenity_mode),
        note: None,
    });

    rows.push(SettingsRow::Header("Diagnostics"));
    rows.push(SettingsRow::Choice {
        id: SettingId::SupportLog,
        key: "support log",
        values: TOGGLE_VALUES,
        current: usize::from(app.support_log_enabled),
        note: Some("redacted lifecycle log for /bug bundles".to_string()),
    });
    rows.push(SettingsRow::Action {
        id: SettingId::Theme,
        key: "theme",
        value: crate::tui::theme::current_theme_name().to_string(),
    });

    rows.push(SettingsRow::Header("Budgets"));
    rows.push(SettingsRow::Choice {
        id: SettingId::BudgetTurns,
        key: "max turns",
        values: BUDGET_TURN_VALUES,
        current: budget_turn_index(app.run_budget.max_turns),
        note: Some("per run".to_string()),
    });
    rows.push(SettingsRow::Choice {
        id: SettingId::BudgetRunTime,
        key: "run time",
        values: BUDGET_RUN_TIME_VALUES,
        current: duration_index(BUDGET_RUN_TIME_VALUES, app.run_budget.max_run_seconds),
        note: Some("per run".to_string()),
    });
    rows.push(SettingsRow::Choice {
        id: SettingId::BudgetGenerationTime,
        key: "generation",
        values: BUDGET_GENERATION_TIME_VALUES,
        current: duration_index(
            BUDGET_GENERATION_TIME_VALUES,
            app.run_budget.max_generation_seconds,
        ),
        note: Some("no-output stall, per call".to_string()),
    });
    rows.push(SettingsRow::Choice {
        id: SettingId::BudgetOutput,
        key: "output",
        values: BUDGET_OUTPUT_VALUES,
        current: output_index(app.run_budget.max_output_chars),
        note: Some("per provider call".to_string()),
    });
    rows.push(SettingsRow::Choice {
        id: SettingId::BudgetToolTime,
        key: "tool time",
        values: BUDGET_TOOL_TIME_VALUES,
        current: duration_index(BUDGET_TOOL_TIME_VALUES, app.run_budget.max_tool_seconds),
        note: Some("per tool call".to_string()),
    });
    rows.push(SettingsRow::Choice {
        id: SettingId::BudgetSessionTurns,
        key: "session turns",
        values: BUDGET_SESSION_TURN_VALUES,
        current: session_turn_index(app.run_budget.max_session_turns),
        note: Some("foreground, across resumes".to_string()),
    });
    rows.push(SettingsRow::Choice {
        id: SettingId::BudgetSessionOutput,
        key: "session output",
        values: BUDGET_SESSION_OUTPUT_VALUES,
        current: session_output_index(app.run_budget.max_session_output_chars),
        note: Some("foreground, across resumes".to_string()),
    });
    rows.push(SettingsRow::Choice {
        id: SettingId::BudgetSessionTime,
        key: "session time",
        values: BUDGET_SESSION_TIME_VALUES,
        current: duration_index(
            BUDGET_SESSION_TIME_VALUES,
            app.run_budget.max_session_active_seconds,
        ),
        note: Some("active only, across resumes".to_string()),
    });
    rows.push(SettingsRow::Choice {
        id: SettingId::BudgetSessionCost,
        key: "session cost",
        values: BUDGET_SESSION_COST_VALUES,
        current: session_cost_index(app.run_budget.max_session_cost_micros),
        note: Some("exact priced usage, across resumes".to_string()),
    });

    rows.push(SettingsRow::Header("Sandbox"));
    let confinement_on = app.sandbox.as_ref().is_some_and(|s| s.is_enabled());
    rows.push(SettingsRow::Choice {
        id: SettingId::SandboxConfinement,
        key: "confinement",
        values: TOGGLE_VALUES,
        current: usize::from(confinement_on),
        note: sandbox_confinement_note(app),
    });
    let deny_network = app.sandbox.as_ref().is_some_and(|s| s.deny_network());
    rows.push(SettingsRow::Choice {
        id: SettingId::SandboxNetwork,
        key: "network",
        values: NETWORK_VALUES,
        current: usize::from(deny_network),
        note: sandbox_network_note(app),
    });

    rows
}

/// The first selectable row index, so a freshly opened screen never rests on a
/// header.
pub(crate) fn first_selectable(rows: &[SettingsRow]) -> usize {
    rows.iter()
        .position(SettingsRow::is_selectable)
        .unwrap_or(0)
}

fn index_of(values: &[&str], needle: &str) -> usize {
    values
        .iter()
        .position(|value| *value == needle)
        .unwrap_or(0)
}

fn budget_turn_index(value: Option<usize>) -> usize {
    value
        .and_then(|value| {
            BUDGET_TURN_VALUES
                .iter()
                .position(|candidate| candidate.parse::<usize>().ok() == Some(value))
        })
        .unwrap_or(0)
}

fn duration_index(values: &[&str], seconds: Option<u64>) -> usize {
    seconds
        .and_then(|seconds| {
            values
                .iter()
                .position(|candidate| parse_duration_seconds(candidate) == Some(seconds))
        })
        .unwrap_or(0)
}

fn output_index(value: Option<usize>) -> usize {
    value
        .and_then(|value| {
            BUDGET_OUTPUT_VALUES
                .iter()
                .position(|candidate| parse_output_chars(candidate) == Some(value))
        })
        .unwrap_or(0)
}

fn session_turn_index(value: Option<usize>) -> usize {
    value
        .and_then(|value| {
            BUDGET_SESSION_TURN_VALUES
                .iter()
                .position(|candidate| candidate.parse::<usize>().ok() == Some(value))
        })
        .unwrap_or(0)
}

fn session_output_index(value: Option<usize>) -> usize {
    value
        .and_then(|value| {
            BUDGET_SESSION_OUTPUT_VALUES
                .iter()
                .position(|candidate| parse_output_chars(candidate) == Some(value))
        })
        .unwrap_or(0)
}

fn session_cost_index(value: Option<u64>) -> usize {
    value
        .and_then(|value| {
            BUDGET_SESSION_COST_VALUES
                .iter()
                .position(|candidate| parse_cost_micros(candidate) == Some(value))
        })
        .unwrap_or(0)
}

pub(crate) fn update_run_budget(
    mut budget: crate::run_budget::RunBudget,
    id: SettingId,
    label: &str,
) -> Option<crate::run_budget::RunBudget> {
    match id {
        SettingId::BudgetTurns => {
            budget.max_turns = parse_optional(label, |value| value.parse::<usize>().ok())
        }
        SettingId::BudgetRunTime => {
            budget.max_run_seconds = parse_optional(label, parse_duration_seconds)
        }
        SettingId::BudgetGenerationTime => {
            budget.max_generation_seconds = parse_optional(label, parse_duration_seconds)
        }
        SettingId::BudgetOutput => {
            budget.max_output_chars = parse_optional(label, parse_output_chars)
        }
        SettingId::BudgetToolTime => {
            budget.max_tool_seconds = parse_optional(label, parse_duration_seconds)
        }
        SettingId::BudgetSessionTurns => {
            budget.max_session_turns = parse_optional(label, |value| value.parse::<usize>().ok())
        }
        SettingId::BudgetSessionOutput => {
            budget.max_session_output_chars = parse_optional(label, parse_output_chars)
        }
        SettingId::BudgetSessionTime => {
            budget.max_session_active_seconds = parse_optional(label, parse_duration_seconds)
        }
        SettingId::BudgetSessionCost => {
            budget.max_session_cost_micros = parse_optional(label, parse_cost_micros)
        }
        _ => return None,
    }
    Some(budget)
}

fn parse_optional<T>(label: &str, parse: impl FnOnce(&str) -> Option<T>) -> Option<T> {
    (label != "off").then(|| parse(label)).flatten()
}

fn parse_duration_seconds(label: &str) -> Option<u64> {
    let (number, multiplier) = if let Some(number) = label.strip_suffix('s') {
        (number, 1)
    } else {
        (label.strip_suffix('m')?, 60)
    };
    number.parse::<u64>().ok()?.checked_mul(multiplier)
}

fn parse_output_chars(label: &str) -> Option<usize> {
    label
        .strip_suffix('k')?
        .parse::<usize>()
        .ok()?
        .checked_mul(1_000)
}

fn parse_cost_micros(label: &str) -> Option<u64> {
    label
        .strip_prefix('$')?
        .parse::<u64>()
        .ok()?
        .checked_mul(1_000_000)
}

fn self_review_note(app: &AppState) -> Option<String> {
    match app.self_review_mode {
        crate::self_review::SelfReviewMode::Auto
            if app.approval_level >= crate::tool::ApprovalLevel::AutoAccept =>
        {
            Some("on at current autonomy".to_string())
        }
        crate::self_review::SelfReviewMode::Auto => Some("off at current autonomy".to_string()),
        _ => None,
    }
}

fn sandbox_confinement_note(app: &AppState) -> Option<String> {
    match app.sandbox.as_ref() {
        Some(sandbox) if !sandbox.backend().is_available() => Some("no backend".to_string()),
        None => Some("unavailable".to_string()),
        _ => None,
    }
}

fn sandbox_network_note(app: &AppState) -> Option<String> {
    match app.sandbox.as_ref() {
        Some(sandbox) if sandbox.deny_network() && !sandbox.backend().supports_network_deny() => {
            Some("unenforced".to_string())
        }
        Some(sandbox) if !sandbox.is_enabled() => Some("no effect while off".to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::event::SettingId;
    use crate::tui::test_utils::app;

    fn smol_profile(preference: crate::smol::SmolPreference) -> crate::smol::SmolProfile {
        crate::smol::SmolProfile::resolve(preference, 128_000)
    }

    fn choice_current(rows: &[SettingsRow], id: SettingId) -> Option<usize> {
        rows.iter().find_map(|row| match row {
            SettingsRow::Choice {
                id: rid, current, ..
            } if *rid == id => Some(*current),
            _ => None,
        })
    }

    #[test]
    fn seeds_all_sections_and_first_selectable_skips_header() {
        let app = app();
        let rows = seed_settings_rows(&app, smol_profile(crate::smol::SmolPreference::Off));
        // Nine section headers group related rows.
        assert_eq!(
            rows.iter()
                .filter(|r| matches!(r, SettingsRow::Header(_)))
                .count(),
            9
        );
        // First selectable is the Model action, not the "Model" header.
        assert!(rows[first_selectable(&rows)].is_selectable());
        assert_eq!(rows[first_selectable(&rows)].id(), Some(SettingId::Model));
    }

    #[test]
    fn autonomy_smol_and_serenity_track_state() {
        let mut app = app();
        app.approval_level = crate::tool::ApprovalLevel::Yolo;
        app.serenity_mode = true;
        let rows = seed_settings_rows(&app, smol_profile(crate::smol::SmolPreference::On));
        assert_eq!(
            choice_current(&rows, SettingId::Autonomy),
            Some(AUTONOMY_VALUES.iter().position(|v| *v == "yolo").unwrap())
        );
        assert_eq!(choice_current(&rows, SettingId::Smol), Some(1));
        assert_eq!(choice_current(&rows, SettingId::Serenity), Some(1));

        app.serenity_mode = false;
        let rows_off = seed_settings_rows(&app, smol_profile(crate::smol::SmolPreference::Off));
        assert_eq!(choice_current(&rows_off, SettingId::Smol), Some(0));
        assert_eq!(choice_current(&rows_off, SettingId::Serenity), Some(0));
    }

    #[test]
    fn smol_setting_shows_explicit_preference_and_effective_profile() {
        let app = app();
        let profile = crate::smol::SmolProfile::resolve(crate::smol::SmolPreference::Off, 8_192);
        let rows = seed_settings_rows(&app, profile);
        let row = rows
            .iter()
            .find(|row| row.id() == Some(SettingId::Smol))
            .expect("SMOL setting");
        assert_eq!(choice_current(&rows, SettingId::Smol), Some(0));
        assert!(matches!(
            row,
            SettingsRow::Choice { note: Some(note), .. }
                if note == "effective: off · 8.2k context"
        ));
    }

    #[test]
    fn budgets_are_off_by_default_and_track_selected_presets() {
        let mut app = app();
        let rows = seed_settings_rows(&app, smol_profile(crate::smol::SmolPreference::Off));
        assert_eq!(choice_current(&rows, SettingId::BudgetTurns), Some(0));
        assert_eq!(choice_current(&rows, SettingId::BudgetRunTime), Some(0));
        assert_eq!(
            choice_current(&rows, SettingId::BudgetSessionTurns),
            Some(0)
        );
        assert_eq!(
            choice_current(&rows, SettingId::BudgetSessionOutput),
            Some(0)
        );
        assert_eq!(choice_current(&rows, SettingId::BudgetSessionTime), Some(0));
        assert_eq!(choice_current(&rows, SettingId::BudgetSessionCost), Some(0));

        app.run_budget = crate::run_budget::RunBudget {
            max_turns: Some(50),
            max_run_seconds: Some(900),
            max_generation_seconds: Some(180),
            max_output_chars: Some(64_000),
            max_tool_seconds: Some(120),
            max_session_turns: Some(500),
            max_session_output_chars: Some(1_000_000),
            max_session_active_seconds: Some(7_200),
            max_session_cost_micros: Some(10_000_000),
        };
        let rows = seed_settings_rows(&app, smol_profile(crate::smol::SmolPreference::Off));
        assert_eq!(choice_current(&rows, SettingId::BudgetTurns), Some(2));
        assert_eq!(choice_current(&rows, SettingId::BudgetRunTime), Some(2));
        assert_eq!(
            choice_current(&rows, SettingId::BudgetGenerationTime),
            Some(2)
        );
        assert_eq!(choice_current(&rows, SettingId::BudgetOutput), Some(2));
        assert_eq!(choice_current(&rows, SettingId::BudgetToolTime), Some(2));
        assert_eq!(
            choice_current(&rows, SettingId::BudgetSessionTurns),
            Some(3)
        );
        assert_eq!(
            choice_current(&rows, SettingId::BudgetSessionOutput),
            Some(3)
        );
        assert_eq!(choice_current(&rows, SettingId::BudgetSessionTime), Some(3));
        assert_eq!(choice_current(&rows, SettingId::BudgetSessionCost), Some(3));
    }

    #[test]
    fn session_cost_budget_parses_preset_into_exact_micro_usd() {
        let budget = update_run_budget(
            crate::run_budget::RunBudget::default(),
            SettingId::BudgetSessionCost,
            "$10",
        )
        .unwrap();

        assert_eq!(budget.max_session_cost_micros, Some(10_000_000));
    }
}
