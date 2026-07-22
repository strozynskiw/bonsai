use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use crate::diff::build_file_diff;
use crate::tool::edit_recovery;
use crate::tool::file_mutation::{
    FileMutationAction, FileMutationContext, FileMutationGuard, FileWriteRequest, ParentDirs,
    WriteContentContext, WritePrecondition, build_mutation_output, file_mutation_tool_ctors,
    noop_mutation_output, reject_replayed_elision_marker, write_effects,
};
use crate::tool::schema::{
    array_property, boolean_property, object, parse_args_with_schema, path_property,
    string_property,
};
use crate::tool::{ActionPlan, ActionPolicy, ReadTracker, Tool, ToolOutput, WorkspaceLockContext};
use crate::yolo::YoloMode;

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old_string: Option<String>,
    new_string: Option<String>,
    #[serde(default)]
    replace_all: bool,
    /// Optional list of find/replace regions applied in order, atomically.
    /// When present, the top-level `old_string`/`new_string`/`replace_all`
    /// are ignored. Each region is applied to the result of the previous one.
    edits: Option<Vec<EditRegion>>,
}

#[derive(Deserialize)]
struct EditRegion {
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

/// Top-level keys models reach for to pack a whole single edit into one ad-hoc
/// field instead of the schema's `old_string`/`new_string`. Seen in the wild:
/// `change` holding a `"replace: OLD -> NEW"` string. The replace tools (Cursor
/// et al.) also taught models `code_edit`/`patch`/`diff`/`edit`.
const EDIT_INSTRUCTION_ALIASES: [&str; 6] =
    ["change", "edit", "patch", "diff", "code_edit", "replace"];

/// Coerce the edit shapes the model actually emits into the canonical
/// `old_string`/`new_string` (or `edits` regions), in place, before parsing.
///
/// Two failure modes dominate in practice:
///   1. Each region written as a single string with an ad-hoc DSL instead of
///      `{"old_string": …, "new_string": …}` — inside the `edits` array.
///   2. The *whole* edit packed into one top-level ad-hoc field (`change`,
///      `edit`, …) instead of `old_string`/`new_string` at all.
///
/// The generic schema-driven repair pass ([`crate::tool::arg_repair`]) runs
/// first, at the dispatch layer: it already parses a region *stringified as
/// JSON* inside `edits` and passes the non-JSON DSL strings here untouched.
/// This coercer stays because only the edit tool can judge the DSL — the two
/// layers are deliberate, not redundant.
///
/// The DSL forms seen in the wild:
///   - `"replace: OLD -> NEW"` (and a bare `"OLD -> NEW"`)
///   - `"replace> NEW"` / `"new_string> NEW"` paired with a top-level `old_string`
///
/// We translate those so the call succeeds on the first try. The split is on the
/// *first* ` -> `, so NEW may itself contain arrows (closures, return types);
/// only OLD containing ` -> ` mis-splits, and that just truncates OLD into text
/// the file won't contain — caught by the exact-match guard in `apply_region` as
/// a clean "not found" error, never a silent wrong edit.
fn normalize_edit_args(args: &mut serde_json::Value) {
    let Some(obj) = args.as_object_mut() else {
        return;
    };
    let top_old = obj
        .get("old_string")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let top_new = obj
        .get("new_string")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);

    if let Some(edits) = obj
        .get_mut("edits")
        .and_then(serde_json::Value::as_array_mut)
    {
        for item in edits.iter_mut() {
            // Unwrap `{"$text": "replace: …"}` / `{"text": "…"}` — models wrap
            // the DSL string in a one-field object (seen live).
            // Reduced to a plain string here, it either coerces below or gets
            // the targeted leftover-string guidance instead of an opaque serde
            // schema error.
            if let Some(raw) = dsl_string_wrapped_in_object(item) {
                *item = serde_json::Value::String(raw);
            }
            if let serde_json::Value::String(raw) = item
                && let Some(region) =
                    edit_region_from_dsl(raw, top_old.as_deref(), top_new.as_deref())
            {
                *item = region;
            }
        }
        return;
    }

    normalize_instruction_alias(obj, top_old.as_deref(), top_new.as_deref());
}

/// A one-string-field object (`{"$text": …}`, `{"text": …}`, `{"value": …}`)
/// standing in for a DSL string inside an `edits` array. Anything carrying
/// canonical keys is left alone.
fn dsl_string_wrapped_in_object(item: &serde_json::Value) -> Option<String> {
    let map = item.as_object()?;
    if map.len() != 1 {
        return None;
    }
    let (key, value) = map.iter().next()?;
    if !matches!(key.as_str(), "$text" | "text" | "value") {
        return None;
    }
    value.as_str().map(str::to_string)
}

/// Lift a single edit the model placed under an ad-hoc top-level key (see
/// [`EDIT_INSTRUCTION_ALIASES`]) into the canonical `old_string`/`new_string`.
/// Only runs when neither canonical field is present, so a real call is never
/// disturbed; leaves an unrecognized value in place so the schema-guided error
/// can steer the model.
fn normalize_instruction_alias(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    top_old: Option<&str>,
    top_new: Option<&str>,
) {
    if obj.contains_key("old_string") && obj.contains_key("new_string") {
        return;
    }
    let Some(key) = EDIT_INSTRUCTION_ALIASES
        .into_iter()
        .find(|key| obj.contains_key(*key))
    else {
        return;
    };
    let Some((old, new)) = alias_old_new(&obj[key], top_old, top_new) else {
        return;
    };
    obj.remove(key);
    obj.insert("old_string".to_string(), old);
    obj.insert("new_string".to_string(), new);
}

/// Extract `(old_string, new_string)` from an alias field's value: either a DSL
/// string (`"replace: OLD -> NEW"`) or an object that already carries the
/// canonical pair. `None` when there isn't enough to act on.
fn alias_old_new(
    value: &serde_json::Value,
    top_old: Option<&str>,
    top_new: Option<&str>,
) -> Option<(serde_json::Value, serde_json::Value)> {
    match value {
        serde_json::Value::String(raw) => {
            let region = edit_region_from_dsl(raw, top_old, top_new)?;
            Some((
                region.get("old_string")?.clone(),
                region.get("new_string")?.clone(),
            ))
        }
        serde_json::Value::Object(map) => {
            let old = map.get("old_string")?;
            let new = map.get("new_string")?;
            (old.is_string() && new.is_string()).then(|| (old.clone(), new.clone()))
        }
        _ => None,
    }
}

/// After [`normalize_edit_args`], any string still sitting in `edits` is a
/// region we could not parse — the model wrote free-form text (commonly a
/// `replace> NEW` chunk with no anchor) instead of `{old_string, new_string}`.
/// Surface that as targeted guidance rather than letting serde reject the whole
/// call with an opaque "failed to parse" blob the model can't act on.
fn reject_uncoerced_edits(args: &serde_json::Value) -> Result<()> {
    let Some(edits) = args.get("edits").and_then(serde_json::Value::as_array) else {
        return Ok(());
    };
    for (i, item) in edits.iter().enumerate() {
        if let Some(text) = item.as_str() {
            anyhow::bail!("edits[{i}]: {}", dsl_leftover_guidance(text));
        }
    }
    Ok(())
}

/// Reject a leftover instruction-alias field (`change`/`edit`/`patch`/…) that
/// [`normalize_instruction_alias`] could not lift into `old_string`/`new_string`
/// — with guidance naming the *specific* problem. The generic "old_string is
/// required" error never mentioned the alias shape, and a model repeated the
/// exact same unusable call 12 times in one session (137) because nothing told
/// it what was wrong with it.
fn reject_unusable_alias(args: &serde_json::Value) -> Result<()> {
    let Some(obj) = args.as_object() else {
        return Ok(());
    };
    if obj.contains_key("old_string") || obj.contains_key("edits") {
        return Ok(());
    }
    for key in EDIT_INSTRUCTION_ALIASES {
        if let Some(text) = obj.get(key).and_then(serde_json::Value::as_str) {
            anyhow::bail!("'{key}': {}", dsl_leftover_guidance(text));
        }
    }
    Ok(())
}

