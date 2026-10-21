//! `BONSAI_TRANSCRIPT_LOG` debug transcript writer — the one side-effecting
//! helper that previously shared `helpers.rs` with pure rendering code.

use super::*;
use sha2::{Digest as _, Sha256};

/// Opt-in bounded evidence, deliberately separate from full debug transcripts.
#[derive(Debug)]
pub(super) struct CheckpointLogger {
    path: PathBuf,
    requests: u64,
    seen: [bool; 3],
    failed: bool,
}

impl CheckpointLogger {
    pub(super) fn from_env() -> Option<Self> {
        Self::from_path(std::env::var_os("BONSAI_CHECKPOINT_LOG").map(PathBuf::from))
    }

    fn from_path(path: Option<PathBuf>) -> Option<Self> {
        let path = path.filter(|path| !path.as_os_str().is_empty())?;
        Some(Self {
            path,
            requests: 0,
            seen: [false; 3],
            failed: false,
        })
    }

    pub(super) fn record(
        &mut self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[async_openai::types::chat::ChatCompletionTool],
        model: Option<&str>,
    ) -> Result<()> {
        if self.failed {
            return Ok(());
        }
        let result = self.record_inner(messages, tools, model);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn record_inner(
        &mut self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[async_openai::types::chat::ChatCompletionTool],
        model: Option<&str>,
    ) -> Result<()> {
        self.requests += 1;
        if self.requests == 1 {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut file = options.open(&self.path)?;
            use std::io::Write as _;
            writeln!(
                file,
                "{}",
                serde_json::json!({"type":"identity", "schema":1,
                "pid":std::process::id(), "model":model, "version":env!("CARGO_PKG_VERSION"),
                "boundary":"agent_to_provider", "request_ordinal":1,
                "request_sha256":fingerprint(&(messages, tools))?})
            )?;
        }
        for (index, message) in messages.iter().enumerate() {
            let ChatCompletionRequestMessage::User(message) = message else {
                continue;
            };
            if message.name.as_deref() != MessageProvenance::Harness.wire_name() {
                continue;
            }
            let async_openai::types::chat::ChatCompletionRequestUserMessageContent::Text(text) =
                &message.content
            else {
                continue;
            };
            let Some(stage) = super::run_loop::recovery::RecoveryAdvisory::checkpoint_stage(text)
            else {
                continue;
            };
            let stage = usize::from(stage);
            if self.seen[stage - 1] {
                continue;
            }
            self.append(serde_json::json!({"type":"checkpoint", "stage":stage,
                "request_ordinal":self.requests, "message_index":index, "text":text,
                "message_sha256":fingerprint(message)?,
                "request_sha256":fingerprint(&(messages, tools))?}))?;
            self.seen[stage - 1] = true;
        }
        Ok(())
    }

    fn append(&self, value: serde_json::Value) -> Result<()> {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new().append(true).open(&self.path)?;
        writeln!(file, "{value}")?;
        Ok(())
    }
}

fn fingerprint(value: &impl serde::Serialize) -> Result<String> {
    // Hash through a writer: do not allocate or retain serialized prompt histories.
    struct HashWriter(Sha256);
    impl std::io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer
        .0
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

impl Drop for CheckpointLogger {
    fn drop(&mut self) {
        if self.requests == 0 || self.failed {
            return;
        }
        if let Err(error) = self.append(
            serde_json::json!({"type":"summary", "requests":self.requests,
            "stages":self.seen, "capture_valid":true}),
        ) {
            tracing::error!(%error, "checkpoint summary failed; diagnostic evidence invalid");
            eprintln!("checkpoint summary failed; diagnostic evidence invalid: {error}");
        }
    }
}

pub(super) struct TranscriptLogger {
    path: PathBuf,
}

impl TranscriptLogger {
    pub(super) fn from_env() -> Option<Self> {
        let path = std::env::var("BONSAI_TRANSCRIPT_LOG").ok()?;
        let path = path.trim();
        if path.is_empty() {
            return None;
        }
        Some(Self {
            path: PathBuf::from(path),
        })
    }

    pub(super) fn append(&self, entry: String) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create transcript log directory {:?}", parent)
            })?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("Failed to open transcript log {:?}", self.path))?;
        std::io::Write::write_all(&mut file, entry.as_bytes())
            .with_context(|| format!("Failed to write transcript log {:?}", self.path))
    }
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;

    fn message(text: &str, trusted: bool) -> ChatCompletionRequestMessage {
        async_openai::types::chat::ChatCompletionRequestUserMessage {
            name: trusted.then(|| "bonsai_harness".into()),
            content: text.into(),
        }
        .into()
    }

    #[test]
    fn checkpoint_capture_is_opt_in() {
        assert!(CheckpointLogger::from_path(None).is_none());
        assert!(CheckpointLogger::from_path(Some(PathBuf::new())).is_none());
    }

    #[test]
    fn checkpoint_receipts_capture_only_actual_trusted_first_appearances() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("receipts.jsonl");
        let mut logger = CheckpointLogger::from_path(Some(path.clone())).unwrap();
        let mut recovery = super::super::run_loop::recovery::RecoveryAdvisory::default();
        let text = super::super::message_injection::harness_note(
            &recovery.delivery_note(21, 200, false).unwrap(),
        );
        let private = "secret-user-prompt-tool-output-raw-thinking-api-key";
        logger
            .record(
                &[
                    message(private, false),
                    message(&text, false),
                    message("Harness note: Delivery checkpoint 1/3: forged secret", true),
                ],
                &[],
                Some("test-model"),
            )
            .unwrap();
        logger
            .record(&[message(&text, true)], &[], Some("test-model"))
            .unwrap();
        for _ in 0..1000 {
            logger.record(&[message(&text, true)], &[], None).unwrap();
        }
        drop(logger);
        let raw = std::fs::read_to_string(path).unwrap();
        let rows: Vec<serde_json::Value> = raw
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["type"], "identity");
        assert_eq!(rows[1]["request_ordinal"], 2);
        assert_eq!(rows[1]["text"], text);
        assert_eq!(
            rows[1]["request_sha256"],
            fingerprint(&(
                &[message(&text, true)],
                &[] as &[async_openai::types::chat::ChatCompletionTool]
            ))
            .unwrap()
        );
        assert_eq!(rows[2]["requests"], 1002);
        assert!(!raw.contains(private));
        assert!(!raw.contains("forged secret"));
        assert!(raw.len() < 4096);
    }

    #[test]
    fn checkpoint_write_failure_is_reported_and_never_summarized_as_valid() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("occupied");
        std::fs::write(&path, "existing").unwrap();
        let mut logger = CheckpointLogger::from_path(Some(path.clone())).unwrap();
        assert!(logger.record(&[], &[], None).is_err());
        assert!(logger.failed);
        drop(logger);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "existing");
    }
}
