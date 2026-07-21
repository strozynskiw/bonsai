use std::time::Duration;

use crate::completions::CompletionShell;
use crate::doctor::{DoctorNetworkMode, DoctorOutputFormat, DoctorRequest};
use crate::headless::{HeadlessConfig, OutputFormat, PromptSource};
use crate::recovery::{RecoveryCommand, RecoveryMode};
use crate::storage::RecoveryId;
use crate::tool::ApprovalLevel;

/// What `-c`/`--continue` asks the TUI to resume on boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeTarget {
    /// `-c` with no id — the most recent prior session for this project.
    Latest,
    /// `-c <id>` — a specific session by id.
    Session(i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CliMode {
    Run {
        resume: Option<ResumeTarget>,
        isolation: RecoveryMode,
        accessibility: crate::tui::accessibility::TuiAccessibility,
    },
    Print {
        config: HeadlessConfig,
    },
    Eval {
        config: crate::eval::EvalCliConfig,
    },
    EvalAdapter {
        command: crate::eval::AdapterCliCommand,
    },
    ModelCatalogCheck {
        path: std::path::PathBuf,
    },
    Doctor {
        request: DoctorRequest,
    },
    /// `bonsai bug` — build a local support bundle without a TUI session.
    Bug {
        description: String,
        include_log: bool,
    },
    Recovery {
        command: RecoveryCommand,
    },
    Completions {
        shell: CompletionShell,
    },
    /// `bonsai update [--check]` — run the native self-update now, ignoring
    /// the startup interval; `--check` reports without installing.
    Update {
        check_only: bool,
    },
    Help,
    Version,
}

pub(crate) fn parse_cli_args(args: &[String]) -> anyhow::Result<CliMode> {
    if args.len() == 1 {
        return Ok(CliMode::Run {
            resume: None,
            isolation: RecoveryMode::Off,
            accessibility: crate::tui::accessibility::TuiAccessibility::default(),
        });
    }

    if args.len() == 2 {
        match args[1].as_str() {
            "--help" | "-h" => return Ok(CliMode::Help),
            "--version" | "-V" => return Ok(CliMode::Version),
            _ => {}
        }
    }

    if args.get(1).is_some_and(|arg| arg == "eval") {
        if args.get(2).is_some_and(|arg| arg == "adapter") {
            if args.len() == 4 && matches!(args[3].as_str(), "--help" | "-h") {
                return Ok(CliMode::Help);
            }
            return Ok(CliMode::EvalAdapter {
                command: parse_eval_adapter_args(&args[3..])?,
            });
        }
        if args.len() == 3 && matches!(args[2].as_str(), "--help" | "-h") {
            return Ok(CliMode::Help);
        }
        return Ok(CliMode::Eval {
            config: parse_eval_args(&args[2..])?,
        });
    }

    if args.get(1).is_some_and(|arg| arg == "model-catalog") {
        if args.len() == 3 && matches!(args[2].as_str(), "--help" | "-h") {
            return Ok(CliMode::Help);
        }
        if args.get(2).is_some_and(|arg| arg == "check") && args.len() == 4 {
            return Ok(CliMode::ModelCatalogCheck {
                path: std::path::PathBuf::from(&args[3]),
            });
        }
        anyhow::bail!("Usage: bonsai model-catalog check <models-dev.json>");
    }

    if args.get(1).is_some_and(|arg| arg == "completions") {
        if args.len() == 3 && matches!(args[2].as_str(), "--help" | "-h") {
            return Ok(CliMode::Help);
        }
        let shell = match args {
            [_, _, shell] => CompletionShell::parse(shell),
            _ => None,
        }
        .ok_or_else(|| anyhow::anyhow!("Usage: bonsai completions bash|zsh|fish"))?;
        return Ok(CliMode::Completions { shell });
    }

    if args.get(1).is_some_and(|arg| arg == "update") {
        let update_args = args.get(2..).unwrap_or_default();
        if matches!(update_args, [flag] if matches!(flag.as_str(), "--help" | "-h")) {
            return Ok(CliMode::Help);
        }
        let check_only = match update_args {
            [] => false,
            [flag] if flag == "--check" => true,
            _ => anyhow::bail!("Usage: bonsai update [--check]"),
        };
        return Ok(CliMode::Update { check_only });
    }

    if args.get(1).is_some_and(|arg| arg == "doctor") {
        let doctor_args = args.get(2..).unwrap_or_default();
        if matches!(doctor_args, [flag] if matches!(flag.as_str(), "--help" | "-h")) {
            return Ok(CliMode::Help);
        }
        let mut request = DoctorRequest::default();
        let mut saw_json = false;
        let mut saw_online = false;
        for flag in doctor_args {
            match flag.as_str() {
                "--json" if !saw_json => {
                    saw_json = true;
                    request.format = DoctorOutputFormat::Json;
                }
                "--online" if !saw_online => {
                    saw_online = true;
                    request.network = DoctorNetworkMode::Online;
                }
                _ => anyhow::bail!("Usage: bonsai doctor [--json] [--online]"),
            }
        }
        return Ok(CliMode::Doctor { request });
    }

    if args.get(1).is_some_and(|arg| arg == "bug") {
        let bug_args = args.get(2..).unwrap_or_default();
        if matches!(bug_args, [flag] if matches!(flag.as_str(), "--help" | "-h")) {
            return Ok(CliMode::Help);
        }
        let mut description: Option<String> = None;
        let mut include_log = false;
        let mut iter = bug_args.iter();
        while let Some(flag) = iter.next() {
            match flag.as_str() {
                "--description" | "-d" => {
                    let value = iter
                        .next()
                        .filter(|value| !value.trim().is_empty())
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "Usage: bonsai bug --description \"<text>\" [--include-log]"
                            )
                        })?;
                    description = Some(value.clone());
                }
                "--include-log" if !include_log => include_log = true,
                _ => anyhow::bail!("Usage: bonsai bug --description \"<text>\" [--include-log]"),
            }
        }
        let description = description.ok_or_else(|| {
            anyhow::anyhow!("Usage: bonsai bug --description \"<text>\" [--include-log]")
        })?;
        return Ok(CliMode::Bug {
            description,
            include_log,
        });
    }

    if args.get(1).is_some_and(|arg| arg == "recovery") {
        return Ok(CliMode::Recovery {
            command: parse_recovery_args(&args[2..])?,
        });
    }

    let mut resume: Option<ResumeTarget> = None;
    let mut print_prompt: Option<PromptSource> = None;
    let mut output_format = OutputFormat::Text;
    let mut headless_only_seen: Option<&'static str> = None;
    let mut autonomy: Option<ApprovalLevel> = None;
    let mut yolo_seen = false;
    let mut model: Option<String> = None;
    let mut reasoning: Option<crate::provider::ReasoningSelection> = None;
    let mut max_turns: Option<usize> = None;
    let mut max_generation_duration: Option<Duration> = None;
    let mut max_streamed_chars: Option<usize> = None;
    let mut max_tool_duration: Option<Duration> = None;
    let mut append_system_prompt: Option<String> = None;
    let mut timeout: Option<Duration> = None;
    let mut isolation: Option<RecoveryMode> = None;
    let mut reduced_motion = false;
    let mut screen_reader = false;
    let mut tui_only_seen: Option<&'static str> = None;
    let mut index = 1;

    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "--continue" | "-c" => {
                // An optional *positive* numeric session id may follow
                // (`-c 52`); without a valid one, resume the latest prior
                // session. A non-positive token (`-c -5`, `-c 0`) is never a
                // real id, so it is not consumed as one here.
                match args
                    .get(index + 1)
                    .and_then(|next| next.parse::<i64>().ok())
                    .filter(|id| *id > 0)
                {
                    Some(id) => {
                        resume = Some(ResumeTarget::Session(id));
                        index += 1;
                    }
                    None => resume = Some(ResumeTarget::Latest),
                }
            }
            "-p" | "--print" => match args.get(index + 1) {
                Some(next) if next == "-" => {
                    set_print_prompt(&mut print_prompt, PromptSource::Stdin)?;
                    index += 1;
                }
                Some(next) if !next.starts_with('-') => {
                    set_print_prompt(&mut print_prompt, PromptSource::Inline(next.clone()))?;
                    index += 1;
                }
                _ => {
                    set_print_prompt(&mut print_prompt, PromptSource::Stdin)?;
                }
            },
            "--output-format" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    anyhow::anyhow!("--output-format requires one of: text, json, stream-json.")
                })?;
                output_format = parse_output_format(value)?;
                remember_headless_flag(&mut headless_only_seen, "--output-format");
            }
            "--autonomy" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    anyhow::anyhow!("--autonomy requires one of: ask, conservative, balanced, auto-accept, yolo.")
                })?;
                set_autonomy(&mut autonomy, value)?;
                remember_headless_flag(&mut headless_only_seen, "--autonomy");
            }
            "--yolo" => {
                if yolo_seen {
                    anyhow::bail!("--yolo can only be provided once.");
                }
                yolo_seen = true;
                remember_headless_flag(&mut headless_only_seen, "--yolo");
            }
            "--model" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("--model requires a model id."))?;
                set_optional_string(&mut model, "--model", value)?;
                remember_headless_flag(&mut headless_only_seen, "--model");
            }
            "--effort" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("--effort requires a reasoning level."))?;
                set_reasoning(&mut reasoning, value)?;
                remember_headless_flag(&mut headless_only_seen, "--effort");
            }
            "--max-turns" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("--max-turns requires a positive integer."))?;
                set_max_turns(&mut max_turns, value)?;
                remember_headless_flag(&mut headless_only_seen, "--max-turns");
            }
            "--max-generation-seconds" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    anyhow::anyhow!("--max-generation-seconds requires a positive integer.")
                })?;
                set_positive_duration(
                    &mut max_generation_duration,
                    "--max-generation-seconds",
                    value,
                )?;
                remember_headless_flag(&mut headless_only_seen, "--max-generation-seconds");
            }
            "--max-output-chars" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    anyhow::anyhow!("--max-output-chars requires a positive integer.")
                })?;
                set_positive_usize(&mut max_streamed_chars, "--max-output-chars", value)?;
                remember_headless_flag(&mut headless_only_seen, "--max-output-chars");
            }
            "--max-tool-seconds" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    anyhow::anyhow!("--max-tool-seconds requires a positive integer.")
                })?;
                set_positive_duration(&mut max_tool_duration, "--max-tool-seconds", value)?;
                remember_headless_flag(&mut headless_only_seen, "--max-tool-seconds");
            }
            "--append-system-prompt" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    anyhow::anyhow!("--append-system-prompt requires non-empty text.")
                })?;
                set_optional_string(&mut append_system_prompt, "--append-system-prompt", value)?;
                remember_headless_flag(&mut headless_only_seen, "--append-system-prompt");
            }
            "--timeout" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    anyhow::anyhow!("--timeout requires a positive second count.")
                })?;
                set_timeout(&mut timeout, value)?;
                remember_headless_flag(&mut headless_only_seen, "--timeout");
            }
            "--isolation" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    anyhow::anyhow!("--isolation requires one of: auto, worktree, off.")
                })?;
                set_isolation(&mut isolation, value)?;
            }
            "--reduced-motion" => {
                if reduced_motion {
                    anyhow::bail!("--reduced-motion can only be provided once.");
                }
                reduced_motion = true;
                tui_only_seen = Some("--reduced-motion");
            }
            "--screen-reader" => {
                if screen_reader {
                    anyhow::bail!("--screen-reader can only be provided once.");
                }
                screen_reader = true;
                tui_only_seen = Some("--screen-reader");
            }
            "--help" | "-h" => {
                return Err(anyhow::anyhow!(
                    "{} cannot be combined with other arguments.",
                    arg
                ));
            }
            "--version" | "-V" => {
                return Err(anyhow::anyhow!(
                    "{} cannot be combined with other arguments.",
                    arg
                ));
            }
            _ => {
                if let Some(value) = arg.strip_prefix("--output-format=") {
                    output_format = parse_output_format(value)?;
                    remember_headless_flag(&mut headless_only_seen, "--output-format");
                } else if let Some(prompt) = arg.strip_prefix("--print=") {
                    set_print_prompt(&mut print_prompt, PromptSource::Inline(prompt.to_string()))?;
                } else if let Some(value) = arg.strip_prefix("--autonomy=") {
                    set_autonomy(&mut autonomy, value)?;
                    remember_headless_flag(&mut headless_only_seen, "--autonomy");
                } else if let Some(value) = arg.strip_prefix("--model=") {
                    set_optional_string(&mut model, "--model", value)?;
                    remember_headless_flag(&mut headless_only_seen, "--model");
                } else if let Some(value) = arg.strip_prefix("--effort=") {
                    set_reasoning(&mut reasoning, value)?;
                    remember_headless_flag(&mut headless_only_seen, "--effort");
                } else if let Some(value) = arg.strip_prefix("--max-turns=") {
                    set_max_turns(&mut max_turns, value)?;
                    remember_headless_flag(&mut headless_only_seen, "--max-turns");
                } else if let Some(value) = arg.strip_prefix("--max-generation-seconds=") {
                    set_positive_duration(
                        &mut max_generation_duration,
                        "--max-generation-seconds",
                        value,
                    )?;
                    remember_headless_flag(&mut headless_only_seen, "--max-generation-seconds");
                } else if let Some(value) = arg.strip_prefix("--max-output-chars=") {
                    set_positive_usize(&mut max_streamed_chars, "--max-output-chars", value)?;
                    remember_headless_flag(&mut headless_only_seen, "--max-output-chars");
                } else if let Some(value) = arg.strip_prefix("--max-tool-seconds=") {
                    set_positive_duration(&mut max_tool_duration, "--max-tool-seconds", value)?;
                    remember_headless_flag(&mut headless_only_seen, "--max-tool-seconds");
                } else if let Some(value) = arg.strip_prefix("--append-system-prompt=") {
                    set_optional_string(
                        &mut append_system_prompt,
                        "--append-system-prompt",
                        value,
                    )?;
                    remember_headless_flag(&mut headless_only_seen, "--append-system-prompt");
                } else if let Some(value) = arg.strip_prefix("--timeout=") {
                    set_timeout(&mut timeout, value)?;
                    remember_headless_flag(&mut headless_only_seen, "--timeout");
                } else if let Some(value) = arg.strip_prefix("--isolation=") {
                    set_isolation(&mut isolation, value)?;
                } else {
                    return Err(anyhow::anyhow!(
                        "Unknown argument: {}. Use --help for supported arguments.",
                        arg
                    ));
                }
            }
        }
        index += 1;
    }

    if yolo_seen && autonomy.replace(ApprovalLevel::Yolo).is_some() {
        anyhow::bail!("--yolo cannot be combined with --autonomy.");
    }
    if let Some(flag) = headless_only_seen
        && print_prompt.is_none()
    {
        return Err(anyhow::anyhow!("{flag} is only valid with -p/--print."));
    }

    if let Some(prompt) = print_prompt {
        if let Some(flag) = tui_only_seen {
            anyhow::bail!("{flag} is only valid in interactive TUI mode.");
        }
        return Ok(CliMode::Print {
            config: HeadlessConfig {
                prompt,
                output_format,
                resume,
                autonomy,
                model,
                reasoning,
                max_turns,
                max_generation_duration,
                max_streamed_chars,
                max_tool_duration,
                append_system_prompt,
                timeout,
                isolation: isolation.unwrap_or_default(),
            },
        });
    }

    Ok(CliMode::Run {
        resume,
        isolation: isolation.unwrap_or(RecoveryMode::Off),
        accessibility: crate::tui::accessibility::TuiAccessibility::new(
            reduced_motion,
            screen_reader,
        ),
    })
}

