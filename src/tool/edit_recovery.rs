//! Failure-recovery helpers for the `edit` tool.
//!
//! When an `edit` call cannot match its `old_string`, a bare "not found" error
//! forces the model to re-read and guess, burning turns. These helpers turn that
//! dead end into actionable context: the closest-matching region with line
//! numbers, a confidence score, and — when it applies — a note that only
//! whitespace differs. For ambiguous matches they name the lines of each
//! occurrence. They never mutate; they only describe.

use std::fmt::Write as _;

use strsim::normalized_levenshtein;

/// Block similarity (`0.0..=1.0`) below which a window is not surfaced, so we
/// never fabricate a misleading suggestion.
const SIMILARITY_FLOOR: f64 = 0.5;

/// Per-line similarity an anchor line must clear to seed a candidate window.
const ANCHOR_FLOOR: f64 = 0.4;

/// Largest content we scan; bigger files skip the scan and return `None`.
const MAX_SCAN_BYTES: usize = 2 * 1024 * 1024;
const MAX_SCAN_LINES: usize = 50_000;

/// Largest `old_string` we build suggestions for. Block scoring is
/// `O(old.len() * window.len())`, so an oversized needle skips the scan.
const MAX_OLD_STRING_BYTES: usize = 8 * 1024;

/// Longest anchor line we Levenshtein against every file line. The per-line
/// scan is `O(anchor.len() * content.len())`, so this caps the multiplier and
/// keeps the not-found path bounded regardless of file size.
const MAX_ANCHOR_BYTES: usize = 512;

/// Most anchor hits expanded into candidate windows.
const MAX_ANCHOR_HITS: usize = 8;

/// Most candidate windows surfaced in one hint.
const MAX_CANDIDATES: usize = 2;

/// Most lines of a candidate window rendered before eliding the remainder.
const MAX_RENDERED_LINES: usize = 12;

/// Most occurrence line numbers listed for an ambiguous match.
const MAX_OCCURRENCES_LISTED: usize = 10;

struct Candidate {
    /// 0-based index of the window's first line in the file.
    start_line: usize,
    /// Block similarity in `0.0..=1.0`.
    score: f64,
    /// Set when the window matches `old_string` apart from whitespace.
    whitespace_note: Option<String>,
}

/// Build a hint pointing at the file region(s) most similar to `old_string`, or
/// `None` when nothing clears [`SIMILARITY_FLOOR`] (or the file is too large).
pub(crate) fn nearest_match_hint(content: &str, old_string: &str) -> Option<String> {
    if content.len() > MAX_SCAN_BYTES {
        return None;
    }
    let file_lines: Vec<&str> = content.lines().collect();
    if file_lines.is_empty() || file_lines.len() > MAX_SCAN_LINES {
        return None;
    }
    let old_lines: Vec<&str> = old_string.lines().collect();
    if old_lines.is_empty() || old_string.len() > MAX_OLD_STRING_BYTES {
        return None;
    }
    let window_len = old_lines.len();
    let total = file_lines.len();

    let candidates = best_windows(&file_lines, &old_lines);
    if candidates.is_empty() {
        return None;
    }

    let plural = if window_len > 1 { "s" } else { "" };
    let mut out = String::new();
    for (i, cand) in candidates.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let first = cand.start_line + 1;
        let last = (cand.start_line + window_len).min(total);
        let _ = write!(
            out,
            "Closest match at line{plural} {} ({}% similar",
            fmt_range(first, last),
            (cand.score * 100.0).round() as u32
        );
        if let Some(note) = &cand.whitespace_note {
            let _ = write!(out, "; {note}");
        }
        out.push_str("):\n");
        out.push_str(&render_window(&file_lines, cand.start_line, window_len));
    }

    // Concrete next step, naming a slightly widened region around the best
    // candidate so the model can re-read and copy the exact bytes.
    let top = &candidates[0];
    let read_first = top.start_line.saturating_sub(1) + 1;
    let read_last = (top.start_line + window_len + 1).min(total);
    let _ = write!(
        out,
        "\nRe-read line{plural} {} and copy the exact text, or retry with the block above.",
        fmt_range(read_first, read_last)
    );
    Some(out)
}

/// Most whitespace-insensitive matches collected before giving up on
/// uniqueness (only the unique case gets the verbatim-text treatment).
const MAX_WS_INSENSITIVE_SPANS: usize = 4;

