use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;

use crate::tool::{ReadCoverage, ReadEvidence, ReadTracker, ReadWindow, digest_content};

pub(crate) const MENTION_FILE_CAP_BYTES: usize = 64 * 1024;
const MENTION_TOTAL_CAP_BYTES: usize = 256 * 1024;
pub(crate) const MENTION_DIRECTORY_ENTRY_CAP: usize = 200;
const IMAGE_MENTION_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg"];

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedMention {
    raw: String,
    path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MentionPathKind {
    File,
    Directory,
}

#[derive(Debug)]
struct MentionPath {
    display_path: String,
    canonical_path: PathBuf,
    kind: MentionPathKind,
}

#[derive(Debug)]
struct MentionExpansion {
    sections: Vec<String>,
    warnings: Vec<String>,
    read_evidence: Vec<ReadEvidence>,
}

#[derive(Debug)]
pub(crate) struct MentionExpansionResult {
    pub(crate) text: String,
    pub(crate) read_evidence: Vec<ReadEvidence>,
}

#[cfg(test)]
pub(crate) async fn expand_mentions_for_context(
    project_root: &Path,
    read_tracker: &ReadTracker,
    text: &str,
) -> Result<String> {
    Ok(
        expand_mentions_for_context_with_evidence(project_root, read_tracker, text)
            .await?
            .text,
    )
}

pub(crate) async fn expand_mentions_for_context_with_evidence(
    project_root: &Path,
    read_tracker: &ReadTracker,
    text: &str,
) -> Result<MentionExpansionResult> {
    expand_mentions_for_context_inner(project_root, read_tracker, text, true).await
}

pub(crate) async fn preview_mentions_for_context(
    project_root: &Path,
    read_tracker: &ReadTracker,
    text: &str,
) -> String {
    expand_mentions_for_context_inner(project_root, read_tracker, text, false)
        .await
        .map(|result| result.text)
        .unwrap_or_else(|_err| text.to_string())
}

async fn expand_mentions_for_context_inner(
    project_root: &Path,
    read_tracker: &ReadTracker,
    text: &str,
    mark_reads: bool,
) -> Result<MentionExpansionResult> {
    let (mentions, mut warnings) = parse_mentions(text);
    if mentions.is_empty() {
        if warnings.is_empty() {
            return Ok(MentionExpansionResult {
                text: text.to_string(),
                read_evidence: Vec::new(),
            });
        }
        let expansion = MentionExpansion {
            sections: Vec::new(),
            warnings,
            read_evidence: Vec::new(),
        };
        return Ok(MentionExpansionResult {
            text: format_mention_context(text, &expansion),
            read_evidence: expansion.read_evidence,
        });
    }

    let canonical_root = tokio::fs::canonicalize(project_root)
        .await
        .with_context(|| {
            format!(
                "Failed to canonicalize project root {}",
                project_root.display()
            )
        })?;
    let mut expansion = MentionExpansion {
        sections: Vec::new(),
        warnings: Vec::new(),
        read_evidence: Vec::new(),
    };
    expansion.warnings.append(&mut warnings);
    let mut seen = HashSet::new();
    let mut total_file_bytes = 0usize;

    for mention in mentions {
        let mention_path =
            match validate_mention_path(&canonical_root, project_root, &mention).await {
                Ok(path) => path,
                Err(warning) => {
                    expansion.warnings.push(warning);
                    continue;
                }
            };
        if !seen.insert(mention_path.canonical_path.clone()) {
            expansion
                .warnings
                .push(format!("{}: duplicate mention skipped", mention.raw));
            continue;
        }

        match mention_path.kind {
            MentionPathKind::File => {
                match expand_file_mention(&mention, &mention_path, &mut total_file_bytes).await {
                    Ok(Some((section, evidence))) => {
                        if mark_reads {
                            if evidence.observation().coverage() == ReadCoverage::Full {
                                if let Some(file_digest) =
                                    evidence.observation().file_digest_at_read()
                                {
                                    read_tracker
                                        .mark_read_with_file_digest(
                                            &mention_path.canonical_path,
                                            file_digest,
                                            true,
                                        )
                                        .await;
                                } else {
                                    read_tracker.mark_read(&mention_path.canonical_path).await;
                                }
                            } else {
                                read_tracker
                                    .mark_read_partial(&mention_path.canonical_path)
                                    .await;
                            }
                        }
                        expansion.sections.push(section);
                        expansion.read_evidence.push(evidence);
                    }
                    Ok(None) => {}
                    Err(warning) => expansion.warnings.push(warning),
                }
            }
            MentionPathKind::Directory => {
                match expand_directory_mention(&mention, &mention_path).await {
                    Ok(section) => expansion.sections.push(section),
                    Err(warning) => expansion.warnings.push(warning),
                }
            }
        }
    }

    if expansion.sections.is_empty() && expansion.warnings.is_empty() {
        Ok(MentionExpansionResult {
            text: text.to_string(),
            read_evidence: expansion.read_evidence,
        })
    } else {
        Ok(MentionExpansionResult {
            text: format_mention_context(text, &expansion),
            read_evidence: expansion.read_evidence,
        })
    }
}

fn parse_mentions(text: &str) -> (Vec<ParsedMention>, Vec<String>) {
    let chars: Vec<char> = text.chars().collect();
    let mut mentions = Vec::new();
    let mut warnings = Vec::new();
    let mut idx = 0usize;
    while idx < chars.len() {
        if chars[idx] != '@' {
            idx += 1;
            continue;
        }
        if !is_mention_start_boundary(&chars, idx) {
            idx += 1;
            continue;
        }
        if chars.get(idx + 1) == Some(&'{') {
            let raw_start = idx;
            match parse_braced_mention(&chars, idx + 2) {
                BracedMention::Parsed { path, end } => {
                    let raw: String = chars[raw_start..end].iter().collect();
                    if path.trim().is_empty() {
                        warnings.push(format!("{raw}: empty mention ignored"));
                    } else {
                        mentions.push(ParsedMention { raw, path });
                    }
                    idx = end;
                }
                BracedMention::Malformed { raw, warning } => {
                    warnings.push(format!("{raw}: {warning}"));
                    idx = raw_start + raw.chars().count().max(1);
                }
            }
            continue;
        }

        let end = next_mention_token_end(&chars, idx + 1);
        if end == idx + 1 {
            warnings.push("@: empty mention ignored".to_string());
            idx += 1;
            continue;
        }
        let raw: String = chars[idx..end].iter().collect();
        let path: String = chars[idx + 1..end].iter().collect();
        mentions.push(ParsedMention { raw, path });
        idx = end;
    }
    (mentions, warnings)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BracedMention {
    Parsed { path: String, end: usize },
    Malformed { raw: String, warning: String },
}

fn parse_braced_mention(chars: &[char], start: usize) -> BracedMention {
    let mut path = String::new();
    let mut idx = start;
    while idx < chars.len() {
        match chars[idx] {
            '}' => {
                return BracedMention::Parsed { path, end: idx + 1 };
            }
            '\\' => {
                let Some(next) = chars.get(idx + 1).copied() else {
                    let raw: String = chars[start.saturating_sub(2)..].iter().collect();
                    return BracedMention::Malformed {
                        raw,
                        warning: "missing closing '}'".to_string(),
                    };
                };
                if matches!(next, '}' | '\\') {
                    path.push(next);
                    idx += 2;
                } else {
                    let raw: String = chars[start.saturating_sub(2)..=idx + 1].iter().collect();
                    return BracedMention::Malformed {
                        raw,
                        warning: format!("invalid escape '\\{next}'"),
                    };
                }
            }
            ch => {
                path.push(ch);
                idx += 1;
            }
        }
    }

    let raw: String = chars[start.saturating_sub(2)..].iter().collect();
    BracedMention::Malformed {
        raw,
        warning: "missing closing '}'".to_string(),
    }
}

fn next_mention_token_end(chars: &[char], start: usize) -> usize {
    let mut end = start;
    while end < chars.len() && !is_unbraced_mention_end_boundary(chars[end]) {
        end += 1;
    }
    end
}

fn is_mention_start_boundary(chars: &[char], idx: usize) -> bool {
    idx == 0
        || chars
            .get(idx - 1)
            .is_some_and(|ch| ch.is_whitespace() || is_opening_mention_boundary(*ch))
}

fn is_opening_mention_boundary(ch: char) -> bool {
    matches!(ch, '(' | '[' | '{' | '<' | '"' | '\'' | '`')
}

fn is_unbraced_mention_end_boundary(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '>' | '"' | '\'' | '`'
        )
}

async fn validate_mention_path(
    canonical_root: &Path,
    project_root: &Path,
    mention: &ParsedMention,
) -> std::result::Result<MentionPath, String> {
    let relative_path = normalize_workspace_relative_path(&mention.path)
        .map_err(|reason| format!("{}: {reason}", mention.raw))?;
    let joined = project_root.join(&relative_path);
    let canonical_path = tokio::fs::canonicalize(&joined)
        .await
        .map_err(|err| format!("{}: path not found ({err})", mention.raw))?;

    if !canonical_path.starts_with(canonical_root) {
        return Err(format!(
            "{}: symlink escape outside project root",
            mention.raw
        ));
    }
    let canonical_relative = canonical_path
        .strip_prefix(canonical_root)
        .map_err(|_err| format!("{}: path outside project root", mention.raw))?;
    if has_hidden_component(canonical_relative) {
        return Err(format!("{}: hidden paths cannot be injected", mention.raw));
    }

    let metadata = tokio::fs::metadata(&canonical_path)
        .await
        .map_err(|err| format!("{}: failed to stat path ({err})", mention.raw))?;
    let kind = if metadata.is_file() {
        MentionPathKind::File
    } else if metadata.is_dir() {
        MentionPathKind::Directory
    } else {
        return Err(format!("{}: unsupported path type", mention.raw));
    };

    Ok(MentionPath {
        display_path: relative_path.to_string_lossy().replace('\\', "/"),
        canonical_path,
        kind,
    })
}

fn normalize_workspace_relative_path(path: &str) -> std::result::Result<PathBuf, &'static str> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("empty mention ignored");
    }
    if trimmed.starts_with('~') {
        return Err("mentions must be workspace-relative");
    }

    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Err("mentions must be workspace-relative");
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => {
                if part.to_string_lossy().starts_with('.') {
                    return Err("hidden paths cannot be injected");
                }
                normalized.push(part);
            }
            Component::ParentDir => return Err("parent-directory components are not allowed"),
            Component::RootDir | Component::Prefix(_) => {
                return Err("mentions must be workspace-relative");
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        Err("empty mention ignored")
    } else {
        Ok(normalized)
    }
}