fn parse_recovery_args(args: &[String]) -> anyhow::Result<RecoveryCommand> {
    let usage =
        "Usage: bonsai recovery list|inspect <id>|merge <id>|keep <id> [branch]|discard <id>";
    match args {
        [command] if command == "list" => Ok(RecoveryCommand::List),
        [command, id] if command == "inspect" => Ok(RecoveryCommand::Inspect {
            id: RecoveryId::parse(id)?,
        }),
        [command, id] if command == "merge" => Ok(RecoveryCommand::Merge {
            id: RecoveryId::parse(id)?,
        }),
        [command, id] if command == "discard" => Ok(RecoveryCommand::Discard {
            id: RecoveryId::parse(id)?,
        }),
        [command, id] if command == "keep" => Ok(RecoveryCommand::Keep {
            id: RecoveryId::parse(id)?,
            branch: None,
        }),
        [command, id, branch] if command == "keep" && !branch.trim().is_empty() => {
            Ok(RecoveryCommand::Keep {
                id: RecoveryId::parse(id)?,
                branch: Some(branch.clone()),
            })
        }
        _ => anyhow::bail!(usage),
    }
}

fn parse_eval_args(args: &[String]) -> anyhow::Result<crate::eval::EvalCliConfig> {
    let mut config = crate::eval::EvalCliConfig::default();
    let mut index = 0;

    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "--suite" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("--suite requires a path."))?;
                config.suite = std::path::PathBuf::from(value);
            }
            "--mode" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("--mode requires one of: mock, live."))?;
                config.mode = parse_eval_mode(value)?;
            }
            "--provider" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("--provider requires a provider id."))?;
                set_optional_string(&mut config.provider, "--provider", value)?;
            }
            "--model" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("--model requires a model id."))?;
                set_optional_string(&mut config.model, "--model", value)?;
            }
            "--effort" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("--effort requires a reasoning level."))?;
                config.effort = Some(parse_eval_effort(value)?);
            }
            "--task" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("--task requires a task id."))?;
                set_optional_string(&mut config.task, "--task", value)?;
            }
            "--seed" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("--seed requires an unsigned integer."))?;
                config.seed = Some(parse_eval_seed(value)?);
            }
            "--out" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("--out requires a directory path."))?;
                config.out_dir = std::path::PathBuf::from(value);
            }
            "--baseline" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("--baseline requires a file path."))?;
                config.baseline = Some(std::path::PathBuf::from(value));
            }
            "--json" => {
                config.json = true;
            }
            "--fail-on-task-failure" => {
                config.fail_on_task_failure = true;
            }
            "--help" | "-h" => {
                return Err(anyhow::anyhow!(
                    "{} cannot be combined with other eval arguments.",
                    arg
                ));
            }
            _ => {
                if let Some(value) = arg.strip_prefix("--suite=") {
                    config.suite = std::path::PathBuf::from(value);
                } else if let Some(value) = arg.strip_prefix("--mode=") {
                    config.mode = parse_eval_mode(value)?;
                } else if let Some(value) = arg.strip_prefix("--provider=") {
                    set_optional_string(&mut config.provider, "--provider", value)?;
                } else if let Some(value) = arg.strip_prefix("--model=") {
                    set_optional_string(&mut config.model, "--model", value)?;
                } else if let Some(value) = arg.strip_prefix("--effort=") {
                    config.effort = Some(parse_eval_effort(value)?);
                } else if let Some(value) = arg.strip_prefix("--task=") {
                    set_optional_string(&mut config.task, "--task", value)?;
                } else if let Some(value) = arg.strip_prefix("--seed=") {
                    config.seed = Some(parse_eval_seed(value)?);
                } else if let Some(value) = arg.strip_prefix("--out=") {
                    config.out_dir = std::path::PathBuf::from(value);
                } else if let Some(value) = arg.strip_prefix("--baseline=") {
                    if value.is_empty() {
                        anyhow::bail!("--baseline requires a file path.");
                    }
                    config.baseline = Some(std::path::PathBuf::from(value));
                } else {
                    return Err(anyhow::anyhow!(
                        "Unknown eval argument: {}. Use --help for supported arguments.",
                        arg
                    ));
                }
            }
        }
        index += 1;
    }

    if config.mode != crate::eval::EvalMode::Live
        && (config.provider.is_some() || config.model.is_some() || config.effort.is_some())
    {
        anyhow::bail!(
            "--provider, --model, and --effort are only valid with bonsai eval --mode live."
        );
    }

    Ok(config)
}

