use std::path::Path;

use anyhow::Result;
use serde::Serialize;

pub(crate) mod ecmascript;
pub(crate) mod go;
pub(crate) mod python;
pub(crate) mod rust;

pub(crate) const SYMBOL_KINDS: &[&str] = &[
    "function",
    "method",
    "class",
    "interface",
    "struct",
    "enum",
    "trait",
    "impl",
    "module",
    "const",
    "static",
    "variable",
    "type",
    "macro",
];

pub(crate) const SYMBOL_LANGUAGES: &[&str] =
    &["auto", "rust", "typescript", "javascript", "python", "go"];

pub(crate) const SYMBOL_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "py", "pyi", "go",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Interface,
    Struct,
    Enum,
    Trait,
    Impl,
    Module,
    Const,
    Static,
    Variable,
    TypeAlias,
    Macro,
}

impl SymbolKind {
    /// Short keyword used when listing a symbol (`fn`, `struct`, `trait`, …).
    /// Shared by symbol-search output and the repository map.
    pub fn label(self) -> &'static str {
        match self {
            SymbolKind::Function => "fn",
            SymbolKind::Method => "method",
            SymbolKind::Class => "class",
            SymbolKind::Interface => "interface",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Trait => "trait",
            SymbolKind::Impl => "impl",
            SymbolKind::Module => "mod",
            SymbolKind::Const => "const",
            SymbolKind::Static => "static",
            SymbolKind::Variable => "var",
            SymbolKind::TypeAlias => "type",
            SymbolKind::Macro => "macro",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Symbol {
    pub kind: SymbolKind,
    pub name: String,
    pub signature: String,
    pub line: usize,
    pub column: usize,
    pub end_line: usize,
    pub end_column: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    #[serde(skip)]
    pub is_public: bool,
}

pub trait SymbolExtractor: Send + Sync {
    fn extract(&self, path: &Path, source: &str) -> Result<Vec<Symbol>>;
}

pub fn extractor_for(language: &str) -> Option<Box<dyn SymbolExtractor>> {
    match language {
        "go" => Some(Box::new(go::GoSymbolExtractor::new())),
        "javascript" => Some(Box::new(ecmascript::EcmaScriptSymbolExtractor::javascript())),
        "python" => Some(Box::new(python::PythonSymbolExtractor::new())),
        "rust" => Some(Box::new(rust::RustSymbolExtractor::new())),
        "typescript" => Some(Box::new(ecmascript::EcmaScriptSymbolExtractor::typescript())),
        _ => None,
    }
}

pub(crate) fn language_for_path(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "rs" => Some("rust"),
        "ts" | "tsx" | "mts" | "cts" => Some("typescript"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "py" | "pyi" => Some("python"),
        "go" => Some("go"),
        _ => None,
    }
}

pub(crate) fn extractor_for_path(path: &Path) -> Option<Box<dyn SymbolExtractor>> {
    extractor_for(language_for_path(path)?)
}

pub(crate) fn parse_symbol_kind(kind: Option<&str>) -> Result<Option<SymbolKind>> {
    match kind {
        None => Ok(None),
        Some("function") => Ok(Some(SymbolKind::Function)),
        Some("method") => Ok(Some(SymbolKind::Method)),
        Some("class") => Ok(Some(SymbolKind::Class)),
        Some("interface") => Ok(Some(SymbolKind::Interface)),
        Some("struct") => Ok(Some(SymbolKind::Struct)),
        Some("enum") => Ok(Some(SymbolKind::Enum)),
        Some("trait") => Ok(Some(SymbolKind::Trait)),
        Some("impl") => Ok(Some(SymbolKind::Impl)),
        Some("module") => Ok(Some(SymbolKind::Module)),
        Some("const") => Ok(Some(SymbolKind::Const)),
        Some("static") => Ok(Some(SymbolKind::Static)),
        Some("variable") => Ok(Some(SymbolKind::Variable)),
        Some("type") => Ok(Some(SymbolKind::TypeAlias)),
        Some("macro") => Ok(Some(SymbolKind::Macro)),
        Some(other) => anyhow::bail!("Unsupported symbol kind: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extractor_for_supports_release_languages() {
        assert!(extractor_for("rust").is_some());
        assert!(extractor_for("typescript").is_some());
        assert!(extractor_for("javascript").is_some());
        assert!(extractor_for("python").is_some());
        assert!(extractor_for("go").is_some());
        assert!(extractor_for("").is_none());
    }

    #[test]
    fn language_detection_handles_release_source_extensions() {
        assert_eq!(language_for_path(Path::new("src/lib.rs")), Some("rust"));
        assert_eq!(
            language_for_path(Path::new("web/component.tsx")),
            Some("typescript")
        );
        assert_eq!(
            language_for_path(Path::new("web/index.mjs")),
            Some("javascript")
        );
        assert_eq!(
            language_for_path(Path::new("pkg/types.pyi")),
            Some("python")
        );
        assert_eq!(language_for_path(Path::new("cmd/main.go")), Some("go"));
        assert_eq!(language_for_path(Path::new("README.md")), None);
    }
}
