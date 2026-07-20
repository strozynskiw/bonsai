//! Small pure string helpers: truncation, previews, list rendering, and a
//! monotonic-ish wall-clock timestamp. No I/O, no message-type knowledge.

use super::*;

pub(super) fn truncate(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{truncated}... ({char_count} chars total)")
    }
}

pub(super) fn evidence_preview(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }
    let preview = text.chars().take(max_chars).collect::<String>();
    format!("{preview}\n...(+{} more chars)", char_count - max_chars)
}

pub(super) fn one_line_preview(text: impl AsRef<str>, max_chars: usize) -> String {
    let one_line = text
        .as_ref()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if one_line.chars().count() <= max_chars {
        one_line
    } else {
        format!(
            "{}...",
            one_line.chars().take(max_chars).collect::<String>()
        )
    }
}

pub(super) fn list_or_placeholder(items: &[String], placeholder: &str) -> String {
    if items.is_empty() {
        placeholder.to_string()
    } else {
        items.join("\n")
    }
}

pub(super) fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}
