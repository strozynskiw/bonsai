//! Lightweight, dependency-free syntax highlighting for fenced code blocks.
//!
//! Token-level only — keywords, strings, comments, numbers, types, and call
//! sites — which covers the common languages an agent pastes (Rust, JS/TS,
//! Python, Go, shell) without pulling in a grammar library.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use crate::tui::theme;

/// Shared keyword set across the languages bonsai most often renders.
/// Token-level highlighting tolerates cross-language false positives.
const KEYWORDS: &[&str] = &[
    // Rust
    "as",
    "async",
    "await",
    "break",
    "const",
    "continue",
    "crate",
    "dyn",
    "else",
    "enum",
    "fn",
    "for",
    "if",
    "impl",
    "in",
    "let",
    "loop",
    "match",
    "mod",
    "move",
    "mut",
    "pub",
    "ref",
    "return",
    "self",
    "Self",
    "static",
    "struct",
    "super",
    "trait",
    "type",
    "unsafe",
    "use",
    "where",
    "while",
    "true",
    "false",
    // JS / TS
    "function",
    "var",
    "class",
    "extends",
    "import",
    "export",
    "from",
    "new",
    "this",
    "typeof",
    "interface",
    "implements",
    "void",
    "null",
    "undefined",
    "default",
    "switch",
    "case",
    // Python
    "def",
    "elif",
    "try",
    "except",
    "finally",
    "with",
    "lambda",
    "not",
    "and",
    "or",
    "pass",
    "raise",
    "global",
    "yield",
    "None",
    "True",
    "False",
    "is",
    // Go
    "func",
    "package",
    "range",
    "go",
    "defer",
    "chan",
    "select",
    "map",
    // Shell
    "echo",
    "cd",
    "export",
    "then",
    "fi",
    "do",
    "done",
    "esac",
];

fn comment_markers(lang: &str) -> &'static [&'static str] {
    match lang {
        "rust" | "rs" | "c" | "h" | "cpp" | "hpp" | "cc" | "js" | "jsx" | "ts" | "tsx" | "java"
        | "kt" | "kotlin" | "swift" | "go" | "scala" | "cs" | "zig" | "dart" => &["//"],
        "py" | "python" | "sh" | "bash" | "shell" | "zsh" | "fish" | "yaml" | "yml" | "toml"
        | "rb" | "ruby" | "make" | "makefile" | "dockerfile" | "conf" | "ini" | "cfg" => &["#"],
        "sql" | "lua" | "hs" | "haskell" | "elm" => &["--"],
        _ => &["//", "#"],
    }
}

/// Highlights one line of code into styled spans.
pub fn highlight_line(line: &str, lang: &str) -> Vec<Span<'static>> {
    let lang = lang.trim().to_ascii_lowercase();
    let markers = comment_markers(&lang);
    let chars: Vec<char> = line.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let mut i = 0usize;

    while i < chars.len() {
        if markers.iter().any(|marker| starts_at(&chars, i, marker)) {
            flush(&mut plain, &mut spans);
            let rest: String = chars[i..].iter().collect();
            spans.push(Span::styled(
                rest,
                Style::default()
                    .fg(theme::palette().syntax_comment)
                    .add_modifier(Modifier::ITALIC),
            ));
            return spans;
        }

        let ch = chars[i];
        if ch == '"' || ch == '\'' || ch == '`' {
            flush(&mut plain, &mut spans);
            let (text, next) = scan_string(&chars, i, ch);
            spans.push(Span::styled(
                text,
                Style::default().fg(theme::palette().syntax_string),
            ));
            i = next;
            continue;
        }
        if ch.is_ascii_digit() {
            flush(&mut plain, &mut spans);
            let (text, next) = scan_while(&chars, i, |c| {
                c.is_ascii_alphanumeric() || c == '_' || c == '.'
            });
            spans.push(Span::styled(
                text,
                Style::default().fg(theme::palette().syntax_number),
            ));
            i = next;
            continue;
        }
        if ch.is_alphabetic() || ch == '_' {
            flush(&mut plain, &mut spans);
            let (word, next) = scan_while(&chars, i, |c| c.is_alphanumeric() || c == '_');
            let style = if KEYWORDS.contains(&word.as_str()) {
                Style::default()
                    .fg(theme::palette().syntax_keyword)
                    .add_modifier(Modifier::BOLD)
            } else if word.chars().next().is_some_and(char::is_uppercase) {
                Style::default().fg(theme::palette().syntax_type)
            } else if chars.get(next) == Some(&'(') {
                Style::default().fg(theme::palette().syntax_function)
            } else {
                Style::default().fg(theme::palette().text)
            };
            spans.push(Span::styled(word, style));
            i = next;
            continue;
        }

        plain.push(ch);
        i += 1;
    }

    flush(&mut plain, &mut spans);
    if spans.is_empty() {
        spans.push(Span::styled(
            String::new(),
            Style::default().fg(theme::palette().text),
        ));
    }
    spans
}

