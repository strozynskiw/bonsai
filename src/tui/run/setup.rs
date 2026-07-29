use super::*;

pub(in crate::tui::run) struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    restored: bool,
    /// Whether the kitty keyboard-protocol flags were pushed at startup, so
    /// restore only pops what was actually pushed — a pop at a terminal that
    /// never saw the push is another stray sequence in its input stream.
    keyboard_enhanced: bool,
    alternate_screen: bool,
    mouse_capture: bool,
    bracketed_paste: bool,
}

impl TerminalSession {
    pub(in crate::tui::run) fn enter(screen_reader: bool) -> Result<Self> {
        enable_raw_mode().context("failed to enable raw mode")?;
        let mut stdout = io::stdout();
        let alternate_screen = !screen_reader;
        let mouse_capture = !screen_reader;
        if alternate_screen && let Err(err) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(err).context("failed to enter alternate screen");
        }
        if let Err(err) = execute!(stdout, EnableBracketedPaste) {
            if alternate_screen {
                let _ = execute!(stdout, LeaveAlternateScreen);
            }
            let _ = stdout.flush();
            drain_pending_terminal_events();
            let _ = disable_raw_mode();
            return Err(err).context("failed to enable bracketed paste");
        }
        // Push the keyboard protocol so `Alt+<key>` is delivered as a
        // single `KeyEvent` with the ALT modifier rather than the legacy
        // VT100 "ESC <key>" sequence. Without this, terminals that don't
        // speak the kitty/iTerm2 protocol split the keystroke into Esc
        // + the letter — so e.g. `Alt+I` (implement plan) clears the
        // composer and types `i` instead.
        //
        // Gated on a support probe rather than pushed blind: pushing the
        // flags at a terminal that doesn't (fully) speak the protocol can
        // leave stray/partial reply bytes in the input stream, where
        // crossterm's parser resynchronizes by consuming the very next
        // byte — the user's first keystroke. The probe itself is a
        // query/reply round-trip crossterm parses safely (raw mode is
        // already on), so nothing leaks into the event queue.
        // Probe before enabling mouse reporting. A click interleaved with the
        // keyboard query can be consumed as part of its reply, leaving the SGR
        // mouse bytes to surface later as literal composer input.
        let keyboard_enhanced = match crossterm::terminal::supports_keyboard_enhancement() {
            Ok(true) => match execute!(
                stdout,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                )
            ) {
                Ok(()) => true,
                Err(err) => {
                    tracing::debug!("keyboard enhancement push failed: {err}");
                    false
                }
            },
            Ok(false) => {
                tracing::debug!("keyboard enhancement not supported; keeping legacy key encoding");
                false
            }
            Err(err) => {
                tracing::debug!("keyboard enhancement probe failed: {err}");
                false
            }
        };
        match restore_raw_mode_if_needed() {
            Ok(true) => {
                tracing::warn!(
                    "terminal capability probe reset raw mode; restored it before enabling mouse capture"
                );
                drain_pending_terminal_events();
            }
            Ok(false) => {}
            Err(err) => {
                if keyboard_enhanced {
                    let _ = execute!(stdout, PopKeyboardEnhancementFlags);
                }
                let _ = execute!(stdout, DisableBracketedPaste);
                if alternate_screen {
                    let _ = execute!(stdout, LeaveAlternateScreen);
                }
                let _ = stdout.flush();
                let _ = disable_raw_mode();
                return Err(err)
                    .context("failed to restore raw mode after terminal capability probe");
            }
        }
        if mouse_capture && let Err(err) = enable_mouse_capture(&mut stdout) {
            // A short write can enable only a prefix of the mouse modes. Undo
            // the whole group while raw mode still contains resulting bytes.
            let _ = execute!(stdout, DisableMouseCapture);
            if keyboard_enhanced {
                let _ = execute!(stdout, PopKeyboardEnhancementFlags);
            }
            let _ = execute!(stdout, DisableBracketedPaste);
            if alternate_screen {
                let _ = execute!(stdout, LeaveAlternateScreen);
            }
            let _ = stdout.flush();
            drain_pending_terminal_events();
            let _ = disable_raw_mode();
            return Err(err).context("failed to enable mouse capture");
        }

        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: terminal_viewport(screen_reader),
            },
        );
        let terminal = match terminal {
            Ok(terminal) => terminal,
            Err(err) => {
                let mut cleanup_stdout = io::stdout();
                if keyboard_enhanced {
                    let _ = execute!(cleanup_stdout, PopKeyboardEnhancementFlags);
                }
                let _ = execute!(cleanup_stdout, DisableBracketedPaste);
                if mouse_capture {
                    let _ = execute!(cleanup_stdout, DisableMouseCapture);
                }
                if alternate_screen {
                    let _ = execute!(cleanup_stdout, LeaveAlternateScreen);
                }
                let _ = disable_raw_mode();
                return Err(err).context("failed to initialize terminal");
            }
        };
        Ok(Self {
            terminal,
            restored: false,
            keyboard_enhanced,
            alternate_screen,
            mouse_capture,
            bracketed_paste: true,
        })
    }

    pub(in crate::tui::run) fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }

    /// Whether the terminal is currently capturing mouse events.
    pub(in crate::tui::run) fn mouse_capture(&self) -> bool {
        self.mouse_capture
    }

    /// Restore raw mode if another terminal participant reset the live tty.
    ///
    /// Crossterm tracks raw mode in process-local state. A terminal host can
    /// reset the actual tty flags without updating that state, leaving
    /// crossterm convinced raw mode is still active while the kernel echoes
    /// mouse reports as `^[...`. Cycle crossterm's state only after checking
    /// the real tty flags, then discard bytes that accumulated while canonical
    /// input and echo were active.
    pub(in crate::tui::run) fn ensure_raw_mode(&mut self) -> io::Result<bool> {
        let recovered = match restore_raw_mode_if_needed() {
            Ok(recovered) => recovered,
            Err(err) => {
                // Mouse reporting and cooked input must never coexist. If raw mode
                // cannot be restored, stop the source of the visible escape bytes.
                if execute!(self.terminal.backend_mut(), DisableMouseCapture).is_ok() {
                    self.mouse_capture = false;
                }
                return Err(err);
            }
        };
        if recovered {
            drain_pending_terminal_events();
        }
        Ok(recovered)
    }

    /// Enable or disable mouse capture at runtime — the "copy mode" toggle.
    /// Releasing capture hands click-drag back to the terminal for native text
    /// selection; re-enabling restores in-app scroll/click. Idempotent, and the
    /// field stays authoritative so [`Self::restore`] only disables what is on.
    pub(in crate::tui::run) fn set_mouse_capture(&mut self, enabled: bool) -> io::Result<()> {
        if self.mouse_capture == enabled {
            return Ok(());
        }
        if enabled {
            if let Err(err) = enable_mouse_capture(self.terminal.backend_mut()) {
                // `EnableMouseCapture` expands to several terminal modes. A
                // short write can enable only a prefix, so always undo the
                // whole group before reporting failure.
                let _ = execute!(self.terminal.backend_mut(), DisableMouseCapture);
                return Err(err);
            }
        } else {
            execute!(self.terminal.backend_mut(), DisableMouseCapture)?;
        }
        self.mouse_capture = enabled;
        Ok(())
    }

    pub(in crate::tui::run) fn restore(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        let result = self.restore_best_effort();
        if result.is_ok() {
            self.restored = true;
        }
        result
    }

    fn restore_best_effort(&mut self) -> Result<()> {
        drain_pending_terminal_events();
        let mut first_error = None;
        record_cleanup_error(
            &mut first_error,
            "failed to reset terminal title",
            crossterm::execute!(
                self.terminal.backend_mut(),
                crossterm::terminal::SetTitle("bonsai")
            ),
        );
        if self.keyboard_enhanced {
            record_cleanup_error(
                &mut first_error,
                "failed to pop keyboard enhancement flags",
                execute!(self.terminal.backend_mut(), PopKeyboardEnhancementFlags),
            );
        }
        if self.bracketed_paste {
            record_cleanup_error(
                &mut first_error,
                "failed to disable bracketed paste",
                execute!(self.terminal.backend_mut(), DisableBracketedPaste),
            );
        }
        if self.mouse_capture {
            record_cleanup_error(
                &mut first_error,
                "failed to disable mouse capture",
                execute!(self.terminal.backend_mut(), DisableMouseCapture),
            );
        }
        if self.alternate_screen {
            record_cleanup_error(
                &mut first_error,
                "failed to leave alternate screen",
                execute!(self.terminal.backend_mut(), LeaveAlternateScreen),
            );
        }
        record_cleanup_error(
            &mut first_error,
            "failed to show cursor",
            self.terminal.show_cursor(),
        );
        record_cleanup_error(
            &mut first_error,
            "failed to flush terminal cleanup",
            self.terminal.backend_mut().flush(),
        );
        // Keep raw mode active until all terminal protocols are disabled and
        // their final events are drained. Otherwise late mouse/keyboard bytes
        // can be echoed into the parent shell as visible escape sequences.
        drain_pending_terminal_events();
        record_cleanup_error(
            &mut first_error,
            "failed to disable raw mode",
            disable_raw_mode(),
        );

        if let Some(err) = first_error {
            Err(err)
        } else {
            Ok(())
        }
    }
}

