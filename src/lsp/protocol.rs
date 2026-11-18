use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::LspError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct LspPosition {
    pub(crate) line: u32,
    pub(crate) character: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct LspRange {
    pub(crate) start: LspPosition,
    pub(crate) end: LspPosition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LspLocation {
    pub(crate) path: PathBuf,
    pub(crate) range: LspRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum LspDiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

impl LspDiagnosticSeverity {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "info",
            Self::Hint => "hint",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LspDiagnostic {
    pub(crate) path: PathBuf,
    pub(crate) range: LspRange,
    pub(crate) severity: LspDiagnosticSeverity,
    pub(crate) code: Option<String>,
    pub(crate) source: Option<String>,
    pub(crate) message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LspSymbol {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) location: LspLocation,
    pub(crate) container_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawLocation {
    uri: String,
    range: LspRange,
}

#[derive(Debug, Deserialize)]
struct RawDiagnostic {
    range: LspRange,
    severity: Option<u8>,
    code: Option<Value>,
    source: Option<String>,
    message: String,
}

#[derive(Debug, Deserialize)]
struct RawPublishDiagnosticsParams {
    uri: String,
    diagnostics: Vec<RawDiagnostic>,
}

#[derive(Debug, Deserialize)]
struct RawDocumentDiagnosticReport {
    items: Vec<RawDiagnostic>,
}

#[derive(Debug, Deserialize)]
struct RawWorkspaceSymbol {
    name: String,
    kind: u32,
    location: Value,
    #[serde(rename = "containerName")]
    container_name: Option<String>,
}

fn location_from_raw(raw: RawLocation) -> Result<LspLocation, LspError> {
    Ok(LspLocation {
        path: uri_to_path(&raw.uri)?,
        range: raw.range,
    })
}

pub(crate) fn parse_workspace_symbols(value: Value) -> Result<Vec<LspSymbol>, LspError> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    let raw = serde_json::from_value::<Vec<RawWorkspaceSymbol>>(value)
        .map_err(|err| LspError::Protocol(format!("invalid workspace symbols: {err}")))?;
    Ok(raw
        .into_iter()
        .filter_map(|symbol| {
            let location = parse_symbol_location(symbol.location).ok()?;
            Some(LspSymbol {
                name: symbol.name,
                kind: symbol_kind_label(symbol.kind).to_string(),
                location,
                container_name: symbol.container_name,
            })
        })
        .collect())
}

fn parse_symbol_location(value: Value) -> Result<LspLocation, LspError> {
    if let Ok(location) = serde_json::from_value::<RawLocation>(value.clone()) {
        return location_from_raw(location);
    }
    let uri = value
        .get("uri")
        .and_then(Value::as_str)
        .ok_or_else(|| LspError::Protocol("symbol location missing uri".into()))?;
    let range = value
        .get("range")
        .cloned()
        .or_else(|| value.get("selectionRange").cloned())
        .ok_or_else(|| LspError::Protocol("symbol location missing range".into()))?;
    Ok(LspLocation {
        path: uri_to_path(uri)?,
        range: serde_json::from_value(range)
            .map_err(|err| LspError::Protocol(format!("invalid symbol range: {err}")))?,
    })
}

pub(crate) fn parse_publish_diagnostics(
    params: Value,
) -> Result<(PathBuf, Vec<LspDiagnostic>), LspError> {
    let raw = serde_json::from_value::<RawPublishDiagnosticsParams>(params)
        .map_err(|err| LspError::Protocol(format!("invalid diagnostics: {err}")))?;
    let path = uri_to_path(&raw.uri)?;
    let diagnostics = raw
        .diagnostics
        .into_iter()
        .map(|diagnostic| diagnostic_from_raw(path.clone(), diagnostic))
        .collect();
    Ok((path, diagnostics))
}

pub(crate) fn parse_document_diagnostic_report(
    file: &Path,
    value: Value,
) -> Result<Vec<LspDiagnostic>, LspError> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    let raw = serde_json::from_value::<RawDocumentDiagnosticReport>(value)
        .map_err(|err| LspError::Protocol(format!("invalid document diagnostics: {err}")))?;
    Ok(raw
        .items
        .into_iter()
        .map(|diagnostic| diagnostic_from_raw(file.to_path_buf(), diagnostic))
        .collect())
}

fn diagnostic_from_raw(path: PathBuf, raw: RawDiagnostic) -> LspDiagnostic {
    LspDiagnostic {
        path,
        range: raw.range,
        severity: match raw.severity.unwrap_or(2) {
            1 => LspDiagnosticSeverity::Error,
            2 => LspDiagnosticSeverity::Warning,
            3 => LspDiagnosticSeverity::Information,
            4 => LspDiagnosticSeverity::Hint,
            _ => LspDiagnosticSeverity::Warning,
        },
        code: raw.code.as_ref().map(code_to_string),
        source: raw.source,
        message: raw.message,
    }
}

fn code_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => value.to_string(),
    }
}

fn symbol_kind_label(kind: u32) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enum_member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type_parameter",
        _ => "symbol",
    }
}

pub(crate) fn path_to_file_uri(path: &Path) -> Result<String, LspError> {
    let path = path
        .canonicalize()
        .map_err(|err| LspError::Protocol(format!("failed to canonicalize path: {err}")))?;
    Ok(format!(
        "file://{}",
        percent_encode(path.to_string_lossy().as_bytes())
    ))
}

pub(crate) fn uri_to_path(uri: &str) -> Result<PathBuf, LspError> {
    let Some(path) = uri.strip_prefix("file://") else {
        return Err(LspError::Protocol(format!(
            "unsupported LSP URI (expected file://): {uri}"
        )));
    };
    let decoded = percent_decode(path)?;
    Ok(PathBuf::from(decoded))
}

fn percent_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for byte in bytes {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn percent_decode(input: &str) -> Result<String, LspError> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(LspError::Protocol("truncated URI escape".into()));
            }
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                .map_err(|err| LspError::Protocol(format!("invalid URI escape: {err}")))?;
            let value = u8::from_str_radix(hex, 16)
                .map_err(|err| LspError::Protocol(format!("invalid URI escape: {err}")))?;
            out.push(value);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|err| LspError::Protocol(format!("invalid UTF-8 in URI: {err}")))
}

impl fmt::Display for LspLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}",
            self.path.display(),
            self.range.start.line + 1,
            self.range.start.character + 1
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uri_round_trips_spaces() {
        let path = std::env::temp_dir().join("bonsai lsp uri.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();
        let uri = path_to_file_uri(&path).unwrap();
        assert!(uri.contains("%20"), "{uri}");
        assert_eq!(uri_to_path(&uri).unwrap(), path.canonicalize().unwrap());
        let _ = std::fs::remove_file(path);
    }
}
