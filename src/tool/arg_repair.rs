//! Schema-driven validate-then-repair for malformed tool-call arguments.
//!
//! Weaker models (DeepSeek-class in particular) routinely emit tool calls
//! whose argument *shapes* violate the tool schema in a handful of predictable
//! ways: `null` for optional fields instead of omitting them, arrays/objects
//! double-encoded as JSON strings, a single object where an array of objects
//! is expected, bare scalars where arrays are expected, scalars quoted as
//! strings, and file paths wrapped in markdown auto-links. This module repairs
//! exactly those shapes, and nothing else.
//!
//! The contract is **validate-then-repair**: every rule starts with a
//! conformance check, and a value that already conforms to the schema is never
//! touched — byte-identical passthrough. That gate is what keeps well-behaved
//! models (GPT, Claude) observationally unaffected, so do NOT "simplify" by
//! repairing unconditionally or removing the conformance check. Repairs are
//! deliberately failure-gated rather than model-gated: model identity is not
//! threaded to the dispatch site, and doesn't need to be.
//!
//! This is the generic layer of the coerce-or-guide philosophy; per-tool
//! coercers still handle shapes only they can judge (`normalize_edit_args` in
//! [`crate::tool::edit`] for the ad-hoc edit DSL, the bare-string patch wrap
//! in [`crate::tool::apply_patch`]). This layer runs first and passes anything
//! it cannot confidently repair through to them, or to the tool's serde
//! guidance.

use serde_json::{Map, Value};

/// One repair applied to a tool call's arguments, for tracing and for the
/// guidance message when validation still fails afterwards.
pub(crate) struct RepairNote {
    /// Dotted path of the repaired field, e.g. `edits[0].replace_all`;
    /// `<arguments>` for the top-level payload itself.
    pub(crate) field: String,
    pub(crate) action: RepairAction,
}

pub(crate) enum RepairAction {
    /// `null` for an optional field dropped (models send null instead of
    /// omitting the key).
    DroppedNull,
    /// A JSON string containing the expected array/object was parsed
    /// (models double-encode structured arguments).
    ParsedEmbeddedJson,
    /// A lone value was wrapped into the expected one-element array
    /// (models send `{...}` or a bare scalar where an array is expected).
    WrappedInArray,
    /// `"5"` → 5, `"true"` → true (models quote scalars).
    ParsedScalarString,
    /// `[path](path)` → `path` on `format: "path"` fields (models autolink
    /// file paths in markdown).
    UnwrappedMarkdownLink,
    /// A case-mismatched enum value canonicalized to its unique
    /// case-insensitive match (models Title-Case enum values).
    CanonicalizedEnumCase,
    /// A tool-specific alias field mapped onto its canonical schema field
    /// (e.g. `start_line`/`end_line` → `offset`/`limit` on `read`, a
    /// convention models import from `read_region` and other harnesses).
    MappedAliasField,
    /// A prose-only `description` field not declared by a closed schema was
    /// discarded. Models often attach this call annotation to `bash`.
    DroppedDescriptiveField,
}

impl std::fmt::Display for RepairNote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let action = match self.action {
            RepairAction::DroppedNull => "dropped null for optional field",
            RepairAction::ParsedEmbeddedJson => "parsed JSON embedded in a string",
            RepairAction::WrappedInArray => "wrapped single value in an array",
            RepairAction::ParsedScalarString => "parsed scalar from a string",
            RepairAction::UnwrappedMarkdownLink => "unwrapped markdown link around path",
            RepairAction::CanonicalizedEnumCase => "canonicalized enum value case",
            RepairAction::MappedAliasField => "mapped alias onto its canonical field",
            RepairAction::DroppedDescriptiveField => "dropped undeclared descriptive metadata",
        };
        write!(f, "{}: {}", self.field, action)
    }
}

/// Unwrap a double-encoded top-level payload: a JSON *string* whose contents
/// parse to a JSON object (some models stringify the whole arguments object a
/// second time). Anything else — including a string containing an array or
/// plain prose — is left for the caller's "must be a JSON object" guidance.
pub(crate) fn unwrap_double_encoded_arguments(value: &mut Value) -> Option<RepairNote> {
    let inner = value.as_str()?;
    let parsed = serde_json::from_str::<Value>(inner).ok()?;
    if !parsed.is_object() {
        return None;
    }
    *value = parsed;
    Some(RepairNote {
        field: "<arguments>".to_string(),
        action: RepairAction::ParsedEmbeddedJson,
    })
}

