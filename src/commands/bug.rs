//! `/bug` — build a local, user-reviewed support bundle.
//!
//! Nothing is ever transmitted: the bundle is one self-contained markdown
//! file under `$BONSAI_HOME/support/`, reviewed section-by-section before a
//! byte is written. Raw prompts, code, credentials, and environment values are
//! never included; the session-log tail is off by default and every included
//! section passes [`crate::redact`] — with a whole-document
//! [`crate::redact::first_secret`] gate as the final safety net.

use std::path::{Path, PathBuf};

use crate::interaction::QuestionOption;
use crate::storage::AuthorizationDecisionRecord;

/// The public repository, once it exists. `None` keeps `/bug` fully local:
/// the closing message explains the repo is not public yet and the bundle
/// file is the artifact to keep. Set to `Some("https://github.com/<owner>/bonsai")`
/// when the repository goes public — the closing message then links a
/// prefilled new-issue URL to attach the bundle to.
pub(crate) const GITHUB_NEW_ISSUE_BASE: Option<&str> = None;

/// How many trailing lines of a log file a bundle may embed. Hard cap: log
/// tails are the riskiest sections, so they stay small and reviewable.
pub(crate) const LOG_TAIL_LINES: usize = 200;

/// One reviewable bundle section. This enum is the single source of truth for
/// the review modal's options AND the rendering order, so a checkbox index can
/// never drift from the section it includes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BundleSection {
    Doctor,
    CompletionReport,
    Authorization,
    UsageBudget,
    SessionLogTail,
    LifecycleTail,
}

impl BundleSection {
    const ALL: [Self; 6] = [
        Self::Doctor,
        Self::CompletionReport,
        Self::Authorization,
        Self::UsageBudget,
        Self::SessionLogTail,
        Self::LifecycleTail,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Doctor => "Doctor report",
            Self::CompletionReport => "Last run summary",
            Self::Authorization => "Authorization decisions",
            Self::UsageBudget => "Usage & budgets",
            Self::SessionLogTail => "Session log tail",
            Self::LifecycleTail => "Lifecycle log tail",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Doctor => "environment and health checks (redacted)",
            Self::CompletionReport => "status, files changed, verification of the last run",
            Self::Authorization => "recent allow/deny decisions (no command output)",
            Self::UsageBudget => "token/cost totals and budget state",
            Self::SessionLogTail => "last 200 redacted log lines — off unless you opt in",
            Self::LifecycleTail => "last 200 lines of the opt-in support log",
        }
    }

    /// The exclusion contract: unrestricted-ish logs require an explicit
    /// check; everything else defaults on.
    fn default_on(self) -> bool {
        !matches!(self, Self::SessionLogTail)
    }
}

/// The sections offered for review, in render order. The lifecycle tail is
/// only offered when the support log is enabled (otherwise there is no file).
pub(crate) fn offered_sections(support_log_enabled: bool) -> Vec<BundleSection> {
    BundleSection::ALL
        .into_iter()
        .filter(|section| support_log_enabled || *section != BundleSection::LifecycleTail)
        .collect()
}

/// The review modal's options for `sections`, defaults pre-checked.
pub(crate) fn question_options(sections: &[BundleSection]) -> Vec<QuestionOption> {
    sections
        .iter()
        .map(|section| {
            let mut option = QuestionOption::new(section.label(), section.description());
            option.preselected = section.default_on();
            option
        })
        .collect()
}

/// Map the review modal's answer indices back onto sections. Out-of-range
/// indices are ignored (the modal cannot produce them; defensive anyway).
pub(crate) fn sections_from_choices(
    sections: &[BundleSection],
    choices: &[usize],
) -> Vec<BundleSection> {
    choices
        .iter()
        .filter_map(|index| sections.get(*index).copied())
        .collect()
}