/// Byte spans of `content` that match `old_string` when *all* whitespace runs
/// (including newlines and indentation) are treated as equivalent.
///
/// This targets a failure mode observed live: a model reads the
/// correct multi-line region, then submits `old_string` with every newline
/// collapsed to a single space — typically because it inlined the code into a
/// one-line `"replace: OLD -> NEW"` shorthand string. Exact matching can never
/// succeed on that input, but token-sequence matching finds the intended
/// region deterministically, so the error can hand back the verbatim on-disk
/// text instead of a fuzzy guess.
pub(crate) fn whitespace_insensitive_spans(content: &str, old_string: &str) -> Vec<(usize, usize)> {
    if content.len() > MAX_SCAN_BYTES
        || old_string.len() > MAX_OLD_STRING_BYTES
        || old_string.trim().is_empty()
    {
        return Vec::new();
    }
    let needle: Vec<&str> = old_string.split_whitespace().collect();
    // A single token would match its every exact occurrence; the plain
    // exact-match path already handles that shape better.
    if needle.len() < 2 {
        return Vec::new();
    }
    let tokens: Vec<(usize, &str)> = content
        .split_whitespace()
        .map(|token| {
            // Safety: split_whitespace yields subslices of `content`.
            let start = token.as_ptr() as usize - content.as_ptr() as usize;
            (start, token)
        })
        .collect();
    let mut spans = Vec::new();
    if tokens.len() < needle.len() {
        return spans;
    }
    for i in 0..=(tokens.len() - needle.len()) {
        if tokens[i].1 != needle[0] {
            continue;
        }
        if (1..needle.len()).all(|j| tokens[i + j].1 == needle[j]) {
            let start = tokens[i].0;
            let last = tokens[i + needle.len() - 1];
            spans.push((start, last.0 + last.1.len()));
            if spans.len() >= MAX_WS_INSENSITIVE_SPANS {
                break;
            }
        }
    }
    spans
}

/// Auto-apply an `old_string` that matches a UNIQUE block of file lines after
/// trimming each line — i.e. only leading/trailing whitespace (indentation)
/// drifted, with the SAME number of lines so structure is preserved. Returns
/// the new file content with that block replaced by `new_string`, re-indented so
/// each replacement line is anchored to the file block's indentation rather than
/// whatever the model typed. `None` when there is no such block, more than one
/// (ambiguous — never auto-apply a guess), or inputs are oversized.
///
/// Line count MUST match, so this deliberately never fires for the
/// newline-collapsed one-line shorthand that [`whitespace_insensitive_spans`]
/// targets: collapsing multi-line code back into one line would corrupt
/// structure, so that case keeps its verbatim-text hint (the caller tries this
/// first, then falls back). Byte-span replacement preserves every byte outside
/// the matched block, including the file's line terminators.
pub(crate) fn line_trimmed_reindented_apply(
    content: &str,
    old_string: &str,
    new_string: &str,
) -> Option<String> {
    if content.len() > MAX_SCAN_BYTES || old_string.len() > MAX_OLD_STRING_BYTES {
        return None;
    }
    // Ignore blank lines the model may have padded at the block edges.
    let old_lines: Vec<&str> = old_string.lines().collect();
    let first = old_lines.iter().position(|l| !l.trim().is_empty())?;
    let last = old_lines.iter().rposition(|l| !l.trim().is_empty())?;
    let old_core = &old_lines[first..=last];
    // A single line is left to the exact path + nearest-match hint: a lone
    // trimmed line matches unintended places too easily to auto-apply safely.
    if old_core.len() < 2 {
        return None;
    }

    let ranges = line_byte_ranges(content);
    let file_lines: Vec<&str> = content.lines().collect();
    let win = old_core.len();
    if file_lines.len() < win {
        return None;
    }
    let mut hit: Option<usize> = None;
    for start in 0..=(file_lines.len() - win) {
        if (0..win).all(|j| file_lines[start + j].trim() == old_core[j].trim()) {
            if hit.is_some() {
                return None; // ambiguous — do not guess
            }
            hit = Some(start);
        }
    }
    let start_line = hit?;

    let (span_start, _) = ranges[start_line];
    let (_, span_end) = ranges[start_line + win - 1];
    // Re-anchor indentation: shift `new_string` from the model's base indent to
    // the file block's base indent, so a replacement typed at the wrong depth
    // still lands correctly.
    let file_base = leading_ws(file_lines[start_line]);
    let old_base = leading_ws(old_core[0]);
    let reindented = reindent(new_string, old_base, file_base);
    Some(format!(
        "{}{}{}",
        &content[..span_start],
        reindented,
        &content[span_end..]
    ))
}