fn parse_eval_adapter_args(args: &[String]) -> anyhow::Result<crate::eval::AdapterCliCommand> {
    const USAGE: &str = "Usage: bonsai eval adapter run --request <path> [--out <dir>] [--force] [--json]\n       bonsai eval adapter import-harbor --result <path> --out <path> [--json]";
    let Some(action) = args.first().map(String::as_str) else {
        anyhow::bail!(USAGE);
    };
    match action {
        "run" => {
            let mut request_path = None;
            let mut out_dir = None;
            let mut force = false;
            let mut json = false;
            parse_adapter_flags(
                &args[1..],
                &mut request_path,
                &mut out_dir,
                &mut force,
                &mut json,
                "--request",
                USAGE,
            )?;
            let request_path = request_path.ok_or_else(|| anyhow::anyhow!(USAGE))?;
            let mut config = crate::eval::AdapterRunConfig::new(request_path);
            if let Some(out_dir) = out_dir {
                config.out_dir = out_dir;
            }
            config.force = force;
            config.json = json;
            Ok(crate::eval::AdapterCliCommand::Run(config))
        }
        "import-harbor" => {
            let mut result_path = None;
            let mut out_path = None;
            let mut force = false;
            let mut json = false;
            parse_adapter_flags(
                &args[1..],
                &mut result_path,
                &mut out_path,
                &mut force,
                &mut json,
                "--result",
                USAGE,
            )?;
            if force {
                anyhow::bail!("--force is only valid with 'eval adapter run'.");
            }
            Ok(crate::eval::AdapterCliCommand::ImportHarbor(
                crate::eval::AdapterImportConfig {
                    result_path: result_path.ok_or_else(|| anyhow::anyhow!(USAGE))?,
                    out_path: out_path.ok_or_else(|| anyhow::anyhow!(USAGE))?,
                    json,
                },
            ))
        }
        _ => anyhow::bail!(USAGE),
    }
}