/// Everything a bundle can embed, as plain data — constructible in tests
/// without a live agent. `None`/empty fields render as an honest "not
/// available" note rather than being silently omitted.
#[derive(Default)]
pub(crate) struct BundleInputs {
    pub(crate) description: String,
    pub(crate) doctor: Option<crate::doctor::DoctorReport>,
    pub(crate) completion: Option<crate::completion_report::CompletionReport>,
    pub(crate) authorization: Vec<AuthorizationDecisionRecord>,
    pub(crate) session_budget: Option<crate::run_budget::SessionBudgetUsage>,
    pub(crate) session_log_tail: Option<String>,
    pub(crate) lifecycle_tail: Option<String>,
}

/// Render the bundle markdown. Every section body is redacted individually,
/// and the assembled document passes a [`crate::redact::first_secret`] gate:
/// if a known secret shape still survives (say, inside machine JSON the
/// per-field pass missed), the whole document is re-redacted and annotated so
/// the user knows to double-check before sharing.
pub(crate) fn render_bundle(inputs: &BundleInputs, included: &[BundleSection]) -> String {
    let mut out = String::new();
    out.push_str("# bonsai bug report\n\n");
    let description = crate::redact::redact(&inputs.description);
    out.push_str(&format!("## Description\n\n{description}\n\n"));
    out.push_str(&format!(
        "## Environment\n\n- bonsai: {}\n- os: {}\n- arch: {}\n\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    ));

    for section in BundleSection::ALL {
        if !included.contains(&section) {
            continue;
        }
        out.push_str(&format!("## {}\n\n", section.label()));
        let body = section_body(section, inputs)
            .unwrap_or_else(|| "_not available for this session_".to_string());
        let body = crate::redact::redact(&body);
        out.push_str(&body);
        out.push_str("\n\n");
    }

    if crate::redact::first_secret(&out).is_some() {
        let mut masked = crate::redact::redact(&out).into_owned();
        masked.push_str(
            "\n> NOTE: a credential-like value survived section redaction and was masked \
             again at the document level. Please review before sharing.\n",
        );
        return masked;
    }
    out
}

fn section_body(section: BundleSection, inputs: &BundleInputs) -> Option<String> {
    match section {
        BundleSection::Doctor => inputs.doctor.as_ref().and_then(|report| {
            serde_json::to_string_pretty(report)
                .ok()
                .map(|json| format!("```json\n{json}\n```"))
        }),
        BundleSection::CompletionReport => inputs.completion.as_ref().and_then(|report| {
            serde_json::to_string_pretty(report)
                .ok()
                .map(|json| format!("```json\n{json}\n```"))
        }),
        BundleSection::Authorization => (!inputs.authorization.is_empty()).then(|| {
            let mut body = String::from(
                "| when (ms) | surface | subject | decision | reason |\n|---|---|---|---|---|\n",
            );
            for record in &inputs.authorization {
                body.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    record.created_at_ms,
                    record.surface,
                    record.subject.replace('|', "\\|"),
                    record.decision,
                    record.reason.replace('|', "\\|"),
                ));
            }
            body
        }),
        BundleSection::UsageBudget => inputs.session_budget.as_ref().and_then(|budget| {
            serde_json::to_string_pretty(budget)
                .ok()
                .map(|json| format!("```json\n{json}\n```"))
        }),
        BundleSection::SessionLogTail => inputs
            .session_log_tail
            .as_ref()
            .map(|tail| format!("```\n{tail}\n```")),
        BundleSection::LifecycleTail => inputs
            .lifecycle_tail
            .as_ref()
            .map(|tail| format!("```\n{tail}\n```")),
    }
}

/// Last [`LOG_TAIL_LINES`] lines of this process's session file with `suffix`
/// (`.log` / `.jsonl`), matched by pid so a concurrent peer session's file is
/// never bundled by mistake.
pub(crate) fn own_session_log_tail(home_dir: &Path, suffix: &str) -> Option<String> {
    let marker = format!("-{}{suffix}", std::process::id());
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(home_dir.join("logs"))
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("bonsai-") && name.ends_with(&marker))
        })
        .collect();
    candidates.sort_unstable();
    let newest = candidates.pop()?;
    let content = std::fs::read_to_string(newest).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(LOG_TAIL_LINES);
    Some(lines[start..].join("\n"))
}