/// Repair the argument object in place against the tool's JSON schema,
/// returning a note per repair. Never touches a schema-conformant value — an
/// empty result means `args` is bit-for-bit what the model sent. Walks the
/// *schema* (declared properties, object sub-properties, array `items`), so
/// undeclared fields are never touched either.
pub(crate) fn repair_arguments(schema: &Value, args: &mut Value) -> Vec<RepairNote> {
    let mut notes = Vec::new();
    if let Some(object) = args.as_object_mut() {
        repair_object(schema, object, "", &mut notes);
    }
    notes
}

fn repair_object(
    schema: &Value,
    object: &mut Map<String, Value>,
    path: &str,
    notes: &mut Vec<RepairNote>,
) {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();

    if schema.get("additionalProperties").and_then(Value::as_bool) == Some(false)
        && !properties.contains_key("description")
        && object.get("description").is_some_and(Value::is_string)
    {
        object.remove("description");
        notes.push(RepairNote {
            field: if path.is_empty() {
                "description".to_string()
            } else {
                format!("{path}.description")
            },
            action: RepairAction::DroppedDescriptiveField,
        });
    }

    // Fill a schema-declared `path` from the alias names other harnesses train
    // models on: `file_path` is Claude Code's canonical name, `file`/`filename`
    // are common elsewhere (observed live: qwen sending `file` to `edit`). An
    // alias only fills an *absent* `path`; when `path` is also present it wins
    // and the redundant alias is dropped so rejected-fields guidance doesn't
    // bounce an otherwise-correct call.
    const PATH_ALIASES: &[&str] = &["file_path", "file", "filename"];
    if properties.contains_key("path") {
        for alias in PATH_ALIASES {
            let Some(value) = object.get(*alias) else {
                continue;
            };
            if !value.is_string() {
                continue;
            }
            let value = object
                .remove(*alias)
                .expect("alias key was just observed present");
            if !object.contains_key("path") {
                object.insert("path".to_string(), value);
            }
            notes.push(RepairNote {
                field: if path.is_empty() {
                    (*alias).to_string()
                } else {
                    format!("{path}.{alias}")
                },
                action: RepairAction::MappedAliasField,
            });
        }
    }

    for (name, property) in properties {
        let Some(value) = object.get_mut(name) else {
            continue;
        };
        let field = if path.is_empty() {
            name.clone()
        } else {
            format!("{path}.{name}")
        };
        // Null for an optional field means "omitted" to the models that send
        // it; a null on a *required* field is left in place so the tool's
        // serde guidance names the field instead of us reporting it missing.
        if value.is_null() && !required.contains(&name.as_str()) {
            object.remove(name);
            notes.push(RepairNote {
                field,
                action: RepairAction::DroppedNull,
            });
            continue;
        }
        repair_value(property, value, &field, notes);
        descend(property, value, &field, notes);
    }
}

/// Recurse into containers following the schema: object sub-properties and
/// object array items. Runs after [`repair_value`], so a just-parsed embedded
/// array/object gets its elements repaired too.
fn descend(property: &Value, value: &mut Value, field: &str, notes: &mut Vec<RepairNote>) {
    match schema_type(property) {
        Some("object") => {
            if let Some(object) = value.as_object_mut() {
                repair_object(property, object, field, notes);
            }
        }
        Some("array") => {
            let Some(items) = property.get("items") else {
                return;
            };
            let Some(elements) = value.as_array_mut() else {
                return;
            };
            for (index, element) in elements.iter_mut().enumerate() {
                let element_field = format!("{field}[{index}]");
                repair_value(items, element, &element_field, notes);
                if schema_type(items) == Some("object")
                    && let Some(object) = element.as_object_mut()
                {
                    repair_object(items, object, &element_field, notes);
                }
            }
        }
        _ => {}
    }
}

