//! Process-wide tracing setup: which surface logs where, and at what level.
//!
//! Three invariants govern every mode:
//! - The user's terminal only ever sees errors by default; warns are advisory
//!   and belong in the per-session log file. `BONSAI_LOG`/`RUST_LOG` opt back
//!   into verbosity.
//! - Interactive TUI runs must never write to stderr (it would corrupt the
//!   alternate screen); they log to a per-session file, degrading to a sink.
//! - Logging setup can never fail a launch: every fallible step degrades.
//!
//! The opt-in support log rides the same subscriber: [`JsonlLifecycleLayer`]
//! captures the structured `bonsai::*` lifecycle events (turns, guards,
//! context, sessions, batching, delegation) as redacted JSONL for `/bug`
//! bundles. The layer is installed unconditionally — the global subscriber is
//! set before storage (and thus the preference) can be read — and gated at
//! runtime by [`support_log_enabled`], so an unset preference costs one atomic
//! load per event and writes no file at all.

use std::sync::atomic::{AtomicBool, Ordering};

/// Plain stderr tracing for CLI subcommands (eval, doctor, recovery, …).
pub(crate) fn init_tracing() {
    init_tracing_to_stderr();
}

/// Runtime gate for the support lifecycle log. Set from the persisted
/// `support_log` preference once storage opens, and flipped live by the
/// `/settings` toggle.
static SUPPORT_LOG_ENABLED: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_support_log_enabled(enabled: bool) {
    SUPPORT_LOG_ENABLED.store(enabled, Ordering::Relaxed);
}

pub(crate) fn support_log_enabled() -> bool {
    SUPPORT_LOG_ENABLED.load(Ordering::Relaxed)
}

/// How many recent per-session log files to keep in `$BONSAI_HOME/logs/`. Older
/// files are pruned on launch so the directory can't grow without bound.
const MAX_SESSION_LOGS: usize = 10;

pub(crate) fn init_tui_tracing() {
    use std::io::IsTerminal;

    if std::io::stderr().is_terminal() {
        // The TUI owns the terminal, so logging to stderr would corrupt the
        // alternate screen. Persist to a per-session file instead of discarding:
        // this is what makes shutdown/timeout diagnostics — and any warning that
        // fires mid-session — recoverable after the fact. Degrade to a no-op sink
        // only when no log file can be opened, so logging can never break launch.
        if !init_tracing_to_file() {
            init_tracing_to_sink();
        }
    } else {
        init_tracing_to_stderr();
    }
}

/// Tracing for headless `-p` runs: stderr stays error-only (script output must
/// remain clean; warns land in the file), while the same pruned per-session
/// file used by TUI runs captures the info-level turn/guard/context
/// diagnostics — without it a misbehaving headless run leaves no trace to
/// diagnose beyond the session database. Degrades to plain stderr tracing when
/// no log file can be opened.
pub(crate) fn init_headless_tracing() {
    use std::sync::Mutex;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{Layer, fmt};

    let Some(file) = open_session_log_file() else {
        init_tracing_to_stderr();
        return;
    };
    let _ = tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_target(false)
                .with_writer(std::io::stderr)
                .with_filter(tracing_filter("error")),
        )
        .with(
            // Targets stay on in the file: `bonsai::turn` / `bonsai::guard` /
            // `bonsai::context` lines are meant to be grepped by target.
            fmt::layer()
                .with_ansi(false)
                .with_writer(Mutex::new(file))
                .with_filter(tracing_filter("info")),
        )
        .with(JsonlLifecycleLayer::new())
        .try_init();
}

/// One JSONL line per `bonsai::*` lifecycle event, for `/bug` support
/// bundles. Deliberately dumb schema:
/// `{"ts":<epoch-ms>,"target":"bonsai::turn","level":"WARN","message":"…","fields":{"k":"v"}}`.
/// Every value — message included — passes [`crate::redact::redact`] before
/// serialization, so the file is safe to attach to an issue as-is. The sink
/// opens lazily on the first enabled event: opt-in means no file exists at
/// all while the preference is off.
pub(crate) struct JsonlLifecycleLayer {
    sink: std::sync::Mutex<JsonlSink>,
}