fn has_hidden_component(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(part) => part.to_string_lossy().starts_with('.'),
        _ => false,
    })
}

async fn expand_file_mention(
    mention: &ParsedMention,
    mention_path: &MentionPath,
    total_file_bytes: &mut usize,
) -> std::result::Result<Option<(String, ReadEvidence)>, String> {
    if is_image_path(&mention_path.canonical_path) {
        return Err(format!(
            "{}: skipped image file {}",
            mention.raw, mention_path.display_path
        ));
    }
    let remaining = MENTION_TOTAL_CAP_BYTES.saturating_sub(*total_file_bytes);
    if remaining == 0 {
        return Err(format!(
            "{}: skipped because total @-mention file cap is exhausted",
            mention.raw
        ));
    }
    let limit = MENTION_FILE_CAP_BYTES.min(remaining);
    let file = tokio::fs::File::open(&mention_path.canonical_path)
        .await
        .map_err(|err| format!("{}: failed to open file ({err})", mention.raw))?;
    let mut bytes = Vec::with_capacity(limit.saturating_add(1));
    file.take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|err| format!("{}: failed to read file ({err})", mention.raw))?;
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    if truncated {
        truncate_incomplete_utf8_tail(&mut bytes);
    }
    if bytes.contains(&0) {
        return Err(format!(
            "{}: skipped binary file {}",
            mention.raw, mention_path.display_path
        ));
    }
    let content = std::str::from_utf8(&bytes).map_err(|_err| {
        format!(
            "{}: skipped non-UTF-8 file {}",
            mention.raw, mention_path.display_path
        )
    })?;
    *total_file_bytes = total_file_bytes.saturating_add(bytes.len());

    let mut section = format!(
        "\n## {}\nFile: {} ({} bytes injected",
        mention.raw,
        mention_path.display_path,
        bytes.len()
    );
    if truncated {
        section.push_str("; truncated");
    }
    section.push_str(")\n");
    for (index, line) in content.lines().enumerate() {
        section.push_str(&format!("{}: {}\n", index + 1, line));
    }
    if content.is_empty() {
        section.push_str("(empty file)\n");
    }
    if truncated {
        section.push_str(&format!(
            "[warning] {} truncated at {} bytes.\n",
            mention_path.display_path, limit
        ));
    }
    let metadata = tokio::fs::metadata(&mention_path.canonical_path)
        .await
        .map_err(|err| format!("{}: failed to stat file ({err})", mention.raw))?;
    let visible_line_count = content.lines().count();
    let file_digest = (!truncated).then(|| digest_content(&bytes));
    let evidence = ReadEvidence::new(
        mention_path.display_path.clone(),
        mention_path.canonical_path.clone(),
        ReadWindow {
            requested_offset: 1,
            requested_limit: visible_line_count,
            start_line: 1,
            end_line: (visible_line_count > 0).then_some(visible_line_count),
            total_lines: (!truncated).then_some(visible_line_count),
        },
        if truncated {
            ReadCoverage::Partial
        } else {
            ReadCoverage::Full
        },
        &section,
        metadata.modified().ok(),
        metadata.len(),
        file_digest,
    );

    Ok(Some((section, evidence)))
}

