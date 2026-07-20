use serde::{Deserialize, Serialize};

/// Maximum number of rendered diff lines kept for preview.
pub const DIFF_MAX_LINES: usize = 2_000;

/// Maximum UTF-8 payload exposed to one file hook. The structured diff is
/// already line-bounded for the UI; this second bound prevents one enormous
/// source line from turning a hook invocation into an unbounded request.
pub const HOOK_DIFF_MAX_BYTES: usize = 128 * 1024;

const DIFF_CONTEXT_LINES: usize = 2;
const DIFF_LCS_MAX_CELLS: usize = 4_000_000;
const HOOK_DIFF_MARKER_RESERVE: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffStatus {
    Created,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub content: String,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffHunk {
    pub old_start: u32,
    pub new_start: u32,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    pub status: DiffStatus,
    pub hunks: Vec<DiffHunk>,
    pub truncated: bool,
    pub old_size: Option<u64>,
    pub new_size: u64,
    #[serde(default)]
    pub added_lines: usize,
    #[serde(default)]
    pub removed_lines: usize,
    /// Additional per-file diffs committed by the same atomic tool call. The
    /// first `FileDiff` remains the compatibility anchor for existing storage
    /// and UI paths; this collection makes a multi-file mutation structured
    /// without flattening it into a text-only success message.
    #[serde(default)]
    pub additional_files: Box<[FileDiff]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiffStats {
    pub added: usize,
    pub removed: usize,
}

impl FileDiff {
    pub(crate) fn own_stats(&self) -> DiffStats {
        DiffStats {
            added: self.added_lines,
            removed: self.removed_lines,
        }
    }

    pub(crate) fn stats(&self) -> DiffStats {
        self.additional_files
            .iter()
            .fold(self.own_stats(), |mut total, diff| {
                let stats = diff.stats();
                total.added = total.added.saturating_add(stats.added);
                total.removed = total.removed.saturating_add(stats.removed);
                total
            })
    }

    pub(crate) fn files(&self) -> impl Iterator<Item = &FileDiff> {
        std::iter::once(self).chain(self.additional_files.iter())
    }

    pub(crate) fn file_count(&self) -> usize {
        1usize.saturating_add(self.additional_files.len())
    }

    pub(crate) fn with_additional_files(mut self, additional_files: Vec<FileDiff>) -> Self {
        self.additional_files = additional_files.into_boxed_slice();
        self
    }

    pub(crate) fn from_files(mut files: Vec<FileDiff>) -> Option<Self> {
        if files.is_empty() {
            return None;
        }
        let additional = files.split_off(1);
        files
            .pop()
            .map(|primary| primary.with_additional_files(additional))
    }

    pub(crate) fn redact_secrets(&mut self) {
        crate::redact::redact_in_place(&mut self.path);
        for line in self.hunks.iter_mut().flat_map(|hunk| hunk.lines.iter_mut()) {
            crate::redact::redact_in_place(&mut line.content);
        }
        for diff in &mut self.additional_files {
            diff.redact_secrets();
        }
    }

    /// Render deterministic, redacted evidence for a file lifecycle hook.
    ///
    /// File hooks fire once per path, so only this diff's own hunks are
    /// rendered. The caller uses the same `FileDiff` instance for policy and
    /// `ToolOutput`; the textual view is capped independently to handle very
    /// long individual lines. A visible marker reports either structural or
    /// byte truncation together with the full change totals.
    pub(crate) fn render_for_hook(&self) -> String {
        let mut redacted = self.clone();
        redacted.additional_files = Box::default();
        redacted.redact_secrets();

        let mut renderer = HookDiffRenderer::new();
        let status = match redacted.status {
            DiffStatus::Created => "created",
            DiffStatus::Modified => "modified",
            DiffStatus::Deleted => "deleted",
        };
        renderer.push(&format!("diff --bonsai {status} {}\n", redacted.path));
        match redacted.status {
            DiffStatus::Created => {
                renderer.push("--- /dev/null\n");
                renderer.push(&format!("+++ {}\n", redacted.path));
            }
            DiffStatus::Modified => {
                renderer.push(&format!("--- {}\n", redacted.path));
                renderer.push(&format!("+++ {}\n", redacted.path));
            }
            DiffStatus::Deleted => {
                renderer.push(&format!("--- {}\n", redacted.path));
                renderer.push("+++ /dev/null\n");
            }
        }
        for hunk in &redacted.hunks {
            renderer.push(&format!("@@ -{} +{} @@\n", hunk.old_start, hunk.new_start));
            for line in &hunk.lines {
                let prefix = match line.kind {
                    DiffLineKind::Context => ' ',
                    DiffLineKind::Added => '+',
                    DiffLineKind::Removed => '-',
                };
                renderer.push_char(prefix);
                renderer.push(&line.content);
                renderer.push("\n");
            }
        }

        let structurally_truncated = redacted.truncated;
        renderer.finish(structurally_truncated, redacted.own_stats())
    }
}

struct HookDiffRenderer {
    output: String,
    byte_truncated: bool,
}

impl HookDiffRenderer {
    fn new() -> Self {
        Self {
            output: String::with_capacity(HOOK_DIFF_MAX_BYTES.min(8 * 1024)),
            byte_truncated: false,
        }
    }

    fn push_char(&mut self, value: char) {
        let mut encoded = [0_u8; 4];
        self.push(value.encode_utf8(&mut encoded));
    }

    fn push(&mut self, value: &str) {
        if self.byte_truncated {
            return;
        }
        let content_limit = HOOK_DIFF_MAX_BYTES.saturating_sub(HOOK_DIFF_MARKER_RESERVE);
        let remaining = content_limit.saturating_sub(self.output.len());
        if value.len() <= remaining {
            self.output.push_str(value);
            return;
        }
        let mut boundary = remaining.min(value.len());
        while boundary > 0 && !value.is_char_boundary(boundary) {
            boundary -= 1;
        }
        self.output.push_str(&value[..boundary]);
        self.byte_truncated = true;
    }

    fn finish(mut self, structurally_truncated: bool, stats: DiffStats) -> String {
        if structurally_truncated || self.byte_truncated {
            if !self.output.ends_with('\n') {
                self.output.push('\n');
            }
            let reason = match (structurally_truncated, self.byte_truncated) {
                (true, true) => "line and byte limits",
                (true, false) => "line limit",
                (false, true) => "byte limit",
                (false, false) => "configured limits",
            };
            self.output.push_str(&format!(
                "[diff truncated by {reason}; limit: {DIFF_MAX_LINES} lines/{HOOK_DIFF_MAX_BYTES} bytes; full totals: +{} -{}]\n",
                stats.added, stats.removed
            ));
        }
        self.output
    }
}

pub fn build_file_diff(path: String, old: Option<&str>, new: &str) -> FileDiff {
    let old_text = old.unwrap_or("");
    let status = if old.is_none() {
        DiffStatus::Created
    } else {
        DiffStatus::Modified
    };
    let old_lines: Vec<&str> = split_lines(old_text);
    let new_lines: Vec<&str> = split_lines(new);
    let hunks = compute_hunks(&old_lines, &new_lines);
    let stats = diff_stats(&hunks);
    let total_lines: usize = hunks.iter().map(|h| h.lines.len()).sum();
    let (hunks, truncated) = if total_lines > DIFF_MAX_LINES {
        let mut out = Vec::new();
        let mut remaining = DIFF_MAX_LINES;
        for h in hunks {
            if remaining == 0 {
                break;
            }
            if h.lines.len() <= remaining {
                remaining -= h.lines.len();
                out.push(h);
            } else {
                let mut lines = h.lines;
                lines.truncate(remaining);
                remaining = 0;
                out.push(DiffHunk {
                    old_start: h.old_start,
                    new_start: h.new_start,
                    lines,
                });
            }
        }
        (out, true)
    } else {
        (hunks, false)
    };
    let new_size = new.len() as u64;
    let old_size = old.map(|s| s.len() as u64);
    FileDiff {
        path,
        status,
        hunks,
        truncated,
        old_size,
        new_size,
        added_lines: stats.added,
        removed_lines: stats.removed,
        additional_files: Box::default(),
    }
}

pub fn build_deleted_file_diff(path: String, old: &str) -> FileDiff {
    let mut diff = build_file_diff(path, Some(old), "");
    diff.status = DiffStatus::Deleted;
    diff
}

fn diff_stats(hunks: &[DiffHunk]) -> DiffStats {
    let mut stats = DiffStats {
        added: 0,
        removed: 0,
    };
    for line in hunks.iter().flat_map(|hunk| hunk.lines.iter()) {
        match line.kind {
            DiffLineKind::Added => stats.added += 1,
            DiffLineKind::Removed => stats.removed += 1,
            DiffLineKind::Context => {}
        }
    }
    stats
}

fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    text.lines().collect()
}

fn compute_hunks(old_lines: &[&str], new_lines: &[&str]) -> Vec<DiffHunk> {
    if old_lines == new_lines {
        return Vec::new();
    }
    if lcs_cell_count_exceeds_budget(old_lines.len(), new_lines.len()) {
        return compute_coarse_hunks(old_lines, new_lines);
    }

    let table = lcs_table(old_lines, new_lines);
    let mut old_idx = old_lines.len();
    let mut new_idx = new_lines.len();
    let mut pending: Vec<DiffLine> = Vec::new();
    while old_idx > 0 || new_idx > 0 {
        if old_idx > 0 && new_idx > 0 && old_lines[old_idx - 1] == new_lines[new_idx - 1] {
            pending.push(DiffLine {
                kind: DiffLineKind::Context,
                content: old_lines[old_idx - 1].to_string(),
                old_line: Some(old_idx as u32),
                new_line: Some(new_idx as u32),
            });
            old_idx -= 1;
            new_idx -= 1;
        } else if new_idx > 0
            && (old_idx == 0 || table[old_idx][new_idx - 1] >= table[old_idx - 1][new_idx])
        {
            pending.push(DiffLine {
                kind: DiffLineKind::Added,
                content: new_lines[new_idx - 1].to_string(),
                old_line: None,
                new_line: Some(new_idx as u32),
            });
            new_idx -= 1;
        } else {
            pending.push(DiffLine {
                kind: DiffLineKind::Removed,
                content: old_lines[old_idx - 1].to_string(),
                old_line: Some(old_idx as u32),
                new_line: None,
            });
            old_idx -= 1;
        }
    }
    pending.reverse();
    group_into_hunks(pending)
}

fn group_into_hunks(lines: Vec<DiffLine>) -> Vec<DiffHunk> {
    if lines.is_empty() {
        return Vec::new();
    }
    let mut hunks: Vec<DiffHunk> = Vec::new();
    let mut current: Vec<DiffLine> = Vec::new();
    let mut leading_context: std::collections::VecDeque<DiffLine> =
        std::collections::VecDeque::with_capacity(DIFF_CONTEXT_LINES + 1);
    let mut trailing_context: usize = 0;

    for line in lines {
        let is_change = matches!(line.kind, DiffLineKind::Added | DiffLineKind::Removed);
        if is_change {
            if current.is_empty() {
                current.extend(leading_context.drain(..));
            }
            current.push(line);
            trailing_context = 0;
            continue;
        }

        if current.is_empty() {
            leading_context.push_back(line);
            if leading_context.len() > DIFF_CONTEXT_LINES {
                leading_context.pop_front();
            }
            continue;
        }

        trailing_context += 1;
        if trailing_context <= DIFF_CONTEXT_LINES {
            current.push(line);
        } else {
            push_hunk(&mut hunks, std::mem::take(&mut current));
            leading_context.clear();
            leading_context.push_back(line);
            trailing_context = 0;
        }
    }

    if !current.is_empty() {
        push_hunk(&mut hunks, current);
    }
    hunks
}

fn push_hunk(hunks: &mut Vec<DiffHunk>, lines: Vec<DiffLine>) {
    if lines
        .iter()
        .any(|line| matches!(line.kind, DiffLineKind::Added | DiffLineKind::Removed))
    {
        let old_start = lines.iter().filter_map(|l| l.old_line).min().unwrap_or(0);
        let new_start = lines.iter().filter_map(|l| l.new_line).min().unwrap_or(0);
        hunks.push(DiffHunk {
            old_start,
            new_start,
            lines,
        });
    }
}

fn compute_coarse_hunks(old_lines: &[&str], new_lines: &[&str]) -> Vec<DiffHunk> {
    let prefix_len = common_prefix_len(old_lines, new_lines);
    let suffix_len = common_suffix_len(&old_lines[prefix_len..], &new_lines[prefix_len..]);

    let old_change_end = old_lines.len() - suffix_len;
    let new_change_end = new_lines.len() - suffix_len;
    let prefix_context_start = prefix_len.saturating_sub(DIFF_CONTEXT_LINES);
    let suffix_context_len = suffix_len.min(DIFF_CONTEXT_LINES);

    let mut lines = Vec::new();
    for (idx, line) in old_lines
        .iter()
        .enumerate()
        .take(prefix_len)
        .skip(prefix_context_start)
    {
        lines.push(DiffLine {
            kind: DiffLineKind::Context,
            content: (*line).to_string(),
            old_line: Some((idx + 1) as u32),
            new_line: Some((idx + 1) as u32),
        });
    }
    for (idx, line) in old_lines
        .iter()
        .enumerate()
        .take(old_change_end)
        .skip(prefix_len)
    {
        lines.push(DiffLine {
            kind: DiffLineKind::Removed,
            content: (*line).to_string(),
            old_line: Some((idx + 1) as u32),
            new_line: None,
        });
    }
    for (idx, line) in new_lines
        .iter()
        .enumerate()
        .take(new_change_end)
        .skip(prefix_len)
    {
        lines.push(DiffLine {
            kind: DiffLineKind::Added,
            content: (*line).to_string(),
            old_line: None,
            new_line: Some((idx + 1) as u32),
        });
    }
    for offset in 0..suffix_context_len {
        let old_idx = old_change_end + offset;
        let new_idx = new_change_end + offset;
        lines.push(DiffLine {
            kind: DiffLineKind::Context,
            content: old_lines[old_idx].to_string(),
            old_line: Some((old_idx + 1) as u32),
            new_line: Some((new_idx + 1) as u32),
        });
    }

    let mut hunks = Vec::new();
    push_hunk(&mut hunks, lines);
    hunks
}

fn common_prefix_len(old_lines: &[&str], new_lines: &[&str]) -> usize {
    old_lines
        .iter()
        .zip(new_lines.iter())
        .take_while(|(old, new)| old == new)
        .count()
}

fn common_suffix_len(old_tail: &[&str], new_tail: &[&str]) -> usize {
    old_tail
        .iter()
        .rev()
        .zip(new_tail.iter().rev())
        .take_while(|(old, new)| old == new)
        .count()
}

fn lcs_cell_count_exceeds_budget(old_len: usize, new_len: usize) -> bool {
    let Some(rows) = old_len.checked_add(1) else {
        return true;
    };
    let Some(cols) = new_len.checked_add(1) else {
        return true;
    };
    match rows.checked_mul(cols) {
        Some(cells) => cells > DIFF_LCS_MAX_CELLS,
        None => true,
    }
}

fn lcs_table(a: &[&str], b: &[&str]) -> Vec<Vec<usize>> {
    let mut table = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in 0..a.len() {
        for j in 0..b.len() {
            if a[i] == b[j] {
                table[i + 1][j + 1] = table[i][j] + 1;
            } else {
                table[i + 1][j + 1] = std::cmp::max(table[i + 1][j], table[i][j + 1]);
            }
        }
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_for_new_file_marks_created() {
        let diff = build_file_diff("hello.txt".to_string(), None, "first\nsecond\n");
        assert_eq!(diff.status, DiffStatus::Created);
        assert!(!diff.hunks.is_empty());
        let added: usize = diff
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .filter(|l| l.kind == DiffLineKind::Added)
            .count();
        assert!(added >= 2);
    }

    #[test]
    fn diff_stats_ignore_trailing_newline_as_extra_blank_row() {
        let diff = build_file_diff("hello.txt".to_string(), None, "first\nsecond\n");

        assert_eq!(
            diff.stats(),
            DiffStats {
                added: 2,
                removed: 0
            }
        );
        assert!(
            !diff
                .hunks
                .iter()
                .flat_map(|hunk| hunk.lines.iter())
                .any(|line| line.kind == DiffLineKind::Added && line.content.is_empty()),
            "final newline must not render as an added blank row"
        );
    }

    #[test]
    fn diff_for_modification_marks_modified() {
        let diff = build_file_diff("file.txt".to_string(), Some("a\nb\nc\n"), "a\nB\nc\n");
        assert_eq!(diff.status, DiffStatus::Modified);
        let mut saw_added = false;
        let mut saw_removed = false;
        for hunk in &diff.hunks {
            for line in &hunk.lines {
                if line.kind == DiffLineKind::Added {
                    saw_added = true;
                }
                if line.kind == DiffLineKind::Removed {
                    saw_removed = true;
                }
            }
        }
        assert!(saw_added);
        assert!(saw_removed);
    }

    #[test]
    fn diff_hunks_keep_only_nearby_context() {
        let old_lines = (1..=30).map(|i| format!("line-{i}")).collect::<Vec<_>>();
        let old = old_lines.join("\n");
        let mut new_lines = old_lines;
        new_lines[24] = "changed".to_string();
        let new = new_lines.join("\n");

        let diff = build_file_diff("file.txt".to_string(), Some(&old), &new);

        assert_eq!(
            diff.stats(),
            DiffStats {
                added: 1,
                removed: 1
            }
        );
        assert_eq!(diff.hunks.len(), 1);
        let hunk = &diff.hunks[0];
        assert!(
            hunk.lines.len() <= 6,
            "one changed line should render with only nearby context: {:?}",
            hunk.lines
        );
        assert!(
            !hunk.lines.iter().any(|line| line.content == "line-1"),
            "distant leading context should be omitted"
        );
    }

    #[test]
    fn large_diff_uses_coarse_hunk_with_common_anchors() {
        let old_lines = (1..=2100).map(|i| format!("line-{i}")).collect::<Vec<_>>();
        let old = old_lines.join("\n");
        let mut new_lines = old_lines;
        new_lines[1050] = "changed".to_string();
        let new = new_lines.join("\n");

        let diff = build_file_diff("large.txt".to_string(), Some(&old), &new);

        assert!(!diff.truncated);
        assert_eq!(
            diff.stats(),
            DiffStats {
                added: 1,
                removed: 1
            }
        );
        assert_eq!(diff.hunks.len(), 1);
        let hunk = &diff.hunks[0];
        assert_eq!(hunk.lines.len(), 6);
        assert!(
            hunk.lines.iter().any(|line| line.content == "line-1049"),
            "prefix context should anchor the coarse hunk: {:?}",
            hunk.lines
        );
        assert!(
            hunk.lines.iter().any(|line| line.content == "line-1053"),
            "suffix context should anchor the coarse hunk: {:?}",
            hunk.lines
        );
        assert!(
            !hunk.lines.iter().any(|line| line.content == "line-1"),
            "distant context should be omitted from coarse hunks"
        );
    }

    #[test]
    fn diff_truncates_when_too_many_lines() {
        let old = (1..=5000)
            .map(|i| format!("old-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = (1..=5000)
            .map(|i| format!("new-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let diff = build_file_diff("big.txt".to_string(), Some(&old), &new);
        assert!(diff.truncated);
        assert_eq!(
            diff.stats(),
            DiffStats {
                added: 5000,
                removed: 5000
            }
        );
        let rendered: usize = diff.hunks.iter().map(|h| h.lines.len()).sum();
        assert!(rendered <= DIFF_MAX_LINES);
    }

    #[test]
    fn hook_diff_is_deterministic_bounded_and_marks_truncation() {
        let old = (1..=5000)
            .map(|i| format!("old-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = (1..=5000)
            .map(|i| format!("new-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let diff = build_file_diff("big.txt".to_string(), Some(&old), &new);

        let first = diff.render_for_hook();
        let second = diff.render_for_hook();

        assert_eq!(first, second);
        assert!(first.len() <= HOOK_DIFF_MAX_BYTES, "{}", first.len());
        assert!(first.contains("[diff truncated by line limit"), "{first}");
        assert!(first.contains("full totals: +5000 -5000"), "{first}");
    }

    #[test]
    fn hook_diff_caps_one_huge_line_at_utf8_boundary() {
        let new = "🦀".repeat(HOOK_DIFF_MAX_BYTES);
        let diff = build_file_diff("huge.txt".to_string(), None, &new);

        let rendered = diff.render_for_hook();

        assert!(rendered.len() <= HOOK_DIFF_MAX_BYTES, "{}", rendered.len());
        assert!(rendered.is_char_boundary(rendered.len()));
        assert!(
            rendered.contains("[diff truncated by byte limit"),
            "{rendered}"
        );
    }

    #[test]
    fn hook_diff_redacts_secrets_without_mutating_tool_output_diff() {
        let secret = format!("sk-proj-{}", "a".repeat(40));
        let diff = build_file_diff("config.txt".to_string(), None, &format!("key={secret}\n"));

        let rendered = diff.render_for_hook();

        assert!(!rendered.contains(&secret), "{rendered}");
        assert!(rendered.contains("[REDACTED:OpenAI API key]"), "{rendered}");
        assert!(
            diff.hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| line.content.contains(&secret)),
            "rendering hook evidence must not mutate the structured ToolOutput diff"
        );
    }

    #[test]
    fn file_diff_redacts_secret_lines() {
        let old_key = format!("sk-{}", "OLDsecret0123456789".repeat(2));
        let new_key = format!("sk-{}", "NEWsecret0123456789".repeat(2));
        let mut diff = build_file_diff(
            "config.txt".to_string(),
            Some(&format!("api_key = \"{old_key}\"\n")),
            &format!("api_key = \"{new_key}\"\n"),
        );

        diff.redact_secrets();

        let rendered = diff
            .hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter())
            .map(|line| line.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!rendered.contains(&old_key), "{rendered}");
        assert!(!rendered.contains(&new_key), "{rendered}");
        assert!(rendered.contains("[REDACTED:OpenAI API key]"), "{rendered}");
    }

    #[test]
    fn bundled_file_diffs_keep_per_file_identity_and_aggregate_stats() {
        let first = build_file_diff("a.rs".to_string(), Some("old\n"), "new\n");
        let second = build_deleted_file_diff("b.rs".to_string(), "one\ntwo\n");
        let bundled = FileDiff::from_files(vec![first, second]).unwrap();

        assert_eq!(bundled.file_count(), 2);
        assert_eq!(
            bundled
                .files()
                .map(|diff| (diff.path.as_str(), diff.status))
                .collect::<Vec<_>>(),
            [
                ("a.rs", DiffStatus::Modified),
                ("b.rs", DiffStatus::Deleted)
            ]
        );
        assert_eq!(
            bundled.stats(),
            DiffStats {
                added: 1,
                removed: 3,
            }
        );
    }
}
