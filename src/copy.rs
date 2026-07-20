use std::borrow::Cow;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};

use crate::tui::app::TranscriptItem;

#[derive(Debug, Clone)]
pub(crate) struct Clipboard {
    sink: Arc<dyn ClipboardSink>,
}

impl Clipboard {
    pub(crate) fn system() -> Self {
        Self {
            sink: Arc::new(SystemClipboard),
        }
    }

    #[cfg(test)]
    pub(crate) fn new(sink: Arc<dyn ClipboardSink>) -> Self {
        Self { sink }
    }

    pub(crate) fn set_text(&self, text: &str) -> Result<()> {
        if text.is_empty() {
            return Err(anyhow!("Nothing to copy yet."));
        }
        self.sink.set_text(text)
    }

    pub(crate) fn get_text(&self) -> Result<Option<String>> {
        self.sink.get_text()
    }

    pub(crate) fn get_image(&self) -> Result<Option<ClipboardImage>> {
        self.sink.get_image()
    }
}

/// Raw RGBA8 clipboard image as delivered by the platform clipboard
/// (row-major, `width * height * 4` bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClipboardImage {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

impl Default for Clipboard {
    fn default() -> Self {
        Self::system()
    }
}

pub(crate) trait ClipboardSink: std::fmt::Debug + Send + Sync {
    fn set_text(&self, text: &str) -> Result<()>;

    /// `Ok(None)` means the clipboard holds no text (not an error).
    fn get_text(&self) -> Result<Option<String>> {
        Ok(None)
    }

    /// `Ok(None)` means the clipboard holds no image (not an error).
    fn get_image(&self) -> Result<Option<ClipboardImage>> {
        Ok(None)
    }
}

#[derive(Debug)]
struct SystemClipboard;

impl ClipboardSink for SystemClipboard {
    fn set_text(&self, text: &str) -> Result<()> {
        let mut clipboard =
            arboard::Clipboard::new().context("Failed to access system clipboard")?;
        clipboard
            .set_text(text.to_string())
            .context("Failed to write to system clipboard")
    }

    fn get_text(&self) -> Result<Option<String>> {
        let mut clipboard =
            arboard::Clipboard::new().context("Failed to access system clipboard")?;
        match clipboard.get_text() {
            Ok(text) => Ok(Some(text)),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(err) => Err(err).context("Failed to read text from system clipboard"),
        }
    }