/// Diagnose why a DSL-ish string couldn't become an edit region, precisely.
fn dsl_leftover_guidance(raw: &str) -> String {
    let body = strip_edit_directive(raw).body;
    if body.contains("\\n") {
        format!(
            "the one-line shorthand cannot express a multi-line edit — the value contains \
             literal \\n text, which would be written into the file as backslash-n characters, \
             not newlines. Send the edit as two separate JSON string fields with REAL newlines \
             in the JSON strings: {{\"old_string\": \"exact existing text\", \"new_string\": \
             \"replacement\"}}. Value: {}",
            short_preview(raw)
        )
    } else if body.matches(" -> ").count() > 1 {
        format!(
            "the \"OLD -> NEW\" shorthand is ambiguous here — the value contains \" -> \" more \
             than once (code with `->` arrows cannot use the shorthand). Send the edit as two \
             separate JSON string fields instead: {{\"old_string\": \"exact existing text\", \
             \"new_string\": \"replacement\"}}. Value: {}",
            short_preview(raw)
        )
    } else {
        format!(
            "this gives only one side of the edit — text to find with no replacement (or vice \
             versa). Send both sides as separate JSON string fields: {{\"old_string\": \"exact \
             existing text\", \"new_string\": \"replacement\"}}. Value: {}",
            short_preview(raw)
        )
    }
}

/// One-line, length-bounded echo of an offending value so the model can spot
/// which entry it was without flooding context.
fn short_preview(text: &str) -> String {
    const MAX_CHARS: usize = 60;
    let oneline = text.replace('\n', "⏎");
    match oneline.char_indices().nth(MAX_CHARS) {
        Some((byte_idx, _)) => format!("{}…", &oneline[..byte_idx]),
        None => oneline,
    }
}

/// Parse one model-authored edit string into a `{old_string, new_string}` object,
/// or `None` when there isn't enough information (left as-is so the schema error
/// can guide the model to the object form).
fn edit_region_from_dsl(
    raw: &str,
    top_old: Option<&str>,
    top_new: Option<&str>,
) -> Option<serde_json::Value> {
    let stripped = strip_edit_directive(raw);
    let body = stripped.body;
    // More than one " -> " is ambiguous — Rust return arrows inside the old
    // text made first-arrow splits silently truncate old_string into text the
    // file doesn't contain (observed as a run of
    // baffling not-found errors). Refuse to guess; the rejection path names the problem.
    if body.matches(" -> ").count() > 1 {
        return None;
    }
    // A literal backslash-n means the model tried to express a MULTI-LINE edit
    // in this one-line shorthand, which cannot encode newlines. Coercing it
    // writes the `\n` text into the file verbatim (observed live: a coerced
    // edit corrupted a file exactly this way — fuzzy-apply matched the collapsed
    // old text, then inserted a new_string full of literal `\n`). Refuse to
    // coerce; the leftover-string guidance tells the model to send real
    // old_string/new_string JSON with real newlines instead.
    if body.contains("\\n") {
        return None;
    }
    if let Some((old, new)) = body.split_once(" -> ") {
        return Some(serde_json::json!({ "old_string": old, "new_string": new }));
    }
    match stripped.side {
        EditDslSide::Old => {
            let new = top_new?;
            Some(serde_json::json!({ "old_string": body, "new_string": new }))
        }
        EditDslSide::New => {
            let old = top_old?;
            Some(serde_json::json!({ "old_string": old, "new_string": body }))
        }
        EditDslSide::Unknown => {
            if let Some(old) = top_old {
                Some(serde_json::json!({ "old_string": old, "new_string": body }))
            } else {
                let new = top_new?;
                Some(serde_json::json!({ "old_string": body, "new_string": new }))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditDslSide {
    Old,
    New,
    Unknown,
}

#[derive(Debug, Clone, Copy)]
struct StrippedEditDirective<'a> {
    body: &'a str,
    side: EditDslSide,
}

/// Strip a leading `replace`/`new_string`/`new` directive. Colon directives
/// (`replace:`) use `: ` as the delimiter, so one following space is dropped;
/// angle directives (`replace>`) attach directly to the content, so following
/// whitespace — frequently significant indentation — is preserved exactly.
fn strip_edit_directive(raw: &str) -> StrippedEditDirective<'_> {
    for prefix in ["old_string:", "old:", "find:"] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            return StrippedEditDirective {
                body: rest.strip_prefix(' ').unwrap_or(rest),
                side: EditDslSide::Old,
            };
        }
    }
    for prefix in ["new_string:", "new:"] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            return StrippedEditDirective {
                body: rest.strip_prefix(' ').unwrap_or(rest),
                side: EditDslSide::New,
            };
        }
    }
    if let Some(rest) = raw.strip_prefix("replace:") {
        return StrippedEditDirective {
            body: rest.strip_prefix(' ').unwrap_or(rest),
            side: EditDslSide::Unknown,
        };
    }
    for prefix in ["old_string>", "old>", "find>"] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            return StrippedEditDirective {
                body: rest,
                side: EditDslSide::Old,
            };
        }
    }
    for prefix in ["new_string>", "new>"] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            return StrippedEditDirective {
                body: rest,
                side: EditDslSide::New,
            };
        }
    }
    if let Some(rest) = raw.strip_prefix("replace>") {
        return StrippedEditDirective {
            body: rest,
            side: EditDslSide::Unknown,
        };
    }
    StrippedEditDirective {
        body: raw,
        side: EditDslSide::Unknown,
    }
}

pub struct EditTool {
    ctx: FileMutationContext,
}

file_mutation_tool_ctors!(EditTool);

#[async_trait]
impl Tool for EditTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::SelfAuthorized
    }

    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing exact text; read the file first. For one change, give `old_string` (exact text to find) and `new_string` (replacement); for several, give an `edits` array of {old_string, new_string} regions applied atomically. Set `replace_all` to replace every occurrence. Use empty new_string to delete content. To insert or append text, anchor on an existing line: include it in old_string and repeat it plus your new text in new_string. To create a new file, use the write tool."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object(
            [
                (
                    "path",
                    path_property("File path relative to project root, or absolute path inside it"),
                ),
                (
                    "old_string",
                    string_property(
                        "The exact existing text to find and replace; required for a single edit unless you use `edits`. Leave empty only to create a new file. Ignored when `edits` is provided.",
                    ),
                ),
                (
                    "new_string",
                    string_property(
                        "The replacement text; required for a single edit unless you use `edits`. Leave empty to delete the old_string from the file. Ignored when `edits` is provided.",
                    ),
                ),
                (
                    "replace_all",
                    boolean_property(
                        "Replace all occurrences of old_string (default: false, replaces only the first). Ignored when `edits` is provided.",
                    ),
                ),
                (
                    "edits",
                    array_property(
                        "Multiple find/replace regions applied in order, atomically. Each region is an object {old_string, new_string}; it may also be written as a single \"old -> new\" string. When provided, the top-level old_string/new_string/replace_all are ignored.",
                        object(
                            [
                                (
                                    "old_string",
                                    string_property("Exact text to find (required)"),
                                ),
                                (
                                    "new_string",
                                    string_property("Replacement text, empty to delete (required)"),
                                ),
                                (
                                    "replace_all",
                                    boolean_property("Replace all occurrences (default: false)"),
                                ),
                            ],
                            &["old_string", "new_string"],
                        ),
                    ),
                ),
            ],
            &["path"],
        )
    }

    async fn execute(&self, mut args: serde_json::Value) -> Result<ToolOutput> {
        normalize_edit_args(&mut args);
        reject_uncoerced_edits(&args)?;
        reject_unusable_alias(&args)?;
        let schema = self.parameters_schema();
        let args: EditArgs = parse_args_with_schema("edit tool", args, Some(&schema))?;
        let guard = self.ctx.guard(FileMutationAction::Edit);
        let workspace_lock = guard.acquire_write_lock().await?;
        let yolo_enabled = guard.yolo_enabled();
        let resolved_path = guard.resolve_path(&args.path)?;

        // The two mutually-exclusive edit shapes, resolved once so the invariant
        // is type-enforced rather than re-asserted with an `expect()` downstream:
        // a multi-region `edits` array, or a single top-level old/new pair. Only
        // holds shared refs into `args`, so it is `Copy`.
        #[derive(Clone, Copy)]
        enum EditMode<'a> {
            Multi(&'a [EditRegion]),
            Single { old: &'a str, new: &'a str },
        }

        let mode = match args.edits.as_deref() {
            Some([]) => anyhow::bail!("edits must contain at least one region"),
            Some(regions) => EditMode::Multi(regions),
            // The single-edit form: validate and bind old/new once, with
            // intent-specific guidance when either is missing — omitting
            // `old_string` is the most common edit-tool failure.
            None => {
                let old_string = args.old_string.as_deref();
                let new_string = args.new_string.as_deref();
                let (Some(old_string), Some(new_string)) = (old_string, new_string) else {
                    return Err(missing_single_edit_field_error(old_string, new_string));
                };

                // Create-file path: only when the top-level old_string is
                // explicitly present and empty.
                if old_string.is_empty() {
                    return self
                        .create_file(
                            &guard,
                            workspace_lock.as_ref(),
                            CreateFileTarget {
                                path: &resolved_path.path,
                                project_relative_path: resolved_path
                                    .project_relative_path
                                    .as_deref(),
                                canonical_path: resolved_path.canonical_path.as_deref(),
                                mutation_target: &resolved_path.mutation_target,
                            },
                            &args.path,
                            new_string,
                            OverwriteExisting::from_yolo(yolo_enabled),
                        )
                        .await;
                }
                EditMode::Single {
                    old: old_string,
                    new: new_string,
                }
            }
        };

        let canonical = resolved_path
            .canonical_path
            .as_ref()
            .with_context(|| format!("File not found: {}", args.path))?;

        let old_content = guard.read_existing_content(canonical, &args.path).await?;

        let new_content = match mode {
            EditMode::Multi(regions) => {
                let mut content = old_content.clone();
                for (i, region) in regions.iter().enumerate() {
                    if region.old_string.is_empty() {
                        anyhow::bail!(
                            "edits[{i}].old_string is empty. Empty old_string is only valid at the top level to create a new file, not inside the `edits` array."
                        );
                    }
                    content = apply_region(
                        &content,
                        &args.path,
                        &region.old_string,
                        &region.new_string,
                        region.replace_all,
                        OnAmbiguous::from_yolo(yolo_enabled),
                    )?;
                }
                content
            }
            EditMode::Single { old, new } => apply_region(
                &old_content,
                &args.path,
                old,
                new,
                args.replace_all,
                OnAmbiguous::from_yolo(yolo_enabled),
            )?,
        };

        if old_content == new_content {
            return Ok(noop_mutation_output(&args.path));
        }

        let effects = write_effects(Some(&old_content), &new_content);
        let plan = ActionPlan::file_mutation(
            "edit",
            std::iter::once(&resolved_path.mutation_target),
            effects,
        );
        let diff = build_file_diff(args.path.clone(), Some(&old_content), &new_content);
        guard.authorize(&plan).await?;

        let write = FileWriteRequest::new(
            canonical,
            &new_content,
            WritePrecondition::Exact(&old_content),
            WriteContentContext::WriteFile,
            ParentDirs::Keep,
        )
        .with_workspace_lock(workspace_lock.as_ref())
        .with_project_relative_path(resolved_path.project_relative_path.as_deref())
        .with_read_tracking_path(Some(canonical));
        guard
            .write_content("edit", &args.path, &diff, write)
            .await?;

        let region_count = match mode {
            EditMode::Multi(regions) => regions.len(),
            EditMode::Single { .. } => 0,
        };
        let action = if new_content.is_empty() {
            "emptied"
        } else {
            "edited"
        };

        let output = build_mutation_output(diff, |stats| {
            if region_count > 1 {
                format!(
                    "File {} successfully: {} ({} regions)\nLines added: {}, Lines removed: {}",
                    action, args.path, region_count, stats.added, stats.removed
                )
            } else {
                format!(
                    "File {} successfully: {}\nLines added: {}, Lines removed: {}",
                    action, args.path, stats.added, stats.removed
                )
            }
        });
        // Show the post-edit state of the changed regions so the model can verify
        // the edit landed without spending a round-trip to re-read the file.
        Ok(with_post_edit_excerpt(output))
    }
}

