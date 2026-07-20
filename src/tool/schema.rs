use anyhow::{Result, anyhow};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

/// Longest rejected payload echoed back verbatim; beyond this we truncate so one
/// bad call can't flood context with its own arguments.
const MAX_BAD_PAYLOAD_CHARS: usize = 512;

pub(crate) fn parse_args<T>(tool_name: &str, args: Value) -> Result<T>
where
    T: DeserializeOwned,
{
    parse_args_with_schema(tool_name, args, None)
}

pub(crate) fn parse_args_with_schema<T>(
    tool_name: &str,
    args: Value,
    schema: Option<&Value>,
) -> Result<T>
where
    T: DeserializeOwned,
{
    let bad_payload = args.to_string();
    serde_json::from_value(args).map_err(|err| {
        let shown = truncate_payload(&bad_payload);
        let schema_hint = schema.and_then(compact_schema_hint);
        if let Some(hint) = schema_hint {
            anyhow!(
                "Failed to parse {tool_name} arguments. Schema: {hint}. Got: {shown}. Error: {err}"
            )
        } else {
            anyhow!("Failed to parse {tool_name} arguments. Got: {shown}. Error: {err}")
        }
    })
}

pub(crate) fn compact_schema_hint(schema: &Value) -> Option<String> {
    compact_schema_hint_depth(schema, 0)
}

fn compact_schema_hint_depth(schema: &Value, depth: usize) -> Option<String> {
    let properties = schema.get("properties").and_then(Value::as_object)?;
    if properties.is_empty() {
        return Some("object with no fields".to_string());
    }
    let required: Vec<String> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect();
    let fields: Vec<String> = properties
        .iter()
        .map(|(name, property)| {
            let mut details = Vec::new();
            if required.iter().any(|f| f == name) {
                details.push("required".to_string());
            }
            if let Some(type_name) = property.get("type").and_then(Value::as_str) {
                details.push(type_name.to_string());
                if type_name == "array"
                    && depth == 0
                    && let Some(item_hint) = property
                        .get("items")
                        .and_then(|items| compact_schema_hint_depth(items, depth + 1))
                {
                    details.push(format!("items: {{{item_hint}}}"));
                }
            }
            if let Some(values) = property.get("enum").and_then(Value::as_array) {
                let enum_values: Vec<&str> =
                    values.iter().filter_map(Value::as_str).take(6).collect();
                if !enum_values.is_empty() {
                    let suffix = if values.len() > enum_values.len() {
                        ", ..."
                    } else {
                        ""
                    };
                    details.push(format!("one of {}{suffix}", enum_values.join("|")));
                }
            }
            if let Some(minimum) = property.get("minimum").and_then(Value::as_i64) {
                details.push(format!("min {minimum}"));
            }
            if let Some(maximum) = property.get("maximum").and_then(Value::as_i64) {
                details.push(format!("max {maximum}"));
            }
            if details.is_empty() {
                name.clone()
            } else {
                format!("{name} ({})", details.join(", "))
            }
        })
        .collect();
    Some(format!("object fields: {}", fields.join("; ")))
}

/// Echo a short payload as-is; truncate a long one on a char boundary with a
/// marker noting how much was elided. The serde `Error` already names the
/// offending field, so the full payload is rarely needed to act on the failure.
fn truncate_payload(payload: &str) -> std::borrow::Cow<'_, str> {
    match payload.char_indices().nth(MAX_BAD_PAYLOAD_CHARS) {
        Some((byte_idx, _)) => {
            let elided = payload.chars().count() - MAX_BAD_PAYLOAD_CHARS;
            std::borrow::Cow::Owned(format!(
                "{}…[+{elided} chars truncated]",
                &payload[..byte_idx]
            ))
        }
        None => std::borrow::Cow::Borrowed(payload),
    }
}

pub(crate) fn object<'a>(
    properties: impl IntoIterator<Item = (&'a str, Value)>,
    required: &[&str],
) -> Value {
    build_object(properties, required, false)
}

