use std::fmt::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;
use ignore::gitignore::Gitignore;
use serde::Deserialize;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

use crate::tool::arg_repair::{RepairAction, RepairNote};
use crate::tool::read_evidence::digest_content;
use crate::tool::schema::{bounded_integer_property, closed_object, parse_args, path_property};
use crate::tool::{
    FileOrDirectoryKind, ProjectPathResolver, ReadCoverage, ReadEvidence, ReadTracker, ReadWindow,
    Tool, ToolOutput, file_or_directory_kind, output, search,
};

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default = "default_offset")]
    offset: usize,
    #[serde(default = "default_limit")]
    limit: usize,
    depth: Option<usize>,
}

fn default_offset() -> usize {
    1
}

fn default_limit() -> usize {
    MAX_READ_LINES
}

const MAX_TREE_DEPTH: usize = 6;
const MAX_READ_LINES: usize = 1000;
const MAX_READ_CHARS: usize = 30_000;
const ADAPTIVE_FIRST_READ_MAX_REQUEST_LINES: usize = 100;
const ADAPTIVE_FIRST_READ_MAX_FILE_LINES: usize = 300;
const ADAPTIVE_FIRST_READ_MAX_CHARS: usize = 12_000;
const READ_OUTPUT_CAP_HINT: &str =
    "Use a narrower offset/limit, grep, or symbol_search for targeted content.";

/// Cap on entries shown in a flat directory listing before truncating, so a
/// `read` on a large directory can't flood the context window.
const MAX_DIR_ENTRIES: usize = 200;

/// Cap on total nodes emitted across a directory tree walk, for the same reason.
const MAX_TREE_ENTRIES: usize = 300;

/// Upper bound on the size of a text file we will slurp into memory. Reading a
/// multi-gigabyte file would otherwise risk OOM. Files above this are paged from
/// disk one line at a time via [`ReadTool::read_large_file_window`].
const MAX_READ_BYTES: u64 = 10 * 1024 * 1024;

/// Per-line display cap for the large-file streaming path: a line longer than
/// this is truncated so one enormous (e.g. minified) line can't flood a window.
const LARGE_LINE_CHARS: usize = 4000;

/// Borrow a line as-is when it's within `max_chars`, otherwise truncate at a
/// char boundary with a marker. `char_indices().nth` stops at `max_chars`, so
/// the common (short) line costs O(max_chars), not O(line length).
fn truncate_line(line: &str, max_chars: usize) -> std::borrow::Cow<'_, str> {
    match line.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => {
            std::borrow::Cow::Owned(format!("{}…[line truncated]", &line[..byte_idx]))
        }
        None => std::borrow::Cow::Borrowed(line),
    }
}

/// Upper bound on image size before base64-encoding, to avoid token/cost blowup.
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// Map the `start_line`/`end_line` line-range convention onto `offset`/`limit`.
/// That convention is canonical on the sibling `read_region` tool and on other
/// harnesses' read tools, so models routinely import it here (observed as
/// closed-schema rejections in the tool-call log). The mapping is unambiguous —
/// `offset = start_line`, `limit = end_line - start_line + 1` — so it is
/// coerced rather than bounced. An alias only fills an *absent* canonical
/// field; when the canonical field is also present it wins and the redundant
/// alias is dropped. Conformant calls carry neither alias and pass through
/// untouched, per the validate-then-repair contract.
fn coerce_line_range_aliases(args: &mut serde_json::Value) -> Vec<RepairNote> {
    let Some(object) = args.as_object_mut() else {
        return Vec::new();
    };
    let mut notes = Vec::new();
    let mapped = |field: &str| RepairNote {
        field: field.to_string(),
        action: RepairAction::MappedAliasField,
    };

    // The generic repair pass only walks declared schema fields, so quoted
    // numbers ("120") on these undeclared aliases are parsed here. A value
    // that isn't a line number is left in place for rejected-fields guidance.
    if let Some(start) = object.get("start_line").and_then(line_number) {
        object.remove("start_line");
        if !object.contains_key("offset") {
            object.insert("offset".to_string(), start.into());
        }
        notes.push(mapped("start_line"));
    }
    if let Some(end) = object.get("end_line").and_then(line_number) {
        object.remove("end_line");
        if !object.contains_key("limit") {
            let offset = object.get("offset").and_then(line_number).unwrap_or(1);
            // An inverted range maps to limit 0, which execute rejects with
            // its existing "limit must be greater than 0" guidance.
            let limit = (end + 1).saturating_sub(offset);
            object.insert("limit".to_string(), limit.into());
        }
        notes.push(mapped("end_line"));
    }
    notes
}