const POST_EDIT_EXCERPT_MAX_LINES: usize = 40;
const POST_EDIT_EXCERPT_MAX_CHARS: usize = 2_000;

/// Append a compact, line-numbered view of the file *after* the edit (the
/// changed regions only) to an [`ToolOutput::Edit`] summary. This gives the
/// model the resulting content directly, removing the usual "re-read to confirm
/// the edit" round-trip. Pass-through for any other output.
fn with_post_edit_excerpt(output: ToolOutput) -> ToolOutput {
    let ToolOutput::Edit { summary, diff } = output else {
        return output;
    };
    let summary = match post_edit_excerpt(&diff) {
        Some(excerpt) => format!("{summary}\n\n{excerpt}"),
        None => summary,
    };
    ToolOutput::Edit { summary, diff }
}

/// Render the post-edit content of each changed hunk (context + added lines,
/// with their resulting line numbers), capped so a large edit can't flood the
/// context. Returns `None` when there is nothing renderable.
fn post_edit_excerpt(diff: &crate::diff::FileDiff) -> Option<String> {
    use crate::diff::DiffLineKind;
    let mut body = String::new();
    let mut lines_used = 0usize;
    let mut truncated = diff.truncated;
    for (hunk_index, hunk) in diff.hunks.iter().enumerate() {
        if lines_used >= POST_EDIT_EXCERPT_MAX_LINES || body.len() >= POST_EDIT_EXCERPT_MAX_CHARS {
            truncated = true;
            break;
        }
        if hunk_index > 0 {
            body.push_str("  …\n");
        }
        for line in &hunk.lines {
            if matches!(line.kind, DiffLineKind::Removed) {
                continue;
            }
            let Some(new_line) = line.new_line else {
                continue;
            };
            if lines_used >= POST_EDIT_EXCERPT_MAX_LINES
                || body.len() >= POST_EDIT_EXCERPT_MAX_CHARS
            {
                truncated = true;
                break;
            }
            body.push_str(&format!("  {new_line}: {}\n", line.content));
            lines_used += 1;
        }
    }
    if body.trim().is_empty() {
        return None;
    }
    let mut excerpt = format!("Result after edit (changed regions):\n{body}");
    if truncated {
        excerpt.push_str("  … (excerpt truncated; read the file for the full view)\n");
    }
    Some(excerpt.trim_end().to_string())
}

/// How [`apply_region`] reacts when `old_string` matches more than once and
/// `replace_all` is not set. Derived from whether yolo mode is enabled.
#[derive(Debug, Clone, Copy)]
enum OnAmbiguous {
    /// Reject the edit and ask for a more specific `old_string` (default).
    Reject,
    /// Proceed using the first match instead of failing (yolo mode).
    Allow,
}

impl OnAmbiguous {
    fn from_yolo(yolo_enabled: bool) -> Self {
        if yolo_enabled {
            Self::Allow
        } else {
            Self::Reject
        }
    }
}