fn truncate_incomplete_utf8_tail(bytes: &mut Vec<u8>) {
    if let Err(err) = std::str::from_utf8(bytes)
        && err.error_len().is_none()
    {
        bytes.truncate(err.valid_up_to());
    }
}

async fn expand_directory_mention(
    mention: &ParsedMention,
    mention_path: &MentionPath,
) -> std::result::Result<String, String> {
    let root = mention_path.canonical_path.clone();
    let display_path = mention_path.display_path.clone();
    let listing = tokio::task::spawn_blocking(move || collect_directory_listing(&root))
        .await
        .map_err(|err| format!("{}: directory listing task failed ({err})", mention.raw))?
        .map_err(|err| format!("{}: failed to list directory ({err})", mention.raw))?;

    let mut section = format!(
        "\n## {}\nDirectory: {}/ ({} entr{} shown",
        mention.raw,
        display_path.trim_end_matches('/'),
        listing.entries.len(),
        if listing.entries.len() == 1 {
            "y"
        } else {
            "ies"
        }
    );
    if listing.truncated {
        section.push_str("; truncated");
    }
    section.push_str(")\n");
    if listing.entries.is_empty() {
        section.push_str("(empty directory)\n");
    } else {
        for entry in listing.entries {
            section.push_str(&entry);
            section.push('\n');
        }
    }
    if listing.truncated {
        section.push_str(&format!(
            "[warning] directory listing capped at {} entries.\n",
            MENTION_DIRECTORY_ENTRY_CAP
        ));
    }
    Ok(section)
}