fn terminal_viewport(screen_reader: bool) -> Viewport {
    if screen_reader {
        let height = crossterm::terminal::size()
            .map(|(_, height)| height.max(1))
            .unwrap_or(24);
        Viewport::Inline(height)
    } else {
        Viewport::Fullscreen
    }
}

#[cfg(unix)]
fn terminal_raw_mode_is_effective() -> io::Result<bool> {
    use std::io::IsTerminal;

    use nix::sys::termios::tcgetattr;

    let stdin = io::stdin();
    let attributes = if stdin.is_terminal() {
        tcgetattr(&stdin).map_err(io::Error::from)?
    } else {
        let tty = std::fs::File::open("/dev/tty")?;
        tcgetattr(&tty).map_err(io::Error::from)?
    };
    Ok(terminal_local_flags_are_raw(attributes.local_flags))
}

#[cfg(unix)]
fn terminal_local_flags_are_raw(local_flags: nix::sys::termios::LocalFlags) -> bool {
    use nix::sys::termios::LocalFlags;

    let cooked_flags =
        LocalFlags::ECHO | LocalFlags::ICANON | LocalFlags::IEXTEN | LocalFlags::ISIG;
    !local_flags.intersects(cooked_flags)
}

#[cfg(not(unix))]
fn terminal_raw_mode_is_effective() -> io::Result<bool> {
    crossterm::terminal::is_raw_mode_enabled()
}