    fn get_image(&self) -> Result<Option<ClipboardImage>> {
        let mut clipboard =
            arboard::Clipboard::new().context("Failed to access system clipboard")?;
        match clipboard.get_image() {
            Ok(image) => Ok(Some(ClipboardImage {
                width: image.width,
                height: image.height,
                rgba: image.bytes.into_owned(),
            })),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(err) => Err(err).context("Failed to read image from system clipboard"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyTarget {
    All,
}

pub fn clean_transcript_item(item: &TranscriptItem) -> Cow<'_, str> {
    match item {
        TranscriptItem::UserMessage { text } | TranscriptItem::AssistantMessage { text } => {
            Cow::Borrowed(text.as_str())
        }
        TranscriptItem::QueuedUserMessage { text, .. } => Cow::Owned(format!("queued: {text}")),
        TranscriptItem::ReasoningSummary { text } => Cow::Owned(format!("reasoning: {text}")),
        TranscriptItem::Edit { text } => Cow::Owned(format!("edit: {text}")),
        TranscriptItem::TodoUpdate { text } => Cow::Owned(format!("todo: {text}")),
        TranscriptItem::WorkLog { text } => Cow::Owned(format!("note: {text}")),
        TranscriptItem::PeerMessage {
            session_id,
            outgoing,
            text,
            ..
        } => Cow::Owned(if *outgoing {
            format!("to agent #{session_id}: {text}")
        } else {
            format!("agent #{session_id}: {text}")
        }),
        TranscriptItem::CommandOutput { kind, text } => {
            Cow::Owned(format!("{}: {}", kind.label(), text))
        }
        TranscriptItem::CompletionReport(report) => {
            Cow::Owned(format!("completion: {}", report.render_compact()))
        }
        TranscriptItem::Error { error } => {
            Cow::Owned(format!("error: {}: {}", error.title, error.detail))
        }
        TranscriptItem::ToolActivity(activity) => Cow::Owned(clean_tool_activity(activity)),
        TranscriptItem::ExecutionGroup(group) => {
            let mut lines = Vec::new();
            for (index, activity) in group.tools.iter().enumerate() {
                let mut line = format!(
                    "{}. {} ({}) - {}",
                    index + 1,
                    activity.name,
                    activity.id,
                    activity.status.label()
                );
                if let Some(result) = &activity.result {
                    line.push_str(&format!(" -> {result}"));
                }
                lines.push(line);
            }
            if lines.is_empty() {
                Cow::Borrowed("execution group: <empty>")
            } else {
                Cow::Owned(format!("execution group:\n{}\n", lines.join("\n")))
            }
        }
    }
}

pub fn clean_tool_activity(activity: &crate::tui::app::ToolActivity) -> String {
    let mut body = format!(
        "tool {}: {} ({})\nargs: {}\n",
        activity.status.label(),
        activity.name,
        activity.id,
        activity.arguments
    );
    if let Some(result) = &activity.result {
        body.push_str(&format!("result: {result}\n"));
    }
    body
}

/// Unlabelled body text for an item — what the user actually sees in
/// the card. This is the string the char-range selection highlights
/// and slices; `clean_transcript_item` adds copy-time labels
/// (`reasoning:`, `error:`, `tool done:`) that are *not* part of the
/// selection stream.
pub fn body_text_for(item: &TranscriptItem) -> Cow<'_, str> {
    match item {
        TranscriptItem::UserMessage { text }
        | TranscriptItem::QueuedUserMessage { text, .. }
        | TranscriptItem::AssistantMessage { text }
        | TranscriptItem::ReasoningSummary { text }
        | TranscriptItem::Edit { text }
        | TranscriptItem::TodoUpdate { text }
        | TranscriptItem::WorkLog { text }
        | TranscriptItem::PeerMessage { text, .. }
        | TranscriptItem::CommandOutput { text, .. } => Cow::Borrowed(text.as_str()),
        TranscriptItem::CompletionReport(report) => Cow::Owned(report.render_compact()),
        TranscriptItem::Error { error } => Cow::Borrowed(error.detail.as_str()),
        TranscriptItem::ExecutionGroup(group) => {
            if group.tools.is_empty() {
                Cow::Borrowed("<empty execution group>")
            } else {
                let first = group
                    .tools
                    .first()
                    .map(|activity| format!("{} ({})", activity.name, activity.id))
                    .unwrap_or_else(|| "execution group".to_string());
                Cow::Owned(format!("execution group: {first}"))
            }
        }
        TranscriptItem::ToolActivity(_) => Cow::Borrowed(""),
    }
}

/// Whether the item supports per-grapheme char-range selection. Tool
/// cards stay item-granular and the hit-test returns the
/// `usize::MAX` "whole" sentinel for them.
pub fn is_text_item(item: &TranscriptItem) -> bool {
    !matches!(
        item,
        TranscriptItem::ToolActivity(_) | TranscriptItem::ExecutionGroup(_)
    )
}

pub fn collect_transcript_text(items: &[TranscriptItem], target: CopyTarget) -> Option<String> {
    let slice: &[TranscriptItem] = match target {
        CopyTarget::All => items,
    };

    if slice.is_empty() {
        return None;
    }

    let text = slice
        .iter()
        .filter(|item| !item.is_suppressed())
        .map(clean_transcript_item)
        .map(|cow| cow.into_owned())
        .collect::<Vec<_>>()
        .join("\n\n")
        .trim_end()
        .to_string();
    (!text.is_empty()).then_some(text)
}

pub(crate) fn copy_text_to(clipboard: &Clipboard, text: &str) -> Result<()> {
    clipboard.set_text(text)
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Arc, Mutex};

    use anyhow::{Result, anyhow};

    use super::{Clipboard, ClipboardImage, ClipboardSink};

    #[derive(Debug, Default)]
    struct FakeClipboard {
        text: Arc<Mutex<Option<String>>>,
        image: Option<ClipboardImage>,
    }

    impl ClipboardSink for FakeClipboard {
        fn set_text(&self, text: &str) -> Result<()> {
            let mut guard = self
                .text
                .lock()
                .map_err(|_| anyhow!("fake clipboard lock poisoned"))?;
            *guard = Some(text.to_string());
            Ok(())
        }

        fn get_text(&self) -> Result<Option<String>> {
            let guard = self
                .text
                .lock()
                .map_err(|_| anyhow!("fake clipboard lock poisoned"))?;
            Ok(guard.clone())
        }

        fn get_image(&self) -> Result<Option<ClipboardImage>> {
            Ok(self.image.clone())
        }
    }

    pub(crate) fn fake_clipboard() -> (Clipboard, Arc<Mutex<Option<String>>>) {
        let text = Arc::new(Mutex::new(None));
        let clipboard = Clipboard::new(Arc::new(FakeClipboard {
            text: Arc::clone(&text),
            image: None,
        }));
        (clipboard, text)
    }

    pub(crate) fn fake_clipboard_with_image(image: ClipboardImage) -> Clipboard {
        Clipboard::new(Arc::new(FakeClipboard {
            text: Arc::new(Mutex::new(None)),
            image: Some(image),
        }))
    }
}

#[cfg(test)]
mod tests {
    use crate::tui::app::{ExecutionGroup, ToolActivity, ToolStatus};
    use crate::tui::event::CommandOutputKind;