#[derive(Debug)]
struct DirectoryListing {
    entries: Vec<String>,
    truncated: bool,
}

fn collect_directory_listing(root: &Path) -> std::io::Result<DirectoryListing> {
    let mut entries = Vec::new();
    let truncated = collect_directory_listing_inner(root, root, &mut entries)?;
    Ok(DirectoryListing { entries, truncated })
}

fn collect_directory_listing_inner(
    root: &Path,
    current: &Path,
    entries: &mut Vec<String>,
) -> std::io::Result<bool> {
    if entries.len() >= MENTION_DIRECTORY_ENTRY_CAP {
        return Ok(true);
    }
    let mut children = std::fs::read_dir(current)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .collect::<Vec<_>>();
    children.sort_by(|left, right| {
        let left_type = left.file_type().ok();
        let right_type = right.file_type().ok();
        let left_dir = left_type.as_ref().is_some_and(std::fs::FileType::is_dir);
        let right_dir = right_type.as_ref().is_some_and(std::fs::FileType::is_dir);
        right_dir
            .cmp(&left_dir)
            .then_with(|| left.file_name().cmp(&right.file_name()))
    });

    for child in children {
        if entries.len() >= MENTION_DIRECTORY_ENTRY_CAP {
            return Ok(true);
        }
        let path = child.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        let is_dir = metadata.is_dir();
        let relative = path
            .strip_prefix(root)
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| child.file_name().to_string_lossy().to_string());
        if metadata.file_type().is_symlink() {
            entries.push(format!("{relative} -> symlink"));
            continue;
        }
        if is_dir {
            entries.push(format!("{relative}/"));
            if collect_directory_listing_inner(root, &path, entries)? {
                return Ok(true);
            }
        } else {
            entries.push(relative);
        }
    }
    Ok(false)
}

fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            IMAGE_MENTION_EXTENSIONS
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
        .unwrap_or(false)
}

fn format_mention_context(text: &str, expansion: &MentionExpansion) -> String {
    let mut expanded = text.to_string();
    expanded.push_str("\n\n# @-mention context\n");
    if !expansion.warnings.is_empty() {
        expanded.push_str("\n## Warnings\n");
        for warning in &expansion.warnings {
            expanded.push_str("- ");
            expanded.push_str(warning);
            expanded.push('\n');
        }
    }
    for section in &expansion.sections {
        expanded.push_str(section);
    }
    expanded
}