enum JsonlSink {
    /// Not opened yet. Opening happens on the first enabled event.
    Pending,
    Open(Box<dyn std::io::Write + Send>),
    /// Opening failed once; never retried, so a broken home dir costs one
    /// attempt per process instead of one per event.
    Dead,
}

impl JsonlLifecycleLayer {
    pub(crate) fn new() -> Self {
        Self {
            sink: std::sync::Mutex::new(JsonlSink::Pending),
        }
    }

    /// Test constructor: an already-open sink capturing into `writer`.
    #[cfg(test)]
    fn with_sink(writer: Box<dyn std::io::Write + Send>) -> Self {
        Self {
            sink: std::sync::Mutex::new(JsonlSink::Open(writer)),
        }
    }

    fn write_line(&self, line: &str) {
        let Ok(mut sink) = self.sink.lock() else {
            return;
        };
        if let JsonlSink::Pending = *sink {
            *sink = match open_session_lifecycle_file() {
                Some(file) => JsonlSink::Open(Box::new(file)),
                None => JsonlSink::Dead,
            };
        }
        if let JsonlSink::Open(writer) = &mut *sink {
            use std::io::Write;
            let _ = writeln!(writer, "{line}");
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for JsonlLifecycleLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if !support_log_enabled() {
            return;
        }
        let metadata = event.metadata();
        if !metadata.target().starts_with("bonsai::") {
            return;
        }

        let mut visitor = RedactingFieldVisitor::default();
        event.record(&mut visitor);

        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or(0);
        let line = serde_json::json!({
            "ts": ts as u64,
            "target": metadata.target(),
            "level": metadata.level().as_str(),
            "message": visitor.message,
            "fields": visitor.fields,
        });
        self.write_line(&line.to_string());
    }
}

/// Collects an event's fields as redacted strings. `message` is split out of
/// the map so the JSONL line reads naturally.
#[derive(Default)]
struct RedactingFieldVisitor {
    message: String,
    fields: std::collections::BTreeMap<String, String>,
}

impl RedactingFieldVisitor {
    fn record(&mut self, field: &tracing::field::Field, value: String) {
        let value = crate::redact::redact(&value).into_owned();
        if field.name() == "message" {
            self.message = value;
        } else {
            self.fields.insert(field.name().to_string(), value);
        }
    }
}

impl tracing::field::Visit for RedactingFieldVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.record(field, value.to_string());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.record(field, format!("{value:?}"));
    }
}