fn restore_raw_mode_if_needed() -> io::Result<bool> {
    if terminal_raw_mode_is_effective()? {
        return Ok(false);
    }
    disable_raw_mode()?;
    enable_raw_mode()?;
    Ok(true)
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if !self.restored {
            let _ = self.restore_best_effort();
            self.restored = true;
        }
    }
}

/// Enable clicks, wheel input, and button-drag reporting without any-motion.
///
/// Crossterm's `EnableMouseCapture` selects modes 1000, 1002, and finally 1003.
/// Resetting 1003 afterward does not reliably fall back to 1002 because terminals
/// may treat these modes as mutually exclusive selectors. Select 1002 directly
/// instead: it reports every event bonsai consumes without flooding input with
/// bare pointer movement (`ESC[<35;…M`).
fn enable_mouse_capture(writer: &mut impl io::Write) -> io::Result<()> {
    execute!(writer, EnableButtonEventMouseCapture)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnableButtonEventMouseCapture;

impl crossterm::Command for EnableButtonEventMouseCapture {
    fn write_ansi(&self, writer: &mut impl std::fmt::Write) -> std::fmt::Result {
        writer.write_str(concat!(
            "\x1b[?1003l", // Clear stale any-motion mode from a prior crash.
            "\x1b[?1000h", // Press, release, and wheel events.
            "\x1b[?1002h", // Pointer movement while a button is held.
            "\x1b[?1015h", // Extended RXVT coordinates.
            "\x1b[?1006h", // Preferred SGR coordinates.
        ))
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        crossterm::Command::execute_winapi(&EnableMouseCapture)
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
    }
}

const TERMINAL_EVENT_DRAIN_LIMIT: usize = 256;
const TERMINAL_EVENT_DRAIN_BUDGET: Duration = Duration::from_millis(10);

fn drain_pending_terminal_events() {
    let started_at = Instant::now();
    let mut drained = 0usize;
    while drained < TERMINAL_EVENT_DRAIN_LIMIT
        && started_at.elapsed() < TERMINAL_EVENT_DRAIN_BUDGET
        && event::poll(Duration::from_millis(0)).unwrap_or(false)
    {
        let _ = event::read();
        drained = drained.saturating_add(1);
    }
    if drained == TERMINAL_EVENT_DRAIN_LIMIT || started_at.elapsed() >= TERMINAL_EVENT_DRAIN_BUDGET
    {
        tracing::debug!(
            drained,
            elapsed_ms = started_at.elapsed().as_millis() as u64,
            "terminal cleanup left pending input events undrained"
        );
    }
}

fn record_cleanup_error(
    first_error: &mut Option<anyhow::Error>,
    context: &'static str,
    result: io::Result<()>,
) {
    match result {
        Ok(()) => {}
        Err(err) if first_error.is_none() => {
            *first_error = Some(anyhow::anyhow!("{context}: {err}"));
        }
        Err(err) => {
            // Keep only the first failure as the returned error so callers see
            // the root cause, but log any later failures so a multi-part
            // cleanup breakdown isn't silently lost.
            tracing::debug!(context, error = %err, "secondary terminal cleanup step failed");
        }
    }
}

pub(in crate::tui::run) struct TuiSink {
    pub(in crate::tui::run) sender: mpsc::UnboundedSender<UiEvent>,
    pub(in crate::tui::run) completion_evidence:
        crate::completion_report::CompletionEvidenceCollector,
}

impl OutputSink for TuiSink {
    fn begin_completion_run(&self) {
        self.completion_evidence.begin_run();
    }

    fn completion_evidence(&self) -> Option<crate::completion_report::CompletionEvidenceSnapshot> {
        Some(self.completion_evidence.snapshot())
    }

    fn assistant_delta(&self, text: &str) {
        let _ = self.sender.send(UiEvent::AssistantDelta(text.to_string()));
    }

    fn assistant_done(&self) {
        let _ = self.sender.send(UiEvent::AssistantDone);
    }

    fn reasoning_delta(&self, text: &str) {
        let _ = self.sender.send(UiEvent::ReasoningDelta(text.to_string()));
    }

    fn attempt_started(&self) {
        let _ = self.sender.send(UiEvent::AttemptStarted);
    }

    fn attempt_discarded(&self) {
        let _ = self.sender.send(UiEvent::AttemptDiscarded);
    }

    fn thinking(&self, text: &str) {
        let _ = self.sender.send(UiEvent::Thinking(text.to_string()));
    }

    fn tool_calls_started(&self, calls: &[ToolCallStart]) {
        if calls.is_empty() {
            return;
        }
        for call in calls {
            self.completion_evidence
                .record_tool_started(&call.id, &call.name, &call.arguments);
        }
        let _ = self.sender.send(UiEvent::ToolCallsStarted {
            calls: calls.to_vec(),
            started_at: Instant::now(),
        });
    }

    fn tool_started(&self, id: &str, name: &str, arguments: &str) {
        self.completion_evidence
            .record_tool_started(id, name, arguments);
        let _ = self.sender.send(UiEvent::ToolStarted {
            id: id.to_string(),
            name: name.to_string(),
            arguments: arguments.to_string(),
            started_at: Instant::now(),
        });
    }

    fn tool_output(&self, id: &str, output: &str) {
        let _ = self.sender.send(UiEvent::ToolOutput {
            id: id.to_string(),
            output: output.to_string(),
            updated_at: Instant::now(),
        });
    }

    fn tool_finished(&self, id: &str, result: &str, status: crate::output::ToolExecutionStatus) {
        self.completion_evidence
            .record_tool_result(id, result, status, None);
        let _ = self.sender.send(UiEvent::ToolFinished {
            id: id.to_string(),
            result: result.to_string(),
            success: status.is_success(),
            finished_at: Instant::now(),
        });
    }

    fn tool_finished_with_diff(
        &self,
        id: &str,
        result: &str,
        status: crate::output::ToolExecutionStatus,
        diff: FileDiff,
    ) {
        self.completion_evidence
            .record_tool_result(id, result, status, Some(&diff));
        let _ = self.sender.send(UiEvent::ToolFinishedWithDiff {
            id: id.to_string(),
            result: result.to_string(),
            success: status.is_success(),
            diff: Box::new(diff),
            finished_at: Instant::now(),
        });
    }

    fn workspace_changed(&self, paths: &[String], intent: &str) {
        self.completion_evidence
            .record_workspace_changes(paths, intent);
        let _ = self.sender.send(UiEvent::WorkspaceChanged {
            paths: paths.to_vec(),
        });
    }

    fn queued_user_message_sent(&self, id: u64, text: &str) {
        let _ = self.sender.send(UiEvent::QueuedUserMessageSent {
            id,
            text: text.to_string(),
        });
    }

    fn context_updated(&self, report: crate::agent::ContextReport) {
        let _ = self.sender.send(UiEvent::ContextUpdated(Box::new(report)));
    }

    fn status(&self, text: &str) {
        let _ = self.sender.send(UiEvent::Status(text.to_string()));
    }

    fn compaction_status(&self, text: &str) {
        let _ = self
            .sender
            .send(UiEvent::CompactionStatus(text.to_string()));
    }

    fn error(&self, text: &str) {
        let _ = self
            .sender
            .send(UiEvent::Error(UiError::new("UI error", text)));
    }
}

pub(in crate::tui::run) fn git_branch(project_root: &std::path::Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8(output.stdout).ok()?;
    let branch = branch.trim();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion_report::{
        CompletionReport, CompletionSessionEvidence, CompletionStatus, classify_completion_status,
    };
    use crate::headless::{HeadlessSink, OutputFormat};
    use crate::output::{OutputSink, ToolExecutionStatus};

    #[test]
    fn screen_reader_uses_inline_terminal_viewport() {
        assert!(matches!(terminal_viewport(false), Viewport::Fullscreen));
        assert!(matches!(
            terminal_viewport(true),
            Viewport::Inline(height) if height > 0
        ));
    }

    #[test]
    fn mouse_capture_finishes_with_any_motion_disabled() {
        let mut output = Vec::new();

        enable_mouse_capture(&mut output).unwrap();

        let stale_motion_reset = output
            .windows(8)
            .position(|window| window == b"\x1b[?1003l")
            .expect("mouse capture should clear stale any-motion mode");
        let button_capture = output
            .windows(8)
            .position(|window| window == b"\x1b[?1002h")
            .expect("mouse capture should enable button-motion mode");
        assert!(stale_motion_reset < button_capture);
        assert!(output.windows(8).any(|window| window == b"\x1b[?1000h"));
        assert!(!output.windows(8).any(|window| window == b"\x1b[?1003h"));
    }

    #[cfg(unix)]
    #[test]
    fn cooked_terminal_local_flags_are_not_raw() {
        use nix::sys::termios::LocalFlags;

        assert!(terminal_local_flags_are_raw(LocalFlags::empty()));
        for cooked_flag in [
            LocalFlags::ECHO,
            LocalFlags::ICANON,
            LocalFlags::IEXTEN,
            LocalFlags::ISIG,
        ] {
            assert!(!terminal_local_flags_are_raw(cooked_flag));
        }
    }

    fn run_script(sink: &dyn OutputSink) {
        sink.begin_completion_run();
        sink.tool_started("call-1", "bash", r#"{"command":"cargo test"}"#);
        sink.tool_finished("call-1", "tests failed", ToolExecutionStatus::Failed);
        sink.tool_started(
            "call-2",
            "bash",
            r#"{"timeout_ms":5000,"command":"cargo test"}"#,
        );
        sink.tool_finished("call-2", "tests passed", ToolExecutionStatus::Succeeded);
        sink.tool_started("call-3", "edit", r#"{"path":"src/main.rs"}"#);
        sink.tool_finished_with_diff(
            "call-3",
            "update main",
            ToolExecutionStatus::Succeeded,
            crate::diff::build_file_diff(
                "src/main.rs".to_string(),
                Some("fn main() {}\n"),
                "fn main() { println!(\"ok\"); }\n",
            ),
        );
    }

    fn report_for(sink: &dyn OutputSink) -> CompletionReport {
        let evidence = sink.completion_evidence().unwrap();
        let status = classify_completion_status(CompletionStatus::Completed, &evidence, None);
        CompletionReport::from_evidence(
            status,
            evidence,
            CompletionSessionEvidence {
                verification: None,
                review: None,
                authorization_decisions: &[],
                usage: crate::agent::UsageTotals::default(),
                session_budget: crate::run_budget::SessionBudgetUsage::default(),
                budget_exhaustion: None,
            },
        )
    }

    #[test]
    fn tui_and_headless_sinks_normalize_the_same_completion_report() {
        let headless = HeadlessSink::new(
            OutputFormat::Json,
            Box::new(Vec::<u8>::new()),
            Box::new(Vec::<u8>::new()),
        );
        let (sender, _receiver) = mpsc::unbounded_channel();
        let tui = TuiSink {
            sender,
            completion_evidence: crate::completion_report::CompletionEvidenceCollector::default(),
        };

        run_script(&headless);
        run_script(&tui);

        assert_eq!(report_for(&headless), report_for(&tui));
    }

    #[test]
    fn failed_untrusted_output_surfaces_failed_tui_and_completion_evidence() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let tui = TuiSink {
            sender,
            completion_evidence: crate::completion_report::CompletionEvidenceCollector::default(),
        };
        let output = crate::tool::ToolOutput::untrusted_context_with_status(
            "mcp:fake:do_write",
            "Error: write not permitted",
            ToolExecutionStatus::Failed,
        );

        tui.begin_completion_run();
        tui.tool_started("call-mcp", "mcp__fake__do_write", "{}");
        tui.tool_finished(
            "call-mcp",
            output.rendered_summary(),
            output.execution_status(),
        );

        assert!(matches!(
            receiver.try_recv(),
            Ok(UiEvent::ToolStarted { .. })
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(UiEvent::ToolFinished { success: false, .. })
        ));
        assert!(tui.completion_evidence().unwrap().unresolved_tool_failure);
    }
}