/// Apply the first matching repair rule to one value. Rule 0 — a conformant
/// value is untouched — gates everything.
fn repair_value(property: &Value, value: &mut Value, field: &str, notes: &mut Vec<RepairNote>) {
    if conforms(property, value) {
        return;
    }
    let action = match schema_type(property) {
        Some("array") => repair_array(property, value),
        Some("object") => repair_embedded_json(value, Value::is_object),
        Some("integer") => {
            repair_scalar_string(value, |token| token.parse::<i64>().ok().map(Into::into))
        }
        Some("number") => repair_scalar_string(value, |token| {
            let number = token.parse::<f64>().ok().filter(|n| n.is_finite())?;
            serde_json::Number::from_f64(number).map(Value::Number)
        }),
        Some("boolean") => repair_scalar_string(value, |token| {
            if token.eq_ignore_ascii_case("true") {
                Some(Value::Bool(true))
            } else if token.eq_ignore_ascii_case("false") {
                Some(Value::Bool(false))
            } else {
                None
            }
        }),
        Some("string") => repair_string(property, value),
        _ => None,
    };
    if let Some(action) = action {
        notes.push(RepairNote {
            field: field.to_string(),
            action,
        });
    }
}

/// Shallow schema conformance: JSON type, enum membership, and (for
/// `format: "path"` strings) not being markdown-link-shaped. Deliberately
/// shallow on containers — element/property repair happens in [`descend`].
fn conforms(property: &Value, value: &Value) -> bool {
    match schema_type(property) {
        Some("string") => {
            let Some(text) = value.as_str() else {
                return false;
            };
            if let Some(values) = enum_values(property)
                && !values.iter().any(|allowed| allowed == &text)
            {
                return false;
            }
            !(is_path_property(property) && markdown_link_target(text).is_some())
        }
        Some("integer") => value.is_i64() || value.is_u64(),
        Some("number") => value.is_number(),
        Some("boolean") => value.is_boolean(),
        Some("array") => value.is_array(),
        Some("object") => value.is_object(),
        _ => true,
    }
}

fn repair_array(property: &Value, value: &mut Value) -> Option<RepairAction> {
    let items_type = property.get("items").and_then(schema_type);
    if let Some(text) = value.as_str() {
        // Double-encoded array (or single object) in a string.
        if let Ok(parsed) = serde_json::from_str::<Value>(text) {
            if let Some(elements) = parsed.as_array()
                && elements
                    .iter()
                    .all(|element| element_matches(items_type, element))
            {
                *value = parsed;
                return Some(RepairAction::ParsedEmbeddedJson);
            }
            if parsed.is_object() && items_type == Some("object") {
                *value = Value::Array(vec![parsed]);
                return Some(RepairAction::ParsedEmbeddedJson);
            }
        }
        // Bare string where an array of strings is expected.
        if items_type == Some("string") {
            let single = std::mem::take(value);
            *value = Value::Array(vec![single]);
            return Some(RepairAction::WrappedInArray);
        }
        return None;
    }
    // A single object/scalar where an array of that type is expected.
    if element_matches(items_type, value) {
        let single = std::mem::take(value);
        *value = Value::Array(vec![single]);
        return Some(RepairAction::WrappedInArray);
    }
    None
}

/// Whether an element has the item type the schema declares. With no declared
/// item type there is nothing to judge, so any element passes.
fn element_matches(items_type: Option<&str>, element: &Value) -> bool {
    match items_type {
        Some("string") => element.is_string(),
        Some("integer") => element.is_i64() || element.is_u64(),
        Some("number") => element.is_number(),
        Some("boolean") => element.is_boolean(),
        Some("object") => element.is_object(),
        Some("array") => element.is_array(),
        _ => true,
    }
}

fn repair_embedded_json(
    value: &mut Value,
    expected: impl Fn(&Value) -> bool,
) -> Option<RepairAction> {
    let text = value.as_str()?;
    let parsed = serde_json::from_str::<Value>(text).ok().filter(expected)?;
    *value = parsed;
    Some(RepairAction::ParsedEmbeddedJson)
}

fn repair_scalar_string(
    value: &mut Value,
    parse: impl Fn(&str) -> Option<Value>,
) -> Option<RepairAction> {
    let parsed = parse(value.as_str()?.trim())?;
    *value = parsed;
    Some(RepairAction::ParsedScalarString)
}

