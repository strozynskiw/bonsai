use std::cell::RefCell;
use std::path::Path;

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser, Tree};

use super::{Symbol, SymbolExtractor, SymbolKind};

thread_local! {
    static JAVASCRIPT_PARSER: RefCell<Parser> = RefCell::new(new_javascript_parser());
    static TYPESCRIPT_PARSER: RefCell<Parser> = RefCell::new(new_typescript_parser(false));
    static TSX_PARSER: RefCell<Parser> = RefCell::new(new_typescript_parser(true));
}

fn new_javascript_parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .expect("tree-sitter-javascript grammar is compiled into the binary");
    parser
}

fn new_typescript_parser(tsx: bool) -> Parser {
    let mut parser = Parser::new();
    let language = if tsx {
        tree_sitter_typescript::LANGUAGE_TSX
    } else {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT
    };
    parser
        .set_language(&language.into())
        .expect("tree-sitter-typescript grammar is compiled into the binary");
    parser
}

#[derive(Debug, Clone, Copy)]
enum EcmaScriptLanguage {
    JavaScript,
    TypeScript,
}

#[derive(Debug)]
pub(crate) struct EcmaScriptSymbolExtractor {
    language: EcmaScriptLanguage,
}

impl EcmaScriptSymbolExtractor {
    pub(crate) fn javascript() -> Self {
        Self {
            language: EcmaScriptLanguage::JavaScript,
        }
    }

    pub(crate) fn typescript() -> Self {
        Self {
            language: EcmaScriptLanguage::TypeScript,
        }
    }

    fn parse(&self, path: &Path, source: &str) -> Option<Tree> {
        match self.language {
            EcmaScriptLanguage::JavaScript => {
                JAVASCRIPT_PARSER.with(|parser| parser.borrow_mut().parse(source, None))
            }
            EcmaScriptLanguage::TypeScript
                if path.extension().and_then(|extension| extension.to_str()) == Some("tsx") =>
            {
                TSX_PARSER.with(|parser| parser.borrow_mut().parse(source, None))
            }
            EcmaScriptLanguage::TypeScript => {
                TYPESCRIPT_PARSER.with(|parser| parser.borrow_mut().parse(source, None))
            }
        }
    }

    fn source_for<'a>(source: &'a str, node: Node<'_>) -> &'a str {
        source.get(node.byte_range()).unwrap_or("")
    }

    fn exported_range(node: Node<'_>) -> Node<'_> {
        let mut range = node;
        let mut parent = node.parent();
        while let Some(ancestor) = parent {
            if ancestor.kind() == "export_statement" {
                range = ancestor;
                break;
            }
            if matches!(ancestor.kind(), "program" | "statement_block") {
                break;
            }
            parent = ancestor.parent();
        }
        range
    }

    fn is_exported(node: Node<'_>) -> bool {
        Self::exported_range(node).kind() == "export_statement"
    }

    fn is_program_level(node: Node<'_>) -> bool {
        let mut parent = node.parent();
        while let Some(ancestor) = parent {
            match ancestor.kind() {
                "export_statement" | "ambient_declaration" => parent = ancestor.parent(),
                "program" => return true,
                _ => return false,
            }
        }
        false
    }

    fn variable_symbol<'tree>(
        source: &str,
        node: Node<'tree>,
    ) -> Option<(SymbolKind, Node<'tree>, Node<'tree>)> {
        let declaration = node.parent()?;
        if !matches!(
            declaration.kind(),
            "lexical_declaration" | "variable_declaration"
        ) {
            return None;
        }
        if !Self::is_program_level(declaration) && !Self::is_exported(declaration) {
            return None;
        }
        let name = node.child_by_field_name("name")?;
        if !matches!(name.kind(), "identifier" | "property_identifier") {
            return None;
        }
        let kind = match node.child_by_field_name("value").map(|value| value.kind()) {
            Some("arrow_function" | "function_expression" | "generator_function") => {
                SymbolKind::Function
            }
            _ if Self::source_for(source, declaration)
                .trim_start()
                .starts_with("const") =>
            {
                SymbolKind::Const
            }
            _ => SymbolKind::Variable,
        };
        Some((kind, name, Self::exported_range(declaration)))
    }