fn init_tracing_to_stderr() {
    use tracing_subscriber::fmt;
    // Error-only by default: warns (catalog drift, degraded extensions, …) are
    // advisory and belong in the per-session log file, not on the user's
    // terminal. BONSAI_LOG/RUST_LOG opt back into verbosity.
    let _ = fmt()
        .with_env_filter(tracing_filter("error"))
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Best-effort per-session file logger for TUI runs. Returns `false` (so the
/// caller falls back to a sink) when the log path can't be resolved/opened or a
/// global subscriber is already installed.
fn init_tracing_to_file() -> bool {
    use std::sync::Mutex;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{Layer, fmt};

    let Some(file) = open_session_log_file() else {
        return false;
    };
    // Targets stay on in the file: `bonsai::turn` / `bonsai::guard` /
    // `bonsai::context` lines are meant to be grepped by target.
    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_ansi(false)
                .with_writer(Mutex::new(file))
                .with_filter(tracing_filter("info")),
        )
        .with(JsonlLifecycleLayer::new())
        .try_init()
        .is_ok()
}

/// Create this session's log file under `$BONSAI_HOME/logs/`, alongside the
/// session database. Each run gets its own file (`bonsai-<start_ms>-<pid>.log`)
/// so concurrent peer sessions never interleave, and old files are pruned to the
/// most recent [`MAX_SESSION_LOGS`]. Returns `None` on any filesystem error so
/// the caller degrades to a sink rather than failing the launch.
fn open_session_log_file() -> Option<std::fs::File> {
    open_session_file(".log")
}

/// The support lifecycle JSONL beside the session log — same naming and
/// pruning, `.jsonl` suffix, opened lazily by [`JsonlLifecycleLayer`].
fn open_session_lifecycle_file() -> Option<std::fs::File> {
    open_session_file(".jsonl")
}

fn open_session_file(suffix: &str) -> Option<std::fs::File> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let paths = crate::storage::BonsaiPaths::discover().ok()?;
    let dir = paths.home_dir().join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    prune_old_session_logs(&dir, MAX_SESSION_LOGS, suffix);

    // Fixed-width millis keeps the lexical order chronological; the pid
    // disambiguates two sessions started in the same millisecond.
    let start_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    let name = format!("bonsai-{start_ms:013}-{}{suffix}", std::process::id());
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(name))
        .ok()
}

/// Delete all but the newest `keep` `bonsai-*<suffix>` files in `dir`.
/// Best-effort: filesystem errors are ignored, since failing to prune must
/// never block a run. Suffix-scoped so `.log` and `.jsonl` populations are
/// pruned independently.
pub(crate) fn prune_old_session_logs(dir: &std::path::Path, keep: usize, suffix: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut logs: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("bonsai-") && name.ends_with(suffix))
        })
        .collect();
    if logs.len() <= keep {
        return;
    }
    // The timestamp prefix makes filename order match creation order.
    logs.sort_unstable();
    for path in logs.iter().take(logs.len() - keep) {
        let _ = std::fs::remove_file(path);
    }
}

fn init_tracing_to_sink() {
    use tracing_subscriber::fmt;
    let _ = fmt()
        .with_env_filter(tracing_filter("warn"))
        .with_target(false)
        .with_writer(std::io::sink)
        .try_init();
}

fn tracing_filter(default_level: &str) -> tracing_subscriber::EnvFilter {
    use tracing_subscriber::EnvFilter;

    let default_level =
        if std::env::var_os("BONSAI_LOG").is_some() || std::env::var_os("RUST_LOG").is_some() {
            "info"
        } else {
            default_level
        };
    EnvFilter::try_from_default_env()
        // An env-provided filter can be arbitrarily verbose; still cap rmcp's
        // chatty auth transport at warn unless the filter names it itself.
        .map(|filter| match "rmcp::transport::auth=warn".parse() {
            Ok(directive) => filter.add_directive(directive),
            Err(_) => filter,
        })
        .unwrap_or_else(|_| EnvFilter::new(tracing_filter_directive(default_level)))
}