fn repair_string(property: &Value, value: &mut Value) -> Option<RepairAction> {
    let text = value.as_str()?;
    if is_path_property(property)
        && let Some(target) = markdown_link_target(text)
    {
        *value = Value::String(target.to_string());
        return Some(RepairAction::UnwrappedMarkdownLink);
    }
    // Enum value with the wrong case, e.g. "Add" for ["add", "remove"]:
    // canonicalize only on a unique case-insensitive match — no fuzzy repair.
    let values = enum_values(property)?;
    let mut matches = values
        .iter()
        .filter(|allowed| allowed.eq_ignore_ascii_case(text));
    let (canonical, ambiguous) = (matches.next()?, matches.next().is_some());
    if ambiguous {
        return None;
    }
    let canonical = (*canonical).to_string();
    *value = Value::String(canonical);
    Some(RepairAction::CanonicalizedEnumCase)
}

/// The target of a markdown link when the *entire* value is one plain link,
/// e.g. `[src/main.rs](src/main.rs)` → `src/main.rs`. Targets with whitespace
/// or nested brackets/parens are rejected; combined with the `format: "path"`
/// gate (no legitimate path contains `](`) this cannot fire on a real path.
fn markdown_link_target(value: &str) -> Option<&str> {
    let rest = value.strip_prefix('[')?;
    let split = rest.find("](")?;
    let (text, tail) = rest.split_at(split);
    let target = tail["](".len()..].strip_suffix(')')?;
    let clean = |s: &str| !s.contains(['[', ']', '(', ')']) && !s.contains(char::is_whitespace);
    (!target.is_empty() && clean(target) && clean(text)).then_some(target)
}

fn schema_type(property: &Value) -> Option<&str> {
    property.get("type").and_then(Value::as_str)
}

fn is_path_property(property: &Value) -> bool {
    property.get("format").and_then(Value::as_str) == Some("path")
}