fn line_number(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Number(number) => number.as_u64(),
        serde_json::Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

pub struct ReadTool {
    project_root: PathBuf,
    read_tracker: ReadTracker,
}

impl ReadTool {
    pub fn new(project_root: PathBuf, read_tracker: ReadTracker) -> Self {
        Self {
            project_root,
            read_tracker,
        }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::ReadOnly
    }

    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a file or directory from the project. Returns file contents with line numbers, directory listings, or base64-encoded images. A first window of up to 100 lines may expand to a small whole file; use read_region when the exact range matters."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        closed_object(
            [
                (
                    "path",
                    path_property(
                        "File or directory path relative to project root, or absolute path inside it",
                    ),
                ),
                (
                    "offset",
                    bounded_integer_property(
                        "Line number to start reading from (1-indexed, default: 1)",
                        Some(1),
                        None,
                    ),
                ),
                (
                    "limit",
                    bounded_integer_property(
                        "Maximum number of lines to read (default: 1000)",
                        Some(1),
                        Some(MAX_READ_LINES as i64),
                    ),
                ),
                (
                    "depth",
                    bounded_integer_property(
                        "When reading a directory, return a tree to this depth instead of the flat listing (1-6)",
                        Some(1),
                        Some(MAX_TREE_DEPTH as i64),
                    ),
                ),
            ],
            &["path"],
        )
    }

    fn coerce_arguments(&self, args: &mut serde_json::Value) -> Vec<RepairNote> {
        coerce_line_range_aliases(args)
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let args: ReadArgs = parse_args("read tool", args)?;

        let resolved_path = ProjectPathResolver::new(&self.project_root)
            .action("read files")
            .resolve_existing(&args.path)?;
        let canonical_path = resolved_path.canonical_path();
        let canonical_root = resolved_path.canonical_root();

        if file_or_directory_kind(canonical_path) == FileOrDirectoryKind::Directory {
            if let Some(depth) = args.depth {
                if depth == 0 {
                    anyhow::bail!("depth must be greater than 0");
                }
                if depth > MAX_TREE_DEPTH {
                    anyhow::bail!("depth must be {MAX_TREE_DEPTH} or less");
                }
                return self
                    .list_directory_tree(canonical_path, canonical_root, depth)
                    .await;
            }
            return self.list_directory(canonical_path, canonical_root).await;
        }

        if self.is_image_path(canonical_path) {
            return self.read_image(canonical_path).await;
        }

        if is_binary(canonical_path).await? {
            let metadata = fs::metadata(canonical_path).await?;
            anyhow::bail!(
                "Binary file detected: {} ({} bytes)",
                args.path,
                metadata.len()
            );
        }

        if args.offset == 0 {
            anyhow::bail!("offset must be 1-indexed (minimum value is 1)");
        }
        if args.limit == 0 {
            anyhow::bail!("limit must be greater than 0");
        }
        let effective_limit = args.limit.min(MAX_READ_LINES);

        let metadata = fs::metadata(canonical_path)
            .await
            .with_context(|| format!("Failed to read file: {}", args.path))?;
        let offset = args.offset.saturating_sub(1);

        // Files above the slurp ceiling are paged from disk one line at a time
        // (bounded memory) rather than rejected — offset/limit pick the window.
        if metadata.len() > MAX_READ_BYTES {
            return self
                .read_large_file_window(
                    canonical_path,
                    &args.path,
                    offset,
                    args.limit,
                    effective_limit,
                )
                .await;
        }

        let content = match fs::read_to_string(canonical_path).await {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::InvalidData => {
                anyhow::bail!(
                    "File is not valid UTF-8: {} (it appears to be binary or to use a non-UTF-8 text encoding).",
                    args.path
                );
            }
            Err(err) => {
                return Err(err).with_context(|| format!("Failed to read file: {}", args.path));
            }
        };

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        let expand_first_window = should_expand_first_window(&args, &lines);
        let start = if expand_first_window {
            0
        } else {
            offset.min(total_lines)
        };
        let end = if expand_first_window {
            total_lines
        } else {
            offset.saturating_add(effective_limit).min(total_lines)
        };
        let file_digest = digest_content(content.as_bytes());

        let mut result = String::new();
        let mut line_truncated = false;
        for (i, line) in lines[start..end].iter().enumerate() {
            let displayed_line = truncate_line(line, LARGE_LINE_CHARS);
            if matches!(&displayed_line, std::borrow::Cow::Owned(_)) {
                line_truncated = true;
            }
            let _ = writeln!(result, "{}: {}", start + i + 1, displayed_line);
        }

        if end < total_lines {
            result.push_str(&format!(
                "\n[File truncated. Total: {} lines. Showing lines {}-{}. Use offset={} to read more.]\n",
                total_lines,
                start + 1,
                end,
                end + 1
            ));
        }
        let char_truncated = result.chars().count() > MAX_READ_CHARS;
        if char_truncated {
            result = output::cap_text(result, MAX_READ_CHARS, READ_OUTPUT_CAP_HINT);
        }
        let visible_end_line = displayed_line_range(&result)
            .map(|(_start, end)| end)
            .or_else(|| (end > start).then_some(end));

        // The model-visible display covers the whole file only when the window
        // spans every line and no output cap hid any content.
        let full = start == 0 && end == total_lines && !char_truncated && !line_truncated;
        // Reuse the digest just computed for the freshness ledger rather than
        // digesting the same bytes inside the tracker.
        self.read_tracker
            .mark_read_with_file_digest(canonical_path, file_digest, full)
            .await;

        // Stat after the read so the recorded mtime and file digest
        // describe the same bytes, and take `len` from the read buffer for the
        // same reason. Reusing the pre-read stat here paired the old file's
        // len/mtime with the new file's digest; because refresh_freshness checks
        // len first, an unchanged-since-read file could then spuriously report
        // Stale.
        let modified = fs::metadata(canonical_path)
            .await
            .ok()
            .and_then(|metadata| metadata.modified().ok());
        let evidence = ReadEvidence::new(
            args.path,
            canonical_path.to_path_buf(),
            ReadWindow {
                requested_offset: args.offset,
                requested_limit: args.limit,
                start_line: start.saturating_add(1),
                end_line: visible_end_line,
                total_lines: Some(total_lines),
            },
            if full {
                ReadCoverage::Full
            } else {
                ReadCoverage::Partial
            },
            &result,
            modified,
            content.len() as u64,
            Some(file_digest),
        );

        Ok(ToolOutput::Read {
            text: result,
            evidence,
        })
    }
}

fn should_expand_first_window(args: &ReadArgs, lines: &[&str]) -> bool {
    if args.offset != 1
        || args.limit > ADAPTIVE_FIRST_READ_MAX_REQUEST_LINES
        || lines.len() > ADAPTIVE_FIRST_READ_MAX_FILE_LINES
    {
        return false;
    }

    let mut rendered_chars = 0usize;
    for (index, line) in lines.iter().enumerate() {
        let mut line_chars = 0usize;
        for _character in line.chars() {
            line_chars = line_chars.saturating_add(1);
            if line_chars > LARGE_LINE_CHARS {
                return false;
            }
        }
        rendered_chars = rendered_chars
            .saturating_add((index + 1).to_string().len())
            .saturating_add(2)
            .saturating_add(line_chars)
            .saturating_add(1);
        if rendered_chars > ADAPTIVE_FIRST_READ_MAX_CHARS {
            return false;
        }
    }
    true
}