/// Like [`object`], but adds `"additionalProperties": false` so the schema
/// rejects fields the tool does not declare. Use for tools with a fixed,
/// known argument set; keep [`object`] for tools that must tolerate legacy or
/// provider-injected extras (the serde structs stay lenient either way).
pub(crate) fn closed_object<'a>(
    properties: impl IntoIterator<Item = (&'a str, Value)>,
    required: &[&str],
) -> Value {
    build_object(properties, required, true)
}

fn build_object<'a>(
    properties: impl IntoIterator<Item = (&'a str, Value)>,
    required: &[&str],
    closed: bool,
) -> Value {
    let properties = properties
        .into_iter()
        .map(|(name, schema)| (name.to_string(), schema))
        .collect::<Map<_, _>>();

    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("object".to_string()));
    schema.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert(
            "required".to_string(),
            Value::Array(
                required
                    .iter()
                    .map(|field| Value::String((*field).to_string()))
                    .collect(),
            ),
        );
    }
    if closed {
        schema.insert("additionalProperties".to_string(), Value::Bool(false));
    }
    Value::Object(schema)
}

pub(crate) fn string_property(description: &str) -> Value {
    typed_property("string", description)
}

/// A string property carrying `"format": "path"`. The marker gates the
/// path-only repairs in [`crate::tool::arg_repair`] (e.g. unwrapping the
/// markdown auto-links some models emit around file paths) to fields that are
/// actually paths — a general string field may legitimately *contain*
/// `[text](target)`. Providers ignore the unknown format keyword.
pub(crate) fn path_property(description: &str) -> Value {
    with_property(
        string_property(description),
        "format",
        Value::String("path".to_string()),
    )
}

pub(crate) fn string_enum_property(description: &str, values: &[&str]) -> Value {
    with_enum(string_property(description), values)
}

pub(crate) fn integer_property(description: &str) -> Value {
    typed_property("integer", description)
}

/// An integer property carrying JSON Schema `minimum`/`maximum` bounds, so the
/// model self-corrects an out-of-range value instead of paying a repair turn.
/// Tools keep their runtime checks as defense-in-depth — not every provider
/// enforces schema bounds.
pub(crate) fn bounded_integer_property(
    description: &str,
    min: Option<i64>,
    max: Option<i64>,
) -> Value {
    let mut schema = integer_property(description);
    if let Some(min) = min {
        schema = with_property(schema, "minimum", Value::Number(min.into()));
    }
    if let Some(max) = max {
        schema = with_property(schema, "maximum", Value::Number(max.into()));
    }
    schema
}

pub(crate) fn boolean_property(description: &str) -> Value {
    typed_property("boolean", description)
}

pub(crate) fn array_property(description: &str, items: Value) -> Value {
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("array".to_string()));
    schema.insert(
        "description".to_string(),
        Value::String(description.to_string()),
    );
    schema.insert("items".to_string(), items);
    Value::Object(schema)
}

pub(crate) fn with_property(mut schema: Value, key: &str, value: Value) -> Value {
    if let Value::Object(properties) = &mut schema {
        properties.insert(key.to_string(), value);
    }
    schema
}

fn typed_property(type_name: &str, description: &str) -> Value {
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String(type_name.to_string()));
    schema.insert(
        "description".to_string(),
        Value::String(description.to_string()),
    );
    Value::Object(schema)
}