fn enum_values(property: &Value) -> Option<Vec<&str>> {
    let values = property
        .get("enum")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    (!values.is_empty()).then_some(values)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::tool::schema::{
        array_property, boolean_property, bounded_integer_property, closed_object, object,
        path_property, string_enum_property, string_property,
    };

    fn edit_like_schema() -> Value {
        closed_object(
            [
                ("path", path_property("File path")),
                (
                    "edits",
                    array_property(
                        "Edit regions",
                        object(
                            [
                                ("old_string", string_property("Old")),
                                ("new_string", string_property("New")),
                                ("replace_all", boolean_property("Replace all")),
                            ],
                            &["old_string", "new_string"],
                        ),
                    ),
                ),
                ("count", bounded_integer_property("How many", Some(1), None)),
                ("mode", string_enum_property("Mode", &["add", "remove"])),
                ("note", string_property("Free text")),
            ],
            &["path"],
        )
    }

    fn repaired(schema: &Value, mut args: Value) -> (Value, Vec<String>) {
        let notes = repair_arguments(schema, &mut args)
            .iter()
            .map(ToString::to_string)
            .collect();
        (args, notes)
    }

    #[test]
    fn valid_arguments_pass_through_untouched() {
        let schema = edit_like_schema();
        let args = json!({
            "path": "src/main.rs",
            "edits": [{"old_string": "a", "new_string": "b", "replace_all": true}],
            "count": 3,
            "mode": "add",
            "note": "[looks](like-a-link)",
        });

        let (out, notes) = repaired(&schema, args.clone());

        assert!(notes.is_empty(), "unexpected repairs: {notes:?}");
        assert_eq!(out, args, "conformant arguments must be byte-identical");
    }

    #[test]
    fn path_aliases_fill_an_absent_path() {
        let schema = edit_like_schema();

        // The exact failure observed live (qwen): `file` instead of `path`.
        let (out, notes) = repaired(
            &schema,
            json!({"file": "src/main.rs", "edits": [{"old_string": "a", "new_string": "b"}]}),
        );
        assert_eq!(out["path"], "src/main.rs");
        assert!(out.get("file").is_none(), "alias must be consumed");
        assert_eq!(notes, ["file: mapped alias onto its canonical field"]);

        // Claude Code's canonical name.
        let (out, _) = repaired(&schema, json!({"file_path": "a.rs"}));
        assert_eq!(out["path"], "a.rs");

        // A present `path` wins; the redundant alias is dropped, not bounced.
        let (out, notes) = repaired(&schema, json!({"path": "keep.rs", "file": "drop.rs"}));
        assert_eq!(out["path"], "keep.rs");
        assert!(out.get("file").is_none());
        assert_eq!(notes, ["file: mapped alias onto its canonical field"]);

        // Non-string alias values are left for rejected-fields guidance.
        let (out, notes) = repaired(&schema, json!({"file": 42}));
        assert!(out.get("path").is_none());
        assert_eq!(out["file"], 42);
        assert!(notes.is_empty());
    }

    #[test]
    fn drops_null_for_optional_field_but_keeps_required_null() {
        let schema = edit_like_schema();

        let (out, notes) = repaired(&schema, json!({"path": null, "note": null}));

        assert_eq!(out, json!({"path": null}));
        assert_eq!(notes, ["note: dropped null for optional field"]);
    }

    #[test]
    fn drops_undeclared_description_from_closed_tool_schema() {
        let schema = closed_object(
            [("command", string_property("Shell command"))],
            &["command"],
        );

        let (out, notes) = repaired(
            &schema,
            json!({"command": "cargo check", "description": "Check the crate"}),
        );

        assert_eq!(out, json!({"command": "cargo check"}));
        assert_eq!(
            notes,
            ["description: dropped undeclared descriptive metadata"]
        );
    }

    #[test]
    fn preserves_declared_or_non_string_description_fields() {
        let declared = closed_object(
            [("description", string_property("Required description"))],
            &["description"],
        );
        let (out, notes) = repaired(&declared, json!({"description": "keep"}));
        assert_eq!(out, json!({"description": "keep"}));
        assert!(notes.is_empty());

        let closed = closed_object(
            [("command", string_property("Shell command"))],
            &["command"],
        );
        let args = json!({"command": "true", "description": {"unsafe": "shape"}});
        let (out, notes) = repaired(&closed, args.clone());
        assert_eq!(out, args);
        assert!(notes.is_empty());
    }

    #[test]
    fn parses_stringified_array_and_object() {
        let schema = object(
            [
                ("files", array_property("Files", string_property("File"))),
                ("options", object([("deep", boolean_property("Deep"))], &[])),
            ],
            &[],
        );

        let (out, notes) = repaired(
            &schema,
            json!({"files": "[\"a.rs\",\"b.rs\"]", "options": "{\"deep\":true}"}),
        );

        assert_eq!(
            out,
            json!({"files": ["a.rs", "b.rs"], "options": {"deep": true}})
        );
        assert_eq!(notes.len(), 2, "{notes:?}");
    }

    #[test]
    fn embedded_json_of_the_wrong_shape_is_untouched() {
        let schema = object([("edits", array_property("Edits", object([], &[])))], &[]);
        // Parses as an array of numbers, but items say object — leave it for
        // the tool's serde guidance rather than guessing.
        let args = json!({"edits": "[1,2]"});

        let (out, notes) = repaired(&schema, args.clone());

        assert_eq!(out, args);
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn wraps_single_object_for_array_of_objects() {
        let schema = edit_like_schema();

        let (out, notes) = repaired(
            &schema,
            json!({"path": "f.rs", "edits": {"old_string": "a", "new_string": "b"}}),
        );

        assert_eq!(
            out,
            json!({"path": "f.rs", "edits": [{"old_string": "a", "new_string": "b"}]})
        );
        assert_eq!(notes, ["edits: wrapped single value in an array"]);
    }

    #[test]
    fn wraps_scalar_for_array_when_items_type_matches() {
        let schema = object(
            [
                ("files", array_property("Files", string_property("File"))),
                ("flags", array_property("Flags", boolean_property("Flag"))),
            ],
            &[],
        );

        let (out, notes) = repaired(&schema, json!({"files": "src/main.rs", "flags": "nope"}));

        // A bare string wraps for an array of strings; a string where an array
        // of booleans is expected has no safe repair.
        assert_eq!(out, json!({"files": ["src/main.rs"], "flags": "nope"}));
        assert_eq!(notes, ["files: wrapped single value in an array"]);
    }

    #[test]
    fn parses_scalar_strings() {
        let schema = object(
            [
                ("count", bounded_integer_property("Count", None, None)),
                // No number_property helper exists; shape matches typed_property.
                ("ratio", json!({"type": "number", "description": "Ratio"})),
                ("deep", boolean_property("Deep")),
            ],
            &[],
        );

        let (out, notes) = repaired(
            &schema,
            json!({"count": " 5 ", "ratio": "3.5", "deep": "True"}),
        );

        assert_eq!(out, json!({"count": 5, "ratio": 3.5, "deep": true}));
        assert_eq!(notes.len(), 3, "{notes:?}");
    }

    #[test]
    fn non_token_scalar_strings_are_untouched() {
        let schema = object(
            [
                ("count", bounded_integer_property("Count", None, None)),
                ("deep", boolean_property("Deep")),
            ],
            &[],
        );
        let args = json!({"count": "5x", "deep": "yes"});

        let (out, notes) = repaired(&schema, args.clone());

        assert_eq!(out, args);
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn unwraps_markdown_link_only_on_path_format_fields() {
        let schema = edit_like_schema();

        let (out, notes) = repaired(
            &schema,
            json!({"path": "[src/main.rs](src/main.rs)", "note": "[a](a)"}),
        );

        // `note` is a general string — `[a](a)` may be legitimate content.
        assert_eq!(out, json!({"path": "src/main.rs", "note": "[a](a)"}));
        assert_eq!(notes, ["path: unwrapped markdown link around path"]);
    }

    #[test]
    fn markdown_link_takes_the_target_and_rejects_partial_links() {
        assert_eq!(
            markdown_link_target("[main.rs](src/main.rs)"),
            Some("src/main.rs")
        );
        // Embedded (non-full-string) links and odd shapes never match.
        assert_eq!(markdown_link_target("see [a](a) here"), None);
        assert_eq!(markdown_link_target("[a](a b)"), None);
        assert_eq!(markdown_link_target("[a]()"), None);
        assert_eq!(markdown_link_target("plain/path.rs"), None);
    }

    #[test]
    fn canonicalizes_enum_case_on_unique_match() {
        let schema = edit_like_schema();

        let (out, notes) = repaired(&schema, json!({"path": "f.rs", "mode": "Add"}));
        assert_eq!(out, json!({"path": "f.rs", "mode": "add"}));
        assert_eq!(notes, ["mode: canonicalized enum value case"]);

        // A value matching no enum entry stays for the tool's guidance.
        let args = json!({"path": "f.rs", "mode": "append"});
        let (out, notes) = repaired(&schema, args.clone());
        assert_eq!(out, args);
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn repairs_nested_array_item_fields() {
        let schema = edit_like_schema();

        let (out, notes) = repaired(
            &schema,
            json!({
                "path": "f.rs",
                "edits": [
                    {"old_string": "a", "new_string": "b", "replace_all": "true"},
                    "{\"old_string\":\"c\",\"new_string\":\"d\"}",
                ],
            }),
        );

        assert_eq!(
            out,
            json!({
                "path": "f.rs",
                "edits": [
                    {"old_string": "a", "new_string": "b", "replace_all": true},
                    {"old_string": "c", "new_string": "d"},
                ],
            })
        );
        assert_eq!(
            notes,
            [
                "edits[0].replace_all: parsed scalar from a string",
                "edits[1]: parsed JSON embedded in a string",
            ]
        );
    }

    #[test]
    fn edit_dsl_strings_pass_through_for_the_edit_tool_coercer() {
        // `normalize_edit_args` (edit.rs) owns the ad-hoc DSL; a non-JSON
        // string element must reach it untouched.
        let schema = edit_like_schema();
        let args = json!({"path": "f.rs", "edits": ["replace: OLD -> NEW"]});

        let (out, notes) = repaired(&schema, args.clone());

        assert_eq!(out, args);
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn undeclared_fields_are_never_touched() {
        let schema = edit_like_schema();
        let args = json!({"path": "f.rs", "extra": null, "junk": "[\"a\"]"});

        let (out, notes) = repaired(&schema, args.clone());

        assert_eq!(out, args);
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn unwrap_double_encoded_arguments_only_for_objects() {
        let mut doubled = json!("{\"path\": \"f.rs\"}");
        let note = unwrap_double_encoded_arguments(&mut doubled);
        assert_eq!(doubled, json!({"path": "f.rs"}));
        assert_eq!(
            note.map(|note| note.to_string()).as_deref(),
            Some("<arguments>: parsed JSON embedded in a string")
        );

        let mut array = json!("[1,2]");
        assert!(unwrap_double_encoded_arguments(&mut array).is_none());
        assert_eq!(array, json!("[1,2]"));

        let mut prose = json!("not json");
        assert!(unwrap_double_encoded_arguments(&mut prose).is_none());

        let mut already_object = json!({"path": "f.rs"});
        assert!(unwrap_double_encoded_arguments(&mut already_object).is_none());
    }
}