fn parse_adapter_flags(
    args: &[String],
    input: &mut Option<std::path::PathBuf>,
    output: &mut Option<std::path::PathBuf>,
    force: &mut bool,
    json: &mut bool,
    input_flag: &str,
    usage: &str,
) -> anyhow::Result<()> {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == input_flag || arg == "--out" {
            index += 1;
            let value = args.get(index).ok_or_else(|| anyhow::anyhow!("{usage}"))?;
            let duplicate = if arg == input_flag {
                input.replace(value.into()).is_some()
            } else {
                output.replace(value.into()).is_some()
            };
            if value.trim().is_empty() || duplicate {
                anyhow::bail!("{arg} requires one non-empty path and may be provided once.");
            }
        } else if let Some(value) = arg.strip_prefix(&format!("{input_flag}=")) {
            if value.is_empty() || input.replace(value.into()).is_some() {
                anyhow::bail!("{input_flag} requires one non-empty path and may be provided once.");
            }
        } else if let Some(value) = arg.strip_prefix("--out=") {
            if value.is_empty() || output.replace(value.into()).is_some() {
                anyhow::bail!("--out requires one non-empty path and may be provided once.");
            }
        } else if arg == "--force" {
            if *force {
                anyhow::bail!("--force may be provided once.");
            }
            *force = true;
        } else if arg == "--json" {
            if *json {
                anyhow::bail!("--json may be provided once.");
            }
            *json = true;
        } else {
            anyhow::bail!("{usage}");
        }
        index += 1;
    }
    Ok(())
}

fn parse_eval_mode(value: &str) -> anyhow::Result<crate::eval::EvalMode> {
    crate::eval::EvalMode::parse(value)
        .ok_or_else(|| anyhow::anyhow!("Invalid --mode '{}'. Expected one of: mock, live.", value))
}

fn parse_eval_seed(value: &str) -> anyhow::Result<u64> {
    value
        .parse::<u64>()
        .map_err(|_| anyhow::anyhow!("Invalid --seed '{}'. Expected an unsigned integer.", value))
}

fn parse_eval_effort(value: &str) -> anyhow::Result<crate::provider::ReasoningSelection> {
    crate::provider::ReasoningSelection::parse(value).ok_or_else(|| {
        anyhow::anyhow!(
            "Invalid --effort '{}'. Expected default, off, on, minimal, low, medium, high, xhigh, max, or budget:<tokens>.",
            value
        )
    })
}

fn set_reasoning(
    slot: &mut Option<crate::provider::ReasoningSelection>,
    value: &str,
) -> anyhow::Result<()> {
    let reasoning = crate::provider::ReasoningSelection::parse(value).ok_or_else(|| {
        anyhow::anyhow!(
            "Invalid --effort '{}'. Expected one of: default, off, on, minimal, low, medium, high, xhigh, max, ultra.",
            value
        )
    })?;
    if slot.replace(reasoning).is_some() {
        anyhow::bail!("--effort can only be provided once.");
    }
    Ok(())
}

fn set_optional_string(slot: &mut Option<String>, flag: &str, value: &str) -> anyhow::Result<()> {
    if value.trim().is_empty() {
        anyhow::bail!("{flag} requires a non-empty value.");
    }
    if slot.replace(value.to_string()).is_some() {
        anyhow::bail!("{flag} can only be provided once.");
    }
    Ok(())
}

fn set_print_prompt(slot: &mut Option<PromptSource>, prompt: PromptSource) -> anyhow::Result<()> {
    if matches!(&prompt, PromptSource::Inline(text) if text.trim().is_empty()) {
        anyhow::bail!("-p/--print requires a non-empty prompt.");
    }
    if slot.replace(prompt).is_some() {
        anyhow::bail!("-p/--print can only be provided once.");
    }
    Ok(())
}

fn remember_headless_flag(slot: &mut Option<&'static str>, flag: &'static str) {
    if slot.is_none() {
        *slot = Some(flag);
    }
}

fn set_autonomy(slot: &mut Option<ApprovalLevel>, value: &str) -> anyhow::Result<()> {
    let level = ApprovalLevel::parse(value).ok_or_else(|| {
        anyhow::anyhow!(
            "Invalid --autonomy '{}'. Expected one of: ask, conservative, balanced, auto-accept, yolo.",
            value
        )
    })?;
    if slot.replace(level).is_some() {
        anyhow::bail!("--autonomy can only be provided once.");
    }
    Ok(())
}

fn set_max_turns(slot: &mut Option<usize>, value: &str) -> anyhow::Result<()> {
    set_positive_usize(slot, "--max-turns", value)
}

fn set_positive_usize(slot: &mut Option<usize>, flag: &str, value: &str) -> anyhow::Result<()> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("Invalid {flag} '{value}'. Expected a positive integer."))?;
    if parsed == 0 {
        anyhow::bail!("{flag} must be greater than 0.");
    }
    if slot.replace(parsed).is_some() {
        anyhow::bail!("{flag} can only be provided once.");
    }
    Ok(())
}

fn set_positive_duration(
    slot: &mut Option<Duration>,
    flag: &str,
    value: &str,
) -> anyhow::Result<()> {
    let seconds = value.parse::<u64>().map_err(|_| {
        anyhow::anyhow!("Invalid {flag} '{value}'. Expected a positive second count.")
    })?;
    if seconds == 0 {
        anyhow::bail!("{flag} must be greater than 0.");
    }
    if slot.replace(Duration::from_secs(seconds)).is_some() {
        anyhow::bail!("{flag} can only be provided once.");
    }
    Ok(())
}

fn set_timeout(slot: &mut Option<Duration>, value: &str) -> anyhow::Result<()> {
    set_positive_duration(slot, "--timeout", value)
}

fn set_isolation(slot: &mut Option<RecoveryMode>, value: &str) -> anyhow::Result<()> {
    let mode = RecoveryMode::parse(value).ok_or_else(|| {
        anyhow::anyhow!("Invalid --isolation '{value}'. Expected one of: auto, worktree, off.")
    })?;
    if slot.replace(mode).is_some() {
        anyhow::bail!("--isolation can only be provided once.");
    }
    Ok(())
}