    use super::*;

    fn user(text: &str) -> TranscriptItem {
        TranscriptItem::UserMessage {
            text: text.to_string(),
        }
    }

    fn assistant(text: &str) -> TranscriptItem {
        TranscriptItem::AssistantMessage {
            text: text.to_string(),
        }
    }

    #[test]
    fn empty_transcript_returns_none() {
        assert!(collect_transcript_text(&[], CopyTarget::All).is_none());
    }

    #[test]
    fn all_target_dumps_entire_transcript() {
        let items = vec![
            user("hi"),
            assistant("hello"),
            user("bye"),
            assistant("bye!"),
        ];
        let text = collect_transcript_text(&items, CopyTarget::All).unwrap();
        assert!(text.contains("hi"));
        assert!(text.contains("hello"));
        assert!(text.contains("bye"));
        assert!(text.contains("bye!"));
    }

    #[test]
    fn all_target_skips_suppressed_items() {
        let items = vec![assistant(""), assistant("visible")];

        let text = collect_transcript_text(&items, CopyTarget::All).unwrap();

        assert_eq!(text, "visible");
        assert!(collect_transcript_text(&[assistant("")], CopyTarget::All).is_none());
    }

    #[test]
    fn user_and_assistant_extract_plain_text() {
        assert_eq!(clean_transcript_item(&user("hi")).as_ref(), "hi");
        assert_eq!(clean_transcript_item(&assistant("ok")).as_ref(), "ok");
    }

    #[test]
    fn error_and_command_output_use_keyed_lines() {
        let err = TranscriptItem::Error {
            error: crate::tui::event::UiError::new("Boom", "detail"),
        };
        assert!(clean_transcript_item(&err).contains("error: Boom: detail"));

        let out = TranscriptItem::CommandOutput {
            kind: CommandOutputKind::Status,
            text: "done".to_string(),
        };
        assert_eq!(clean_transcript_item(&out).as_ref(), "status: done");
    }

    #[test]
    fn tool_activity_extracts_args_and_result() {
        let activity = ToolActivity {
            id: "call-1".to_string(),
            name: "bash".to_string(),
            arguments: "{\"command\":\"date\"}".to_string(),
            status: ToolStatus::Succeeded,
            result: Some("ok".to_string()),
            diff: None,
            started_at: std::time::Instant::now(),
            finished_at: None,
        };
        let item = TranscriptItem::ToolActivity(activity);
        let text = clean_transcript_item(&item);
        assert!(text.contains("tool done: bash"));
        assert!(text.contains("args:"));
        assert!(text.contains("result: ok"));
    }

    #[test]
    fn execution_group_copy_includes_tools() {
        let group = TranscriptItem::ExecutionGroup(ExecutionGroup {
            id: 1,
            finished_at: None,
            tools: vec![
                ToolActivity {
                    id: "call-1".to_string(),
                    name: "read".to_string(),
                    arguments: "{}".to_string(),
                    status: ToolStatus::Failed,
                    result: Some("boom".to_string()),
                    diff: None,
                    started_at: std::time::Instant::now(),
                    finished_at: Some(std::time::Instant::now()),
                },
                ToolActivity {
                    id: "call-2".to_string(),
                    name: "write".to_string(),
                    arguments: "{}".to_string(),
                    status: ToolStatus::Succeeded,
                    result: None,
                    diff: None,
                    started_at: std::time::Instant::now(),
                    finished_at: None,
                },
            ],
        });
        let text = clean_transcript_item(&group);

        let expected =
            "execution group:\n1. read (call-1) - failed -> boom\n2. write (call-2) - done\n";
        assert_eq!(text.as_ref(), expected);
    }

    #[test]
    fn execution_group_body_and_text_state() {
        let group = TranscriptItem::ExecutionGroup(ExecutionGroup {
            id: 1,
            finished_at: None,
            tools: vec![ToolActivity {
                id: "call-1".to_string(),
                name: "read".to_string(),
                arguments: "{\"file\":\"a\"}".to_string(),
                status: ToolStatus::Running,
                result: None,
                diff: None,
                started_at: std::time::Instant::now(),
                finished_at: None,
            }],
        });
        let body = body_text_for(&group);
        assert!(body.contains("read"));
        assert!(body.contains("call-1"));
        assert!(!is_text_item(&group));
    }
}
