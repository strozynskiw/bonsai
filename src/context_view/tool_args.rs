/// Individual edit old/new strings up to this long stay verbatim in the
/// model-visible history. Compacted args are MODEL INPUT: a model
/// learned the previous "replace: OLD -> NEW" one-line preview format from its
/// own history and began emitting it as real edit calls — whitespace-collapsed,
/// "..."-truncated, with literal `\n` text where newlines belonged — which the
/// DSL coercion plus fuzzy-apply then wrote into the file. History must only
/// ever show the canonical schema shape ({"old_string": …, "new_string": …});
/// only oversized *values* are elided, and only with a marker the edit/write
/// tools refuse to apply (`reject_replayed_elision_marker`). Deliberate, don't
/// simplify back to a lossy preview DSL.
const EDIT_ARGUMENT_VERBATIM_MAX_CHARS: usize = 2_000;
/// Successful write bodies through this size stay visible in canonical tool
/// arguments. Keeping normal new-file writes in context lets the model reuse
/// what it just authored without an immediate verification read; only large
/// bodies pay the cache-preserving elision tradeoff.
const WRITE_ARGUMENT_VERBATIM_MAX_CHARS: usize = 8_000;

pub(crate) fn pretty_tool_arguments(arguments: &str) -> String {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| arguments.to_string())
}

pub(crate) fn compact_tool_arguments_for_context(name: &str, arguments: &str) -> String {
    let Some(value) = compact_tool_arguments_value(name, arguments) else {
        return arguments.to_string();
    };
    serde_json::to_string(&value).unwrap_or_else(|_err| arguments.to_string())
}

fn compact_tool_arguments_value(name: &str, arguments: &str) -> Option<serde_json::Value> {
    match name {
        "write" => compact_write_arguments(arguments),
        "edit" => compact_edit_arguments(arguments),
        _ => None,
    }
}

fn compact_write_arguments(arguments: &str) -> Option<serde_json::Value> {
    let value = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
    let map = value.as_object()?;
    let path = map.get("path")?.as_str()?;
    let content = map.get("content")?.as_str()?;
    if content.chars().count() <= WRITE_ARGUMENT_VERBATIM_MAX_CHARS {
        return None;
    }
    Some(serde_json::json!({
        "path": path,
        "content": omitted_text_summary(content),
    }))
}

/// Elide only oversized old/new values, preserving the canonical call shape.
/// Returns `None` when every value is small enough to stay verbatim (the
/// common case — most edits are a few hundred chars per side).
fn compact_edit_arguments(arguments: &str) -> Option<serde_json::Value> {
    let mut value = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
    let map = value.as_object_mut()?;
    let mut changed = false;

    for key in ["old_string", "new_string"] {
        if let Some(slot) = map.get_mut(key) {
            changed |= elide_oversized_edit_string(slot);
        }
    }
    if let Some(edits) = map
        .get_mut("edits")
        .and_then(serde_json::Value::as_array_mut)
    {
        for edit in edits.iter_mut() {
            let Some(region) = edit.as_object_mut() else {
                continue;
            };
            for key in ["old_string", "new_string"] {
                if let Some(slot) = region.get_mut(key) {
                    changed |= elide_oversized_edit_string(slot);
                }
            }
        }
    }
    changed.then_some(value)
}

/// Replace an oversized edit-string value with a single-line elision marker in
/// the `<content elided: …>` family, which `reject_replayed_elision_marker`
/// refuses if a model ever replays it as real call content.
fn elide_oversized_edit_string(slot: &mut serde_json::Value) -> bool {
    let Some(text) = slot.as_str() else {
        return false;
    };
    let chars = text.chars().count();
    if chars <= EDIT_ARGUMENT_VERBATIM_MAX_CHARS {
        return false;
    }
    *slot = serde_json::Value::String(format!(
        "<content elided: your edit SUCCEEDED and this {chars}-char text was applied; this is only \
         a context-saving placeholder — never emit it in a call; read the file for its current \
         content.>"
    ));
    true
}

/// Placeholder for a large write body elided from the conversation. It must be
/// self-describing: models re-emit calls from their visible history, and a bare
/// marker was observed being replayed as literal file content and,
/// worse, misread as *the model's own mistake* — prompting redundant rewrites of
/// an already-correct file (observed live: the same report written three times).
///
/// So the reassurance leads, right after the guard-required `<content elided:`
/// prefix: the model reads left-to-right and latches onto the first words, so
/// "your write SUCCEEDED" must come before the byte/line counts. The write/edit
/// tools additionally refuse marker-shaped content outright.
fn omitted_text_summary(text: &str) -> String {
    format!(
        "<content elided: your write SUCCEEDED and the real body ({} bytes, {}) is on disk, not \
         lost — this is only a context-saving view of what you sent. Do not resend this or rewrite \
         the file; re-read it if you need the content.>",
        text.len(),
        plural_count(text.lines().count(), "line")
    )
}

fn plural_count(count: usize, singular: &str) -> String {
    let suffix = if count == 1 { "" } else { "s" };
    format!("{count} {singular}{suffix}")
}