fn tracing_filter_directive(default_level: &str) -> String {
    // Dependencies never log louder than warn — and at the error-only stderr
    // default they follow bonsai down, so a stray dependency warn can't leak
    // onto the terminal either.
    let deps_level = if default_level == "error" {
        "error"
    } else {
        "warn"
    };
    format!("bonsai={default_level},{deps_level}")
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    /// A `Write` sink capturing into a shared buffer the test can read back.
    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl SharedBuffer {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    /// Serializes tests that flip the process-global support-log gate; without
    /// it the parallel test runner races one test's enable against another's
    /// silent-when-disabled assertion.
    static GATE_LOCK: Mutex<()> = Mutex::new(());

    fn with_lifecycle_layer(enabled: bool, emit: impl FnOnce()) -> String {
        use tracing_subscriber::layer::SubscriberExt;

        let _gate = GATE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let buffer = SharedBuffer::default();
        let layer = super::JsonlLifecycleLayer::with_sink(Box::new(buffer.clone()));
        let subscriber = tracing_subscriber::registry().with(layer);
        super::set_support_log_enabled(enabled);
        tracing::subscriber::with_default(subscriber, emit);
        super::set_support_log_enabled(false);
        buffer.contents()
    }

    #[test]
    fn lifecycle_layer_is_silent_until_opted_in() {
        let output = with_lifecycle_layer(false, || {
            tracing::info!(target: "bonsai::turn", turn = 3, "turn complete");
        });
        assert!(output.is_empty(), "opt-out must write nothing: {output}");
    }

    #[test]
    fn lifecycle_layer_captures_bonsai_targets_as_redacted_jsonl() {
        let output = with_lifecycle_layer(true, || {
            tracing::warn!(
                target: "bonsai::guard",
                guard = "read_storm",
                detail = "token sk-ant-api03-abcdefghijklmnopqrstuvwx1234567890abcdefghijklmnopqrstuvwx1234-abcdAA",
                "guard fired"
            );
            // Non-bonsai targets never land in the support log.
            tracing::warn!(target: "reqwest::connect", "should not appear");
        });

        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 1, "exactly the bonsai event: {output}");
        let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["target"], "bonsai::guard");
        assert_eq!(parsed["level"], "WARN");
        assert_eq!(parsed["message"], "guard fired");
        assert_eq!(parsed["fields"]["guard"], "read_storm");
        let detail = parsed["fields"]["detail"].as_str().unwrap();
        assert!(
            !detail.contains("sk-ant-"),
            "secrets must be redacted before serialization: {detail}"
        );
    }

    #[test]
    fn prune_keeps_newest_per_suffix_and_ignores_foreign_files() {
        let dir = tempfile::TempDir::new().unwrap();
        // 15 session logs whose timestamp prefixes sort chronologically, plus
        // lifecycle JSONL files that must be pruned as their own population.
        for millis in 0..15u128 {
            std::fs::write(
                dir.path().join(format!("bonsai-{millis:013}-42.log")),
                b"log",
            )
            .unwrap();
        }
        for millis in 0..3u128 {
            std::fs::write(
                dir.path().join(format!("bonsai-{millis:013}-42.jsonl")),
                b"{}",
            )
            .unwrap();
        }
        // Files that must never be touched: the session DB and an unrelated log.
        std::fs::write(dir.path().join("bonsai.db"), b"db").unwrap();
        std::fs::write(dir.path().join("other.log"), b"nope").unwrap();

        super::prune_old_session_logs(dir.path(), 10, ".log");
        super::prune_old_session_logs(dir.path(), 10, ".jsonl");

        let mut remaining: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name.starts_with("bonsai-") && name.ends_with(".log"))
            .collect();
        remaining.sort();
        // The 10 newest (millis 5..=14) survive; the 5 oldest are pruned.
        assert_eq!(remaining.len(), 10);
        assert_eq!(remaining.first().unwrap(), "bonsai-0000000000005-42.log");
        assert_eq!(remaining.last().unwrap(), "bonsai-0000000000014-42.log");
        // The jsonl population is below its own cap: untouched.
        let jsonl_count = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name.ends_with(".jsonl"))
            .count();
        assert_eq!(jsonl_count, 3);
        // Non-matching files are left alone.
        assert!(dir.path().join("bonsai.db").exists());
        assert!(dir.path().join("other.log").exists());
    }

    #[test]
    fn tui_file_tracing_can_default_to_info_without_env_filter() {
        assert_eq!(super::tracing_filter_directive("info"), "bonsai=info,warn");
        assert_eq!(super::tracing_filter_directive("warn"), "bonsai=warn,warn");
    }

    #[test]
    fn stderr_tracing_defaults_to_errors_only_including_dependencies() {
        assert_eq!(
            super::tracing_filter_directive("error"),
            "bonsai=error,error"
        );
    }
}