/// Apply a single find/replace region to `content`, returning the new text.
/// Errors if `old_string` is absent or ambiguous (unless `replace_all`).
fn apply_region(
    content: &str,
    args_path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    on_ambiguous: OnAmbiguous,
) -> Result<String> {
    // A replayed context-elision placeholder as the replacement would inject
    // marker garbage instead of code — same trap as in the write tool.
    if !new_string.is_empty() {
        reject_replayed_elision_marker(new_string, args_path)?;
    }
    let occurrences = content.matches(old_string).count();
    if occurrences == 0 {
        // Indentation/trailing-whitespace drifted but the block is structurally
        // intact (same line count, each line trimmed-equal): auto-apply the
        // UNIQUE match, re-anchored to the file's indentation, instead of
        // bouncing the model through a re-read. This is the standard fuzzy-apply
        // behavior; the unique-match gate means it never edits an unintended
        // region, and it deliberately does
        // NOT fire for the newline-collapsed one-line shorthand handled below
        // (collapsing lines back would corrupt structure — that keeps its
        // verbatim-text hint so the model retries with real newlines).
        if let Some(applied) =
            edit_recovery::line_trimmed_reindented_apply(content, old_string, new_string)
        {
            return Ok(applied);
        }
        // Whitespace-flattened old_string (newlines collapsed to spaces —
        // the one-line shorthand form does this) still identifies its region
        // unambiguously by token sequence. Hand back the verbatim on-disk text
        // so the retry can match exactly, instead of a fuzzy guess.
        let spans = edit_recovery::whitespace_insensitive_spans(content, old_string);
        if let [(start, end)] = spans.as_slice() {
            // Expand to whole lines so the echoed text carries the true leading
            // indentation — it stays a literal substring of the file, so the
            // model can use it as old_string verbatim.
            let line_start = content[..*start].rfind('\n').map(|i| i + 1).unwrap_or(0);
            let line_end = content[*end..]
                .find('\n')
                .map(|i| *end + i)
                .unwrap_or(content.len());
            let verbatim = &content[line_start..line_end];
            let line = edit_recovery::line_of_byte(content, line_start);
            let shown = match verbatim.char_indices().nth(2_400) {
                Some((byte_idx, _)) => format!("{}…", &verbatim[..byte_idx]),
                None => verbatim.to_string(),
            };
            anyhow::bail!(
                "old_string not found in file '{args_path}' — but it matches the region at line \
                 {line} once all whitespace is ignored. Your old_string has newlines/indentation \
                 collapsed (the one-line \"OLD -> NEW\" shorthand does this). Retry with the exact \
                 on-disk text as old_string, preserving every newline and space:\n\n{shown}"
            );
        }
        if spans.len() > 1 {
            let lines = spans
                .iter()
                .map(|(start, _)| edit_recovery::line_of_byte(content, *start).to_string())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "old_string not found in file '{args_path}' — ignoring whitespace it matches {} \
                 regions (lines {lines}). Your old_string has newlines collapsed; retry with the \
                 exact on-disk text of the one region you mean, preserving every newline and space.",
                spans.len()
            );
        }
        let mut msg = format!("old_string not found in file '{args_path}'.");
        // Already-applied guard: no `old_string`, but the file already contains
        // your `new_string` (or, for a deletion, the target text is simply
        // gone) — the change looks already done. Say so plainly so the model
        // continues instead of re-reading the file in circles to "re-check".
        // This is common when a task overlaps existing uncommitted work
        // (observed live: the target files were already edited by other in-flight
        // work, so its edits missed and it looped re-reading rather than moving
        // on). The read now returns real bytes, but the model still needs a
        // clear "done, move on" signal to stop re-orienting.
        if edit_appears_already_applied(content, old_string, new_string) {
            msg.push_str(
                "\nThe file already contains your new_string and not your old_string, so this change appears already applied. Treat it as done and move on to the next change — do not re-read the file to re-verify.",
            );
        } else {
            match edit_recovery::nearest_match_hint(content, old_string) {
                Some(hint) => {
                    msg.push_str("\n\n");
                    msg.push_str(&hint);
                }
                None => msg.push_str(
                    "\nMake sure it matches exactly, including whitespace and line breaks.",
                ),
            }
        }
        anyhow::bail!("{msg}");
    }

    if !replace_all && occurrences > 1 && matches!(on_ambiguous, OnAmbiguous::Reject) {
        anyhow::bail!(
            "old_string appears {} times in file '{}' (lines {}).\nAdd surrounding context to target one occurrence, set replace_all to true to change all, or use an `edits` array.",
            occurrences,
            args_path,
            edit_recovery::occurrence_lines_label(content, old_string),
        );
    }

    if replace_all {
        Ok(content.replace(old_string, new_string))
    } else {
        let index = content.find(old_string).context("index vanished")?;
        Ok(format!(
            "{}{}{}",
            &content[..index],
            new_string,
            &content[index + old_string.len()..]
        ))
    }
}

/// Heuristic for a no-op edit whose result is already present: `old_string` is
/// absent but the `new_string` already appears (or, for a deletion, the target
/// text is simply gone). Conservative — a non-deletion match requires a
/// multi-line or reasonably long `new_string` so a short common replacement
/// (e.g. `});`) can't spuriously read as "already applied".
fn edit_appears_already_applied(content: &str, old_string: &str, new_string: &str) -> bool {
    if content.contains(old_string) {
        return false;
    }
    let trimmed = new_string.trim();
    if trimmed.is_empty() {
        // Deletion whose target text is already gone.
        return true;
    }
    // Distinctive enough that finding it in the file is real evidence the change
    // landed, not a coincidental substring — guards against short punctuation
    // replacements like `}` or `});` reading as "already applied" (a false
    // positive would wrongly tell the model to skip a real edit).
    let distinctive = trimmed.chars().count() >= 8
        && trimmed.chars().filter(|c| c.is_alphanumeric()).count() >= 4;
    distinctive && content.contains(trimmed)
}

/// Build an intent-specific error for the single-edit form (no `edits` array)
/// when `old_string` and/or `new_string` is missing. Called only when at least
/// one is absent. Models most often omit `old_string` while trying to append or
/// while confusing `edit` with `write`; each arm names the exact field to add
/// plus the recipe for the likely intent. Keeps the legacy "<field> is required"
/// wording so prior guidance (and tests) still match.
fn missing_single_edit_field_error(
    old_string: Option<&str>,
    new_string: Option<&str>,
) -> anyhow::Error {
    match (old_string, new_string) {
        (None, Some(_)) => anyhow::anyhow!(
            "old_string is required when edits is not provided — it is the exact existing text to find and replace, and you gave only new_string.\n\
             - To replace text: set old_string to the exact text to find.\n\
             - To insert or append: set old_string to a unique existing line near the target and repeat it unchanged at the start of new_string.\n\
             - To create a new file: use the write tool instead."
        ),
        (Some(_), None) => anyhow::anyhow!(
            "new_string is required when edits is not provided. To delete the matched text, set new_string to \"\" (an empty string)."
        ),
        // Neither field provided (e.g. only `path`, or the wrong field names).
        _ => anyhow::anyhow!(
            "old_string is required when edits is not provided. For a single edit, give old_string (exact text to find) and new_string (replacement); for several edits, give an edits array of {{old_string, new_string}} regions. To create a new file, use the write tool."
        ),
    }
}

/// Whether the create-file path may overwrite a file that already exists.
/// Derived from whether yolo mode is enabled.
#[derive(Debug, Clone, Copy)]
enum OverwriteExisting {
    /// Overwrite the existing file (yolo mode).
    Allow,
    /// Refuse and tell the caller to use old_string/new_string instead (default).
    Reject,
}

#[derive(Debug, Clone, Copy)]
struct CreateFileTarget<'a> {
    path: &'a Path,
    project_relative_path: Option<&'a Path>,
    canonical_path: Option<&'a Path>,
    mutation_target: &'a crate::tool::MutationTarget,
}

impl OverwriteExisting {
    fn from_yolo(yolo_enabled: bool) -> Self {
        if yolo_enabled {
            Self::Allow
        } else {
            Self::Reject
        }
    }
}

