use std::cell::RefCell;
use std::path::Path;

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

use super::{Symbol, SymbolExtractor, SymbolKind};

thread_local! {
    static GO_PARSER: RefCell<Parser> = RefCell::new(new_go_parser());
}

fn new_go_parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_go::LANGUAGE.into())
        .expect("tree-sitter-go grammar is compiled into the binary");
    parser
}

#[derive(Debug, Default)]
pub(crate) struct GoSymbolExtractor;

impl GoSymbolExtractor {
    pub(crate) fn new() -> Self {
        Self
    }

    fn source_for<'a>(source: &'a str, node: Node<'_>) -> &'a str {
        source.get(node.byte_range()).unwrap_or("")
    }

    fn is_exported(name: &str) -> bool {
        name.chars().next().is_some_and(char::is_uppercase)
    }

    fn declaration_range<'tree>(node: Node<'tree>, declaration_kind: &str) -> Node<'tree> {
        node.parent()
            .filter(|parent| parent.kind() == declaration_kind && parent.named_child_count() == 1)
            .unwrap_or(node)
    }

    fn is_package_scope(node: Node<'_>) -> bool {
        let mut parent = node.parent();
        while let Some(ancestor) = parent {
            match ancestor.kind() {
                "source_file" => return true,
                "block" | "function_declaration" | "method_declaration" => return false,
                _ => parent = ancestor.parent(),
            }
        }
        false
    }

    fn make_symbol(
        source: &str,
        kind: SymbolKind,
        name_node: Node<'_>,
        range_node: Node<'_>,
        signature_prefix: Option<&str>,
    ) -> Option<Symbol> {
        let name = Self::source_for(source, name_node).trim().to_string();
        if name.is_empty() {
            return None;
        }
        let range_text = Self::source_for(source, range_node).trim();
        let first_line = range_text.lines().next().unwrap_or(range_text).trim();
        let signature = signature_prefix.map_or_else(
            || first_line.to_string(),
            |prefix| format!("{prefix} {first_line}"),
        );
        let position = range_node.start_position();
        let end_position = range_node.end_position();
        Some(Symbol {
            kind,
            is_public: Self::is_exported(&name),
            name,
            signature,
            line: position.row + 1,
            column: position.column + 1,
            end_line: end_position.row + 1,
            end_column: end_position.column + 1,
            start_byte: range_node.start_byte(),
            end_byte: range_node.end_byte(),
        })
    }

    fn declaration_values(
        source: &str,
        node: Node<'_>,
        kind: SymbolKind,
        declaration_kind: &str,
        signature_prefix: &str,
    ) -> Vec<Symbol> {
        if !Self::is_package_scope(node) {
            return Vec::new();
        }
        let range = Self::declaration_range(node, declaration_kind);
        let prefix = (range == node).then_some(signature_prefix);
        let mut cursor = node.walk();
        node.children_by_field_name("name", &mut cursor)
            .filter_map(|name| Self::make_symbol(source, kind, name, range, prefix))
            .collect()
    }

    fn symbol_for(source: &str, node: Node<'_>) -> Option<Symbol> {
        let (kind, name, range, prefix) = match node.kind() {
            "function_declaration" => (
                SymbolKind::Function,
                node.child_by_field_name("name")?,
                node,
                None,
            ),
            "method_declaration" | "method_elem" => (
                SymbolKind::Method,
                node.child_by_field_name("name")?,
                node,
                None,
            ),
            "type_alias" => {
                let range = Self::declaration_range(node, "type_declaration");
                (
                    SymbolKind::TypeAlias,
                    node.child_by_field_name("name")?,
                    range,
                    (range == node).then_some("type"),
                )
            }
            "type_spec" => {
                let type_node = node.child_by_field_name("type")?;
                let kind = match type_node.kind() {
                    "struct_type" => SymbolKind::Struct,
                    "interface_type" => SymbolKind::Interface,
                    _ => SymbolKind::TypeAlias,
                };
                let range = Self::declaration_range(node, "type_declaration");
                (
                    kind,
                    node.child_by_field_name("name")?,
                    range,
                    (range == node).then_some("type"),
                )
            }
            _ => return None,
        };
        Self::make_symbol(source, kind, name, range, prefix)
    }

    fn walk(&self, source: &str, root: Node<'_>) -> Vec<Symbol> {
        let mut symbols = Vec::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "const_spec" => symbols.extend(Self::declaration_values(
                    source,
                    node,
                    SymbolKind::Const,
                    "const_declaration",
                    "const",
                )),
                "var_spec" => symbols.extend(Self::declaration_values(
                    source,
                    node,
                    SymbolKind::Variable,
                    "var_declaration",
                    "var",
                )),
                _ => {
                    if let Some(symbol) = Self::symbol_for(source, node) {
                        symbols.push(symbol);
                    }
                }
            }
            let mut cursor = node.walk();
            let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
        symbols
    }
}

impl SymbolExtractor for GoSymbolExtractor {
    fn extract(&self, _path: &Path, source: &str) -> Result<Vec<Symbol>> {
        let tree = GO_PARSER
            .with(|parser| parser.borrow_mut().parse(source, None))
            .context("Failed to parse Go source")?;
        Ok(self.walk(source, tree.root_node()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_go_functions_methods_types_and_package_values() {
        let source = r#"package client

const (
    DefaultTimeout = 10
    privateTimeout = 5
)
var ClientCount, retryCount int

type Client struct{}
type Store interface { Get() string }
type ClientID = string

func NewClient() Client { return Client{} }
func (Client) Request() {}
func local() {
    var ignored int
}
"#;
        let symbols = GoSymbolExtractor::new()
            .extract(Path::new("client.go"), source)
            .unwrap();

        for (kind, name) in [
            (SymbolKind::Const, "DefaultTimeout"),
            (SymbolKind::Const, "privateTimeout"),
            (SymbolKind::Variable, "ClientCount"),
            (SymbolKind::Variable, "retryCount"),
            (SymbolKind::Struct, "Client"),
            (SymbolKind::Interface, "Store"),
            (SymbolKind::TypeAlias, "ClientID"),
            (SymbolKind::Function, "NewClient"),
            (SymbolKind::Method, "Request"),
        ] {
            assert!(
                symbols
                    .iter()
                    .any(|symbol| symbol.kind == kind && symbol.name == name),
                "missing {kind:?} {name}: {symbols:#?}"
            );
        }
        assert!(!symbols.iter().any(|symbol| symbol.name == "ignored"));
        assert!(
            symbols
                .iter()
                .find(|symbol| symbol.name == "NewClient")
                .unwrap()
                .is_public
        );
        assert!(
            !symbols
                .iter()
                .find(|symbol| symbol.name == "privateTimeout")
                .unwrap()
                .is_public
        );
    }
}
