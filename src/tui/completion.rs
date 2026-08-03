//! Inline completion for the composer. Typing `/` opens the command popover;
//! typing `@path` opens workspace path completion backed by fff.

use crate::tui::app::{AppState, CompletionCandidate};
use crate::tui::path_search::PathKind;

/// Upper bound on candidates kept in a completion list. The popover shows a
/// 5-row window and the ↑/↓ cursor scrolls through the rest, so this bounds
/// scrolling depth, not the rendered height.
const MAX_COMPLETION_CANDIDATES: usize = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub candidates: Vec<CompletionCandidate>,
    pub replacement: String,
    pub replacement_range: Option<(usize, usize)>,
}

pub fn compute_completion(app: &AppState) -> Option<Completion> {
    let buffer = app.input();
    if !buffer.starts_with('/')
        && let Some(token) = mention_token_at(buffer, app.composer.cursor)
    {
        return path_completion(app, token);
    }

    if !buffer.starts_with('/') {
        return None;
    }
    let (cmd, arg) = match buffer.split_once(char::is_whitespace) {
        Some((cmd, rest)) => (cmd, rest.trim_start()),
        None => (buffer.trim_end(), ""),
    };

    if !buffer.chars().any(char::is_whitespace) {
        // A bare "/" lists every command so the picker doubles as a menu.
        return command_completion(app, cmd);
    }
    arg_completion(app, cmd, arg)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct CommandMatchRank {
    tier: u8,
    position: usize,
}

/// Command floated to the top of the picker whenever a filtered search surfaces
/// it, ahead of match quality and name order.
const PINNED_COMMAND: &str = "/model";

fn command_completion(_app: &AppState, prefix: &str) -> Option<Completion> {
    let mut matches: Vec<(&crate::commands::CommandMetadata, CommandMatchRank)> =
        crate::commands::COMMANDS
            .iter()
            .filter_map(|command| command_match_rank(command, prefix).map(|rank| (command, rank)))
            .collect();
    // Pin only when the user is actually filtering: the bare `/` menu browses
    // every command and keeps its natural order.
    let pinning = prefix.len() > 1;
    matches.sort_by(|(left, left_rank), (right, right_rank)| {
        // `/model` floats to the top while filtering, but only when it matches
        // by name (tiers 0–2) — i.e. the user is plausibly typing toward it. A
        // description-only match (tier 3) must not make `/model` dominate the
        // command list for unrelated filters.
        let floated = |name: &str, rank: &CommandMatchRank| {
            pinning && name == PINNED_COMMAND && rank.tier <= 2
        };
        // `false` (a floated command) sorts before `true`; everything else keeps
        // match-quality, then name, order.
        (!floated(left.name, left_rank))
            .cmp(&!floated(right.name, right_rank))
            .then_with(|| left_rank.cmp(right_rank))
            .then_with(|| left.name.cmp(right.name))
    });

    let candidates: Vec<CompletionCandidate> = matches
        .iter()
        .take(MAX_COMPLETION_CANDIDATES)
        .map(|(command, _rank)| CompletionCandidate::Command {
            label: command.name.to_string(),
            description: crate::commands::command_completion_preview(command.name)
                .unwrap_or_else(|| command.description.to_string()),
        })
        .collect();
    let first = candidates.first().and_then(|c| match c {
        CompletionCandidate::Command { label, .. } => Some(label.clone()),
        _ => None,
    });
    first.map(|label| Completion {
        candidates,
        replacement: label,
        replacement_range: None,
    })
}

fn command_match_rank(
    command: &crate::commands::CommandMetadata,
    prefix: &str,
) -> Option<CommandMatchRank> {
    let name_needle = prefix.to_lowercase();
    let description_needle = name_needle.strip_prefix('/').unwrap_or(&name_needle);
    let name = command.name.to_lowercase();
    let description = command.description.to_lowercase();

    if name == name_needle {
        return Some(CommandMatchRank {
            tier: 0,
            position: 0,
        });
    }
    if name.starts_with(&name_needle) {
        return Some(CommandMatchRank {
            tier: 1,
            position: 0,
        });
    }
    if let Some(position) = name.find(&name_needle) {
        return Some(CommandMatchRank { tier: 2, position });
    }
    if !description_needle.is_empty()
        && let Some(position) = description.find(description_needle)
    {
        return Some(CommandMatchRank { tier: 3, position });
    }
    None
}

fn arg_completion(app: &AppState, cmd: &str, arg: &str) -> Option<Completion> {
    let lower = arg.to_lowercase();
    match cmd {
        "/authorize" | "/unauthorize" => {
            let mut candidates: Vec<CompletionCandidate> = app
                .provider_choices
                .iter()
                .filter(|choice| {
                    choice.provider_id.to_lowercase().contains(&lower)
                        || choice.provider_label.to_lowercase().contains(&lower)
                })
                .map(|choice| CompletionCandidate::Provider {
                    id: choice.provider_id.clone(),
                    display: choice.provider_label.clone(),
                })
                .collect();
            candidates.sort_by(|a, b| match (a, b) {
                (
                    CompletionCandidate::Provider { display: a, .. },
                    CompletionCandidate::Provider { display: b, .. },
                ) => a.cmp(b),
                _ => std::cmp::Ordering::Equal,
            });
            candidates.truncate(MAX_COMPLETION_CANDIDATES);
            let first = candidates.first().and_then(|c| match c {
                CompletionCandidate::Provider { id, .. } => Some(id.clone()),
                _ => None,
            });
            first.map(|id| Completion {
                candidates,
                replacement: format!("{cmd} {id}"),
                replacement_range: None,
            })
        }
        "/model" => {
            let mut candidates: Vec<CompletionCandidate> = app
                .cached_model_choices
                .iter()
                .filter(|choice| choice.matches_filter(&lower))
                .map(|choice| CompletionCandidate::Model {
                    provider_id: choice.provider_id.clone(),
                    model: choice.picker_model_label().to_string(),
                    replacement: choice
                        .selection_input()
                        .unwrap_or_else(|| choice.display_model().to_string()),
                    pricing: choice.pricing_compact(),
                })
                .collect();
            candidates.sort_by(|a, b| match (a, b) {
                (
                    CompletionCandidate::Model { model: a, .. },
                    CompletionCandidate::Model { model: b, .. },
                ) => a.cmp(b),
                _ => std::cmp::Ordering::Equal,
            });
            candidates.dedup();
            candidates.truncate(MAX_COMPLETION_CANDIDATES);
            let first = candidates.first().and_then(|c| match c {
                CompletionCandidate::Model { replacement, .. } => Some(replacement.clone()),
                _ => None,
            });
            first.map(|model| Completion {
                candidates,
                replacement: format!("{cmd} {model}"),
                replacement_range: None,
            })
        }
        "/theme" => {
            // The `export` subcommand plus every theme name.
            let export = std::iter::once("export")
                .filter(|name| name.contains(&lower))
                .map(|name| ArgumentCandidate {
                    label: name.to_string(),
                    detail: "write current palette to a file".to_string(),
                    replacement: format!("{cmd} {name}"),
                });
            let names = crate::tui::theme::theme_names()
                .into_iter()
                .filter(|name| name.contains(&lower))
                .map(|name| {
                    let value = name.to_string();
                    ArgumentCandidate {
                        label: value.clone(),
                        detail: "theme".to_string(),
                        replacement: format!("{cmd} {value}"),
                    }
                });
            argument_completion(export.chain(names))
        }
        "/compact" => literal_arg_completion(
            cmd,
            &lower,
            "option",
            ["preview", "target=", "provider", "deterministic"],
        ),
        "/copy" => literal_arg_completion(cmd, &lower, "scope", ["all"]),
        "/doctor" => doctor_arg_completion(cmd, arg),
        "/autonomy" => literal_arg_completion(
            cmd,
            &lower,
            "level",
            [
                "ask",
                "conservative",
                "balanced",
                "auto-accept",
                "yolo",
                "status",
            ],
        ),
        "/self-review" => {
            literal_arg_completion(cmd, &lower, "mode", ["auto", "on", "ask", "off", "status"])
        }
        "/serenity" => literal_arg_completion(cmd, &lower, "mode", ["on", "off", "status"]),
        "/smol" => literal_arg_completion(cmd, &lower, "mode", ["on", "off", "status"]),
        "/permissions" => literal_arg_completion(cmd, &lower, "action", ["list", "remove"]),
        "/sandbox" => sandbox_arg_completion(cmd, arg),
        "/yolo" => argument_completion(
            ["on", "off", "status"]
                .into_iter()
                .filter(|value| value.starts_with(&lower))
                .map(|value| ArgumentCandidate {
                    label: value.to_string(),
                    detail: "mode".to_string(),
                    replacement: format!("{cmd} {value}"),
                }),
        ),
        "/resume" | "/forget" => session_arg_completion(app, cmd, &lower),
        "/retry" => retry_arg_completion(app, cmd, &lower),
        "/tasks" => tasks_arg_completion(app, cmd, arg),
        _ => None,
    }
}

fn literal_arg_completion<const N: usize>(
    cmd: &str,
    lower: &str,
    detail: &str,
    values: [&str; N],
) -> Option<Completion> {
    argument_completion(
        values
            .into_iter()
            .filter(|value| value.starts_with(lower))
            .map(|value| ArgumentCandidate {
                label: value.to_string(),
                detail: detail.to_string(),
                replacement: format!("{cmd} {value}"),
            }),
    )
}

fn doctor_arg_completion(cmd: &str, arg: &str) -> Option<Completion> {
    let mut selected = arg
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    let prefix = if arg.ends_with(char::is_whitespace) {
        String::new()
    } else {
        selected.pop().unwrap_or_default()
    };
    let base = selected.join(" ");
    argument_completion(
        [("json", "format"), ("online", "network probe")]
            .into_iter()
            .filter(|(value, _)| {
                value.starts_with(&prefix) && !selected.iter().any(|seen| seen == value)
            })
            .map(|(value, detail)| ArgumentCandidate {
                label: value.to_string(),
                detail: detail.to_string(),
                replacement: if base.is_empty() {
                    format!("{cmd} {value}")
                } else {
                    format!("{cmd} {base} {value}")
                },
            }),
    )
}

fn sandbox_arg_completion(cmd: &str, arg: &str) -> Option<Completion> {
    let lower = arg.to_lowercase();
    if let Some(rest) = lower.strip_prefix("net")
        && rest.starts_with(char::is_whitespace)
    {
        let network_arg = rest.trim_start();
        return literal_arg_completion(cmd, network_arg, "network", ["on", "off"]).map(
            |mut completion| {
                for candidate in &mut completion.candidates {
                    if let CompletionCandidate::Argument {
                        replacement, label, ..
                    } = candidate
                    {
                        *replacement = format!("{cmd} net {label}");
                    }
                }
                completion.replacement = completion
                    .candidates
                    .first()
                    .and_then(|candidate| match candidate {
                        CompletionCandidate::Argument { replacement, .. } => {
                            Some(replacement.clone())
                        }
                        _ => None,
                    })
                    .unwrap_or(completion.replacement);
                completion
            },
        );
    }

    argument_completion(
        [
            ArgumentCandidate {
                label: "on".to_string(),
                detail: "option".to_string(),
                replacement: format!("{cmd} on"),
            },
            ArgumentCandidate {
                label: "off".to_string(),
                detail: "option".to_string(),
                replacement: format!("{cmd} off"),
            },
            ArgumentCandidate {
                label: "net".to_string(),
                detail: "option".to_string(),
                replacement: format!("{cmd} net "),
            },
            ArgumentCandidate {
                label: "status".to_string(),
                detail: "option".to_string(),
                replacement: format!("{cmd} status"),
            },
        ]
        .into_iter()
        .filter(|candidate| candidate.label.starts_with(&lower)),
    )
}

#[derive(Debug, Clone)]
struct ArgumentCandidate {
    label: String,
    detail: String,
    replacement: String,
}

fn argument_completion(
    candidates: impl IntoIterator<Item = ArgumentCandidate>,
) -> Option<Completion> {
    let mut candidates: Vec<CompletionCandidate> = candidates
        .into_iter()
        .map(|candidate| CompletionCandidate::Argument {
            label: candidate.label,
            detail: candidate.detail,
            replacement: candidate.replacement,
        })
        .collect();
    candidates.sort_by(|left, right| match (left, right) {
        (
            CompletionCandidate::Argument { label: left, .. },
            CompletionCandidate::Argument { label: right, .. },
        ) => left.cmp(right),
        _ => std::cmp::Ordering::Equal,
    });
    candidates.truncate(MAX_COMPLETION_CANDIDATES);
    let first = candidates.first().and_then(|candidate| match candidate {
        CompletionCandidate::Argument { replacement, .. } => Some(replacement.clone()),
        _ => None,
    });
    first.map(|replacement| Completion {
        candidates,
        replacement,
        replacement_range: None,
    })
}

fn session_arg_completion(app: &AppState, cmd: &str, lower: &str) -> Option<Completion> {
    argument_completion(app.session_choices.iter().filter_map(|session| {
        let id = session.id.to_string();
        let title = session_title(session);
        let haystack = format!("{id} {title}").to_lowercase();
        if !haystack.contains(lower) {
            return None;
        }
        Some(ArgumentCandidate {
            label: format!("#{id}"),
            detail: title,
            replacement: format!("{cmd} {id}"),
        })
    }))
}

fn session_title(session: &crate::storage::SessionSummary) -> String {
    let summary = session.summary.trim();
    if !summary.is_empty() {
        return summary.to_string();
    }
    let name = session.name.trim();
    if !name.is_empty() {
        return name.to_string();
    }
    format!("{} · {}", session.provider_id, session.model)
}

fn retry_arg_completion(app: &AppState, cmd: &str, lower: &str) -> Option<Completion> {
    let mut shortcuts = std::collections::BTreeMap::new();
    for model in &app.cached_model_choices {
        for (key, reasoning) in &model.shortcut_bindings {
            shortcuts.entry(*key).or_insert_with(|| ArgumentCandidate {
                label: key.to_string(),
                detail: format!(
                    "{} · {} · {}",
                    model.provider_label, model.display_name, reasoning
                ),
                replacement: format!("{cmd} {key}"),
            });
        }
    }

    argument_completion(
        shortcuts
            .into_values()
            .filter(|candidate| candidate.label.starts_with(lower)),
    )
}

fn tasks_arg_completion(app: &AppState, cmd: &str, arg: &str) -> Option<Completion> {
    let trimmed = arg.trim_start();
    let lower = trimmed.to_lowercase();
    if !trimmed.chars().any(char::is_whitespace) {
        if "stop".starts_with(&lower) {
            return argument_completion([ArgumentCandidate {
                label: "stop".to_string(),
                detail: "subcommand".to_string(),
                replacement: format!("{cmd} stop "),
            }]);
        }
        return None;
    }

    let mut parts = trimmed.splitn(2, char::is_whitespace);
    if parts.next() != Some("stop") {
        return None;
    }
    let id_prefix = parts.next().unwrap_or("").trim_start().to_lowercase();
    argument_completion(app.background_tasks.iter().filter_map(|task| {
        if !matches!(
            task.status,
            crate::background::BackgroundTaskStatus::Running
        ) {
            return None;
        }
        if !task.id.to_lowercase().contains(&id_prefix) {
            return None;
        }
        Some(ArgumentCandidate {
            label: task.id.clone(),
            detail: task.compact_command(48),
            replacement: format!("{cmd} stop {}", task.id),
        })
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MentionToken {
    start: usize,
    end: usize,
    query: String,
}

fn path_completion(app: &AppState, token: MentionToken) -> Option<Completion> {
    let search = app.path_search.as_ref()?;
    let candidates: Vec<CompletionCandidate> = search
        .search(&token.query, MAX_COMPLETION_CANDIDATES)
        .into_iter()
        .map(|choice| CompletionCandidate::Path {
            path: choice.path,
            kind: choice.kind,
        })
        .collect();
    let first = candidates.first().and_then(|candidate| match candidate {
        CompletionCandidate::Path { path, kind } => Some(path_replacement(path, *kind)),
        _ => None,
    });
    first.map(|replacement| Completion {
        candidates,
        replacement,
        replacement_range: Some((token.start, token.end)),
    })
}

pub fn path_replacement(path: &str, kind: PathKind) -> String {
    let mut path = path.to_string();
    if matches!(kind, PathKind::Directory) && !path.ends_with('/') {
        path.push('/');
    }
    if path
        .chars()
        .any(|ch| ch.is_whitespace() || ch == '{' || ch == '}')
    {
        let escaped = path.replace('\\', "\\\\").replace('}', "\\}");
        format!("@{{{escaped}}}")
    } else {
        format!("@{path}")
    }
}

fn mention_token_at(buffer: &str, cursor: usize) -> Option<MentionToken> {
    use crate::tui::text_bounds;

    // `cursor` is a grapheme index (matching `Composer`), but the scan below
    // works in char space. Translate the cursor in via byte offsets, and the
    // resulting `start`/`end` back to grapheme indices on the way out, so the
    // returned range stays valid for `Composer::replace_range`.
    let chars: Vec<char> = buffer.chars().collect();
    let cursor_byte = text_bounds::byte_index_for_grapheme_index(buffer, cursor);
    let cursor = buffer[..cursor_byte].chars().count().min(chars.len());

    let to_grapheme = |char_idx: usize| -> usize {
        let byte = buffer
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(buffer.len());
        text_bounds::grapheme_index_after_byte(buffer, byte)
    };

    let mut braced_match = None;
    let mut idx = 0usize;
    while idx + 1 < chars.len() {
        if chars[idx] == '@' && chars[idx + 1] == '{' && is_mention_start_boundary(&chars, idx) {
            let content_start = idx + 2;
            let close = find_unescaped_brace_close(&chars, content_start);
            let token_end = close.map(|end| end + 1).unwrap_or(chars.len());
            if cursor >= idx && cursor <= token_end {
                let query_end = close.unwrap_or(token_end);
                let query: String = chars[content_start..query_end].iter().collect();
                braced_match = Some(MentionToken {
                    start: to_grapheme(idx),
                    end: to_grapheme(token_end),
                    query,
                });
            }
            idx = token_end.max(idx + 2);
        } else {
            idx += 1;
        }
    }
    if braced_match.is_some() {
        return braced_match;
    }

    if cursor > 0
        && chars
            .get(cursor - 1)
            .is_some_and(|ch| is_unbraced_mention_end_boundary(*ch))
    {
        return None;
    }
    let mut start = cursor;
    while start > 0 && !is_unbraced_mention_start_boundary(chars[start - 1]) {
        start -= 1;
    }
    let mut end = cursor;
    while end < chars.len() && !is_unbraced_mention_end_boundary(chars[end]) {
        end += 1;
    }
    if start >= end || chars.get(start) != Some(&'@') {
        return None;
    }
    let query: String = chars[start + 1..end].iter().collect();
    Some(MentionToken {
        start: to_grapheme(start),
        end: to_grapheme(end),
        query,
    })
}

fn is_mention_start_boundary(chars: &[char], idx: usize) -> bool {
    idx == 0
        || chars
            .get(idx - 1)
            .is_some_and(|ch| is_unbraced_mention_start_boundary(*ch))
}

fn is_unbraced_mention_start_boundary(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '(' | '[' | '{' | '<' | '"' | '\'' | '`')
}

fn is_unbraced_mention_end_boundary(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '>' | '"' | '\'' | '`'
        )
}

fn find_unescaped_brace_close(chars: &[char], start: usize) -> Option<usize> {
    let mut escaped = false;
    for (idx, ch) in chars.iter().enumerate().skip(start) {
        if escaped {
            escaped = false;
            continue;
        }
        match *ch {
            '\\' => escaped = true,
            '}' => return Some(idx),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background::{BackgroundTaskSnapshot, BackgroundTaskStatus};
    use crate::provider::ReasoningSelection;
    use crate::tui::app::AppState;
    use crate::tui::test_utils::{model_option, provider_options};
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn app_with_provider_choices(input: &str) -> AppState {
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        app.composer.set_text(input.to_string());
        app.provider_choices = provider_options();
        app
    }

    #[test]
    fn provider_arg_completion_replacement_uses_provider_id() {
        let app = app_with_provider_choices("/authorize Open");

        let completion = compute_completion(&app).expect("completion should be present");

        assert_eq!(completion.replacement, "/authorize opencode");
        assert_eq!(completion.replacement_range, None);
        assert!(matches!(
            completion.candidates.first(),
            Some(CompletionCandidate::Provider { id, display })
                if id == "opencode" && display == "OpenCode Go"
        ));
    }

    #[test]
    fn model_arg_completion_replacement_uses_model_name() {
        let mut app = app_with_provider_choices("/model q");
        app.cached_model_choices = vec![model_option("opencode", "OpenCode Go", "qwen3.7-max")];

        let completion = compute_completion(&app).expect("completion should be present");

        assert_eq!(completion.replacement, "/model qwen3.7-max");
        assert_eq!(completion.replacement_range, None);
        assert!(matches!(
            completion.candidates.first(),
            Some(CompletionCandidate::Model { provider_id, model, replacement, .. })
                if provider_id == "opencode" && model == "qwen3.7-max" && replacement == "qwen3.7-max"
        ));
    }

    #[test]
    fn model_arg_completion_prefers_connection_scoped_model_name() {
        let mut app = app_with_provider_choices("/model q");
        let mut option = model_option("opencode", "OpenCode Go", "qwen3.7-max");
        option.connection_id = "opencode".to_string();
        option.model_id = Some("opencode/qwen3.7-max".to_string());
        app.cached_model_choices = vec![option];

        let completion = compute_completion(&app).expect("completion should be present");

        assert_eq!(completion.replacement, "/model opencode:qwen3.7-max");
        assert!(matches!(
            completion.candidates.first(),
            Some(CompletionCandidate::Model { provider_id, model, replacement, .. })
                if provider_id == "opencode" && model == "qwen3.7-max" && replacement == "opencode:qwen3.7-max"
        ));
    }

    #[test]
    fn theme_arg_completion_replacement_uses_theme_name() {
        let app = app_with_provider_choices("/theme for");

        let completion = compute_completion(&app).expect("completion should be present");

        assert_eq!(completion.replacement, "/theme forest");
        assert!(matches!(
            completion.candidates.first(),
            Some(CompletionCandidate::Argument { label, detail, replacement })
                if label == "forest" && detail == "theme" && replacement == "/theme forest"
        ));
    }

    #[test]
    fn compact_arg_completion_suggests_preview() {
        let app = app_with_provider_choices("/compact pr");

        let completion = compute_completion(&app).expect("completion should be present");

        assert_eq!(completion.replacement, "/compact preview");
        assert!(matches!(
            completion.candidates.first(),
            Some(CompletionCandidate::Argument { label, detail, replacement })
                if label == "preview" && detail == "option" && replacement == "/compact preview"
        ));
    }

    #[test]
    fn copy_arg_completion_suggests_all() {
        let app = app_with_provider_choices("/copy a");

        let completion = compute_completion(&app).expect("completion should be present");

        assert_eq!(completion.replacement, "/copy all");
        assert!(matches!(
            completion.candidates.first(),
            Some(CompletionCandidate::Argument { label, detail, replacement })
                if label == "all" && detail == "scope" && replacement == "/copy all"
        ));
    }

    #[test]
    fn doctor_arg_completion_suggests_json() {
        let app = app_with_provider_choices("/doctor j");

        let completion = compute_completion(&app).expect("completion should be present");

        assert_eq!(completion.replacement, "/doctor json");
        assert!(matches!(
            completion.candidates.first(),
            Some(CompletionCandidate::Argument { label, detail, replacement })
                if label == "json" && detail == "format" && replacement == "/doctor json"
        ));
    }

    #[test]
    fn doctor_arg_completion_suggests_online_after_json() {
        let app = app_with_provider_choices("/doctor json o");

        let completion = compute_completion(&app).expect("completion should be present");

        assert_eq!(completion.replacement, "/doctor json online");
        assert!(matches!(
            completion.candidates.first(),
            Some(CompletionCandidate::Argument { label, detail, replacement })
                if label == "online"
                    && detail == "network probe"
                    && replacement == "/doctor json online"
        ));
    }

    #[test]
    fn resume_arg_completion_replacement_uses_session_id() {
        let mut app = app_with_provider_choices("/resume pol");
        app.session_choices = vec![session_summary(42, "Polish TUI", "")];

        let completion = compute_completion(&app).expect("completion should be present");

        assert_eq!(completion.replacement, "/resume 42");
        assert!(matches!(
            completion.candidates.first(),
            Some(CompletionCandidate::Argument { label, detail, replacement })
                if label == "#42" && detail == "Polish TUI" && replacement == "/resume 42"
        ));
    }

    #[test]
    fn forget_arg_completion_matches_session_id() {
        let mut app = app_with_provider_choices("/forget 77");
        app.session_choices = vec![session_summary(77, "Old session", "")];

        let completion = compute_completion(&app).expect("completion should be present");

        assert_eq!(completion.replacement, "/forget 77");
        assert!(matches!(
            completion.candidates.first(),
            Some(CompletionCandidate::Argument { label, replacement, .. })
                if label == "#77" && replacement == "/forget 77"
        ));
    }

    #[test]
    fn tasks_stop_arg_completion_replacement_uses_running_task_id() {
        let mut app = app_with_provider_choices("/tasks stop bg");
        app.background_tasks = vec![
            background_task("bg-1", BackgroundTaskStatus::Succeeded),
            background_task("bg-2", BackgroundTaskStatus::Running),
        ];

        let completion = compute_completion(&app).expect("completion should be present");

        assert_eq!(completion.replacement, "/tasks stop bg-2");
        assert!(matches!(
            completion.candidates.first(),
            Some(CompletionCandidate::Argument { label, detail, replacement })
                if label == "bg-2" && detail.contains("cargo test") && replacement == "/tasks stop bg-2"
        ));
    }

    #[test]
    fn tasks_arg_completion_suggests_stop_subcommand() {
        let app = app_with_provider_choices("/tasks st");

        let completion = compute_completion(&app).expect("completion should be present");

        assert_eq!(completion.replacement, "/tasks stop ");
        assert!(matches!(
            completion.candidates.first(),
            Some(CompletionCandidate::Argument { label, detail, replacement })
                if label == "stop" && detail == "subcommand" && replacement == "/tasks stop "
        ));
    }

    #[test]
    fn yolo_arg_completion_suggests_status() {
        let app = app_with_provider_choices("/yolo st");

        let completion = compute_completion(&app).expect("completion should be present");

        assert_eq!(completion.replacement, "/yolo status");
        assert!(matches!(
            completion.candidates.first(),
            Some(CompletionCandidate::Argument { label, detail, replacement })
                if label == "status" && detail == "mode" && replacement == "/yolo status"
        ));
    }

    #[test]
    fn autonomy_arg_completion_suggests_auto_accept() {
        let app = app_with_provider_choices("/autonomy au");

        let completion = compute_completion(&app).expect("completion should be present");

        assert_eq!(completion.replacement, "/autonomy auto-accept");
        assert!(matches!(
            completion.candidates.first(),
            Some(CompletionCandidate::Argument { label, detail, replacement })
                if label == "auto-accept" && detail == "level" && replacement == "/autonomy auto-accept"
        ));
    }

    #[test]
    fn sandbox_arg_completion_suggests_status() {
        let app = app_with_provider_choices("/sandbox st");

        let completion = compute_completion(&app).expect("completion should be present");

        assert_eq!(completion.replacement, "/sandbox status");
        assert!(matches!(
            completion.candidates.first(),
            Some(CompletionCandidate::Argument { label, detail, replacement })
                if label == "status" && detail == "option" && replacement == "/sandbox status"
        ));
    }

    #[test]
    fn sandbox_net_arg_completion_keeps_network_option_open() {
        let app = app_with_provider_choices("/sandbox n");

        let completion = compute_completion(&app).expect("completion should be present");

        assert_eq!(completion.replacement, "/sandbox net ");
        assert!(matches!(
            completion.candidates.first(),
            Some(CompletionCandidate::Argument { label, detail, replacement })
                if label == "net" && detail == "option" && replacement == "/sandbox net "
        ));

        let app = app_with_provider_choices(&completion.replacement);
        let network = compute_completion(&app).expect("network completion should be present");

        assert_eq!(network.replacement, "/sandbox net off");
        assert!(network.candidates.iter().any(|candidate| matches!(
            candidate,
            CompletionCandidate::Argument { label, detail, replacement }
                if label == "on" && detail == "network" && replacement == "/sandbox net on"
        )));
    }

    #[test]
    fn command_argument_hints_have_completion_or_are_free_form() {
        let completion_commands = [
            "/authorize",
            "/autonomy",
            "/compact",
            "/copy",
            "/doctor",
            "/forget",
            "/model",
            "/permissions",
            "/pure",
            "/resume",
            "/retry",
            "/sandbox",
            "/self-review",
            "/serenity",
            "/smol",
            "/tasks",
            "/theme",
            "/unauthorize",
            "/yolo",
        ];
        let free_form_commands = [
            "/agents",
            "/bug",
            "/config",
            "/export",
            "/hooks",
            "/mcp",
            "/memory",
            "/providers",
            "/remember",
            "/search",
            "/skill",
            "/skills",
            "/wizard",
        ];

        for command in crate::commands::COMMANDS
            .iter()
            .filter(|command| command.usage_hint.is_some())
        {
            assert!(
                completion_commands.contains(&command.name)
                    || free_form_commands.contains(&command.name),
                "{} has a usage hint but no argument completion classification",
                command.name
            );
        }
    }

    #[test]
    fn retry_arg_completion_suggests_assigned_model_shortcut() {
        let mut app = app_with_provider_choices("/retry f");
        let mut option = model_option("codex", "Codex", "gpt-5.5");
        option.shortcut_bindings = vec![(
            crate::model_role::ModelShortcutKey::new('f').unwrap(),
            ReasoningSelection::High,
        )];
        app.cached_model_choices = vec![option];

        let completion = compute_completion(&app).expect("completion should be present");

        assert_eq!(completion.replacement, "/retry f");
        assert!(matches!(
            completion.candidates.first(),
            Some(CompletionCandidate::Argument { label, detail, replacement })
                if label == "f"
                    && detail == "Codex · gpt-5.5 · high"
                    && replacement == "/retry f"
        ));
    }

    #[test]
    fn command_popup_filters_to_sessions_command() {
        let app = app_with_provider_choices("/ses");

        let completion = compute_completion(&app).expect("completion should be present");

        assert_eq!(completion.replacement, "/sessions");
        assert!(matches!(
            completion.candidates.first(),
            Some(CompletionCandidate::Command { label, description })
                if label == "/sessions" && description.contains("List saved sessions")
        ));
    }

    #[test]
    fn command_popup_pins_model_command_to_top() {
        // `/mode` is an exact match and would normally rank first, but `/model`
        // is pinned above every other result whenever it appears.
        let app = app_with_provider_choices("/mode");

        let completion = compute_completion(&app).expect("completion should be present");

        assert!(matches!(
            completion.candidates.first(),
            Some(CompletionCandidate::Command { label, .. }) if label == "/model"
        ));
        assert_eq!(completion.replacement, "/model");
        // The pinned command does not crowd out the other matches.
        assert!(completion.candidates.iter().any(|candidate| matches!(
            candidate,
            CompletionCandidate::Command { label, .. } if label == "/mode"
        )));
    }

    #[test]
    fn command_popup_does_not_list_dynamic_model_shortcuts() {
        // One-letter model shortcuts are resolved by the command dispatcher, not
        // by static command metadata, so the popup must not advertise them.
        for cmd in ["/c", "/s", "/f"] {
            let app = app_with_provider_choices(cmd);
            let completion = compute_completion(&app).expect("completion should be present");
            assert!(
                completion.candidates.iter().all(|candidate| !matches!(
                    candidate,
                    CompletionCandidate::Command { label, .. } if label == cmd
                )),
                "typing {cmd} should not list {cmd}",
            );
        }
    }

    #[test]
    fn command_popup_filters_to_resume_command() {
        let app = app_with_provider_choices("/res");

        let completion = compute_completion(&app).expect("completion should be present");

        assert_eq!(completion.replacement, "/resume");
        assert!(matches!(
            completion.candidates.first(),
            Some(CompletionCandidate::Command { label, description })
                if label == "/resume" && description.contains("Resume")
        ));
    }

    fn session_summary(id: i64, name: &str, summary: &str) -> crate::storage::SessionSummary {
        crate::storage::SessionSummary {
            id: crate::storage::SessionId::from_raw(id),
            project_path: "/tmp/project".to_string(),
            name: name.to_string(),
            summary: summary.to_string(),
            provider_id: "codex".to_string(),
            model: "openai/gpt-5.5".to_string(),
            reasoning: ReasoningSelection::default(),
            status: crate::storage::SessionStatus::Completed,
            terminal_reason: None,
            updated_at_ms: 0,
            message_count: 0,
            prompt_token_count: 0,
            completion_token_count: 0,
            cache_read_input_token_count: 0,
            cache_creation_input_token_count: 0,
            cache_measured_input_token_count: 0,
            cost_micros: 0,
            no_cache_cost_micros: 0,
            source_plan_id: None,
        }
    }

    fn background_task(id: &str, status: BackgroundTaskStatus) -> BackgroundTaskSnapshot {
        BackgroundTaskSnapshot {
            id: id.to_string(),
            incarnation: "test-task".to_string(),
            command: "cargo test".to_string(),
            cwd: PathBuf::from("/tmp/project"),
            status,
            started_at: SystemTime::UNIX_EPOCH,
            finished_at: None,
            exit_code: None,
            timeout_secs: 30,
            timed_out: false,
            tail: String::new(),
            tail_truncated: false,
            total_output_chars: 0,
            version: 1,
            tool_call_id: None,
        }
    }

    #[test]
    fn mention_token_detects_mid_sentence_path() {
        let token = mention_token_at("please read @src/ma now", 19).expect("mention");

        assert_eq!(
            token,
            MentionToken {
                start: 12,
                end: 19,
                query: "src/ma".to_string(),
            }
        );
    }

    #[test]
    fn mention_token_detects_parenthesized_path() {
        let input = "please check (@src/ma), now";
        let start = input.find('@').expect("test input has mention");
        let end = start + "@src/ma".len();
        let token = mention_token_at(input, end).expect("mention");

        assert_eq!(
            token,
            MentionToken {
                start,
                end,
                query: "src/ma".to_string(),
            }
        );
    }

    #[test]
    fn mention_token_returns_grapheme_indices_past_combining_marks() {
        // "é " where é is `e` + combining acute: 2 chars but 1 grapheme. The
        // returned range must be in grapheme space (start=2, end=9), not char
        // space (which would be off by one and corrupt `replace_range`).
        let input = "e\u{301} @src/ma";
        let token = mention_token_at(input, 9).expect("mention");

        assert_eq!(
            token,
            MentionToken {
                start: 2,
                end: 9,
                query: "src/ma".to_string(),
            }
        );
    }

    #[test]
    fn mention_token_ignores_email_addresses() {
        let input = "mail person@example.com";

        assert!(mention_token_at(input, input.len()).is_none());
    }

    #[test]
    fn mention_token_detects_braced_path_with_spaces() {
        let token = mention_token_at("look @{docs/my file", 18).expect("mention");

        assert_eq!(token.start, 5);
        assert_eq!(token.end, 19);
        assert_eq!(token.query, "docs/my file");
    }

    #[test]
    fn path_replacement_braces_paths_with_spaces() {
        assert_eq!(
            path_replacement("docs/my file.md", PathKind::File),
            "@{docs/my file.md}"
        );
        assert_eq!(
            path_replacement("docs/my folder", PathKind::Directory),
            "@{docs/my folder/}"
        );
    }
}