impl ReadTool {
    /// Page a file too large to slurp by streaming it one line at a time and
    /// emitting only the requested `[offset, offset+limit)` window — memory stays
    /// bounded by the window and model-visible character cap regardless of file size.
    /// Read tracking falls back to len+mtime (no whole-file digest, since we never
    /// load the whole file). `offset` is 0-indexed.
    async fn read_large_file_window(
        &self,
        canonical_path: &std::path::Path,
        display_path: &str,
        offset: usize,
        requested_limit: usize,
        limit: usize,
    ) -> Result<ToolOutput> {
        let metadata = fs::metadata(canonical_path)
            .await
            .with_context(|| format!("Failed to read file: {display_path}"))?;
        let file = fs::File::open(canonical_path)
            .await
            .with_context(|| format!("Failed to read file: {display_path}"))?;
        let mut lines = tokio::io::BufReader::new(file).lines();

        // Marked by len+mtime, and as a *partial* read: we only paged a window,
        // so a later `write` that would clobber the rest is rejected.
        self.read_tracker.mark_read_partial(canonical_path).await;

        let mut result = String::new();
        let mut line_no = 0usize; // 1-indexed line most recently read
        let mut emitted = 0usize;
        let mut more = false;
        loop {
            let line = match lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(err) if err.kind() == std::io::ErrorKind::InvalidData => {
                    anyhow::bail!(
                        "File is not valid UTF-8: {display_path} (it appears to be binary or to use a non-UTF-8 text encoding)."
                    );
                }
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("Failed to read file: {display_path}"));
                }
            };
            line_no += 1;
            if line_no <= offset {
                continue; // before the window
            }
            if emitted >= limit || result.len() >= MAX_READ_CHARS {
                more = true; // window full (line count or byte cap) — more remains
                break;
            }
            // A single line longer than this is almost always machine-generated
            // (a minified bundle); truncate it so it can't flood the window.
            let _ = writeln!(
                result,
                "{}: {}",
                line_no,
                truncate_line(&line, LARGE_LINE_CHARS)
            );
            emitted += 1;
        }

        let mut result = if emitted == 0 && !more {
            format!(
                "[Large file: offset {} is past the end of the file.]",
                offset + 1
            )
        } else {
            result
        };
        if more {
            let next_offset = offset + emitted + 1;
            result.push_str(&format!(
                "\n[Large file (> {} MiB): showing from line {}. Use offset={} to read more.]\n",
                MAX_READ_BYTES / (1024 * 1024),
                offset + 1,
                next_offset
            ));
        }

        let evidence = ReadEvidence::new(
            display_path.to_string(),
            canonical_path.to_path_buf(),
            ReadWindow {
                requested_offset: offset.saturating_add(1),
                requested_limit,
                start_line: offset.saturating_add(1),
                end_line: (emitted > 0).then_some(offset + emitted),
                total_lines: (!more).then_some(line_no),
            },
            ReadCoverage::Partial,
            &result,
            metadata.modified().ok(),
            metadata.len(),
            None,
        );

        Ok(ToolOutput::Read {
            text: result,
            evidence,
        })
    }

    async fn list_directory(
        &self,
        path: &std::path::Path,
        project_root: &std::path::Path,
    ) -> Result<ToolOutput> {
        let respect_gitignore = search::env_flag_enabled("BONSAI_READ_RESPECT_GITIGNORE", true);
        let gitignore = if respect_gitignore {
            search::build_gitignore(project_root, path)
        } else {
            None
        };

        let mut entries = Vec::new();
        let mut read_dir = fs::read_dir(path)
            .await
            .with_context(|| format!("Failed to read directory: {}", path.display()))?;

        while let Some(entry) = read_dir
            .next_entry()
            .await
            .context("Failed to read directory entry")?
        {
            let entry_path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let entry_type = entry.file_type().await.ok();
            let is_dir = entry_type.as_ref().map(|ft| ft.is_dir()).unwrap_or(false);
            let relative = entry_path
                .strip_prefix(project_root)
                .unwrap_or(entry_path.as_path());

            // Hide dotfiles and gitignored entries by default, matching `glob`
            // and `grep`, so a bare `read` on the project root doesn't dump
            // `.git/`, `target/`, or `node_modules/`.
            if search::is_hidden_or_gitignored_kind(
                relative,
                gitignore.as_ref(),
                respect_gitignore,
                is_dir,
            ) {
                continue;
            }

            let metadata = entry.metadata().await.ok();
            let type_char = if is_dir {
                "d"
            } else if entry_type
                .as_ref()
                .map(|ft| ft.is_symlink())
                .unwrap_or(false)
            {
                "l"
            } else {
                "-"
            };

            let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
            let modified = metadata
                .as_ref()
                .and_then(|m| m.modified().ok())
                .map(|t| {
                    let since_epoch = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
                    let secs = since_epoch.as_secs();
                    let (y, m, d) = unix_to_ymd(secs);
                    let h = (secs % 86400) / 3600;
                    let min = (secs % 3600) / 60;
                    format!("{:04}-{:02}-{:02} {:02}:{:02}", y, m, d, h, min)
                })
                .unwrap_or_else(|| "unknown".to_string());

            entries.push((name, type_char.to_string(), size, modified));
        }

        entries.sort_by(|a, b| dir_first_ordering(a.1 == "d", &a.0, b.1 == "d", &b.0));

        let dir_name = directory_display_name(path);
        let total = entries.len();
        let truncated = total > MAX_DIR_ENTRIES;
        if truncated {
            entries.truncate(MAX_DIR_ENTRIES);
        }

        let mut result = format!("Directory: {}/\n\n", dir_name);
        for (name, type_char, size, modified) in &entries {
            result.push_str(&format!(
                "  {}  {:>10}  {}  {}\n",
                type_char, size, modified, name
            ));
        }

        if total == 0 {
            result.push_str("  (empty directory)\n");
        }

        if truncated {
            result.push_str(&output::truncation_note(&format!(
                "showing {MAX_DIR_ENTRIES} of {total} entries. Narrow with a subdirectory path, or use glob/grep."
            )));
        }

        Ok(ToolOutput::Text(result))
    }

    async fn list_directory_tree(
        &self,
        path: &std::path::Path,
        project_root: &std::path::Path,
        depth: usize,
    ) -> Result<ToolOutput> {
        let respect_gitignore = search::env_flag_enabled("BONSAI_READ_RESPECT_GITIGNORE", true);
        let gitignore = if respect_gitignore {
            search::build_gitignore(project_root, path)
        } else {
            None
        };

        let dir_name = directory_display_name(path);
        let mut result = format!("Directory tree: {dir_name}/ (depth {depth})\n\n{dir_name}/\n");
        let entries =
            sorted_tree_entries(path, project_root, gitignore.as_ref(), respect_gitignore).await?;
        if entries.is_empty() {
            result.push_str("  (empty directory)\n");
            return Ok(ToolOutput::Text(result));
        }

        let mut stack = vec![TreeFrame {
            entries,
            index: 0,
            prefix: String::new(),
            remaining_depth: depth,
        }];

        let mut emitted = 0usize;
        let mut truncated = false;
        while let Some(frame) = stack.last_mut() {
            if frame.index >= frame.entries.len() {
                stack.pop();
                continue;
            }
            if emitted >= MAX_TREE_ENTRIES {
                truncated = true;
                break;
            }

            let entry = frame.entries[frame.index].clone();
            let is_last = frame.index + 1 == frame.entries.len();
            let prefix = frame.prefix.clone();
            let remaining_depth = frame.remaining_depth;
            frame.index += 1;

            let connector = if is_last { "`-- " } else { "|-- " };
            result.push_str(&prefix);
            result.push_str(connector);
            result.push_str(&entry.display_name());
            result.push('\n');
            emitted += 1;

            if entry.is_dir && remaining_depth > 1 {
                let children = sorted_tree_entries(
                    &entry.path,
                    project_root,
                    gitignore.as_ref(),
                    respect_gitignore,
                )
                .await?;
                if !children.is_empty() {
                    let child_prefix = if is_last { "    " } else { "|   " };
                    stack.push(TreeFrame {
                        entries: children,
                        index: 0,
                        prefix: format!("{prefix}{child_prefix}"),
                        remaining_depth: remaining_depth - 1,
                    });
                }
            }
        }

        if truncated {
            result.push_str(&output::truncation_note(&format!(
                "tree capped at {MAX_TREE_ENTRIES} entries. Narrow with a subdirectory path, or lower depth."
            )));
        }

        Ok(ToolOutput::Text(result))
    }

    async fn read_image(&self, path: &std::path::Path) -> Result<ToolOutput> {
        let metadata = fs::metadata(path)
            .await
            .with_context(|| format!("Failed to read image: {}", path.display()))?;
        if metadata.len() > MAX_IMAGE_BYTES {
            anyhow::bail!(
                "Image too large to read: {} ({} bytes, limit {MAX_IMAGE_BYTES} bytes).",
                path.display(),
                metadata.len()
            );
        }

        let bytes = fs::read(path)
            .await
            .with_context(|| format!("Failed to read image: {}", path.display()))?;

        let size = bytes.len();
        let mime_type = mime_for_path(path);
        let base64_data =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);

        let description = format!(
            "Image file: {} ({} bytes, {})",
            path.file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default(),
            size,
            mime_type
        );

        self.read_tracker.mark_read(path).await;

        Ok(ToolOutput::Image {
            mime_type,
            base64_data,
            description,
        })
    }

    fn is_image_path(&self, path: &std::path::Path) -> bool {
        mime_for_path(path).starts_with("image/")
    }
}