fn flush(plain: &mut String, spans: &mut Vec<Span<'static>>) {
    if !plain.is_empty() {
        spans.push(Span::styled(
            std::mem::take(plain),
            Style::default().fg(theme::palette().text),
        ));
    }
}

fn starts_at(chars: &[char], at: usize, marker: &str) -> bool {
    marker
        .chars()
        .enumerate()
        .all(|(offset, expected)| chars.get(at + offset) == Some(&expected))
}

fn scan_string(chars: &[char], start: usize, quote: char) -> (String, usize) {
    let mut text = String::new();
    text.push(chars[start]);
    let mut i = start + 1;
    while i < chars.len() {
        let ch = chars[i];
        text.push(ch);
        i += 1;
        if ch == '\\' && i < chars.len() {
            text.push(chars[i]);
            i += 1;
            continue;
        }
        if ch == quote {
            break;
        }
    }
    (text, i)
}

fn scan_while(chars: &[char], start: usize, keep: impl Fn(char) -> bool) -> (String, usize) {
    let mut i = start;
    let mut text = String::new();
    while i < chars.len() && keep(chars[i]) {
        text.push(chars[i]);
        i += 1;
    }
    (text, i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(spans: &[Span<'static>]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    fn style_of<'a>(spans: &'a [Span<'static>], text: &str) -> &'a Style {
        &spans
            .iter()
            .find(|span| span.content.as_ref() == text)
            .unwrap_or_else(|| panic!("span '{text}' not found in {spans:?}"))
            .style
    }

    #[test]
    fn highlights_rust_keywords_strings_and_types() {
        let spans = highlight_line(r#"pub fn greet(name: &str) -> String { "hi" }"#, "rust");

        assert_eq!(
            joined(&spans),
            r#"pub fn greet(name: &str) -> String { "hi" }"#,
            "highlighting must not alter the text"
        );
        assert_eq!(
            style_of(&spans, "fn").fg,
            Some(theme::palette().syntax_keyword)
        );
        assert_eq!(
            style_of(&spans, "String").fg,
            Some(theme::palette().syntax_type)
        );
        assert_eq!(
            style_of(&spans, "\"hi\"").fg,
            Some(theme::palette().syntax_string)
        );
        assert_eq!(
            style_of(&spans, "greet").fg,
            Some(theme::palette().syntax_function)
        );
    }

    #[test]
    fn line_comments_are_dimmed() {
        let spans = highlight_line("let x = 1; // done", "rust");
        assert_eq!(
            style_of(&spans, "// done").fg,
            Some(theme::palette().syntax_comment)
        );
    }

    #[test]
    fn bash_double_dash_flags_are_not_comments() {
        let spans = highlight_line("cargo test --all", "bash");
        assert!(
            !spans.iter().any(|span| span.content.contains("--all")
                && span.style.fg == Some(theme::palette().syntax_comment)),
            "--all is a flag, not a comment: {spans:?}"
        );
    }

    #[test]
    fn python_hash_comment_is_dimmed() {
        let spans = highlight_line("x = 1  # counter", "python");
        assert_eq!(
            style_of(&spans, "# counter").fg,
            Some(theme::palette().syntax_comment)
        );
    }
}
