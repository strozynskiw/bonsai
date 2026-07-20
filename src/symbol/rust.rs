use std::cell::RefCell;
use std::path::Path;

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

use super::{Symbol, SymbolExtractor, SymbolKind};

thread_local! {
    /// Reused across `extract` calls on the same thread so the parser is
    /// allocated and `set_language` runs once per thread rather than once per
    /// file (symbol search invokes `extract` for every `.rs` file it walks).
    static RUST_PARSER: RefCell<Parser> = RefCell::new(new_rust_parser());
}

fn new_rust_parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .expect("tree-sitter-rust grammar is compiled into the binary");
    parser
}

pub(crate) struct RustSymbolExtractor {}

impl RustSymbolExtractor {
    pub fn new() -> Self {
        Self {}
    }

    fn source_for<'a>(source: &'a str, node: Node<'_>) -> &'a str {
        let start = node.start_byte();
        let end = node.end_byte();
        source.get(start..end).unwrap_or("")
    }

    fn kind_for(node_kind: &str) -> Option<SymbolKind> {
        match node_kind {
            "function_item" => Some(SymbolKind::Function),
            "struct_item" => Some(SymbolKind::Struct),
            "enum_item" => Some(SymbolKind::Enum),
            "trait_item" => Some(SymbolKind::Trait),
            "impl_item" => Some(SymbolKind::Impl),
            "mod_item" => Some(SymbolKind::Module),
            "const_item" => Some(SymbolKind::Const),
            "static_item" => Some(SymbolKind::Static),
            "type_item" => Some(SymbolKind::TypeAlias),
            "macro_definition" => Some(SymbolKind::Macro),
            _ => None,
        }
    }

    fn name_for(source: &str, node: Node<'_>) -> String {
        node.child_by_field_name("name")
            .map(|name| Self::source_for(source, name).trim().to_string())
            .unwrap_or_else(|| match node.kind() {
                // `impl_item` has no `name` field; surface the implemented type
                // (the `type` field) so impls are searchable by their type name.
                "impl_item" => node
                    .child_by_field_name("type")
                    .map(|ty| format!("impl {}", Self::source_for(source, ty).trim()))
                    .unwrap_or_else(|| "impl".to_string()),
                _ => node.kind().to_string(),
            })
    }

    fn signature_for(source: &str, node: Node<'_>) -> String {
        let text = Self::source_for(source, node).trim();
        text.lines().next().unwrap_or(text).trim().to_string()
    }

    /// Pre-order traversal of the syntax tree using an explicit stack rather
    /// than recursion, so deeply nested input cannot overflow the call stack.
    fn walk(&self, source: &str, root: Node<'_>, out: &mut Vec<Symbol>) {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if let Some(kind) = Self::kind_for(node.kind()) {
                let position = node.start_position();
                let end_position = node.end_position();
                let signature = Self::signature_for(source, node);
                out.push(Symbol {
                    kind,
                    name: Self::name_for(source, node),
                    is_public: signature.trim_start().starts_with("pub"),
                    signature,
                    line: position.row + 1,
                    column: position.column + 1,
                    end_line: end_position.row + 1,
                    end_column: end_position.column + 1,
                    start_byte: node.start_byte(),
                    end_byte: node.end_byte(),
                });
            }

            // Push children in reverse so they pop in source order, preserving
            // the original pre-order output.
            let mut cursor = node.walk();
            let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
    }
}

impl Default for RustSymbolExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl SymbolExtractor for RustSymbolExtractor {
    fn extract(&self, _path: &Path, source: &str) -> Result<Vec<Symbol>> {
        let tree = RUST_PARSER
            .with(|parser| parser.borrow_mut().parse(source, None))
            .context("Failed to parse Rust source")?;
        let mut symbols = Vec::new();
        self.walk(source, tree.root_node(), &mut symbols);
        Ok(symbols)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_symbols() {
        let extractor = RustSymbolExtractor::new();
        let source = r#"
pub struct Foo;
enum Bar { A }
trait Baz {}
impl Foo { fn method(&self) {} }
const X: i32 = 1;
static Y: i32 = 2;
type Alias = Foo;
macro_rules! hello { () => {} }
fn top_level() {}
"#;

        let symbols = extractor.extract(Path::new("src/lib.rs"), source).unwrap();
        assert!(
            symbols
                .iter()
                .any(|s| s.kind == SymbolKind::Struct && s.name == "Foo")
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.kind == SymbolKind::Enum && s.name == "Bar")
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.kind == SymbolKind::Trait && s.name == "Baz")
        );
        // `impl Foo` is indexed under a name that carries the implemented type,
        // so it is reachable by a `query` that names the type.
        assert!(
            symbols
                .iter()
                .any(|s| s.kind == SymbolKind::Impl && s.name.contains("Foo"))
        );
        // The walk descends into the impl body and surfaces nested items.
        assert!(
            symbols
                .iter()
                .any(|s| s.kind == SymbolKind::Function && s.name == "method")
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.kind == SymbolKind::Const && s.name == "X")
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.kind == SymbolKind::Static && s.name == "Y")
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.kind == SymbolKind::TypeAlias && s.name == "Alias")
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.kind == SymbolKind::Function && s.name == "top_level")
        );
    }

    #[test]
    fn reports_one_based_line_and_column() {
        let extractor = RustSymbolExtractor::new();
        // `fn first` starts on line 1, column 1; `fn second` on line 3.
        let source = "fn first() {}\n\nfn second() {}\n";

        let symbols = extractor.extract(Path::new("src/lib.rs"), source).unwrap();
        let first = symbols.iter().find(|s| s.name == "first").unwrap();
        let second = symbols.iter().find(|s| s.name == "second").unwrap();

        assert_eq!((first.line, first.column), (1, 1));
        assert_eq!(first.end_line, 1);
        assert_eq!(second.line, 3);
    }

    #[test]
    fn reports_line_and_byte_ranges() {
        let extractor = RustSymbolExtractor::new();
        let source = "fn first() {\n    println!(\"one\");\n}\nfn second() {}\n";

        let symbols = extractor.extract(Path::new("src/lib.rs"), source).unwrap();
        let first = symbols.iter().find(|s| s.name == "first").unwrap();

        assert_eq!(first.line, 1);
        assert_eq!(first.end_line, 3);
        assert_eq!(
            &source[first.start_byte..first.end_byte],
            "fn first() {\n    println!(\"one\");\n}"
        );
    }
}