/// Last [`LOG_TAIL_LINES`] lines of the newest session file with `suffix`,
/// regardless of pid — the standalone CLI runs as its own process, so the
/// session being reported on is whichever ran most recently.
fn newest_session_log_tail(home_dir: &Path, suffix: &str) -> Option<String> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(home_dir.join("logs"))
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("bonsai-") && name.ends_with(suffix))
        })
        .collect();
    candidates.sort_unstable();
    let newest = candidates.pop()?;
    let content = std::fs::read_to_string(newest).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(LOG_TAIL_LINES);
    Some(lines[start..].join("\n"))
}

/// The `bonsai bug` CLI flow: no live agent, no session, no modal — an
/// offline doctor report plus (with `--include-log`) the newest session-log
/// tail, written with the default sections. Prints nothing itself; the caller
/// prints the returned closing lines.
pub(crate) async fn run_standalone(
    description: &str,
    include_log: bool,
) -> anyhow::Result<Vec<String>> {
    let project_root =
        std::env::current_dir().map_err(|err| anyhow::anyhow!("Cannot resolve cwd: {err}"))?;
    let doctor =
        crate::doctor::collect_standalone(&project_root, crate::doctor::DoctorNetworkMode::Offline)
            .await;
    let home_dir = crate::storage::BonsaiPaths::discover()?
        .home_dir()
        .to_path_buf();

    let session_log_tail = include_log
        .then(|| newest_session_log_tail(&home_dir, ".log"))
        .flatten();
    let lifecycle_tail = newest_session_log_tail(&home_dir, ".jsonl");
    let mut included = vec![BundleSection::Doctor];
    if session_log_tail.is_some() {
        included.push(BundleSection::SessionLogTail);
    }
    if lifecycle_tail.is_some() {
        included.push(BundleSection::LifecycleTail);
    }
    let inputs = BundleInputs {
        description: description.to_string(),
        doctor: Some(doctor),
        session_log_tail,
        lifecycle_tail,
        ..BundleInputs::default()
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    let path = bundle_path(&home_dir, now_ms)?;
    std::fs::write(&path, render_bundle(&inputs, &included))?;
    Ok(closing_messages(description, &path))
}

/// `$BONSAI_HOME/support/bug-<YYYYMMDD>T<HHMMSS>Z.md`, directory created.
pub(crate) fn bundle_path(home_dir: &Path, now_ms: u64) -> std::io::Result<PathBuf> {
    let dir = home_dir.join("support");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("bug-{}.md", format_utc_compact(now_ms))))
}

/// Epoch millis → `YYYYMMDDTHHMMSSZ`. Civil-from-days per Howard Hinnant's
/// algorithm — no date dependency for one filename.
pub(crate) fn format_utc_compact(now_ms: u64) -> String {
    let secs = now_ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };

    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// The prefilled new-issue URL, once the repo is public.
pub(crate) fn issue_url(description: &str, bundle_path: &Path) -> Option<String> {
    let base = GITHUB_NEW_ISSUE_BASE?;
    let title: String = description.chars().take(80).collect();
    let body = format!(
        "<!-- bonsai {} on {}/{} -->\n\nPlease drag the bundle file into this issue:\n`{}`\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        bundle_path.display(),
    );
    reqwest::Url::parse_with_params(
        &format!("{base}/issues/new"),
        [("title", title.as_str()), ("body", body.as_str())],
    )
    .ok()
    .map(String::from)
}

/// The user-facing closing lines after a bundle is written.
pub(crate) fn closing_messages(description: &str, path: &Path) -> Vec<String> {
    let mut messages = vec![format!("Bug bundle written to {}.", path.display())];
    match issue_url(description, path) {
        Some(url) => messages.push(format!(
            "File the report by opening {url} and attaching the bundle file."
        )),
        None => messages.push(
            "The bonsai repository is not public yet — keep this file and attach it \
             wherever you report the issue."
                .to_string(),
        ),
    }
    messages
}

