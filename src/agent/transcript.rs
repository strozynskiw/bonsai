//! `BONSAI_TRANSCRIPT_LOG` debug transcript writer — the one side-effecting
//! helper that previously shared `helpers.rs` with pure rendering code.

use super::*;

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