fn with_enum(schema: Value, values: &[&str]) -> Value {
    with_property(
        schema,
        "enum",
        Value::Array(
            values
                .iter()
                .map(|value| Value::String((*value).to_string()))
                .collect(),
        ),
    )
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[test]
    fn object_omits_empty_required_list() {
        let schema = object([("query", string_property("Search text"))], &[]);

        assert!(schema.get("required").is_none());
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["query"]["description"], "Search text");
    }

    #[test]
    fn path_property_marks_format_path() {
        let schema = path_property("File path");

        assert_eq!(schema["type"], "string");
        assert_eq!(schema["description"], "File path");
        assert_eq!(schema["format"], "path");
    }

    #[test]
    fn enum_property_preserves_values() {
        let schema = string_enum_property("Mode", &["list", "wait"]);

        assert_eq!(schema["type"], "string");
        assert_eq!(schema["description"], "Mode");
        assert_eq!(schema["enum"], serde_json::json!(["list", "wait"]));
    }

    #[test]
    fn bounded_integer_property_emits_present_bounds_only() {
        let both = bounded_integer_property("Depth", Some(1), Some(6));
        assert_eq!(both["type"], "integer");
        assert_eq!(both["minimum"], 1);
        assert_eq!(both["maximum"], 6);

        let min_only = bounded_integer_property("Limit", Some(1), None);
        assert_eq!(min_only["minimum"], 1);
        assert!(min_only.get("maximum").is_none());
    }

    #[test]
    fn closed_object_sets_additional_properties_false() {
        let open = object([("path", string_property("Path"))], &["path"]);
        assert!(open.get("additionalProperties").is_none());

        let closed = closed_object([("path", string_property("Path"))], &["path"]);
        assert_eq!(
            closed["additionalProperties"],
            serde_json::Value::Bool(false)
        );
        assert_eq!(closed["required"], serde_json::json!(["path"]));
    }

    #[test]
    fn parse_args_truncates_long_payload_but_keeps_short() {
        #[derive(Debug, Deserialize)]
        struct Args {
            #[serde(rename = "title")]
            _title: String,
        }

        let long_value = "x".repeat(MAX_BAD_PAYLOAD_CHARS + 50);
        let err = parse_args::<Args>("demo", serde_json::json!({"junk": long_value}))
            .expect_err("missing field should fail");
        let text = err.to_string();
        assert!(text.contains("chars truncated]"), "{text}");

        let short = parse_args::<Args>("demo", serde_json::json!({"junk": "tiny"}))
            .expect_err("missing field should fail")
            .to_string();
        assert!(short.contains("\"junk\":\"tiny\""), "{short}");
        assert!(!short.contains("truncated"), "{short}");
    }

    #[test]
    fn parse_args_reports_tool_name_payload_and_parse_detail() {
        #[derive(Debug, Deserialize)]
        struct Args {
            #[serde(rename = "title")]
            _title: String,
        }

        let err = parse_args::<Args>("demo", serde_json::json!({"name": "wrong"}))
            .expect_err("missing field should fail");
        let text = err.to_string();

        assert!(text.contains("demo"), "{text}");
        assert!(text.contains("\"name\":\"wrong\""), "{text}");
        assert!(text.contains("missing field `title`"), "{text}");
    }

    #[test]
    fn compact_schema_hint_includes_nested_array_items() {
        let schema = object(
            [
                ("path", string_property("File path")),
                (
                    "edits",
                    array_property(
                        "Regions",
                        object(
                            [
                                ("old_string", string_property("Text to find")),
                                ("new_string", string_property("Replacement")),
                            ],
                            &["old_string", "new_string"],
                        ),
                    ),
                ),
            ],
            &["path"],
        );

        let hint = compact_schema_hint(&schema).expect("schema has properties");
        // Top-level fields appear.
        assert!(hint.contains("path (required, string)"), "{hint}");
        // Array field names its items with required fields.
        assert!(hint.contains("edits (array, items: {"), "{hint}");
        assert!(hint.contains("old_string (required, string)"), "{hint}");
        assert!(hint.contains("new_string (required, string)"), "{hint}");
    }

    #[test]
    fn parse_args_with_schema_includes_hint_on_failure() {
        #[derive(Debug, Deserialize)]
        struct Args {
            #[serde(rename = "title")]
            _title: String,
        }

        let schema = object([("title", string_property("Title"))], &["title"]);
        let err = parse_args_with_schema::<Args>(
            "demo",
            serde_json::json!({"wrong": "value"}),
            Some(&schema),
        )
        .expect_err("missing field should fail");

        let text = err.to_string();
        assert!(text.contains("Schema:"), "{text}");
        assert!(text.contains("title"), "{text}");
    }
}