fn displayed_line_range(rendered: &str) -> Option<(usize, usize)> {
    let mut first = None;
    let mut last = None;
    for line in rendered.lines() {
        let Some((number, _rest)) = line.split_once(": ") else {
            continue;
        };
        let Ok(parsed) = number.trim().parse::<usize>() else {
            continue;
        };
        first.get_or_insert(parsed);
        last = Some(parsed);
    }
    Some((first?, last?))
}

#[derive(Debug)]
struct TreeFrame {
    entries: Vec<TreeEntry>,
    index: usize,
    prefix: String,
    remaining_depth: usize,
}

#[derive(Debug, Clone)]
struct TreeEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
}

impl TreeEntry {
    fn display_name(&self) -> String {
        if self.is_dir {
            format!("{}/", self.name)
        } else {
            self.name.clone()
        }
    }
}

/// List a directory's immediate children for the tree renderer, pruning hidden
/// and gitignored entries. `root` is the directory the tree is rooted at, used
/// to make each entry's path relative for gitignore matching (the `gitignore`
/// is built once at `root`).
async fn sorted_tree_entries(
    dir: &std::path::Path,
    root: &std::path::Path,
    gitignore: Option<&Gitignore>,
    respect_gitignore: bool,
) -> Result<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    let mut read_dir = fs::read_dir(dir)
        .await
        .with_context(|| format!("Failed to read directory: {}", dir.display()))?;

    while let Some(entry) = read_dir
        .next_entry()
        .await
        .context("Failed to read directory entry")?
    {
        let path = entry.path();
        let file_type = entry.file_type().await.ok();
        let is_dir = file_type
            .as_ref()
            .is_some_and(|file_type| file_type.is_dir());

        let relative = path.strip_prefix(root).unwrap_or(&path);
        if search::is_hidden_or_gitignored_kind(relative, gitignore, respect_gitignore, is_dir) {
            continue;
        }

        entries.push(TreeEntry {
            name: entry.file_name().to_string_lossy().to_string(),
            path,
            is_dir,
        });
    }

    entries.sort_by(|left, right| {
        dir_first_ordering(left.is_dir, &left.name, right.is_dir, &right.name)
    });
    Ok(entries)
}

/// Ordering that lists directories before files, then sorts by name. Shared by
/// the flat listing and the tree renderer.
fn dir_first_ordering(
    a_is_dir: bool,
    a_name: &str,
    b_is_dir: bool,
    b_name: &str,
) -> std::cmp::Ordering {
    match (a_is_dir, b_is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a_name.cmp(b_name),
    }
}

fn directory_display_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string())
}

async fn is_binary(path: &std::path::Path) -> Result<bool> {
    let mut file = fs::File::open(path)
        .await
        .with_context(|| format!("Failed to open file: {}", path.display()))?;

    let mut buffer = vec![0u8; 8192];
    let bytes_read = file
        .read(&mut buffer)
        .await
        .context("Failed to read file bytes for binary detection")?;

    buffer.truncate(bytes_read);
    Ok(buffer.contains(&0))
}

fn mime_for_path(path: &std::path::Path) -> String {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("png") => "image/png".to_string(),
        Some("jpg") | Some("jpeg") => "image/jpeg".to_string(),
        Some("gif") => "image/gif".to_string(),
        Some("webp") => "image/webp".to_string(),
        Some("svg") => "image/svg+xml".to_string(),
        _ => "application/octet-stream".to_string(),
    }
}

