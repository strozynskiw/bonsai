mod agent;
mod background;
mod background_wake;
mod bootstrap;
mod cli;
mod commands;
mod completion_report;
mod completions;
mod config;
mod context;
mod context_view;
mod copy;
mod db_enum;
mod diff;
mod doctor;
mod episode;
mod eval;
mod extension;
mod headless;
mod hooks;
mod interaction;
mod logging;
mod lsp;
mod mcp;
mod memory;
mod mention;
mod model_catalog;
mod model_resolution;
mod model_role;
mod onboarding;
mod output;
mod peer;
mod permissions;
mod plan;
mod process_group;
mod provider;
mod recovery;
mod redact;
mod release;
mod resource;
mod review;
mod run_budget;
mod runtime;
mod sandbox;
mod self_review;
mod session;
mod session_activity;
mod session_persist;
mod smol;
mod storage;
mod subagent;
mod symbol;
mod task_intent;
mod terminal;
mod todo;
mod tool;
mod tui;
mod update;
mod util;
mod verification;
mod workspace_trust;
mod yolo;

use crate::cli::CliMode;
use std::future::Future;
use std::time::{Duration, Instant};

const TOKIO_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

fn main() -> anyhow::Result<()> {
    if dotenv_enabled(std::env::var("BONSAI_DOTENV").ok().as_deref()) {
        let _ = dotenvy::dotenv();
    }
    let args: Vec<String> = std::env::args().collect();
    let cli_mode = match cli::parse_cli_args(&args) {
        Ok(mode) => mode,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(2);
        }
    };

    match cli_mode {
        CliMode::Run {
            resume,
            isolation,
            accessibility,
        } => {
            logging::init_tui_tracing();
            block_on_with_shutdown(recovery::run_tui(resume, isolation, accessibility))?
        }
        CliMode::Print { config } => {
            logging::init_headless_tracing();
            match block_on_with_shutdown(headless::run(config))? {
                Ok(outcome) => {
                    let code = outcome.exit_code();
                    if code != 0 {
                        std::process::exit(code);
                    }
                    Ok(())
                }
                Err(err) => {
                    std::process::exit(err.exit_code());
                }
            }
        }
        CliMode::Eval { config } => {
            logging::init_tracing();
            match block_on_with_shutdown(eval::run(config))? {
                Ok(outcome) if outcome.should_fail_process() => std::process::exit(1),
                Ok(_) => {}
                Err(err) => {
                    eprintln!("{err:#}");
                    std::process::exit(1);
                }
            }
            Ok(())
        }
        CliMode::EvalAdapter { command } => {
            logging::init_tracing();
            match block_on_with_shutdown(eval::execute_adapter(command))? {
                Ok(outcome) if outcome.should_fail_process() => std::process::exit(1),
                Ok(_) => Ok(()),
                Err(err) => {
                    eprintln!("{err:#}");
                    std::process::exit(1);
                }
            }
        }
        CliMode::ModelCatalogCheck { path } => {
            logging::init_tracing();
            let catalog = model_catalog::load_models_dev_cache(&path)?;
            println!("Models.dev catalog ok: {} models", catalog.len());
            Ok(())
        }
        CliMode::Bug {
            description,
            include_log,
        } => {
            logging::init_tracing();
            match block_on_with_shutdown(commands::bug::run_standalone(&description, include_log))?
            {
                Ok(message) => {
                    println!("{message}");
                    Ok(())
                }
                Err(err) => {
                    eprintln!("{err:#}");
                    std::process::exit(1);
                }
            }
        }
        CliMode::Doctor { request } => {
            logging::init_headless_tracing();
            let project_root = std::env::current_dir()?;
            let report =
                block_on_with_shutdown(doctor::collect_standalone(&project_root, request.network))?;
            println!("{}", report.render(request.format)?);
            if report.has_failures() {
                std::process::exit(1);
            }
            Ok(())
        }
        CliMode::Recovery { command } => {
            logging::init_tracing();
            match block_on_with_shutdown(recovery::execute(command))? {
                Ok(()) => Ok(()),
                Err(err) => {
                    eprintln!("{err:#}");
                    std::process::exit(1);
                }
            }
        }
        CliMode::Completions { shell } => {
            print!("{}", completions::render(shell));
            Ok(())
        }
        CliMode::Update { check_only } => {
            logging::init_headless_tracing();
            let paths = storage::BonsaiPaths::discover()?;
            let project_root = std::env::current_dir()?;
            // Project config stays inert here: update policy is a user-machine
            // concern and `bonsai update` may run in an untrusted checkout.
            let config = config::load_without_project_config(&project_root, paths.home_dir());
            let outcome = block_on_with_shutdown(update::run_forced_update(
                paths.home_dir(),
                &config.update,
                check_only,
            ))?;
            report_update_outcome(&outcome);
            Ok(())
        }
        CliMode::Help => {
            println!("{}", cli::help_text());
            Ok(())
        }
        CliMode::Version => {
            println!("bonsai {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

/// One plain line per outcome; failures exit non-zero so scripts can tell.
fn report_update_outcome(outcome: &update::UpdateOutcome) {
    use update::{NotifyReason, UpdateOutcome};
    match outcome {
        UpdateOutcome::Staged { version } => {
            println!("updated to v{version} — restart bonsai to apply");
        }
        UpdateOutcome::AlreadyStaged { version } => {
            println!("update v{version} is already staged — restart bonsai to apply");
        }
        UpdateOutcome::AlreadyCurrent => {
            println!("already up to date (v{})", env!("CARGO_PKG_VERSION"));
        }
        UpdateOutcome::DevBuild => {
            println!("development build — self-update is disabled");
        }
        // Forced runs bypass mode/interval, so these two are unreachable in
        // practice; keep the output self-explanatory if that ever changes.
        UpdateOutcome::Disabled => {
            println!("self-update is disabled by `[update] mode = \"off\"` in config.toml");
        }
        UpdateOutcome::TooSoon => {
            println!("update check skipped (checked recently)");
        }
        UpdateOutcome::Busy => {
            println!("another bonsai session is updating; try again shortly");
        }
        UpdateOutcome::NotifyOnly { version, reason } => {
            let hint = match reason {
                NotifyReason::Notify => "run `bonsai update` to install it",
                NotifyReason::Homebrew => "install it with `brew upgrade bonsai`",
                NotifyReason::NotWritable => {
                    "the install location is not writable; rerun the documented installer"
                }
                NotifyReason::SelfHashMismatch => {
                    "the installed binary does not match its signed manifest; reinstall via install.sh"
                }
                NotifyReason::Pinned => "held back by `[update] pin` in config.toml",
            };
            println!("signed release v{version} is available — {hint}");
        }
        UpdateOutcome::CheckFailed => {
            eprintln!("update check failed: could not reach GitHub releases");
            std::process::exit(1);
        }
        UpdateOutcome::VerificationFailed => {
            eprintln!(
                "update verification failed: fetched release metadata did not match a valid signed manifest"
            );
            std::process::exit(1);
        }
    }
}

fn dotenv_enabled(value: Option<&str>) -> bool {
    !value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    })
}

fn block_on_with_shutdown<F>(future: F) -> anyhow::Result<F::Output>
where
    F: Future,
{
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let output = runtime.block_on(future);
    let started_at = Instant::now();
    runtime.shutdown_timeout(TOKIO_SHUTDOWN_TIMEOUT);
    let elapsed = started_at.elapsed();
    if elapsed >= TOKIO_SHUTDOWN_TIMEOUT.saturating_sub(Duration::from_millis(10)) {
        tracing::warn!(
            elapsed_ms = elapsed.as_millis() as u64,
            timeout_ms = TOKIO_SHUTDOWN_TIMEOUT.as_millis() as u64,
            "tokio runtime shutdown reached timeout"
        );
    } else {
        tracing::info!(
            elapsed_ms = elapsed.as_millis() as u64,
            timeout_ms = TOKIO_SHUTDOWN_TIMEOUT.as_millis() as u64,
            "tokio runtime shutdown completed"
        );
    }
    // The last line before `main` returns and libc runs process teardown. If the
    // shell prompt returns noticeably after this timestamp, the remaining delay
    // is OS-level (reaping child processes, atexit) and not our shutdown code —
    // which the earlier per-phase lines have already accounted for.
    tracing::info!("process exiting");
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::dotenv_enabled;

    #[test]
    fn benchmark_can_disable_repository_dotenv_loading() {
        for value in ["0", "false", "off", "NO", " off "] {
            assert!(!dotenv_enabled(Some(value)), "{value}");
        }
        assert!(dotenv_enabled(None));
        assert!(dotenv_enabled(Some("on")));
    }
}