impl EditTool {
    async fn create_file(
        &self,
        guard: &FileMutationGuard<'_>,
        workspace_lock: Option<&crate::storage::WorkspaceLockGuard>,
        target: CreateFileTarget<'_>,
        args_path: &str,
        content: &str,
        overwrite: OverwriteExisting,
    ) -> Result<ToolOutput> {
        let write_path = target.canonical_path.unwrap_or(target.path);
        let old_content = if target.path.exists() || target.canonical_path.is_some() {
            if let Some(canonical) = target.canonical_path {
                guard.ensure_not_directory(canonical, args_path)?;
            }
            if matches!(overwrite, OverwriteExisting::Reject) {
                anyhow::bail!(
                    "File '{}' already exists. Use old_string and new_string to edit existing files.",
                    args_path
                );
            }
            Some(guard.read_existing_content(write_path, args_path).await?)
        } else {
            None
        };

        let parent_dirs = if old_content.is_none() {
            ParentDirs::Create
        } else {
            ParentDirs::Keep
        };
        let effects = write_effects(old_content.as_deref(), content);
        let plan =
            ActionPlan::file_mutation("edit", std::iter::once(target.mutation_target), effects);
        let diff = build_file_diff(args_path.to_string(), old_content.as_deref(), content);
        guard.authorize(&plan).await?;
        let write = FileWriteRequest::new(
            write_path,
            content,
            old_content
                .as_deref()
                .map_or(WritePrecondition::Absent, WritePrecondition::Exact),
            WriteContentContext::CreateFile,
            parent_dirs,
        )
        .with_workspace_lock(workspace_lock)
        .with_project_relative_path(target.project_relative_path)
        .with_read_tracking_path(target.canonical_path);
        guard.write_content("edit", args_path, &diff, write).await?;

        let action = if old_content.is_some() {
            "overwritten"
        } else {
            "created"
        };
        Ok(build_mutation_output(diff, |stats| {
            format!(
                "File {action} successfully: {}\nLines added: {}, Lines removed: {}",
                args_path, stats.added, stats.removed
            )
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::{
        Config, ConfigSource, FailureBehavior, HookAction, HookDef, HookEvent, HookMatcher,
    };
    use crate::extension::status::ExtensionRegistry;
    use crate::interaction::InteractionService;
    use crate::permissions::PermissionManager;
    use crate::tool::test_utils::TestFixture;
    use crate::yolo::YoloMode;
    use serde_json::json;

    fn balanced_tool(fixture: &TestFixture) -> EditTool {
        let yolo_mode = YoloMode::with_level(crate::tool::ApprovalLevel::Balanced);
        let policy = ActionPolicy::new(
            PermissionManager::memory_only(),
            Arc::new(InteractionService::noninteractive()),
            yolo_mode.clone(),
        );
        EditTool::with_hooks_and_locks(
            fixture.project_root.clone(),
            fixture.read_tracker.clone(),
            yolo_mode,
            Arc::new(crate::hooks::HookEngine::disabled()),
            WorkspaceLockContext::disabled(fixture.project_root.clone()),
            policy,
        )
    }

    // `normalize_edit_args` coerces the string DSL the model actually emits
    // (mined from real failed calls) into the canonical region objects.

    fn normalized(value: serde_json::Value) -> serde_json::Value {
        let mut value = value;
        normalize_edit_args(&mut value);
        value
    }

    #[test]
    fn dsl_replace_colon_arrow_splits_old_and_new() {
        let out = normalized(json!({"path": "f.rs", "edits": ["replace: OLD -> NEW"]}));
        assert_eq!(out["edits"][0]["old_string"], "OLD");
        assert_eq!(out["edits"][0]["new_string"], "NEW");
    }

    #[test]
    fn dsl_bare_arrow_without_directive_splits() {
        let out = normalized(json!({"path": "f.rs", "edits": ["foo() -> bar()"]}));
        assert_eq!(out["edits"][0]["old_string"], "foo()");
        assert_eq!(out["edits"][0]["new_string"], "bar()");
    }

    #[test]
    fn dsl_with_multiple_arrows_is_left_for_targeted_guidance() {
        // A first-arrow split silently truncated old_string whenever the OLD
        // side contained a Rust `->` (observed live: a run of baffling not-found
        // errors). Multiple arrows are ambiguous — refuse to guess and let the
        // rejection path steer to explicit old_string/new_string fields.
        let out = normalized(json!({"path": "f.rs", "edits": ["replace: cb -> |x| -> u8 { x }"]}));
        assert!(
            out["edits"][0].is_string(),
            "ambiguous DSL must stay uncoerced: {out}"
        );
        let err = reject_uncoerced_edits(&out).expect_err("leftover string must be rejected");
        assert!(err.to_string().contains("more than once"), "{err:#}");
        assert!(err.to_string().contains("old_string"), "{err:#}");
    }

    #[test]
    fn dsl_string_wrapped_in_text_object_coerces() {
        // MiniMax wrapped the DSL string in a one-field object.
        let out = normalized(json!({
            "path": "f.rs",
            "edits": [{"$text": "replace: OLD -> NEW"}]
        }));
        assert_eq!(out["edits"][0]["old_string"], "OLD");
        assert_eq!(out["edits"][0]["new_string"], "NEW");
    }

    #[test]
    fn change_alias_without_replacement_gets_targeted_guidance() {
        // The model sent {"change": "replace: OLD"} (no ` -> `,
        // no replacement) 12 times — the generic "old_string is required"
        // error never named the problem. The targeted rejection does.
        let out = normalized(json!({
            "path": "f.rs",
            "change": "replace: if self.tasklog.sections.is_empty() {"
        }));
        let err = reject_unusable_alias(&out).expect_err("unusable alias must be rejected");
        let text = err.to_string();
        assert!(text.contains("'change'"), "{text}");
        assert!(text.contains("only one side"), "{text}");
        assert!(text.contains("old_string"), "{text}");
    }

    #[test]
    fn dsl_angle_content_form_pairs_with_top_level_old_string() {
        // `replace> NEW` / `new_string> NEW` carry only the new text; the old
        // text comes from a sibling top-level old_string. Leading indentation
        // after the angle directive is preserved exactly.
        let out = normalized(json!({
            "path": "f.rs",
            "old_string": "    let a = 1;",
            "edits": ["replace>    let a = 2;"],
        }));
        assert_eq!(out["edits"][0]["old_string"], "    let a = 1;");
        assert_eq!(out["edits"][0]["new_string"], "    let a = 2;");
    }

    #[test]
    fn dsl_old_side_pairs_with_top_level_new_string() {
        // MiniMax put the old side in an edits string while
        // placing the replacement in top-level new_string. Because `edits`
        // takes precedence, the normalizer must lift the pair into one region.
        let out = normalized(json!({
            "path": "f.rs",
            "new_string": "    let a = 2;",
            "edits": ["replace:     let a = 1;"],
        }));
        assert_eq!(out["edits"][0]["old_string"], "    let a = 1;");
        assert_eq!(out["edits"][0]["new_string"], "    let a = 2;");
    }

    #[test]
    fn dsl_old_string_directive_wrapped_in_text_object_pairs_with_top_level_new_string() {
        // A model sent `{"$text":"old_string>..."}` plus top-level
        // new_string. Preserve the old side's leading whitespace exactly.
        let out = normalized(json!({
            "path": "f.rs",
            "new_string": "        assert!(fixed);",
            "edits": [{"$text": "old_string>        assert!(broken);"}],
        }));
        assert_eq!(out["edits"][0]["old_string"], "        assert!(broken);");
        assert_eq!(out["edits"][0]["new_string"], "        assert!(fixed);");
    }

    #[test]
    fn dsl_leaves_canonical_objects_untouched() {
        let out = normalized(json!({
            "path": "f.rs",
            "edits": [{"old_string": "a -> b", "new_string": "c"}],
        }));
        // An object region is preserved verbatim — including an arrow in its text.
        assert_eq!(out["edits"][0]["old_string"], "a -> b");
        assert_eq!(out["edits"][0]["new_string"], "c");
    }

    #[test]
    fn dsl_content_only_without_old_is_left_for_schema_error() {
        // No arrow and no top-level old_string -> nothing to find; leave the
        // string in place so parsing fails with the schema-guided error.
        let out = normalized(json!({"path": "f.rs", "edits": ["replace>just new text"]}));
        assert!(out["edits"][0].is_string());
    }

    // A whole single edit packed into a top-level ad-hoc field (`change`, `edit`,
    // …) — not the `edits` array — is lifted into old_string/new_string.

    #[test]
    fn alias_change_dsl_string_becomes_old_and_new() {
        let out = normalized(json!({
            "path": "f.rs",
            "change": "replace: /// old doc -> /// new doc",
        }));
        assert_eq!(out["old_string"], "/// old doc");
        assert_eq!(out["new_string"], "/// new doc");
        assert!(out.get("change").is_none(), "alias key consumed");
    }

    #[test]
    fn alias_change_old_side_pairs_with_top_level_new_string() {
        let out = normalized(json!({
            "path": "f.rs",
            "change": "replace: old line",
            "new_string": "new line",
        }));
        assert_eq!(out["old_string"], "old line");
        assert_eq!(out["new_string"], "new line");
        assert!(out.get("change").is_none(), "alias key consumed");
    }

    #[test]
    fn alias_object_form_is_lifted() {
        let out = normalized(json!({
            "path": "f.rs",
            "edit": {"old_string": "a", "new_string": "b"},
        }));
        assert_eq!(out["old_string"], "a");
        assert_eq!(out["new_string"], "b");
    }

    #[test]
    fn alias_ignored_when_canonical_fields_present() {
        // A real call carrying its own old_string is never disturbed.
        let out = normalized(json!({
            "path": "f.rs",
            "old_string": "a",
            "new_string": "b",
            "change": "replace: x -> y",
        }));
        assert_eq!(out["old_string"], "a");
        assert_eq!(out["new_string"], "b");
        assert_eq!(out["change"], "replace: x -> y");
    }

    #[test]
    fn alias_without_arrow_is_left_for_schema_error() {
        // No arrow and no top-level old_string -> can't split; leave it so the
        // intent-aware error guides the model.
        let out = normalized(json!({"path": "f.rs", "change": "just some prose"}));
        assert!(out.get("old_string").is_none());
        assert_eq!(out["change"], "just some prose");
    }

    #[tokio::test]
    async fn execute_accepts_top_level_change_alias() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "alpha\nbeta\n");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        tool.execute(json!({
            "path": "test.txt",
            "change": "replace: alpha -> ALPHA",
        }))
        .await
        .unwrap();

        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "ALPHA\nbeta\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_classifies_a_symlink_alias_from_its_protected_target() {
        use std::os::unix::fs::symlink;

        let fixture = TestFixture::new();
        let target = fixture.create_file(".env", "TOKEN=old\n");
        fixture
            .read_tracker
            .mark_read(&target.canonicalize().unwrap())
            .await;
        symlink(".env", fixture.project_root.join("settings.txt")).unwrap();
        let tool = balanced_tool(&fixture);

        let error = tool
            .execute(json!({
                "path": "settings.txt",
                "old_string": "TOKEN=old",
                "new_string": "TOKEN=new"
            }))
            .await
            .expect_err("editing a protected resolved target must require permission");

        assert!(
            error.to_string().contains("Permission prompt unavailable"),
            "{error:#}"
        );
        assert_eq!(std::fs::read_to_string(target).unwrap(), "TOKEN=old\n");
    }

    #[tokio::test]
    async fn execute_accepts_model_dsl_string_edit() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "alpha\nbeta\n");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        tool.execute(json!({
            "path": "test.txt",
            "edits": ["replace: alpha -> ALPHA"],
        }))
        .await
        .unwrap();

        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "ALPHA\nbeta\n");
    }

    #[tokio::test]
    async fn execute_rejects_uncoercible_edits_string_with_guidance() {
        // `replace> NEW` with no anchor can't become a region; the model gets
        // targeted guidance, not an opaque serde "failed to parse" blob.
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "alpha\nbeta\n");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let err = tool
            .execute(json!({
                "path": "test.txt",
                "edits": ["replace> just new content with no anchor"],
            }))
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("edits[0]: this gives only one side"), "{err}");
        assert!(err.contains("old_string"), "{err}");
        assert!(!err.contains("Failed to parse"), "{err}");
        // File is untouched.
        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "alpha\nbeta\n");
    }

    #[test]
    fn reject_uncoerced_edits_flags_only_leftover_strings() {
        // A well-formed region array passes.
        assert!(
            reject_uncoerced_edits(&json!({
                "path": "f.rs",
                "edits": [{"old_string": "a", "new_string": "b"}],
            }))
            .is_ok()
        );
        // A leftover string is reported by index, with a one-line preview.
        let err = reject_uncoerced_edits(&json!({
            "path": "f.rs",
            "edits": [{"old_string": "a", "new_string": "b"}, "loose text\nsecond line"],
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("edits[1]"), "{err}");
        assert!(err.contains('⏎'), "newlines flattened in preview: {err}");
    }

    // Direct unit tests for the pure `apply_region` core, exercising the
    // string-replacement edge cases without going through async `execute`.

    #[test]
    fn apply_region_counts_non_overlapping_matches() {
        // `str::matches` is non-overlapping: "aaaa" -> 2, "aaa" -> 1.
        // Two matches without replace_all is ambiguous and rejected.
        let err = apply_region("aaaa", "f.txt", "aa", "b", false, OnAmbiguous::Reject).unwrap_err();
        assert!(err.to_string().contains("appears 2 times"));

        // A single match is unambiguous and replaces the first "aa".
        let out = apply_region("aaa", "f.txt", "aa", "b", false, OnAmbiguous::Reject).unwrap();
        assert_eq!(out, "ba");
    }

    #[test]
    fn already_applied_edit_says_so_instead_of_a_bare_not_found() {
        // old_string absent, but new_string already present → "already applied".
        let content = "fn f() {\n    let x = 2;\n}\n";
        let err = apply_region(
            content,
            "f.txt",
            "    let x = 1;",
            "    let x = 2;",
            false,
            OnAmbiguous::Reject,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("appears already applied"), "{err}");
        assert!(err.contains("do not re-read"), "{err}");
    }

    #[test]
    fn edit_appears_already_applied_is_conservative() {
        // Multi-line replacement already present → applied.
        assert!(edit_appears_already_applied(
            "a\nnew line one\nnew line two\n",
            "old\nblock",
            "new line one\nnew line two"
        ));
        // Deletion whose target is already gone → applied.
        assert!(edit_appears_already_applied("kept\n", "removed line", ""));
        // Short replacement that merely happens to appear → NOT flagged.
        assert!(!edit_appears_already_applied(
            "});\nother\n",
            "missing",
            "});"
        ));
        // old_string actually present → not applied (handled by the exact path).
        assert!(!edit_appears_already_applied(
            "has old\n",
            "old",
            "new value here"
        ));
    }

    #[test]
    fn apply_region_handles_multibyte_old_string() {
        // `é` is two bytes; the byte-index slicing must stay on char boundaries.
        let out = apply_region(
            "I like café au lait",
            "f.txt",
            "café",
            "tea",
            false,
            OnAmbiguous::Reject,
        )
        .unwrap();
        assert_eq!(out, "I like tea au lait");

        // An emoji `old_string` (4 bytes) must also slice correctly.
        let out = apply_region(
            "hi 😀 there",
            "f.txt",
            "😀",
            "X",
            false,
            OnAmbiguous::Reject,
        )
        .unwrap();
        assert_eq!(out, "hi X there");
    }

    #[test]
    fn apply_region_new_string_containing_old_is_not_rescanned() {
        // Single-match path: the replacement isn't re-searched for `old_string`.
        let out = apply_region(
            "hello",
            "f.txt",
            "hello",
            "hello world",
            false,
            OnAmbiguous::Reject,
        )
        .unwrap();
        assert_eq!(out, "hello world");

        // Replace-all path: each original match is replaced exactly once.
        let out = apply_region("x x", "f.txt", "x", "xx", true, OnAmbiguous::Reject).unwrap();
        assert_eq!(out, "xx xx");
    }

    #[test]
    fn apply_region_rejects_ambiguous_when_not_allowed() {
        let err =
            apply_region("foo foo", "f.txt", "foo", "bar", false, OnAmbiguous::Reject).unwrap_err();
        assert!(err.to_string().contains("appears 2 times"));
    }

    #[test]
    fn apply_region_allows_ambiguous_uses_first_match() {
        let out =
            apply_region("foo foo", "f.txt", "foo", "bar", false, OnAmbiguous::Allow).unwrap();
        assert_eq!(out, "bar foo");
    }

    #[test]
    fn apply_region_replace_all() {
        let out = apply_region(
            "foo bar foo",
            "f.txt",
            "foo",
            "qux",
            true,
            OnAmbiguous::Reject,
        )
        .unwrap();
        assert_eq!(out, "qux bar qux");
    }

    #[tokio::test]
    async fn test_edit_single_occurrence() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "Hello world");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "old_string": "world",
                "new_string": "bonsai"
            }))
            .await
            .unwrap();

        let output = match result {
            ToolOutput::Edit { summary, .. } => summary,
            _ => panic!("Expected Edit output"),
        };

        assert!(output.contains("File edited successfully"));
        assert!(output.contains("Lines added: 1, Lines removed: 1"));
        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "Hello bonsai");
    }

    #[tokio::test]
    async fn edit_file_hook_receives_the_same_removed_and_added_lines() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("hooked.txt", "old line\nkept\n");
        fixture
            .read_tracker
            .mark_read(&file_path.canonicalize().unwrap())
            .await;
        let hook = HookDef {
            name: "capture-edit-diff".to_string(),
            event: HookEvent::PreFileWrite,
            matcher: HookMatcher::default(),
            action: HookAction::Shell {
                command: "cat > .edit-hook-input.json".to_string(),
            },
            timeout_secs: 5,
            blocking: true,
            on_failure: FailureBehavior::Block,
            capabilities: Vec::new(),
            enabled: true,
        };
        let hooks = Arc::new(
            crate::hooks::HookEngine::build(
                &Config {
                    hooks: vec![(hook, ConfigSource::Global)],
                    ..Config::default()
                },
                fixture.project_root.clone(),
                PermissionManager::memory_only_hooks(),
                Arc::new(InteractionService::noninteractive()),
                Arc::new(ExtensionRegistry::new()),
                None,
            )
            .await,
        );
        let tool = EditTool::with_hooks_and_locks(
            fixture.project_root.clone(),
            fixture.read_tracker.clone(),
            YoloMode::new(),
            hooks,
            WorkspaceLockContext::disabled(fixture.project_root.clone()),
            ActionPolicy::testing(),
        );

        let output = tool
            .execute(json!({
                "path": "hooked.txt",
                "old_string": "old line",
                "new_string": "new line"
            }))
            .await
            .expect("edit should pass its hook");
        let hook_input: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(fixture.project_root.join(".edit-hook-input.json")).unwrap(),
        )
        .unwrap();
        let hook_diff = hook_input["diff"].as_str().unwrap_or_default();

        assert!(hook_diff.contains("-old line"), "{hook_diff}");
        assert!(hook_diff.contains("+new line"), "{hook_diff}");
        let ToolOutput::Edit { diff, .. } = output else {
            panic!("expected edit output");
        };
        assert_eq!(hook_diff, diff.render_for_hook());
    }

    #[tokio::test]
    async fn test_edit_absolute_project_path() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "Hello world");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;
        let path_arg = file_path.display().to_string();

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        tool.execute(json!({
            "path": path_arg,
            "old_string": "world",
            "new_string": "bonsai"
        }))
        .await
        .unwrap();

        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "Hello bonsai");
    }

    #[tokio::test]
    async fn test_edit_replace_all() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "foo\nbar\nfoo\n");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "old_string": "foo",
                "new_string": "qux",
                "replace_all": true
            }))
            .await
            .unwrap();
        let output = match result {
            ToolOutput::Edit { summary, .. } => summary,
            _ => panic!("Expected Edit output"),
        };

        assert!(output.contains("Lines added: 2, Lines removed: 2"));

        assert_eq!(
            std::fs::read_to_string(file_path).unwrap(),
            "qux\nbar\nqux\n"
        );
    }

    #[tokio::test]
    async fn test_edit_no_match() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "Hello world");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "old_string": "nonexistent",
                "new_string": "replacement"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("old_string not found"));
    }

    #[tokio::test]
    async fn test_edit_no_match_suggests_nearest_region() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.rs", "fn main() {\n    let x = 1;\n}\n");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.rs",
                // Eight-space indent where the file uses four; not a substring,
                // so the exact match fails and recovery should kick in.
                "old_string": "        let x = 1;",
                "new_string": "        let x = 2;"
            }))
            .await;

        let err = result.unwrap_err().to_string();
        assert!(err.contains("old_string not found"), "{err}");
        // A whitespace-only mismatch is caught by the token matcher, which
        // hands back the verbatim on-disk text — strictly more actionable than
        // the fuzzy "closest match" hint it used to get.
        assert!(err.contains("once all whitespace is ignored"), "{err}");
        assert!(err.contains("line 2"), "{err}");
        assert!(err.contains("    let x = 1;"), "{err}");
    }

    #[tokio::test]
    async fn test_edit_ambiguous_lists_occurrence_lines() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "foo\nbar\nfoo\n");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "old_string": "foo",
                "new_string": "qux"
            }))
            .await;

        let err = result.unwrap_err().to_string();
        assert!(err.contains("appears 2 times"), "{err}");
        assert!(err.contains("lines 1, 3"), "{err}");
    }

    #[tokio::test]
    async fn test_edit_identical_content() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "Same content");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "old_string": "Same content",
                "new_string": "Same content"
            }))
            .await;

        // A no-op edit succeeds: the desired end state already holds, and an
        // error here only triggers re-read/retry loops.
        let output = result.unwrap();
        assert!(
            output
                .rendered_summary()
                .contains("already contains exactly this content"),
            "{}",
            output.rendered_summary()
        );
        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "Same content");
    }

    #[tokio::test]
    async fn test_edit_multiple_matches_without_replace_all() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "foo bar foo");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "old_string": "foo",
                "new_string": "qux"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("appears 2 times"));
    }

    #[tokio::test]
    async fn test_edit_delete_content() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "Hello world");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "old_string": "world",
                "new_string": ""
            }))
            .await
            .unwrap();
        let output = match result {
            ToolOutput::Edit { summary, .. } => summary,
            _ => panic!("Expected Edit output"),
        };

        assert!(output.contains("Lines added: 1, Lines removed: 1"));
        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "Hello ");
    }

    #[tokio::test]
    async fn test_edit_create_file() {
        let fixture = TestFixture::new();

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "new.txt",
                "old_string": "",
                "new_string": "New file content"
            }))
            .await
            .unwrap();

        let output = match result {
            ToolOutput::Edit { summary, .. } => summary,
            _ => panic!("Expected Edit output"),
        };

        assert!(output.contains("File created successfully"));
        assert!(output.contains("Lines added: 1, Lines removed: 0"));

        let file_path = fixture.project_root.join("new.txt");
        assert!(file_path.exists());
        assert_eq!(
            std::fs::read_to_string(file_path).unwrap(),
            "New file content"
        );
    }

    #[tokio::test]
    async fn test_edit_rejects_missing_top_level_new_string() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "foo\nbar\n");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "old_string": "foo"
            }))
            .await;

        assert!(result.is_err());
        // old_string-but-no-new_string steers toward deletion via empty string.
        let err = result.unwrap_err().to_string();
        assert!(err.contains("new_string is required"), "{err}");
        assert!(err.contains("set new_string to \"\""), "{err}");
        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "foo\nbar\n");
    }

    #[tokio::test]
    async fn test_edit_rejects_missing_top_level_old_string() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "foo\nbar\n");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "new_string": "qux"
            }))
            .await;

        assert!(result.is_err());
        // new_string-but-no-old_string is the append/insert mistake from the
        // screenshot: the message names the insert recipe and the write tool.
        let err = result.unwrap_err().to_string();
        assert!(err.contains("old_string is required"), "{err}");
        assert!(err.contains("insert or append"), "{err}");
        assert!(err.contains("write tool"), "{err}");
        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "foo\nbar\n");
    }

    #[tokio::test]
    async fn test_edit_rejects_path_only_create() {
        let fixture = TestFixture::new();

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "new.txt"
            }))
            .await;

        assert!(result.is_err());
        // Neither field: point at both valid shapes and the write tool.
        let err = result.unwrap_err().to_string();
        assert!(err.contains("old_string is required"), "{err}");
        assert!(err.contains("edits array"), "{err}");
        assert!(err.contains("write tool"), "{err}");
        assert!(!fixture.project_root.join("new.txt").exists());
    }

    #[test]
    fn missing_single_edit_field_error_is_intent_specific() {
        // Only new_string -> append/insert guidance, never the bare "what".
        let only_new = missing_single_edit_field_error(None, Some("x")).to_string();
        assert!(only_new.contains("old_string is required"), "{only_new}");
        assert!(only_new.contains("insert or append"), "{only_new}");

        // Only old_string -> deletion-via-empty-string guidance.
        let only_old = missing_single_edit_field_error(Some("x"), None).to_string();
        assert!(only_old.contains("new_string is required"), "{only_old}");
        assert!(only_old.contains("empty string"), "{only_old}");

        // Neither -> both shapes spelled out, with literal braces rendered.
        let neither = missing_single_edit_field_error(None, None).to_string();
        assert!(neither.contains("old_string is required"), "{neither}");
        assert!(neither.contains("{old_string, new_string}"), "{neither}");
    }

    #[tokio::test]
    async fn test_edit_create_file_with_absolute_project_path() {
        let fixture = TestFixture::new();
        let file_path = fixture.project_root.join("absolute-new.txt");
        let path_arg = file_path.display().to_string();

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        tool.execute(json!({
            "path": path_arg,
            "old_string": "",
            "new_string": "New file content"
        }))
        .await
        .unwrap();

        assert!(file_path.exists());
        assert_eq!(
            std::fs::read_to_string(file_path).unwrap(),
            "New file content"
        );
    }

    #[tokio::test]
    async fn test_edit_rejects_parent_dir_traversal() {
        let fixture = TestFixture::new();

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "../outside.txt",
                "old_string": "",
                "new_string": "content"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("outside project root"));
    }

    #[tokio::test]
    async fn test_edit_rejects_absolute_path_outside_project() {
        let fixture = TestFixture::new();
        let outside_dir = tempfile::TempDir::new().unwrap();
        let outside_path = outside_dir.path().join("outside.txt");

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": outside_path.display().to_string(),
                "old_string": "",
                "new_string": "content"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("outside project root"));
        assert!(!outside_path.exists());
    }

    #[tokio::test]
    async fn test_edit_multi_region_applies_all_atomically() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "alpha\nbeta\ngamma\n");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "edits": [
                    {"old_string": "alpha", "new_string": "ALPHA"},
                    {"old_string": "gamma", "new_string": "GAMMA"}
                ]
            }))
            .await
            .unwrap();
        let output = match result {
            ToolOutput::Edit { summary, .. } => summary,
            _ => panic!("Expected Edit output"),
        };

        assert!(output.contains("2 regions"));
        assert_eq!(
            std::fs::read_to_string(file_path).unwrap(),
            "ALPHA\nbeta\nGAMMA\n"
        );
    }

    #[tokio::test]
    async fn test_edit_multi_region_chains_results() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "hello world");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        tool.execute(json!({
            "path": "test.txt",
            "edits": [
                {"old_string": "hello", "new_string": "hello there"},
                {"old_string": "there", "new_string": "THERE"}
            ]
        }))
        .await
        .unwrap();

        // Second region matches the "there" introduced by the first region.
        assert_eq!(
            std::fs::read_to_string(file_path).unwrap(),
            "hello THERE world"
        );
    }

    #[tokio::test]
    async fn test_edit_multi_region_aborts_on_missing_match() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "alpha\nbeta\n");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "edits": [
                    {"old_string": "alpha", "new_string": "ALPHA"},
                    {"old_string": "missing", "new_string": "x"}
                ]
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("old_string not found"));
        // File is unchanged because the whole edit aborted before writing.
        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "alpha\nbeta\n");
    }

    #[tokio::test]
    async fn test_edit_multi_region_rejects_empty_old_string() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "hello");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "edits": [
                    {"old_string": "", "new_string": "x"}
                ]
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("edits[0].old_string is empty"));
    }

    #[tokio::test]
    async fn test_edit_multi_region_rejects_empty_edits_array() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "hello");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "edits": []
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("edits must"));
        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "hello");
    }

    #[tokio::test]
    async fn test_edit_multi_region_rejects_missing_new_string() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "hello");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "edits": [
                    {"old_string": "hello"}
                ]
            }))
            .await;

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to parse edit tool arguments")
        );
        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "hello");
    }

    #[tokio::test]
    async fn test_edit_multi_region_supports_replace_all_per_region() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "foo\nbar\nfoo\n");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        tool.execute(json!({
            "path": "test.txt",
            "edits": [
                {"old_string": "foo", "new_string": "qux", "replace_all": true},
                {"old_string": "bar", "new_string": "BAR"}
            ]
        }))
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(file_path).unwrap(),
            "qux\nBAR\nqux\n"
        );
    }

    #[tokio::test]
    async fn flattened_old_string_gets_verbatim_region_back() {
        // The model read the correct region, then
        // submitted it with every newline collapsed to a space (the one-line
        // shorthand form does this). The error must hand back the exact on-disk
        // text so the retry can match.
        let fixture = TestFixture::new();
        let source = "#[derive(Debug, Default)]\nstruct ParseState {\n    project: Project,\n    title_set: bool,\n    current_section: Option<String>,\n}\n";
        let file_path = fixture.create_file("parser.rs", source);
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let err = tool
            .execute(json!({
                "path": "parser.rs",
                "edits": ["replace: #[derive(Debug, Default)] struct ParseState { project: Project, title_set: bool, current_section: Option<String>, } -> #[derive(Debug, Default)] struct ParseState { project: Project, title_set: bool, current_section: Option<usize>, }"],
            }))
            .await
            .expect_err("flattened old_string cannot exact-match")
            .to_string();

        assert!(err.contains("once all whitespace is ignored"), "{err}");
        assert!(err.contains("line 1"), "{err}");
        // The verbatim multi-line region is echoed back for the retry.
        assert!(
            err.contains("struct ParseState {\n    project: Project,"),
            "{err}"
        );
        assert!(err.contains("shorthand"), "{err}");
        // File untouched.
        assert_eq!(std::fs::read_to_string(file_path).unwrap(), source);
    }

    #[tokio::test]
    async fn edit_rejects_replayed_elision_marker_as_replacement() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.rs", "fn real() {}\n");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let err = tool
            .execute(json!({
                "path": "test.rs",
                "old_string": "fn real() {}",
                "new_string": "<omitted: 9053 bytes, 296 lines>"
            }))
            .await
            .expect_err("a replayed elision marker must be refused");
        assert!(
            err.to_string().contains("context-elision placeholder"),
            "{err:#}"
        );
        assert_eq!(
            std::fs::read_to_string(file_path).unwrap(),
            "fn real() {}\n",
            "the file must be untouched"
        );
    }

    #[tokio::test]
    async fn edit_result_includes_post_edit_excerpt_with_line_numbers() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.rs", "fn one() {}\nfn two() {}\nfn three() {}\n");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;

        let tool = EditTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({
                "path": "test.rs",
                "old_string": "fn two() {}",
                "new_string": "fn two() { println!(\"hi\"); }"
            }))
            .await
            .unwrap();

        let summary = match result {
            ToolOutput::Edit { summary, .. } => summary,
            _ => panic!("Expected Edit output"),
        };
        assert!(summary.contains("File edited successfully"));
        // The model sees the post-edit state so it need not re-read to confirm.
        assert!(
            summary.contains("Result after edit (changed regions):"),
            "summary should carry the excerpt: {summary}"
        );
        assert!(
            summary.contains("2: fn two() { println!(\"hi\"); }"),
            "excerpt should show the changed line with its new line number: {summary}"
        );
    }

    #[test]
    fn post_edit_excerpt_caps_a_large_edit() {
        let old = String::new();
        let new: String = (1..=200).map(|n| format!("line {n}\n")).collect();
        let diff = crate::diff::build_file_diff("big.txt".to_string(), Some(&old), &new);
        let excerpt = post_edit_excerpt(&diff).expect("excerpt for a real change");
        let line_count = excerpt.lines().count();
        assert!(
            line_count <= POST_EDIT_EXCERPT_MAX_LINES + 3,
            "excerpt must stay bounded, got {line_count} lines"
        );
        assert!(excerpt.contains("truncated"));
    }

    #[tokio::test]
    async fn yolo_edit_overwrites_from_empty_old_string() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "Old content");
        let yolo_mode = YoloMode::new();
        yolo_mode.set_enabled(true);

        let tool = EditTool::with_yolo_mode(
            fixture.project_root.clone(),
            fixture.read_tracker.clone(),
            yolo_mode,
        );
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "old_string": "",
                "new_string": "Replacement"
            }))
            .await
            .unwrap();

        let output = match result {
            ToolOutput::Edit { summary, .. } => summary,
            _ => panic!("Expected Edit output"),
        };
        assert!(output.contains("File overwritten successfully"));
        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "Replacement");
    }

    #[tokio::test]
    async fn yolo_edit_overwrites_without_prior_read() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "Hello world");
        let yolo_mode = YoloMode::new();
        yolo_mode.set_enabled(true);

        let tool = EditTool::with_yolo_mode(
            fixture.project_root.clone(),
            fixture.read_tracker.clone(),
            yolo_mode,
        );
        tool.execute(json!({
            "path": "test.txt",
            "old_string": "world",
            "new_string": "bonsai"
        }))
        .await
        .unwrap();

        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "Hello bonsai");
    }

    #[tokio::test]
    async fn yolo_edit_ignores_stale_read_check() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "Hello world");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;
        std::fs::write(&file_path, "Hello changed world").unwrap();
        let yolo_mode = YoloMode::new();
        yolo_mode.set_enabled(true);

        let tool = EditTool::with_yolo_mode(
            fixture.project_root.clone(),
            fixture.read_tracker.clone(),
            yolo_mode,
        );
        tool.execute(json!({
            "path": "test.txt",
            "old_string": "changed",
            "new_string": "YOLO"
        }))
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(file_path).unwrap(),
            "Hello YOLO world"
        );
    }

    #[tokio::test]
    async fn yolo_edit_uses_first_match_when_multiple_matches() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "foo bar foo");
        let yolo_mode = YoloMode::new();
        yolo_mode.set_enabled(true);

        let tool = EditTool::with_yolo_mode(
            fixture.project_root.clone(),
            fixture.read_tracker.clone(),
            yolo_mode,
        );
        tool.execute(json!({
            "path": "test.txt",
            "old_string": "foo",
            "new_string": "qux"
        }))
        .await
        .unwrap();

        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "qux bar foo");
    }

    #[tokio::test]
    async fn yolo_edit_allows_absolute_path_outside_project() {
        let fixture = TestFixture::new();
        let outside_dir = tempfile::TempDir::new().unwrap();
        let outside_path = outside_dir.path().join("outside.txt");
        std::fs::write(&outside_path, "outside old").unwrap();
        let yolo_mode = YoloMode::new();
        yolo_mode.set_enabled(true);

        let tool = EditTool::with_yolo_mode(
            fixture.project_root.clone(),
            fixture.read_tracker.clone(),
            yolo_mode,
        );
        tool.execute(json!({
            "path": outside_path.display().to_string(),
            "old_string": "old",
            "new_string": "new"
        }))
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(outside_path).unwrap(),
            "outside new"
        );
    }

    #[tokio::test]
    async fn yolo_edit_allows_parent_dir_outside_project() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let project_root = temp_dir.path().join("project");
        std::fs::create_dir(&project_root).unwrap();
        let outside_path = temp_dir.path().join("outside.txt");
        std::fs::write(&outside_path, "outside old").unwrap();
        let yolo_mode = YoloMode::new();
        yolo_mode.set_enabled(true);

        let tool = EditTool::with_yolo_mode(project_root, ReadTracker::new(), yolo_mode);
        tool.execute(json!({
            "path": "../outside.txt",
            "old_string": "old",
            "new_string": "new"
        }))
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(outside_path).unwrap(),
            "outside new"
        );
    }
}