/// The full `/bug` flow for surfaces running through the shared command
/// handler: gather (offline doctor + session evidence), review (the generic
/// multi-select question when an interactive surface exists; defaults
/// otherwise), render, write. Returns the user-facing closing lines, or the
/// cancel notice — the only error is failing to write the file.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_bug_flow(
    description: &str,
    agent: &crate::agent::Agent,
    storage: Option<&crate::storage::Storage>,
    catalog: Option<&crate::model_catalog::ModelCatalog>,
    session_store: &crate::session::SessionStore,
    registry: &crate::provider::ProviderRegistry,
    project_root: &Path,
) -> Result<Vec<String>, String> {
    // Gather first: the review modal should describe what actually exists.
    let sandbox = agent.sandbox();
    let doctor = Some(
        crate::doctor::collect(crate::doctor::DoctorContext {
            project_root,
            storage,
            catalog,
            catalog_loaded_cleanly: catalog.is_some(),
            config: agent.config(),
            session_store: Some(session_store),
            registry: Some(registry),
            sandbox: &sandbox,
            network_mode: crate::doctor::DoctorNetworkMode::Offline,
            mcp_reachability: None,
            release_status: None,
        })
        .await,
    );
    let session_id = agent.parent_session_id();
    let (completion, authorization) = match (storage, session_id) {
        (Some(storage), Some(session_id)) => (
            storage
                .latest_completion_report(session_id)
                .await
                .unwrap_or(None),
            storage
                .recent_authorization_decisions(session_id, 20)
                .await
                .unwrap_or_default(),
        ),
        _ => (None, Vec::new()),
    };

    let support_log_enabled = crate::logging::support_log_enabled();
    let offered = offered_sections(support_log_enabled);
    let included = match agent.interaction() {
        Some(interaction) => {
            let options = question_options(&offered);
            let prompt = format!(
                "Review what the bug bundle for \"{description}\" will include. \
                 Nothing is sent anywhere — the bundle is a local file you attach \
                 yourself. Space toggles a section, Enter writes the bundle, Esc \
                 cancels."
            );
            match interaction
                .request(
                    |request_id| crate::interaction::InteractionRequest::Question {
                        request_id,
                        prompt,
                        header: Some("Bug bundle review".to_string()),
                        options,
                        multiple: true,
                        origin: None,
                    },
                )
                .await
            {
                Ok(crate::interaction::InteractionOutcome::Question(Some(
                    crate::interaction::InteractionAnswer::Choices(choices),
                ))) => sections_from_choices(&offered, &choices),
                Ok(crate::interaction::InteractionOutcome::Question(None))
                | Err(crate::interaction::InteractionStatus::Cancelled) => {
                    return Ok(vec![
                        "Bug report cancelled; nothing was written.".to_string(),
                    ]);
                }
                // Noninteractive surfaces (and any unexpected outcome shape)
                // degrade to the defaults rather than failing the report.
                _ => offered
                    .iter()
                    .copied()
                    .filter(|section| section.default_on())
                    .collect(),
            }
        }
        None => offered
            .iter()
            .copied()
            .filter(|section| section.default_on())
            .collect(),
    };

    let home_dir = storage.map(|storage| storage.home_dir().to_path_buf());
    let inputs = BundleInputs {
        description: description.to_string(),
        doctor,
        completion,
        authorization,
        session_budget: Some(agent.session_budget_usage()),
        session_log_tail: home_dir.as_deref().and_then(|home| {
            included
                .contains(&BundleSection::SessionLogTail)
                .then(|| own_session_log_tail(home, ".log"))
                .flatten()
        }),
        lifecycle_tail: home_dir.as_deref().and_then(|home| {
            included
                .contains(&BundleSection::LifecycleTail)
                .then(|| own_session_log_tail(home, ".jsonl"))
                .flatten()
        }),
    };

    let home_dir = match home_dir {
        Some(home_dir) => home_dir,
        None => crate::storage::BonsaiPaths::discover()
            .map_err(|err| format!("Cannot locate the bonsai home directory: {err:#}"))?
            .home_dir()
            .to_path_buf(),
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    let path = bundle_path(&home_dir, now_ms)
        .map_err(|err| format!("Cannot create the support directory: {err}"))?;
    let rendered = render_bundle(&inputs, &included);
    std::fs::write(&path, rendered)
        .map_err(|err| format!("Cannot write {}: {err}", path.display()))?;

    Ok(closing_messages(description, &path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs_with_secrets() -> BundleInputs {
        let secret = "token sk-ant-api03-abcdefghijklmnopqrstuvwx1234567890abcdefghijklmnopqrstuvwx1234-abcdAA";
        BundleInputs {
            description: format!("it broke; my key is {secret}"),
            doctor: None,
            completion: None,
            authorization: vec![AuthorizationDecisionRecord {
                id: 1,
                tool_call_id: None,
                surface: "bash".to_string(),
                subject: format!("curl -H 'Authorization: Bearer {secret}'"),
                effects: vec!["network".to_string()],
                risk_tier: "high".to_string(),
                rule_source: "none".to_string(),
                autonomy_level: "balanced".to_string(),
                sandbox_posture: "enabled".to_string(),
                decision: "allow".to_string(),
                reason: "user approved".to_string(),
                created_at_ms: 1,
            }],
            session_budget: None,
            session_log_tail: Some(format!("line one\nAUTH={secret}\nline three")),
            lifecycle_tail: None,
        }
    }

    #[test]
    fn seeded_secrets_never_survive_rendering() {
        let inputs = inputs_with_secrets();
        let all = offered_sections(true);
        let rendered = render_bundle(&inputs, &all);
        assert!(
            !rendered.contains("sk-ant-"),
            "secret leaked into the bundle: {rendered}"
        );
        assert!(rendered.contains("## Description"));
        assert!(rendered.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn empty_selection_renders_the_minimal_bundle() {
        let inputs = BundleInputs {
            description: "just the facts".to_string(),
            ..BundleInputs::default()
        };
        let rendered = render_bundle(&inputs, &[]);
        assert!(rendered.contains("just the facts"));
        assert!(rendered.contains("## Environment"));
        assert!(
            !rendered.contains("## Doctor report"),
            "no section may render without being selected"
        );
    }

    #[test]
    fn option_indices_map_back_onto_their_sections() {
        let sections = offered_sections(false);
        assert!(
            !sections.contains(&BundleSection::LifecycleTail),
            "no lifecycle section without the opt-in log"
        );
        let options = question_options(&sections);
        assert_eq!(options.len(), sections.len());
        // The session-log tail is the one default-off option.
        let log_index = sections
            .iter()
            .position(|section| *section == BundleSection::SessionLogTail)
            .unwrap();
        for (index, option) in options.iter().enumerate() {
            assert_eq!(option.preselected, index != log_index, "{}", option.label);
        }

        let picked = sections_from_choices(&sections, &[0, log_index, 99]);
        assert_eq!(
            picked,
            vec![BundleSection::Doctor, BundleSection::SessionLogTail]
        );
    }

    #[test]
    fn utc_filename_format_is_stable() {
        // 2026-07-17 15:30:05 UTC.
        assert_eq!(format_utc_compact(1_784_302_205_000), "20260717T153005Z");
        // Epoch and a leap-year date.
        assert_eq!(format_utc_compact(0), "19700101T000000Z");
        assert_eq!(format_utc_compact(1_709_164_800_000), "20240229T000000Z");
    }

    #[test]
    fn issue_url_is_none_while_private_and_encodes_when_set() {
        // The const is None while the repo is private; closing messages degrade.
        let messages = closing_messages("spaces & newlines", Path::new("/tmp/b.md"));
        assert_eq!(messages.len(), 2);
        if GITHUB_NEW_ISSUE_BASE.is_none() {
            assert!(messages[1].contains("not public yet"));
        } else {
            assert!(messages[1].contains("/issues/new?"));
        }
    }
}