fn parse_output_format(value: &str) -> anyhow::Result<OutputFormat> {
    OutputFormat::parse(value).ok_or_else(|| {
        anyhow::anyhow!(
            "Invalid --output-format '{}'. Expected one of: text, json, stream-json.",
            value
        )
    })
}

pub(crate) fn help_text() -> String {
    format!(
        "\
bonsai {}

Usage:
  bonsai [--help | --version | --continue [id] | -c [id]] [--isolation auto|worktree|off] [--reduced-motion] [--screen-reader]
  bonsai [-c [id]] -p [\"<prompt>\"|-] [headless options]
  bonsai [-c [id]] --print \"<prompt>\" [headless options]
  bonsai eval [--suite <path>] [--mode mock|live] [--provider <id>] [--model <id>] [--effort <level>] [--task <id>] [--seed <u64>] [--out <dir>] [--baseline <path>] [--json] [--fail-on-task-failure]
  bonsai eval adapter run --request <path> [--out <dir>] [--force] [--json]
  bonsai eval adapter import-harbor --result <path> --out <path> [--json]
  bonsai model-catalog check <models-dev.json>
  bonsai doctor [--json] [--online]
  bonsai update [--check]
  bonsai bug --description <text> [--include-log]
  bonsai recovery list|inspect <id>|merge <id>|keep <id> [branch]|discard <id>
  bonsai completions bash|zsh|fish

Install from source:
  cargo install --git https://github.com/strozynskiw/bonsai --locked

Resume a session:
  -c/--continue resumes the most recent session for this project; pass a
  session id (e.g. `bonsai -c 52`) to resume a specific one. On exit, bonsai
  prints the session id and the exact command to bring it back.

Provider authentication:
  Use /authorize in the TUI to save provider auth in the session store.
  Optional env bootstrap/override: OPENCODE_API_KEY, OPENCODE_ZEN_MODEL,
  OPENCODE_ZEN_BASE_URL, OPENAI_COMPATIBLE_BASE_URL, OPENAI_COMPATIBLE_MODEL,
  OPENAI_COMPATIBLE_API_KEY, ANTHROPIC_COMPATIBLE_BASE_URL, ANTHROPIC_COMPATIBLE_MODEL,
  ANTHROPIC_COMPATIBLE_API_KEY, ANTHROPIC_API_KEY, MINIMAX_API_KEY,
  MINIMAX_CODING_PLAN_API_KEY, ZAI_API_KEY, ZAI_CODING_PLAN_API_KEY,
  MOONSHOT_API_KEY, KIMI_CODING_PLAN_API_KEY, MIMO_API_KEY,
  MIMO_CODING_PLAN_API_KEY, DEEPSEEK_API_KEY, DASHSCOPE_API_KEY,
  DASHSCOPE_TOKEN_PLAN_API_KEY, GEMINI_API_KEY, XAI_API_KEY, MISTRAL_API_KEY,
  HUNYUAN_API_KEY
  Provider selection: BONSAI_PROVIDER=<provider-id>
  Provider setting overrides: *_API_KEY, *_MODEL, *_BASE_URL

Release diagnostics:
  `bonsai doctor` runs offline checks for storage, configuration, provider auth,
  sandbox, Git, language tooling, MCP configuration, and terminal support.
  Use `bonsai doctor --json` for the redacted machine-readable support form.

Self-update:
  Official builds check GitHub for newer signed releases at startup and stage
  them automatically; a staged update applies on the next launch. `bonsai
  update` runs the same verified install immediately (`--check` only reports).
  Configure with `[update]` in config.toml: `mode = \"auto\"|\"notify\"|\"off\"`,
  optional `pin = \"X.Y.Z\"` to hold a version.

Interactive recovery isolation:
  `bonsai --isolation worktree` runs the TUI in a managed Git worktree while
  keeping sessions, permissions, and resume history attached to the source
  project. `auto` follows the saved autonomy level; interactive isolation is
  off when the flag is omitted. On exit, inspect, merge, keep, or discard the
  printed recovery id with `bonsai recovery ...`.

Interactive accessibility:
  --reduced-motion freezes spinners and shimmer while preserving timers.
  --screen-reader also uses a linear, labelled transcript in the normal
  terminal buffer without mouse capture or an alternate screen.

Headless print mode:
  -p/--print runs one non-interactive coding-agent turn and exits.
  -p with no prompt, or -p -, reads the prompt from stdin. Use --print=<text>
  when the prompt starts with '-'.
  -c/--continue with -p resumes the latest or specified session before running
  the new prompt. Fresh runs honor BONSAI_PROVIDER; resumed runs keep the
  session provider/model unless --model overrides the model for that run.

  Options:
    --output-format text|json|stream-json
    --autonomy ask|conservative|balanced|auto-accept|yolo
    --yolo
    --model <id>
    --effort <level>
    --max-turns <n>
    --max-generation-seconds <secs>
    --max-output-chars <n>
    --max-tool-seconds <secs>
    --append-system-prompt <text>
    --timeout <secs>
    --isolation auto|worktree|off

  Recovery isolation:
    Mutating runs (balanced, auto-accept, or yolo) use an isolated Git worktree
    by default. The source checkout is never changed automatically. After the
    run, use `bonsai recovery inspect <id>`, then merge, keep, or discard it.
    Merge refuses if the source working tree or Git index changed after start.
    Use --isolation off to explicitly opt out (including outside Git).

  Environment:
    BONSAI_PROVIDER=<provider-id> selects the provider for fresh headless runs.
    BONSAI_AUTONOMY=<level> sets the default headless autonomy.
    BONSAI_SELF_REVIEW=auto|on|ask|off controls self-review.

  Exit codes:
    0 completed
    1 tool/provider/runtime failure
    2 CLI usage
    3 auth/config
    124 timeout
    130 SIGINT
    143 SIGTERM

  Examples:
    bonsai -p \"Say exactly: ok\" --output-format json
    echo \"Say exactly: ok\" | bonsai -p
    BONSAI_AUTONOMY=balanced bonsai -p 'run `git status` via bash'
    bonsai -c -p \"What did we decide?\"

Eval harness:
  bonsai eval runs the source-controlled eval suite. Mock mode is deterministic
  and is the default; live mode uses --provider, BONSAI_PROVIDER, or the current
    saved provider. Reports are written under target/eval/<run-id>/report.json.
  Live runs accept explicit --model and --effort overrides. Suite/task budgets
  are reported as failures; --fail-on-task-failure makes them fail the process.
  --baseline selects an exact suite/provider/model/effort reference and makes a
  configured correctness regression fail the process.
  `eval adapter` runs pinned external benchmark contracts through the same
  headless surface. It never downloads datasets, runtimes, or container images.

Model catalog:
  bonsai model-catalog check validates a Models.dev JSON payload with Bonsai's
  parser. scripts/update-model-catalog.sh fetches the live payload and can update
  the local cache used by runtime model descriptions.
",
        env!("CARGO_PKG_VERSION"),
    )
}

#[cfg(test)]
mod tests {
    use super::{CliMode, ResumeTarget, help_text, parse_cli_args};
    use crate::completions::CompletionShell;
    use std::time::Duration;

    use crate::doctor::{DoctorNetworkMode, DoctorOutputFormat, DoctorRequest};
    use crate::headless::{HeadlessConfig, OutputFormat, PromptSource};
    use crate::recovery::RecoveryMode;
    use crate::tool::ApprovalLevel;

    fn expected_print_config(prompt: &str, output_format: OutputFormat) -> HeadlessConfig {
        HeadlessConfig {
            prompt: PromptSource::Inline(prompt.to_string()),
            output_format,
            resume: None,
            autonomy: None,
            model: None,
            reasoning: None,
            max_turns: None,
            max_generation_duration: None,
            max_streamed_chars: None,
            max_tool_duration: None,
            append_system_prompt: None,
            timeout: None,
            isolation: RecoveryMode::Auto,
        }
    }

    #[test]
    fn parse_cli_args_accepts_help() {
        let args = vec!["bonsai".to_string(), "--help".to_string()];
        assert_eq!(parse_cli_args(&args).unwrap(), CliMode::Help);
    }

    #[test]
    fn parse_cli_args_defaults_to_run() {
        let args = vec!["bonsai".to_string()];
        assert_eq!(
            parse_cli_args(&args).unwrap(),
            CliMode::Run {
                resume: None,
                isolation: RecoveryMode::Off,
                accessibility: Default::default(),
            }
        );
    }

    #[test]
    fn parse_cli_args_accepts_update() {
        let args = |rest: &[&str]| {
            let mut args = vec!["bonsai".to_string(), "update".to_string()];
            args.extend(rest.iter().map(|arg| arg.to_string()));
            args
        };
        assert_eq!(
            parse_cli_args(&args(&[])).unwrap(),
            CliMode::Update { check_only: false }
        );
        assert_eq!(
            parse_cli_args(&args(&["--check"])).unwrap(),
            CliMode::Update { check_only: true }
        );
        assert_eq!(parse_cli_args(&args(&["--help"])).unwrap(), CliMode::Help);
        assert!(parse_cli_args(&args(&["--force"])).is_err());
        assert!(parse_cli_args(&args(&["--check", "--check"])).is_err());
    }

    #[test]
    fn parse_cli_args_accepts_continue() {
        let args = vec!["bonsai".to_string(), "-c".to_string()];
        assert_eq!(
            parse_cli_args(&args).unwrap(),
            CliMode::Run {
                resume: Some(ResumeTarget::Latest),
                isolation: RecoveryMode::Off,
                accessibility: Default::default(),
            }
        );
    }

    #[test]
    fn parse_cli_args_accepts_continue_with_session_id() {
        let args = vec!["bonsai".to_string(), "-c".to_string(), "52".to_string()];
        assert_eq!(
            parse_cli_args(&args).unwrap(),
            CliMode::Run {
                resume: Some(ResumeTarget::Session(52)),
                isolation: RecoveryMode::Off,
                accessibility: Default::default(),
            }
        );
    }

    #[test]
    fn parse_cli_args_accepts_explicit_tui_worktree_isolation() {
        let args = vec![
            "bonsai".to_string(),
            "--isolation".to_string(),
            "worktree".to_string(),
        ];
        assert_eq!(
            parse_cli_args(&args).unwrap(),
            CliMode::Run {
                resume: None,
                isolation: RecoveryMode::Worktree,
                accessibility: Default::default(),
            }
        );
    }

    #[test]
    fn parse_cli_args_accepts_tui_accessibility_modes() {
        let args = vec![
            "bonsai".to_string(),
            "--reduced-motion".to_string(),
            "--screen-reader".to_string(),
        ];

        assert_eq!(
            parse_cli_args(&args).unwrap(),
            CliMode::Run {
                resume: None,
                isolation: RecoveryMode::Off,
                accessibility: crate::tui::accessibility::TuiAccessibility::new(true, true),
            }
        );
    }

    #[test]
    fn parse_cli_args_rejects_tui_accessibility_flags_in_print_mode() {
        let args = vec![
            "bonsai".to_string(),
            "--screen-reader".to_string(),
            "--print".to_string(),
            "hello".to_string(),
        ];

        assert_eq!(
            parse_cli_args(&args).unwrap_err().to_string(),
            "--screen-reader is only valid in interactive TUI mode."
        );
    }

    #[test]
    fn parse_cli_args_continue_without_id_resumes_latest() {
        // A non-numeric token after -c is not consumed as an id.
        let args = vec![
            "bonsai".to_string(),
            "-c".to_string(),
            "-p".to_string(),
            "hi".to_string(),
        ];
        match parse_cli_args(&args).unwrap() {
            CliMode::Print { config } => {
                assert_eq!(config.resume, Some(ResumeTarget::Latest));
                assert_eq!(config.prompt, PromptSource::Inline("hi".to_string()));
            }
            other => panic!("expected print mode, got {other:?}"),
        }
    }

    #[test]
    fn parse_cli_args_accepts_version() {
        let args = vec!["bonsai".to_string(), "--version".to_string()];
        assert_eq!(parse_cli_args(&args).unwrap(), CliMode::Version);
    }

    #[test]
    fn parse_cli_args_accepts_supported_completion_shells() {
        for (name, shell) in [
            ("bash", CompletionShell::Bash),
            ("zsh", CompletionShell::Zsh),
            ("fish", CompletionShell::Fish),
        ] {
            let args = vec![
                "bonsai".to_string(),
                "completions".to_string(),
                name.to_string(),
            ];
            assert_eq!(
                parse_cli_args(&args).unwrap(),
                CliMode::Completions { shell }
            );
        }
    }

    #[test]
    fn parse_cli_args_rejects_unknown_completion_shell() {
        let args = vec![
            "bonsai".to_string(),
            "completions".to_string(),
            "powershell".to_string(),
        ];
        assert_eq!(
            parse_cli_args(&args).unwrap_err().to_string(),
            "Usage: bonsai completions bash|zsh|fish"
        );
    }

    #[test]
    fn parse_cli_args_accepts_short_print_with_default_text_output() {
        let args = vec!["bonsai".to_string(), "-p".to_string(), "hello".to_string()];

        assert_eq!(
            parse_cli_args(&args).unwrap(),
            CliMode::Print {
                config: expected_print_config("hello", OutputFormat::Text)
            }
        );
    }

    #[test]
    fn parse_cli_args_accepts_long_print_with_output_format() {
        let args = vec![
            "bonsai".to_string(),
            "--print".to_string(),
            "hello".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
        ];

        assert_eq!(
            parse_cli_args(&args).unwrap(),
            CliMode::Print {
                config: expected_print_config("hello", OutputFormat::StreamJson)
            }
        );
    }

    #[test]
    fn parse_cli_args_accepts_equals_forms() {
        let args = vec![
            "bonsai".to_string(),
            "--print=hello".to_string(),
            "--output-format=json".to_string(),
        ];

        assert_eq!(
            parse_cli_args(&args).unwrap(),
            CliMode::Print {
                config: expected_print_config("hello", OutputFormat::Json)
            }
        );
    }

    #[test]
    fn parse_cli_args_accepts_eval_defaults() {
        let args = vec!["bonsai".to_string(), "eval".to_string()];

        assert_eq!(
            parse_cli_args(&args).unwrap(),
            CliMode::Eval {
                config: crate::eval::EvalCliConfig::default(),
            }
        );
    }

    #[test]
    fn parse_cli_args_accepts_eval_options() {
        let args = vec![
            "bonsai".to_string(),
            "eval".to_string(),
            "--suite".to_string(),
            "custom.toml".to_string(),
            "--mode=live".to_string(),
            "--provider".to_string(),
            "anthropic".to_string(),
            "--model".to_string(),
            "claude-sonnet-4-5".to_string(),
            "--effort=high".to_string(),
            "--task=t01".to_string(),
            "--seed".to_string(),
            "42".to_string(),
            "--out=tmp/eval".to_string(),
            "--baseline=eval/baselines/release-v1.toml".to_string(),
            "--json".to_string(),
            "--fail-on-task-failure".to_string(),
        ];

        assert_eq!(
            parse_cli_args(&args).unwrap(),
            CliMode::Eval {
                config: crate::eval::EvalCliConfig {
                    suite: std::path::PathBuf::from("custom.toml"),
                    mode: crate::eval::EvalMode::Live,
                    provider: Some("anthropic".to_string()),
                    model: Some("claude-sonnet-4-5".to_string()),
                    effort: Some(crate::provider::ReasoningSelection::High),
                    task: Some("t01".to_string()),
                    seed: Some(42),
                    out_dir: std::path::PathBuf::from("tmp/eval"),
                    baseline: Some(std::path::PathBuf::from("eval/baselines/release-v1.toml")),
                    json: true,
                    fail_on_task_failure: true,
                },
            }
        );
    }

    #[test]
    fn parse_cli_args_accepts_external_adapter_commands() {
        let run = vec![
            "bonsai".to_string(),
            "eval".to_string(),
            "adapter".to_string(),
            "run".to_string(),
            "--request=request.json".to_string(),
            "--out".to_string(),
            "artifacts".to_string(),
            "--force".to_string(),
            "--json".to_string(),
        ];
        assert_eq!(
            parse_cli_args(&run).unwrap(),
            CliMode::EvalAdapter {
                command: crate::eval::AdapterCliCommand::Run(crate::eval::AdapterRunConfig {
                    request_path: std::path::PathBuf::from("request.json"),
                    out_dir: std::path::PathBuf::from("artifacts"),
                    force: true,
                    json: true,
                })
            }
        );

        let import = vec![
            "bonsai".to_string(),
            "eval".to_string(),
            "adapter".to_string(),
            "import-harbor".to_string(),
            "--result".to_string(),
            "result.json".to_string(),
            "--out=normalized.json".to_string(),
        ];
        assert_eq!(
            parse_cli_args(&import).unwrap(),
            CliMode::EvalAdapter {
                command: crate::eval::AdapterCliCommand::ImportHarbor(
                    crate::eval::AdapterImportConfig {
                        result_path: std::path::PathBuf::from("result.json"),
                        out_path: std::path::PathBuf::from("normalized.json"),
                        json: false,
                    }
                )
            }
        );
    }

    #[test]
    fn parse_cli_args_rejects_incomplete_external_adapter_commands() {
        for args in [
            vec!["bonsai", "eval", "adapter", "run"],
            vec![
                "bonsai",
                "eval",
                "adapter",
                "import-harbor",
                "--result",
                "x",
            ],
        ] {
            let args = args.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert!(parse_cli_args(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn parse_cli_args_accepts_model_catalog_check_path() {
        let args = vec![
            "bonsai".to_string(),
            "model-catalog".to_string(),
            "check".to_string(),
            "models/builtin/anthropic.toml".to_string(),
        ];

        assert_eq!(
            parse_cli_args(&args).unwrap(),
            CliMode::ModelCatalogCheck {
                path: std::path::PathBuf::from("models/builtin/anthropic.toml"),
            }
        );
    }

    #[test]
    fn parse_cli_args_rejects_model_catalog_check_wrong_arity() {
        let args = vec![
            "bonsai".to_string(),
            "model-catalog".to_string(),
            "check".to_string(),
        ];

        let err = parse_cli_args(&args).unwrap_err();

        assert_eq!(
            err.to_string(),
            "Usage: bonsai model-catalog check <models-dev.json>"
        );
    }

    #[test]
    fn parse_cli_args_accepts_bug_with_description() {
        let args: Vec<String> = ["bonsai", "bug", "--description", "picker crashed"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            parse_cli_args(&args).unwrap(),
            CliMode::Bug {
                description: "picker crashed".to_string(),
                include_log: false,
            }
        );

        let with_log: Vec<String> = ["bonsai", "bug", "-d", "x", "--include-log"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            parse_cli_args(&with_log).unwrap(),
            CliMode::Bug {
                description: "x".to_string(),
                include_log: true,
            }
        );

        // A description is required; a bare or malformed invocation is usage.
        for bad in [
            vec!["bonsai".to_string(), "bug".to_string()],
            ["bonsai", "bug", "--description"]
                .into_iter()
                .map(String::from)
                .collect(),
            ["bonsai", "bug", "--nope"]
                .into_iter()
                .map(String::from)
                .collect(),
        ] {
            assert!(parse_cli_args(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn parse_cli_args_accepts_doctor_text_and_json() {
        let text = vec!["bonsai".to_string(), "doctor".to_string()];
        assert_eq!(
            parse_cli_args(&text).unwrap(),
            CliMode::Doctor {
                request: DoctorRequest::default(),
            }
        );

        let json = vec![
            "bonsai".to_string(),
            "doctor".to_string(),
            "--json".to_string(),
        ];
        assert_eq!(
            parse_cli_args(&json).unwrap(),
            CliMode::Doctor {
                request: DoctorRequest {
                    format: DoctorOutputFormat::Json,
                    network: DoctorNetworkMode::Offline,
                },
            }
        );

        let online_json = vec![
            "bonsai".to_string(),
            "doctor".to_string(),
            "--online".to_string(),
            "--json".to_string(),
        ];
        assert_eq!(
            parse_cli_args(&online_json).unwrap(),
            CliMode::Doctor {
                request: DoctorRequest {
                    format: DoctorOutputFormat::Json,
                    network: DoctorNetworkMode::Online,
                },
            }
        );
    }

    #[test]
    fn parse_cli_args_rejects_unknown_doctor_arguments() {
        let args = vec![
            "bonsai".to_string(),
            "doctor".to_string(),
            "--network".to_string(),
        ];
        assert_eq!(
            parse_cli_args(&args).unwrap_err().to_string(),
            "Usage: bonsai doctor [--json] [--online]"
        );

        let duplicate = vec![
            "bonsai".to_string(),
            "doctor".to_string(),
            "--online".to_string(),
            "--online".to_string(),
        ];
        assert_eq!(
            parse_cli_args(&duplicate).unwrap_err().to_string(),
            "Usage: bonsai doctor [--json] [--online]"
        );
    }

    #[test]
    fn parse_cli_args_rejects_eval_provider_in_mock_mode() {
        let args = vec![
            "bonsai".to_string(),
            "eval".to_string(),
            "--provider".to_string(),
            "anthropic".to_string(),
        ];

        assert!(parse_cli_args(&args).is_err());
    }

    #[test]
    fn parse_cli_args_accepts_continue_with_print() {
        let args = vec![
            "bonsai".to_string(),
            "--continue".to_string(),
            "-p".to_string(),
            "hello".to_string(),
        ];

        match parse_cli_args(&args).unwrap() {
            CliMode::Print { config } => {
                assert_eq!(config.resume, Some(ResumeTarget::Latest));
                assert_eq!(config.prompt, PromptSource::Inline("hello".to_string()));
            }
            other => panic!("expected print mode, got {other:?}"),
        }
    }

    #[test]
    fn parse_cli_args_accepts_stdin_print_variants() {
        for args in [
            vec!["bonsai".to_string(), "-p".to_string()],
            vec!["bonsai".to_string(), "-p".to_string(), "-".to_string()],
            vec![
                "bonsai".to_string(),
                "-p".to_string(),
                "--output-format".to_string(),
                "json".to_string(),
            ],
        ] {
            match parse_cli_args(&args).unwrap() {
                CliMode::Print { config } => {
                    assert_eq!(config.prompt, PromptSource::Stdin);
                }
                other => panic!("expected print mode, got {other:?}"),
            }
        }
    }

    #[test]
    fn parse_cli_args_accepts_headless_knobs() {
        let args = vec![
            "bonsai".to_string(),
            "--autonomy=balanced".to_string(),
            "--model".to_string(),
            "gpt-test".to_string(),
            "--effort=high".to_string(),
            "--max-turns=3".to_string(),
            "--max-generation-seconds=4".to_string(),
            "--max-output-chars".to_string(),
            "5000".to_string(),
            "--max-tool-seconds=6".to_string(),
            "--append-system-prompt".to_string(),
            "be terse".to_string(),
            "--timeout".to_string(),
            "9".to_string(),
            "--isolation=worktree".to_string(),
            "-p".to_string(),
            "hello".to_string(),
        ];

        match parse_cli_args(&args).unwrap() {
            CliMode::Print { config } => {
                assert_eq!(config.autonomy, Some(ApprovalLevel::Balanced));
                assert_eq!(config.model.as_deref(), Some("gpt-test"));
                assert_eq!(
                    config.reasoning,
                    Some(crate::provider::ReasoningSelection::High)
                );
                assert_eq!(config.max_turns, Some(3));
                assert_eq!(config.max_generation_duration, Some(Duration::from_secs(4)));
                assert_eq!(config.max_streamed_chars, Some(5_000));
                assert_eq!(config.max_tool_duration, Some(Duration::from_secs(6)));
                assert_eq!(config.append_system_prompt.as_deref(), Some("be terse"));
                assert_eq!(config.timeout, Some(Duration::from_secs(9)));
                assert_eq!(config.isolation, RecoveryMode::Worktree);
            }
            other => panic!("expected print mode, got {other:?}"),
        }
    }

    #[test]
    fn parse_cli_args_accepts_yolo_shorthand() {
        let args = vec![
            "bonsai".to_string(),
            "--yolo".to_string(),
            "-p".to_string(),
            "hello".to_string(),
        ];

        match parse_cli_args(&args).unwrap() {
            CliMode::Print { config } => {
                assert_eq!(config.autonomy, Some(ApprovalLevel::Yolo));
            }
            other => panic!("expected print mode, got {other:?}"),
        }
    }

    #[test]
    fn parse_cli_args_rejects_zero_budget_limits() {
        for flag in [
            "--max-generation-seconds=0",
            "--max-output-chars=0",
            "--max-tool-seconds=0",
        ] {
            let args = vec![
                "bonsai".to_string(),
                "-p".to_string(),
                "hello".to_string(),
                flag.to_string(),
            ];

            let error = parse_cli_args(&args).unwrap_err();

            assert!(error.to_string().contains("must be greater than 0"));
        }
    }

    #[test]
    fn parse_cli_args_rejects_yolo_with_autonomy() {
        let args = vec![
            "bonsai".to_string(),
            "--yolo".to_string(),
            "--autonomy".to_string(),
            "balanced".to_string(),
            "-p".to_string(),
            "hello".to_string(),
        ];

        assert!(parse_cli_args(&args).is_err());
    }

    #[test]
    fn parse_cli_args_rejects_output_format_without_print() {
        let args = vec![
            "bonsai".to_string(),
            "--output-format".to_string(),
            "json".to_string(),
        ];

        assert!(parse_cli_args(&args).is_err());
    }

    #[test]
    fn parse_cli_args_accepts_recovery_commands() {
        let list = vec![
            "bonsai".to_string(),
            "recovery".to_string(),
            "list".to_string(),
        ];
        assert_eq!(
            parse_cli_args(&list).unwrap(),
            CliMode::Recovery {
                command: crate::recovery::RecoveryCommand::List,
            }
        );

        let keep = vec![
            "bonsai".to_string(),
            "recovery".to_string(),
            "keep".to_string(),
            "123-test".to_string(),
            "feature/recovered".to_string(),
        ];
        assert_eq!(
            parse_cli_args(&keep).unwrap(),
            CliMode::Recovery {
                command: crate::recovery::RecoveryCommand::Keep {
                    id: crate::storage::RecoveryId::parse("123-test").unwrap(),
                    branch: Some("feature/recovered".to_string()),
                },
            }
        );
    }

    #[test]
    fn parse_cli_args_rejects_invalid_isolation() {
        let args = vec![
            "bonsai".to_string(),
            "-p".to_string(),
            "hello".to_string(),
            "--isolation=magic".to_string(),
        ];
        assert!(parse_cli_args(&args).is_err());
    }

    #[test]
    fn parse_cli_args_rejects_invalid_output_format() {
        let args = vec![
            "bonsai".to_string(),
            "-p".to_string(),
            "hello".to_string(),
            "--output-format".to_string(),
            "yaml".to_string(),
        ];

        assert!(parse_cli_args(&args).is_err());
    }

    #[test]
    fn parse_cli_args_rejects_unknown_flags() {
        let args = vec!["bonsai".to_string(), "--weird".to_string()];
        assert!(parse_cli_args(&args).is_err());
    }

    #[test]
    fn parse_cli_args_rejects_extra_args() {
        let args = vec![
            "bonsai".to_string(),
            "--version".to_string(),
            "--help".to_string(),
        ];
        assert!(parse_cli_args(&args).is_err());
    }

    #[test]
    fn help_text_mentions_install_helpful_commands() {
        let help = help_text();
        assert!(help.contains("--continue") && help.contains("cargo install --git"));
        assert!(help.contains("-p [\"<prompt>\"|-]") && help.contains("--output-format"));
        assert!(help.contains("--autonomy") && help.contains("BONSAI_AUTONOMY"));
        assert!(help.contains("124 timeout") && help.contains("143 SIGTERM"));
        assert!(help.contains("bonsai eval") && help.contains("--mode mock|live"));
        assert!(help.contains("BONSAI_PROVIDER") && help.contains("*_API_KEY"));
        assert!(help.contains("/authorize") && help.contains("Optional env bootstrap"));
        assert!(help.contains("bonsai completions bash|zsh|fish"));
    }
}
