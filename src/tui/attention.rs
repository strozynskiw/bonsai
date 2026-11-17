//! Tells the user when the TUI needs them: a long run finished, or an approval
//! or question is waiting. The terminal bell and window title are the default;
//! `BONSAI_NOTIFY=desktop` adds a desktop notification and `off` disables all.
//! Notification text is fixed — it never carries prompt, path, or code text.

use std::time::{Duration, Instant};

use crate::tui::event::{DetailModal, ModalKind, PromptFamily, TaskState};

/// Runs shorter than this finish quietly; you were probably still watching.
const LONG_RUN: Duration = Duration::from_secs(10);
const ENV: &str = "BONSAI_NOTIFY";
const IDLE_TITLE: &str = "bonsai";

/// How far a signal goes beyond the TUI itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotifyMode {
    Off,
    Bell,
    Desktop,
}

impl NotifyMode {
    pub(crate) fn from_env() -> Self {
        Self::parse(std::env::var(ENV).ok().as_deref())
    }

    fn parse(value: Option<&str>) -> Self {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("off" | "0" | "false" | "none") => Self::Off,
            Some("desktop") => Self::Desktop,
            _ => Self::Bell,
        }
    }
}

/// Something worth pulling the user back for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Attention {
    NeedsInput,
    RunFinished,
}

impl Attention {
    fn title(self) -> &'static str {
        match self {
            Self::NeedsInput => "bonsai · needs your input",
            Self::RunFinished => "bonsai · done",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::NeedsInput => "Waiting for your approval or answer",
            Self::RunFinished => "Run finished",
        }
    }
}

/// Edge detector over per-frame observations. Pure: no I/O, so the rules are
/// unit-testable; [`signal`] performs the effects.
#[derive(Debug, Default)]
pub(crate) struct AttentionTracker {
    run_started: Option<Instant>,
    prompt_open: bool,
    title_dirty: bool,
}

impl AttentionTracker {
    /// Feed one frame's state. Returns the event to signal, if any, and
    /// whether the window title should go back to idle.
    pub(crate) fn observe(
        &mut self,
        running: bool,
        needs_input: bool,
        now: Instant,
    ) -> (Option<Attention>, bool) {
        let mut event = None;
        if needs_input && !self.prompt_open {
            event = Some(Attention::NeedsInput);
        }
        self.prompt_open = needs_input;
        match (running, self.run_started) {
            (true, None) => self.run_started = Some(now),
            (false, Some(started)) => {
                self.run_started = None;
                if event.is_none() && !needs_input && now.duration_since(started) >= LONG_RUN {
                    event = Some(Attention::RunFinished);
                }
            }
            _ => {}
        }
        if event.is_some() {
            self.title_dirty = true;
            return (event, false);
        }
        // Back to work (a new run or the prompt answered): clear the title.
        let reset = self.title_dirty && (running && !needs_input);
        if reset {
            self.title_dirty = false;
        }
        (None, reset)
    }
}

/// Whether the TUI is blocked on the user: an approval prompt or a question.
pub(crate) fn needs_input(modal: Option<&ModalKind>) -> bool {
    modal.is_some_and(|modal| {
        PromptFamily::of_modal(modal).is_some()
            || matches!(modal, ModalKind::Detail(DetailModal::QuestionPrompt { .. }))
    })
}

pub(crate) fn is_running(state: TaskState) -> bool {
    matches!(state, TaskState::Running | TaskState::Cancelling)
}

/// Bell + title (and optionally a desktop notification) for `event`, or a
/// title reset. Write failures are ignored: a missed bell is not an error.
pub(crate) fn signal(mode: NotifyMode, event: Option<Attention>, reset_title: bool) {
    use std::io::Write;
    if mode == NotifyMode::Off {
        return;
    }
    let mut out = std::io::stdout();
    if let Some(event) = event {
        let _ = write!(out, "\x07\x1b]0;{}\x07", event.title());
        if mode == NotifyMode::Desktop {
            desktop(event.message());
        }
    } else if reset_title {
        let _ = write!(out, "\x1b]0;{IDLE_TITLE}\x07");
    } else {
        return;
    }
    let _ = out.flush();
}

/// Fire-and-forget desktop notification with fixed text.
fn desktop(message: &'static str) {
    let command = if cfg!(target_os = "macos") {
        let mut command = std::process::Command::new("osascript");
        command.args([
            "-e",
            &format!("display notification \"{message}\" with title \"bonsai\""),
        ]);
        command
    } else {
        let mut command = std::process::Command::new("notify-send");
        command.args(["bonsai", message]);
        command
    };
    let mut command = command;
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    std::thread::spawn(move || {
        if let Err(error) = command.status() {
            tracing::debug!(%error, "desktop notification failed");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_defaults_to_bell_and_parses_overrides() {
        assert_eq!(NotifyMode::parse(None), NotifyMode::Bell);
        assert_eq!(NotifyMode::parse(Some("off")), NotifyMode::Off);
        assert_eq!(NotifyMode::parse(Some(" Desktop ")), NotifyMode::Desktop);
        assert_eq!(NotifyMode::parse(Some("whatever")), NotifyMode::Bell);
    }

    #[test]
    fn long_runs_signal_on_finish_short_runs_stay_quiet() {
        let t0 = Instant::now();
        let mut tracker = AttentionTracker::default();
        assert_eq!(tracker.observe(true, false, t0), (None, false));
        assert_eq!(
            tracker.observe(false, false, t0 + Duration::from_secs(3)),
            (None, false)
        );
        tracker.observe(true, false, t0 + Duration::from_secs(5));
        assert_eq!(
            tracker.observe(false, false, t0 + Duration::from_secs(20)),
            (Some(Attention::RunFinished), false)
        );
        // Idle frames repeat nothing.
        assert_eq!(
            tracker.observe(false, false, t0 + Duration::from_secs(21)),
            (None, false)
        );
    }

    #[test]
    fn a_prompt_signals_once_and_the_title_resets_when_work_resumes() {
        let t0 = Instant::now();
        let mut tracker = AttentionTracker::default();
        tracker.observe(true, false, t0);
        assert_eq!(
            tracker.observe(true, true, t0),
            (Some(Attention::NeedsInput), false)
        );
        assert_eq!(tracker.observe(true, true, t0), (None, false));
        assert_eq!(
            tracker.observe(true, false, t0),
            (None, true),
            "answering the prompt restores the idle title"
        );
        assert_eq!(tracker.observe(true, false, t0), (None, false));
    }

    #[test]
    fn a_prompt_left_open_at_run_end_does_not_double_signal() {
        let t0 = Instant::now();
        let mut tracker = AttentionTracker::default();
        tracker.observe(true, false, t0);
        tracker.observe(true, true, t0 + Duration::from_secs(30));
        assert_eq!(
            tracker.observe(false, true, t0 + Duration::from_secs(31)),
            (None, false)
        );
    }
}