    fn symbol_for(&self, source: &str, node: Node<'_>) -> Option<Symbol> {
        let (kind, name_node, range_node) = match node.kind() {
            "function_declaration" | "generator_function_declaration" | "function_signature" => (
                SymbolKind::Function,
                node.child_by_field_name("name")?,
                Self::exported_range(node),
            ),
            "method_definition" | "method_signature" | "abstract_method_signature" => {
                (SymbolKind::Method, node.child_by_field_name("name")?, node)
            }
            "class_declaration" | "abstract_class_declaration" => (
                SymbolKind::Class,
                node.child_by_field_name("name")?,
                Self::exported_range(node),
            ),
            "interface_declaration" => (
                SymbolKind::Interface,
                node.child_by_field_name("name")?,
                Self::exported_range(node),
            ),
            "enum_declaration" => (
                SymbolKind::Enum,
                node.child_by_field_name("name")?,
                Self::exported_range(node),
            ),
            "type_alias_declaration" => (
                SymbolKind::TypeAlias,
                node.child_by_field_name("name")?,
                Self::exported_range(node),
            ),
            "module" => (
                SymbolKind::Module,
                node.child_by_field_name("name")?,
                Self::exported_range(node),
            ),
            "variable_declarator" => Self::variable_symbol(source, node)?,
            _ => return None,
        };
        let name = Self::source_for(source, name_node).trim().to_string();
        if name.is_empty() {
            return None;
        }
        let signature_text = Self::source_for(source, range_node).trim();
        let signature = signature_text
            .lines()
            .next()
            .unwrap_or(signature_text)
            .trim()
            .to_string();
        let is_public = Self::is_exported(range_node)
            || (kind == SymbolKind::Method
                && !name.starts_with('#')
                && !signature.starts_with("private ")
                && !signature.starts_with("protected "));
        let position = range_node.start_position();
        let end_position = range_node.end_position();
        Some(Symbol {
            kind,
            name,
            signature,
            line: position.row + 1,
            column: position.column + 1,
            end_line: end_position.row + 1,
            end_column: end_position.column + 1,
            start_byte: range_node.start_byte(),
            end_byte: range_node.end_byte(),
            is_public,
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

impl SymbolExtractor for EcmaScriptSymbolExtractor {
    fn extract(&self, path: &Path, source: &str) -> Result<Vec<Symbol>> {
        let tree = self.parse(path, source).with_context(|| {
            format!(
                "Failed to parse {} source",
                match self.language {
                    EcmaScriptLanguage::JavaScript => "JavaScript",
                    EcmaScriptLanguage::TypeScript => "TypeScript",
                }
            )
        })?;
        Ok(self.walk(source, tree.root_node()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_javascript_functions_classes_methods_and_values() {
        let source = r#"export function run() {}
class Client {
  request() {}
  #secret() {}
}
const handler = () => {};
let value = 1;
"#;
        let symbols = EcmaScriptSymbolExtractor::javascript()
            .extract(Path::new("index.js"), source)
            .unwrap();

        assert!(symbols.iter().any(|symbol| {
            symbol.kind == SymbolKind::Function && symbol.name == "run" && symbol.is_public
        }));
        assert!(
            symbols
                .iter()
                .any(|symbol| symbol.kind == SymbolKind::Class && symbol.name == "Client")
        );
        assert!(symbols.iter().any(|symbol| {
            symbol.kind == SymbolKind::Method && symbol.name == "request" && symbol.is_public
        }));
        assert!(symbols.iter().any(|symbol| {
            symbol.kind == SymbolKind::Method && symbol.name == "#secret" && !symbol.is_public
        }));
        assert!(
            symbols
                .iter()
                .any(|symbol| { symbol.kind == SymbolKind::Function && symbol.name == "handler" })
        );
        assert!(
            symbols
                .iter()
                .any(|symbol| { symbol.kind == SymbolKind::Variable && symbol.name == "value" })
        );
    }

    #[test]
    fn extracts_typescript_surface_and_parses_tsx() {
        let source = r#"export interface Store { get(id: string): string }
export type StoreId = string;
export enum State { Ready }
export abstract class Base { abstract load(): void }
export const View = () => <main />;
"#;
        let symbols = EcmaScriptSymbolExtractor::typescript()
            .extract(Path::new("view.tsx"), source)
            .unwrap();

        for (kind, name) in [
            (SymbolKind::Interface, "Store"),
            (SymbolKind::TypeAlias, "StoreId"),
            (SymbolKind::Enum, "State"),
            (SymbolKind::Class, "Base"),
            (SymbolKind::Function, "View"),
        ] {
            assert!(
                symbols
                    .iter()
                    .any(|symbol| symbol.kind == kind && symbol.name == name),
                "missing {kind:?} {name}: {symbols:#?}"
            );
        }
    }
}
