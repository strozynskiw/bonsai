//! Shared helpers for capping model-visible tool output so a single call can't
//! flood the context window.
//!
//! Tools cap by their natural unit — character count here,
//! match count in [`crate::tool::search::format_truncation`] — but share one
//! footer vocabulary: a bracketed `[Truncated: …]` note that states what was
//! dropped and the exact next action to recover it.
//!
//! Scope: this covers the count-based search footer and the char-based caps for
//! directory `read` and `grep` content. `bash` output spooling
//! ([`crate::tool::ToolOutput::Command`] → `.bonsai/tool-output/`) and `read`
//! large-file paging keep their own bespoke capping; unifying those is a later
//! refactor, not part of this helper.

/// Wrap a truncation reason in the standard bracketed footer. The returned
/// string carries its own leading blank line so callers can `push_str` it
/// directly onto their output.
pub(crate) fn truncation_note(body: &str) -> String {
    format!("\n\n[Truncated: {body}]")
}

/// Cap `text` to `max_chars` characters. Returns it unchanged when within
/// budget; otherwise truncates on a char boundary and appends the standardized
/// footer stating total size, included size, and the next action to see more.
pub(crate) fn cap_text(text: String, max_chars: usize, next_action: &str) -> String {
    let Some((byte_idx, _)) = text.char_indices().nth(max_chars) else {
        return text;
    };
    let total = max_chars + text[byte_idx..].chars().count();
    let mut capped = text;
    capped.truncate(byte_idx);
    capped.push_str(&truncation_note(&format!(
        "showing {max_chars} of {total} chars. {next_action}"
    )));
    capped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_text_passes_short_text_through() {
        let text = "hello".to_string();
        assert_eq!(cap_text(text.clone(), 100, "next"), text);
    }

    #[test]
    fn cap_text_truncates_with_total_included_and_next_action() {
        let text = "a".repeat(50);
        let out = cap_text(text, 10, "Narrow the query.");
        assert!(out.starts_with(&"a".repeat(10)), "{out}");
        assert!(out.contains("showing 10 of 50 chars"), "{out}");
        assert!(out.contains("Narrow the query."), "{out}");
        assert!(out.contains("[Truncated:"), "{out}");
    }

    #[test]
    fn cap_text_truncates_on_char_boundary() {
        // Each 'é' is two UTF-8 bytes; capping mid-string must not split one.
        let text = "é".repeat(20);
        let out = cap_text(text, 5, "next");
        // Prefix is exactly 5 chars (10 bytes) plus the footer.
        let prefix: String = out.chars().take_while(|&c| c == 'é').collect();
        assert_eq!(prefix.chars().count(), 5, "{out}");
        assert!(out.contains("showing 5 of 20 chars"), "{out}");
    }
}