fn unix_to_ymd(secs: u64) -> (i32, u32, u32) {
    let days = secs / 86400;
    let mut days = days as i64;
    let mut y = 1970;
    loop {
        let days_in_year = if is_leap(y) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        y += 1;
    }
    let months = [
        31,
        if is_leap(y) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0;
    for (i, &dim) in months.iter().enumerate() {
        if days < dim as i64 {
            m = i as u32 + 1;
            break;
        }
        days -= dim as i64;
    }
    (y, m, days as u32 + 1)
}

fn is_leap(y: i32) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::read_evidence::ReadFreshness;
    use crate::tool::test_utils::TestFixture;
    use serde_json::json;

    fn read_output(output: ToolOutput) -> (String, ReadEvidence) {
        match output {
            ToolOutput::Read { text, evidence } => (text, evidence),
            other => panic!("Expected read output, got {other:?}"),
        }
    }

    #[test]
    fn test_mime_for_path() {
        assert_eq!(
            mime_for_path(PathBuf::from("image.png").as_path()),
            "image/png"
        );
        assert_eq!(
            mime_for_path(PathBuf::from("photo.jpg").as_path()),
            "image/jpeg"
        );
        assert_eq!(
            mime_for_path(PathBuf::from("icon.svg").as_path()),
            "image/svg+xml"
        );
        assert_eq!(
            mime_for_path(PathBuf::from("file.txt").as_path()),
            "application/octet-stream"
        );
    }

    #[test]
    fn test_coerce_line_range_aliases_maps_to_offset_and_limit() {
        let mut args = json!({"path": "src/lib.rs", "start_line": 150, "end_line": 175});
        let notes = coerce_line_range_aliases(&mut args);
        assert_eq!(notes.len(), 2);
        assert_eq!(
            args,
            json!({"path": "src/lib.rs", "offset": 150, "limit": 26})
        );
    }

    #[test]
    fn test_coerce_line_range_aliases_parses_quoted_numbers() {
        let mut args = json!({"path": "a.rs", "start_line": "32", "end_line": "120"});
        coerce_line_range_aliases(&mut args);
        assert_eq!(args, json!({"path": "a.rs", "offset": 32, "limit": 89}));
    }

    #[test]
    fn test_coerce_line_range_aliases_end_line_alone_reads_from_top() {
        let mut args = json!({"path": "a.rs", "end_line": 40});
        coerce_line_range_aliases(&mut args);
        assert_eq!(args, json!({"path": "a.rs", "limit": 40}));
    }

    #[test]
    fn test_coerce_line_range_aliases_canonical_fields_win() {
        let mut args =
            json!({"path": "a.rs", "offset": 10, "limit": 5, "start_line": 99, "end_line": 200});
        coerce_line_range_aliases(&mut args);
        assert_eq!(args, json!({"path": "a.rs", "offset": 10, "limit": 5}));
    }

    #[test]
    fn test_coerce_line_range_aliases_untouched_when_conformant() {
        let mut args = json!({"path": "a.rs", "offset": 10, "limit": 5});
        let notes = coerce_line_range_aliases(&mut args);
        assert!(notes.is_empty());
        assert_eq!(args, json!({"path": "a.rs", "offset": 10, "limit": 5}));
    }

    #[test]
    fn test_coerce_line_range_aliases_leaves_unparseable_alias() {
        let mut args = json!({"path": "a.rs", "start_line": true});
        let notes = coerce_line_range_aliases(&mut args);
        assert!(notes.is_empty());
        assert_eq!(args, json!({"path": "a.rs", "start_line": true}));
    }

    #[test]
    fn test_coerce_line_range_aliases_inverted_range_yields_zero_limit() {
        let mut args = json!({"path": "a.rs", "start_line": 50, "end_line": 10});
        coerce_line_range_aliases(&mut args);
        // limit 0 is rejected by execute with its existing guidance.
        assert_eq!(args, json!({"path": "a.rs", "offset": 50, "limit": 0}));
    }

    #[test]
    fn test_unix_to_ymd() {
        // 2024-01-01 00:00:00 UTC
        let (y, m, d) = unix_to_ymd(1704067200);
        assert_eq!((y, m, d), (2024, 1, 1));
    }

    #[test]
    fn test_is_leap() {
        assert!(is_leap(2024));
        assert!(!is_leap(2023));
        assert!(!is_leap(1900));
        assert!(is_leap(2000));
    }

    #[tokio::test]
    async fn test_read_simple_file() {
        let fixture = TestFixture::new();
        fixture.create_file("test.txt", "Line 1\nLine 2\nLine 3");

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt"
            }))
            .await
            .unwrap();

        let (output, evidence) = read_output(result);

        assert!(output.contains("1: Line 1"));
        assert!(output.contains("2: Line 2"));
        assert!(output.contains("3: Line 3"));
        assert_eq!(evidence.display_path(), "test.txt");
        assert_eq!(evidence.coverage(), ReadCoverage::Full);
        assert_eq!(evidence.freshness(), ReadFreshness::Fresh);
        assert_eq!(evidence.window().start_line, 1);
        assert_eq!(evidence.window().end_line, Some(3));
        assert_eq!(evidence.window().total_lines, Some(3));
        assert!(evidence.file_digest_at_read().is_some());
    }

    #[tokio::test]
    async fn large_files_paginate_instead_of_rejecting() {
        let fixture = TestFixture::new();
        // Build a file just over the slurp ceiling (~11 MiB) so it takes the
        // streaming window path rather than the in-memory slurp.
        let mut content = String::with_capacity(12 * 1024 * 1024);
        let filler = "x".repeat(1000);
        for n in 1..=11_000 {
            content.push_str(&format!("line {n}: {filler}\n"));
        }
        assert!(content.len() as u64 > MAX_READ_BYTES);
        fixture.create_file("big.txt", &content);

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({ "path": "big.txt", "offset": 5, "limit": 3 }))
            .await
            .expect("a large file should paginate, not reject");

        let (output, evidence) = read_output(result);
        // The requested window (lines 5-7) with line numbers, and nothing else.
        assert!(
            output.contains("5: line 5: "),
            "output: {}",
            &output[..200.min(output.len())]
        );
        assert!(output.contains("7: line 7: "));
        assert!(!output.contains("8: line 8: "));
        // And a pointer to page further.
        assert!(output.contains("Use offset=8 to read more"));
        assert_eq!(evidence.coverage(), ReadCoverage::Partial);
        assert_eq!(evidence.freshness(), ReadFreshness::Fresh);
        assert_eq!(evidence.window().start_line, 5);
        assert_eq!(evidence.window().end_line, Some(7));
        assert_eq!(evidence.window().total_lines, None);
        assert!(evidence.file_digest_at_read().is_none());
    }

    #[tokio::test]
    async fn test_read_with_pagination() {
        let fixture = TestFixture::new();
        fixture.create_file("test.txt", "Line 1\nLine 2\nLine 3\nLine 4\nLine 5");

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "offset": 2,
                "limit": 2
            }))
            .await
            .unwrap();

        let (output, evidence) = read_output(result);

        assert!(output.contains("2: Line 2"));
        assert!(output.contains("3: Line 3"));
        assert!(!output.contains("1: Line 1"));
        assert!(!output.contains("4: Line 4"));
        assert_eq!(evidence.coverage(), ReadCoverage::Partial);
        assert_eq!(evidence.window().start_line, 2);
        assert_eq!(evidence.window().end_line, Some(3));
        assert_eq!(evidence.window().total_lines, Some(5));
    }

    #[tokio::test]
    async fn read_evidence_refresh_marks_same_length_edit_stale() {
        let fixture = TestFixture::new();
        let path = fixture.create_file("test.txt", "aaaaa");

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt"
            }))
            .await
            .unwrap();
        let (_, mut evidence) = read_output(result);

        tokio::fs::write(path, "bbbbb").await.unwrap();
        evidence.refresh_freshness().await;

        assert_eq!(evidence.freshness(), ReadFreshness::Stale);
    }

    #[tokio::test]
    async fn read_evidence_refresh_marks_deleted_file() {
        let fixture = TestFixture::new();
        let path = fixture.create_file("test.txt", "content");

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt"
            }))
            .await
            .unwrap();
        let (_, mut evidence) = read_output(result);

        tokio::fs::remove_file(path).await.unwrap();
        evidence.refresh_freshness().await;

        assert_eq!(evidence.freshness(), ReadFreshness::Deleted);
    }

    #[tokio::test]
    async fn read_evidence_refresh_marks_non_regular_replacement_stale() {
        let fixture = TestFixture::new();
        let path = fixture.create_file("test.txt", "content");

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt"
            }))
            .await
            .unwrap();
        let (_, mut evidence) = read_output(result);

        tokio::fs::remove_file(&path).await.unwrap();
        tokio::fs::create_dir(&path).await.unwrap();
        evidence.refresh_freshness().await;

        assert_eq!(evidence.freshness(), ReadFreshness::Stale);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_evidence_refresh_marks_file_symlink_escape_stale() {
        let fixture = TestFixture::new();
        let path = fixture.create_file("test.txt", "same");
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("test.txt"), "same").unwrap();

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt"
            }))
            .await
            .unwrap();
        let (_, mut evidence) = read_output(result);

        tokio::fs::remove_file(&path).await.unwrap();
        std::os::unix::fs::symlink(outside.path().join("test.txt"), &path).unwrap();
        evidence.refresh_freshness().await;

        assert_eq!(evidence.freshness(), ReadFreshness::Stale);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_evidence_refresh_marks_parent_symlink_escape_stale() {
        let fixture = TestFixture::new();
        fixture.create_file("dir/test.txt", "same");
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("test.txt"), "same").unwrap();

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "dir/test.txt"
            }))
            .await
            .unwrap();
        let (_, mut evidence) = read_output(result);

        tokio::fs::remove_dir_all(fixture.project_root.join("dir"))
            .await
            .unwrap();
        std::os::unix::fs::symlink(outside.path(), fixture.project_root.join("dir")).unwrap();
        evidence.refresh_freshness().await;

        assert_eq!(evidence.freshness(), ReadFreshness::Stale);
    }

    #[tokio::test]
    async fn test_read_directory() {
        let fixture = TestFixture::new();
        fixture.create_file("dir/file1.txt", "content");
        fixture.create_file("dir/file2.txt", "content");

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "dir"
            }))
            .await
            .unwrap();

        let output = match result {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };

        assert!(output.contains("Directory: dir/"));
        assert!(output.contains("file1.txt"));
        assert!(output.contains("file2.txt"));
    }

    #[tokio::test]
    async fn read_directory_excludes_hidden_and_gitignored() {
        let fixture = TestFixture::new();
        fixture.create_file("proj/.gitignore", "ignored.txt\n");
        fixture.create_file("proj/ignored.txt", "x");
        fixture.create_file("proj/kept.txt", "x");
        fixture.create_file("proj/.secret", "x");

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let output = match tool.execute(json!({"path": "proj"})).await.unwrap() {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };

        assert!(output.contains("kept.txt"), "{output}");
        assert!(!output.contains("ignored.txt"), "{output}");
        assert!(!output.contains(".secret"), "{output}");
        assert!(!output.contains(".gitignore"), "{output}");
    }

    #[tokio::test]
    async fn read_directory_scoped_path_respects_root_gitignore() {
        let fixture = TestFixture::new();
        fixture.create_gitignore(&["proj/ignored.txt"]);
        fixture.create_file("proj/ignored.txt", "x");
        fixture.create_file("proj/kept.txt", "x");

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let output = match tool.execute(json!({"path": "proj"})).await.unwrap() {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };

        assert!(output.contains("kept.txt"), "{output}");
        assert!(!output.contains("ignored.txt"), "{output}");
    }

    #[tokio::test]
    async fn read_directory_caps_entries_with_footer() {
        let fixture = TestFixture::new();
        for i in 0..(MAX_DIR_ENTRIES + 5) {
            fixture.create_file(&format!("big/f{i:04}.txt"), "x");
        }

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let output = match tool.execute(json!({"path": "big"})).await.unwrap() {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };

        assert!(
            output.contains(&format!(
                "showing {MAX_DIR_ENTRIES} of {} entries",
                MAX_DIR_ENTRIES + 5
            )),
            "{output}"
        );
        assert!(output.contains("[Truncated:"), "{output}");
    }

    #[tokio::test]
    async fn read_directory_tree_excludes_hidden_and_gitignored() {
        let fixture = TestFixture::new();
        fixture.create_file("proj/.gitignore", "ignored/\n");
        fixture.create_file("proj/ignored/secret.txt", "x");
        fixture.create_file("proj/kept/file.txt", "x");
        fixture.create_file("proj/.hiddendir/h.txt", "x");

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let output = match tool
            .execute(json!({"path": "proj", "depth": 3}))
            .await
            .unwrap()
        {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };

        assert!(output.contains("kept/"), "{output}");
        // `ignored/` is a directory-only ignore pattern; it must still be pruned.
        assert!(!output.contains("ignored"), "{output}");
        assert!(!output.contains(".hiddendir"), "{output}");
        assert!(!output.contains(".gitignore"), "{output}");
    }

    #[tokio::test]
    async fn read_directory_tree_scoped_path_respects_root_gitignore() {
        let fixture = TestFixture::new();
        fixture.create_gitignore(&["proj/ignored/\n"]);
        fixture.create_file("proj/ignored/secret.txt", "x");
        fixture.create_file("proj/kept/file.txt", "x");

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let output = match tool
            .execute(json!({"path": "proj", "depth": 3}))
            .await
            .unwrap()
        {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };

        assert!(output.contains("kept/"), "{output}");
        assert!(!output.contains("ignored"), "{output}");
        assert!(!output.contains("secret.txt"), "{output}");
    }

    #[tokio::test]
    async fn read_directory_tree_caps_entries_with_footer() {
        let fixture = TestFixture::new();
        for i in 0..(MAX_TREE_ENTRIES + 5) {
            fixture.create_file(&format!("big/f{i:04}.txt"), "x");
        }

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let output = match tool
            .execute(json!({"path": "big", "depth": 1}))
            .await
            .unwrap()
        {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };

        assert!(output.contains("tree capped at"), "{output}");
        assert!(output.contains("[Truncated:"), "{output}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_directory_lists_symlink_without_following() {
        let fixture = TestFixture::new();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("escape.txt"), "x").unwrap();
        fixture.create_file("dir/real.txt", "x");
        std::os::unix::fs::symlink(outside.path(), fixture.project_root.join("dir/link")).unwrap();

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let output = match tool.execute(json!({"path": "dir"})).await.unwrap() {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };

        // The symlink is listed (as type `l`) but its target is not expanded.
        assert!(output.contains("link"), "{output}");
        assert!(!output.contains("escape.txt"), "{output}");
    }

    #[tokio::test]
    async fn test_read_directory_tree_respects_depth() {
        let fixture = TestFixture::new();
        fixture.create_file("dir/a/nested.txt", "content");
        fixture.create_file("dir/b/deeper/hidden.txt", "content");
        fixture.create_file("dir/root.txt", "content");

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "dir",
                "depth": 2
            }))
            .await
            .unwrap();

        let output = match result {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };

        assert_eq!(
            output,
            "Directory tree: dir/ (depth 2)\n\n\
             dir/\n\
             |-- a/\n\
             |   `-- nested.txt\n\
             |-- b/\n\
             |   `-- deeper/\n\
             `-- root.txt\n"
        );
        assert!(!output.contains("hidden.txt"));
    }

    #[tokio::test]
    async fn test_read_directory_tree_rejects_zero_depth() {
        let fixture = TestFixture::new();
        fixture.create_file("dir/file.txt", "content");

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "dir",
                "depth": 0
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("depth"));
    }

    #[tokio::test]
    async fn read_schema_encodes_bounds_and_is_closed() {
        let fixture = TestFixture::new();
        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let schema = tool.parameters_schema();

        assert_eq!(
            schema["additionalProperties"],
            serde_json::Value::Bool(false)
        );
        assert_eq!(schema["properties"]["offset"]["minimum"], 1);
        assert_eq!(schema["properties"]["limit"]["minimum"], 1);
        assert_eq!(schema["properties"]["limit"]["maximum"], MAX_READ_LINES);
        assert_eq!(schema["properties"]["depth"]["minimum"], 1);
        assert_eq!(schema["properties"]["depth"]["maximum"], MAX_TREE_DEPTH);
    }

    #[tokio::test]
    async fn test_read_binary_file() {
        let fixture = TestFixture::new();
        fixture.create_binary_file("binary.bin");

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "binary.bin"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Binary file detected"));
    }

    #[tokio::test]
    async fn test_read_image() {
        let fixture = TestFixture::new();
        fixture.create_image_file("image.png");

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "image.png"
            }))
            .await
            .unwrap();

        match result {
            ToolOutput::Image {
                mime_type,
                base64_data,
                description,
            } => {
                assert_eq!(mime_type, "image/png");
                assert!(!base64_data.is_empty());
                assert!(description.contains("image.png"));
            }
            _ => panic!("Expected Image output"),
        }
    }

    #[tokio::test]
    async fn test_read_nonexistent() {
        let fixture = TestFixture::new();

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "nonexistent.txt"
            }))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn read_wrong_path_suggests_a_near_existing_file() {
        let fixture = TestFixture::new();
        fixture.create_file("src/widgets/input.rs", "content");

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let err = tool
            .execute(json!({ "path": "src/widget/input.rs" }))
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("Path not found: src/widget/input.rs"), "{err}");
        assert!(err.contains("Did you mean:"), "{err}");
        assert!(err.contains("src/widgets/input.rs"), "{err}");
    }

    #[tokio::test]
    async fn test_read_tracking() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "content");
        let canonical_path = file_path.canonicalize().unwrap();

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        tool.execute(json!({
            "path": "test.txt"
        }))
        .await
        .unwrap();

        assert!(fixture.read_tracker.is_read(&canonical_path).await);
    }

    #[tokio::test]
    async fn test_read_image_marks_read() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_image_file("image.png");
        let canonical_path = file_path.canonicalize().unwrap();

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        tool.execute(json!({
            "path": "image.png"
        }))
        .await
        .unwrap();

        assert!(fixture.read_tracker.is_read(&canonical_path).await);
    }

    #[tokio::test]
    async fn test_read_invalid_offset_does_not_mark_read() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "content");
        let canonical_path = file_path.canonicalize().unwrap();

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "offset": 0
            }))
            .await;

        assert!(result.is_err());
        assert!(!fixture.read_tracker.is_read(&canonical_path).await);
    }

    #[tokio::test]
    async fn test_read_rejects_zero_limit() {
        let fixture = TestFixture::new();
        fixture.create_file("test.txt", "content");

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "limit": 0
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("limit"));
    }

    #[tokio::test]
    async fn test_read_truncated_notice() {
        let fixture = TestFixture::new();
        let content = (1..=301)
            .map(|i| format!("Line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        fixture.create_file("test.txt", &content);

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "limit": 5
            }))
            .await
            .unwrap();

        let (output, evidence) = read_output(result);

        assert!(output.contains("[File truncated"));
        assert!(output.contains("Total: 301 lines"));
        assert_eq!(evidence.coverage(), ReadCoverage::Partial);
    }

    #[tokio::test]
    async fn bounded_first_window_expands_to_small_whole_file() {
        let fixture = TestFixture::new();
        let content = (1..=250)
            .map(|line| format!("line {line}: compact"))
            .collect::<Vec<_>>()
            .join("\n");
        let path = fixture.create_file("compact.txt", &content);
        let canonical_path = path.canonicalize().unwrap();
        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        let (output, evidence) = read_output(
            tool.execute(json!({
                "path": "compact.txt",
                "offset": 1,
                "limit": 80
            }))
            .await
            .unwrap(),
        );

        assert!(output.contains("250: line 250: compact"), "{output}");
        assert!(!output.contains("[File truncated"), "{output}");
        assert_eq!(evidence.coverage(), ReadCoverage::Full);
        assert_eq!(evidence.window().requested_limit, 80);
        assert_eq!(evidence.window().end_line, Some(250));
        assert!(fixture.read_tracker.was_fully_read(&canonical_path).await);
    }

    #[tokio::test]
    async fn adaptive_first_window_keeps_large_files_windowed() {
        let fixture = TestFixture::new();
        let wide_content = (1..=250)
            .map(|line| format!("line {line}: {}", "x".repeat(80)))
            .collect::<Vec<_>>()
            .join("\n");
        let long_content = (1..=500)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        fixture.create_file("wide.txt", &wide_content);
        fixture.create_file("long.txt", &long_content);
        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        for path in ["wide.txt", "long.txt"] {
            let (output, evidence) = read_output(
                tool.execute(json!({ "path": path, "offset": 1, "limit": 80 }))
                    .await
                    .unwrap(),
            );
            assert!(output.contains("[File truncated"), "{path}: {output}");
            assert_eq!(evidence.coverage(), ReadCoverage::Partial, "{path}");
            assert_eq!(evidence.window().end_line, Some(80), "{path}");
        }
    }

    #[tokio::test]
    async fn read_clamps_oversized_line_limit() {
        let fixture = TestFixture::new();
        let content = (1..=MAX_READ_LINES + 20)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        fixture.create_file("long.txt", &content);

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "long.txt",
                "limit": MAX_READ_LINES + 500
            }))
            .await
            .unwrap();

        let (output, evidence) = read_output(result);

        assert!(output.contains(&format!("{MAX_READ_LINES}: line {MAX_READ_LINES}")));
        assert!(
            !output.contains(&format!(
                "{}: line {}",
                MAX_READ_LINES + 1,
                MAX_READ_LINES + 1
            )),
            "{output}"
        );
        assert!(output.contains("Use offset=1001 to read more"), "{output}");
        assert_eq!(evidence.coverage(), ReadCoverage::Partial);
        assert_eq!(evidence.window().requested_limit, MAX_READ_LINES + 500);
        assert_eq!(evidence.window().end_line, Some(MAX_READ_LINES));
    }

    #[tokio::test]
    async fn read_caps_model_visible_text_by_chars() {
        let fixture = TestFixture::new();
        let long_line = "x".repeat(1000);
        let content = (1..=80)
            .map(|line| format!("line {line} {long_line}"))
            .collect::<Vec<_>>()
            .join("\n");
        fixture.create_file("wide.txt", &content);

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "wide.txt",
                "limit": 80
            }))
            .await
            .unwrap();

        let (output, evidence) = read_output(result);

        assert!(output.contains("[Truncated: showing 30000 of"), "{output}");
        assert!(output.contains(READ_OUTPUT_CAP_HINT), "{output}");
        assert_eq!(evidence.coverage(), ReadCoverage::Partial);
        assert!(
            evidence.window().end_line.is_some_and(|line| line < 80),
            "{:?}",
            evidence.window()
        );
    }

    #[tokio::test]
    async fn test_read_huge_offset_does_not_panic() {
        let fixture = TestFixture::new();
        fixture.create_file("test.txt", "Line 1\nLine 2\nLine 3");

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "offset": usize::MAX,
                "limit": usize::MAX,
            }))
            .await
            .unwrap();

        let (output, evidence) = read_output(result);

        // An offset past EOF yields no lines, and crucially no overflow panic
        // from the previously unchecked `offset + limit` addition.
        assert!(output.is_empty());
        assert_eq!(evidence.coverage(), ReadCoverage::Partial);
        assert_eq!(evidence.window().end_line, None);
    }

    #[tokio::test]
    async fn large_single_line_file_is_paged_and_truncated() {
        // A file above the ceiling that is one enormous line (e.g. a minified
        // bundle) is no longer rejected — it pages, and the giant line is
        // truncated so it can't flood the window or exhaust memory.
        let fixture = TestFixture::new();
        let big = "a".repeat(MAX_READ_BYTES as usize + 1024);
        fixture.create_file("big.txt", &big);

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let output = match tool
            .execute(json!({ "path": "big.txt" }))
            .await
            .expect("a large single-line file should page, not reject")
        {
            ToolOutput::Text(text) | ToolOutput::Read { text, .. } => text,
            other => panic!("expected Text output, got {other:?}"),
        };

        assert!(
            output.starts_with("1: "),
            "the line is emitted with a number"
        );
        assert!(
            output.contains("[line truncated]"),
            "the giant line is truncated"
        );
        // Bounded well below the ~10 MiB on disk (per-line cap + marker).
        assert!(
            output.len() < LARGE_LINE_CHARS + 64,
            "output: {} bytes",
            output.len()
        );
    }

    #[tokio::test]
    async fn small_file_with_truncated_line_is_partial_read() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("long.txt", &"a".repeat(LARGE_LINE_CHARS + 1));
        let canonical_path = file_path.canonicalize().unwrap();

        let tool = ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({ "path": "long.txt" }))
            .await
            .expect("read should succeed");

        let (output, evidence) = read_output(result);
        assert!(output.contains("[line truncated]"), "{output}");
        assert_eq!(evidence.coverage(), ReadCoverage::Partial);
        assert!(
            !fixture.read_tracker.was_fully_read(&canonical_path).await,
            "a hidden long-line tail must not authorize overwrite"
        );
    }
}