/// Byte `(start, end)` of each line as `str::lines` yields it — `end` excludes
/// the trailing `\n` (and a preceding `\r`), and a trailing newline produces no
/// empty final entry, so indices align 1:1 with `content.lines()`.
fn line_byte_ranges(content: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let bytes = content.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            let mut end = i;
            if end > start && bytes[end - 1] == b'\r' {
                end -= 1;
            }
            ranges.push((start, end));
            start = i + 1;
        }
    }
    if start < content.len() {
        ranges.push((start, content.len()));
    }
    ranges
}

/// Shift each non-blank line of `new_string` from `old_base` leading whitespace
/// to `file_base`, preserving any relative (deeper) indentation. Lines that
/// don't start with `old_base` are left verbatim — re-anchoring only rewrites
/// the uniform prefix the model used, never the model's internal structure.
fn reindent(new_string: &str, old_base: &str, file_base: &str) -> String {
    if old_base == file_base {
        return new_string.to_string();
    }
    new_string
        .split('\n')
        .map(|line| match line.strip_prefix(old_base) {
            _ if line.trim().is_empty() => line.to_string(),
            Some(rest) => format!("{file_base}{rest}"),
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 1-based line number of a byte offset.
pub(crate) fn line_of_byte(content: &str, byte: usize) -> usize {
    content[..byte.min(content.len())]
        .bytes()
        .filter(|&b| b == b'\n')
        .count()
        + 1
}

/// 1-based start lines of `needle`'s occurrences, de-duplicated in order and
/// capped at `limit`; the bool is true when more distinct lines exist beyond it.
///
/// One linear pass over `content`: `match_indices` yields byte offsets in
/// increasing order, so line numbers are non-decreasing and duplicates are
/// adjacent (an O(1) last-element check de-dupes them), and newlines are counted
/// incrementally between consecutive matches rather than re-scanned from the
/// start each time. This keeps the ambiguous-match error path O(content) even
/// when `needle` is a single common character occurring very many times.
pub(crate) fn occurrence_lines(content: &str, needle: &str, limit: usize) -> (Vec<usize>, bool) {
    let mut lines: Vec<usize> = Vec::new();
    if needle.is_empty() || limit == 0 {
        return (lines, false);
    }
    let mut scanned = 0usize;
    let mut line = 1usize;
    for (idx, _) in content.match_indices(needle) {
        line += content[scanned..idx]
            .bytes()
            .filter(|&b| b == b'\n')
            .count();
        scanned = idx;
        if lines.last() != Some(&line) {
            if lines.len() == limit {
                return (lines, true);
            }
            lines.push(line);
        }
    }
    (lines, false)
}

/// `a, b, c` (capped, with a trailing `…` when more distinct lines exist) for the
/// ambiguous-match error. One linear pass via [`occurrence_lines`].
pub(crate) fn occurrence_lines_label(content: &str, needle: &str) -> String {
    let (lines, more) = occurrence_lines(content, needle, MAX_OCCURRENCES_LISTED);
    let mut out = lines
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    if more {
        out.push_str(", …");
    }
    out
}

/// Find the best-aligned candidate window(s) for `old_lines` within the file.
fn best_windows(file_lines: &[&str], old_lines: &[&str]) -> Vec<Candidate> {
    let window_len = old_lines.len();
    let anchor_idx = old_lines
        .iter()
        .position(|l| !l.trim().is_empty())
        .unwrap_or(0);
    let anchor = old_lines[anchor_idx].trim();
    if anchor.is_empty() || anchor.len() > MAX_ANCHOR_BYTES {
        return Vec::new();
    }
    let old_joined = old_lines.join("\n");

    // Rank file lines by similarity to the anchor, compared trimmed so that
    // indentation differences do not hide the region. The length guard bounds
    // Levenshtein cost against a pathologically long single line.
    let mut anchor_hits: Vec<(usize, f64)> = file_lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let candidate = line.trim();
            let score = if candidate.len() > anchor.len().saturating_mul(4) + 80 {
                0.0
            } else {
                normalized_levenshtein(anchor, candidate)
            };
            (i, score)
        })
        .filter(|(_, s)| *s >= ANCHOR_FLOOR)
        .collect();
    anchor_hits.sort_by(|a, b| b.1.total_cmp(&a.1));
    anchor_hits.truncate(MAX_ANCHOR_HITS);

    // Expand each anchor hit into an aligned window and score the whole block.
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut seen_starts: Vec<usize> = Vec::new();
    for (file_idx, _) in anchor_hits {
        let start = file_idx.saturating_sub(anchor_idx);
        if start + window_len > file_lines.len() || seen_starts.contains(&start) {
            continue;
        }
        seen_starts.push(start);

        let window_lines = &file_lines[start..start + window_len];
        let window_joined = window_lines.join("\n");
        // A window far longer than the needle cannot be a near match, and block
        // scoring is O(old.len() * window.len()); skip it on both counts.
        if window_joined.len() > old_joined.len().saturating_mul(4) + 80 {
            continue;
        }
        let score = normalized_levenshtein(&old_joined, &window_joined);
        if score < SIMILARITY_FLOOR {
            continue;
        }
        candidates.push(Candidate {
            start_line: start,
            score,
            whitespace_note: whitespace_difference_note(window_lines, old_lines),
        });
    }

    // Best first, then any further non-overlapping windows up to the cap.
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut chosen: Vec<Candidate> = Vec::new();
    for cand in candidates {
        let overlaps = chosen.iter().any(|c| {
            c.start_line < cand.start_line + window_len
                && cand.start_line < c.start_line + window_len
        });
        if !overlaps {
            chosen.push(cand);
        }
        if chosen.len() >= MAX_CANDIDATES {
            break;
        }
    }
    chosen
}

/// When `window_lines` matches `old_lines` apart from whitespace, describe the
/// difference; otherwise `None` (a real content difference).
fn whitespace_difference_note(window_lines: &[&str], old_lines: &[&str]) -> Option<String> {
    if window_lines.len() != old_lines.len() {
        return None;
    }
    let same_trimmed = window_lines
        .iter()
        .zip(old_lines)
        .all(|(w, o)| w.trim() == o.trim());
    if !same_trimmed {
        return None;
    }
    let tab_mismatch = old_lines
        .iter()
        .zip(window_lines)
        .any(|(o, w)| leading_ws(o).contains('\t') != leading_ws(w).contains('\t'));
    let note = if tab_mismatch {
        "differs only in whitespace (tabs vs spaces)"
    } else {
        "differs only in whitespace/indentation"
    };
    Some(note.to_string())
}

/// Render a window as `  {line}: {text}`, eliding past [`MAX_RENDERED_LINES`].
fn render_window(file_lines: &[&str], start: usize, window_len: usize) -> String {
    let end = (start + window_len).min(file_lines.len());
    let shown_end = end.min(start + MAX_RENDERED_LINES);
    let mut out = String::new();
    for (offset, line) in file_lines[start..shown_end].iter().enumerate() {
        let _ = writeln!(out, "  {}: {}", start + offset + 1, line);
    }
    let elided = end - shown_end;
    if elided > 0 {
        let _ = writeln!(
            out,
            "  … ({elided} more line{})",
            if elided == 1 { "" } else { "s" }
        );
    }
    out
}

/// Leading whitespace of a line.
fn leading_ws(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// `a` for a single line, `a-b` for a range (1-based, inclusive).
fn fmt_range(first: usize, last: usize) -> String {
    if first >= last {
        format!("{first}")
    } else {
        format!("{first}-{last}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occurrence_lines_reports_distinct_match_lines() {
        let content = "foo\nbar\nfoo baz\n";
        assert_eq!(occurrence_lines(content, "foo", 10), (vec![1, 3], false));
    }

    #[test]
    fn occurrence_lines_dedupes_repeats_on_one_line() {
        // Three matches on a single line collapse to one entry.
        assert_eq!(occurrence_lines("a a a", "a", 10), (vec![1], false));
    }

    #[test]
    fn occurrence_lines_caps_and_flags_more() {
        // One match per line over 15 lines, capped at 10.
        let content: String = (0..15).map(|_| "x\n").collect();
        let (lines, more) = occurrence_lines(&content, "x", 10);
        assert_eq!(lines, (1..=10).collect::<Vec<_>>());
        assert!(more);
    }

    #[test]
    fn occurrence_lines_label_formats_and_truncates() {
        assert_eq!(occurrence_lines_label("foo\nbar\nfoo\n", "foo"), "1, 3");
        let content: String = (0..15).map(|_| "x\n").collect();
        let label = occurrence_lines_label(&content, "x");
        assert!(label.starts_with("1, 2, 3"), "{label}");
        assert!(label.ends_with(", …"), "{label}");
    }

    #[test]
    fn hint_flags_whitespace_only_difference() {
        let content = "fn main() {\n        let x = 1;\n}\n";
        // Same text, four-space indent instead of eight.
        let hint = nearest_match_hint(content, "    let x = 1;").expect("a hint");
        assert!(hint.contains("Closest match at line 2"), "{hint}");
        assert!(hint.contains("differs only in whitespace"), "{hint}");
        assert!(hint.contains("Re-read line"), "{hint}");
    }

    #[test]
    fn hint_locates_a_single_changed_line_in_a_block() {
        let content = "fn main() {\n    let x = 1;\n    println!(\"{}\", x);\n}\n";
        let old = "fn main() {\n    let y = 2;\n    println!(\"{}\", x);";
        let hint = nearest_match_hint(content, old).expect("a hint");
        assert!(hint.contains("Closest match at lines 1-3"), "{hint}");
        assert!(hint.contains("% similar"), "{hint}");
        assert!(hint.contains("Re-read lines"), "{hint}");
    }

    #[test]
    fn hint_is_none_for_unrelated_text() {
        let content = "Hello world\n";
        assert!(nearest_match_hint(content, "nonexistent").is_none());
    }

    #[test]
    fn reindented_apply_fixes_indentation_drift() {
        // File is indented 8 spaces; model's old/new use 4. The unique match is
        // applied and re-anchored to the file's 8-space indent.
        let content = "fn main() {\n        let x = 1;\n        let y = 2;\n}\n";
        let old = "    let x = 1;\n    let y = 2;";
        let new = "    let x = 10;\n    let y = 20;";
        let out = line_trimmed_reindented_apply(content, old, new).expect("unique drift match");
        assert_eq!(
            out, "fn main() {\n        let x = 10;\n        let y = 20;\n}\n",
            "replacement must land at the file's 8-space indent, surrounding bytes intact",
        );
    }

    #[test]
    fn reindented_apply_is_none_when_ambiguous() {
        // Two structurally-identical blocks → refuse to guess.
        let content = "  a();\n  b();\n\n  a();\n  b();\n";
        assert!(line_trimmed_reindented_apply(content, "a();\nb();", "c();\nd();").is_none());
    }

    #[test]
    fn reindented_apply_skips_single_line_and_collapsed_shorthand() {
        // Single trimmed line: left to the exact path / hint.
        assert!(
            line_trimmed_reindented_apply("    let x = 1;\n", "let x = 1;", "let x = 2;").is_none()
        );
        // Newline-collapsed shorthand (one line vs two): line count differs, so
        // this never fires — the verbatim-hint path handles it instead.
        let content = "    let x = 1;\n    let y = 2;\n";
        assert!(line_trimmed_reindented_apply(content, "let x = 1; let y = 2;", "z();").is_none());
    }

    #[test]
    fn reindented_apply_preserves_relative_indentation() {
        let content = "class C:\n    def f(self):\n        return 1\n";
        // Model drops the outer 4-space indent on both lines but keeps the inner
        // step; re-anchoring restores the outer indent and preserves the step.
        let old = "def f(self):\n    return 1";
        let new = "def f(self):\n    return 2";
        let out = line_trimmed_reindented_apply(content, old, new).expect("unique match");
        assert_eq!(out, "class C:\n    def f(self):\n        return 2\n");
    }

    #[test]
    fn hint_skips_oversized_files() {
        let mut content = String::with_capacity(MAX_SCAN_LINES + 16);
        for _ in 0..=MAX_SCAN_LINES {
            content.push_str("x\n");
        }
        assert!(nearest_match_hint(&content, "let x = 1;").is_none());
    }
}
