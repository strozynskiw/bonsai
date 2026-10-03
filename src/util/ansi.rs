//! Strip terminal control sequences from text that leaves the process.
//!
//! Captured command output carries the escape sequences that were meant for a
//! terminal: color, cursor movement, charset switching. Those bytes are noise
//! in model context, in the transcript, and in persisted snapshots — they
//! destabilize text comparisons and prompt-cache prefixes without carrying
//! meaning. Layout whitespace (`\n`, `\t`) is preserved; every other control
//! byte is dropped, including the `\r` of a CRLF pair and the `\r` a progress
//! bar used to redraw in place.

use std::borrow::Cow;
use std::iter::Peekable;
use std::str::Chars;

/// Strip ANSI/VT escape sequences and non-layout control bytes from `input`.
///
/// Borrows the input when there is nothing to strip, so callers can run this on
/// every tool result without paying for an allocation in the common case.
pub(crate) fn strip_terminal_controls(input: &str) -> Cow<'_, str> {
    if !has_terminal_controls(input) {
        return Cow::Borrowed(input);
    }

    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\u{1b}' => skip_escape_sequence(&mut chars),
            '\n' | '\t' => output.push(ch),
            ch if ch.is_control() => {}
            ch => output.push(ch),
        }
    }
    Cow::Owned(output)
}

/// [`strip_terminal_controls`] in place, only reallocating when something was
/// stripped.
pub(crate) fn strip_terminal_controls_in_place(text: &mut String) {
    if let Cow::Owned(stripped) = strip_terminal_controls(text) {
        *text = stripped;
    }
}

/// Whether `input` holds anything [`strip_terminal_controls`] would remove.
fn has_terminal_controls(input: &str) -> bool {
    input
        .chars()
        .any(|ch| ch == '\u{1b}' || (ch.is_control() && !matches!(ch, '\n' | '\t')))
}

/// Consume the rest of an escape sequence whose introducing `ESC` the caller
/// already consumed. Unknown or truncated sequences are dropped, never passed
/// through: a stray `ESC` is exactly the byte this module exists to remove.
fn skip_escape_sequence(chars: &mut Peekable<Chars<'_>>) {
    match chars.next() {
        None => {}
        // CSI: parameter and intermediate bytes, then a final byte in `@`..=`~`.
        Some('[') => {
            for ch in chars.by_ref() {
                if ('@'..='~').contains(&ch) {
                    break;
                }
            }
        }
        // OSC, DCS, SOS, PM, APC: string sequences ended by BEL or ST (`ESC \`).
        Some(']' | 'P' | 'X' | '^' | '_') => {
            while let Some(ch) = chars.next() {
                if ch == '\u{7}' {
                    break;
                }
                if ch == '\u{1b}' {
                    let _ = chars.next_if_eq(&'\\');
                    break;
                }
            }
        }
        // Intermediate bytes (`ESC ( B`, `ESC # 8`, …) run to a final byte.
        Some(ch) if ('\u{20}'..='\u{2f}').contains(&ch) => {
            for ch in chars.by_ref() {
                if !('\u{20}'..='\u{2f}').contains(&ch) {
                    break;
                }
            }
        }
        // Every other escape sequence is two bytes (`ESC =`, `ESC >`, `ESC c`, …).
        Some(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_csi_color_sequences() {
        assert_eq!(
            strip_terminal_controls("\u{1b}[31merror\u{1b}[0m: bad"),
            "error: bad"
        );
        assert_eq!(
            strip_terminal_controls("\u{1b}[1;38;2;10;20;30mstyled\u{1b}[m"),
            "styled"
        );
    }

    #[test]
    fn removes_the_charset_and_reset_fragments_from_the_reported_case() {
        // Session 119: `cargo fmt --all -- --check` leaked `^[[31m`, `^[[32m`,
        // and `^[(B^[[m` into a model-facing excerpt.
        assert_eq!(
            strip_terminal_controls("a\u{1b}(B\u{1b}[mb\u{1b}[32mc"),
            "abc"
        );
        assert_eq!(strip_terminal_controls("\u{1b}#8\u{1b}[Htext"), "text");
    }

    #[test]
    fn removes_osc_strings_and_their_bel_or_st_terminators() {
        assert_eq!(strip_terminal_controls("x\u{1b}]0;title\u{7}y"), "xy");
        assert_eq!(
            strip_terminal_controls("x\u{1b}]8;;http://e\u{1b}\\y"),
            "xy"
        );
        assert_eq!(strip_terminal_controls("x\u{1b}Pq\u{1b}\\y"), "xy");
    }

    #[test]
    fn keeps_layout_whitespace_and_drops_other_controls() {
        // `\r\n` normalizes to `\n`; BEL, backspace, and DEL disappear.
        assert_eq!(
            strip_terminal_controls("a\tb\nc\r\nd\u{7}e\u{8}f\u{7f}g"),
            "a\tb\nc\ndefg"
        );
    }

    #[test]
    fn keeps_utf8_text_and_drops_a_trailing_lone_escape() {
        assert_eq!(strip_terminal_controls("\u{1b}[1mhéllo ✓\u{1b}"), "héllo ✓");
    }

    #[test]
    fn borrows_clean_text_and_only_strips_when_needed() {
        let clean = "plain output\n\tindented";
        assert!(matches!(strip_terminal_controls(clean), Cow::Borrowed(_)));

        let mut text = String::from("\u{1b}[31mred\u{1b}[0m");
        strip_terminal_controls_in_place(&mut text);
        assert_eq!(text, "red");

        let mut untouched = String::from("nothing to do");
        strip_terminal_controls_in_place(&mut untouched);
        assert_eq!(untouched, "nothing to do");
    }
}
