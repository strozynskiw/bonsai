use std::cell::RefCell;
use std::path::Path;

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

use super::{Symbol, SymbolExtractor, SymbolKind};

thread_local! {
    static PYTHON_PARSER: RefCell<Parser> = RefCell::new(new_python_parser());
}

fn new_python_parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .expect("tree-sitter-python grammar is compiled into the binary");
    parser
}

#[derive(Debug, Default)]
pub(crate) struct PythonSymbolExtractor;

impl PythonSymbolExtractor {
    pub(crate) fn new() -> Self {
        Self
    }

    fn source_for<'a>(source: &'a str, node: Node<'_>) -> &'a str {
        source.get(node.byte_range()).unwrap_or("")
    }

    fn public_name(name: &str) -> bool {
        !name.starts_with('_') || (name.starts_with("__") && name.ends_with("__"))
    }

    fn function_kind(node: Node<'_>) -> SymbolKind {
        let mut parent = node.parent();
        while let Some(ancestor) = parent {
            match ancestor.kind() {
                "class_definition" => return SymbolKind::Method,
                "function_definition" => return SymbolKind::Function,
                _ => parent = ancestor.parent(),
            }
        }
        SymbolKind::Function
    }

    fn range_node(node: Node<'_>) -> Node<'_> {
        node.parent()
            .filter(|parent| parent.kind() == "decorated_definition")
            .unwrap_or(node)
    }

    fn is_module_assignment(node: Node<'_>) -> bool {
        node.parent()
            .filter(|parent| parent.kind() == "expression_statement")
            .and_then(|parent| parent.parent())
            .is_some_and(|parent| parent.kind() == "module")
    }

    fn symbol_for(&self, source: &str, node: Node<'_>) -> Option<Symbol> {
        let (kind, name_node, range_node) = match node.kind() {
            "class_definition" => (
                SymbolKind::Class,
                node.child_by_field_name("name")?,
                Self::range_node(node),
            ),
            "function_definition" => (
                Self::function_kind(node),
                node.child_by_field_name("name")?,
                Self::range_node(node),
            ),
            "type_alias_statement" => (
                SymbolKind::TypeAlias,
                node.child_by_field_name("left")?,
                node,
            ),
            "assignment" if Self::is_module_assignment(node) => {
                let name = node.child_by_field_name("left")?;
                if name.kind() != "identifier" {
                    return None;
                }
                let name_text = Self::source_for(source, name);
                let kind = if name_text
                    .chars()
                    .filter(|character| character.is_alphabetic())
                    .all(|character| character.is_uppercase())
                {
                    SymbolKind::Const
                } else {
                    SymbolKind::Variable
                };
                (kind, name, node.parent().unwrap_or(node))
            }
            _ => return None,
        };
        let name = Self::source_for(source, name_node).trim().to_string();
        if name.is_empty() {
            return None;
        }
        let position = range_node.start_position();
        let end_position = range_node.end_position();
        let signature_text = Self::source_for(source, node).trim();
        let signature = signature_text
            .lines()
            .next()
            .unwrap_or(signature_text)
            .trim()
            .to_string();
        Some(Symbol {
            kind,
            is_public: Self::public_name(&name),
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

    fn walk(&self, source: &str, root: Node<'_>) -> Vec<Symbol> {
        let mut symbols = Vec::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if let Some(symbol) = self.symbol_for(source, node) {
                symbols.push(symbol);
            }
            let mut cursor = node.walk();
            let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
        symbols
    }
}

impl SymbolExtractor for PythonSymbolExtractor {
    fn extract(&self, _path: &Path, source: &str) -> Result<Vec<Symbol>> {
        let tree = PYTHON_PARSER
            .with(|parser| parser.borrow_mut().parse(source, None))
            .context("Failed to parse Python source")?;
        Ok(self.walk(source, tree.root_node()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_python_definitions_and_module_values() {
        let source = r#"MAX_RETRIES = 3
value = 1

class Client:
    @classmethod
    def build(cls) -> "Client":
        return cls()

def run() -> None:
    pass

type Result = str | int
"#;
        let symbols = PythonSymbolExtractor::new()
            .extract(Path::new("client.py"), source)
            .unwrap();

        assert!(
            symbols
                .iter()
                .any(|symbol| { symbol.kind == SymbolKind::Const && symbol.name == "MAX_RETRIES" })
        );
        assert!(
            symbols
                .iter()
                .any(|symbol| { symbol.kind == SymbolKind::Variable && symbol.name == "value" })
        );
        assert!(
            symbols
                .iter()
                .any(|symbol| symbol.kind == SymbolKind::Class && symbol.name == "Client")
        );
        let method = symbols
            .iter()
            .find(|symbol| symbol.kind == SymbolKind::Method && symbol.name == "build")
            .unwrap();
        assert_eq!(
            method.line, 5,
            "decorator should be part of the symbol range"
        );
        assert!(
            symbols
                .iter()
                .any(|symbol| symbol.kind == SymbolKind::Function && symbol.name == "run")
        );
        assert!(
            symbols
                .iter()
                .any(|symbol| { symbol.kind == SymbolKind::TypeAlias && symbol.name == "Result" })
        );
    }

    #[test]
    fn nested_function_is_not_misclassified_as_a_method() {
        let source =
            "class Outer:\n    def method(self):\n        def nested():\n            pass\n";
        let symbols = PythonSymbolExtractor::new()
            .extract(Path::new("nested.py"), source)
            .unwrap();

        assert!(
            symbols
                .iter()
                .any(|symbol| { symbol.kind == SymbolKind::Method && symbol.name == "method" })
        );
        assert!(
            symbols
                .iter()
                .any(|symbol| { symbol.kind == SymbolKind::Function && symbol.name == "nested" })
        );
    }
}
