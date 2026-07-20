use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::LspError;

/// One file extension and the LSP language id sent in `didOpen`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LanguageFileType {
    extension: String,
    language_id: String,
}

impl LanguageFileType {
    fn new(extension: &str, language_id: &str) -> Self {
        Self {
            extension: extension.to_string(),
            language_id: language_id.to_string(),
        }
    }
}

/// Declarative description of one language server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LanguageServerSpec {
    pub(crate) id: String,
    pub(crate) display_name: String,
    pub(crate) file_types: Vec<LanguageFileType>,
    pub(crate) root_markers: Vec<String>,
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) command_env: Option<String>,
    pub(crate) install_hint: String,
    pub(crate) initialization_options: Value,
    pub(crate) request_timeout_ms: u64,
}

impl LanguageServerSpec {
    pub(crate) fn rust() -> Self {
        Self {
            id: "rust".to_string(),
            display_name: "Rust".to_string(),
            file_types: vec![LanguageFileType::new("rs", "rust")],
            root_markers: vec!["Cargo.toml".to_string(), "rust-project.json".to_string()],
            command: "rust-analyzer".to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            command_env: Some("BONSAI_RUST_ANALYZER".to_string()),
            install_hint:
                "Install rust-analyzer (for rustup: `rustup component add rust-analyzer`), or use built-in symbol_search/grep without LSP."
                    .to_string(),
            initialization_options: Value::Object(Default::default()),
            request_timeout_ms: 20_000,
        }
    }

    pub(crate) fn typescript_javascript() -> Self {
        Self {
            id: "typescript-javascript".to_string(),
            display_name: "TypeScript/JavaScript".to_string(),
            file_types: vec![
                LanguageFileType::new("ts", "typescript"),
                LanguageFileType::new("mts", "typescript"),
                LanguageFileType::new("cts", "typescript"),
                LanguageFileType::new("tsx", "typescriptreact"),
                LanguageFileType::new("js", "javascript"),
                LanguageFileType::new("mjs", "javascript"),
                LanguageFileType::new("cjs", "javascript"),
                LanguageFileType::new("jsx", "javascriptreact"),
            ],
            root_markers: vec![
                "tsconfig.json".to_string(),
                "jsconfig.json".to_string(),
                "package.json".to_string(),
            ],
            command: "typescript-language-server".to_string(),
            args: vec!["--stdio".to_string()],
            env: BTreeMap::new(),
            command_env: Some("BONSAI_TYPESCRIPT_LANGUAGE_SERVER".to_string()),
            install_hint:
                "Install with `npm install -g typescript-language-server typescript`, or use built-in symbol_search/grep without LSP."
                    .to_string(),
            initialization_options: Value::Object(Default::default()),
            request_timeout_ms: 20_000,
        }
    }

    pub(crate) fn python() -> Self {
        Self {
            id: "python".to_string(),
            display_name: "Python".to_string(),
            file_types: vec![
                LanguageFileType::new("py", "python"),
                LanguageFileType::new("pyi", "python"),
                LanguageFileType::new("pyw", "python"),
            ],
            root_markers: vec![
                "pyrightconfig.json".to_string(),
                "pyproject.toml".to_string(),
                "setup.py".to_string(),
                "setup.cfg".to_string(),
                "requirements.txt".to_string(),
                "Pipfile".to_string(),
                "uv.lock".to_string(),
            ],
            command: "pyright-langserver".to_string(),
            args: vec!["--stdio".to_string()],
            env: BTreeMap::new(),
            command_env: Some("BONSAI_PYRIGHT_LANGSERVER".to_string()),
            install_hint:
                "Install Pyright with `npm install -g pyright` (or `pip install pyright`), or use built-in symbol_search/grep without LSP."
                    .to_string(),
            initialization_options: Value::Object(Default::default()),
            request_timeout_ms: 20_000,
        }
    }

    pub(crate) fn go() -> Self {
        Self {
            id: "go".to_string(),
            display_name: "Go".to_string(),
            file_types: vec![LanguageFileType::new("go", "go")],
            root_markers: vec!["go.work".to_string(), "go.mod".to_string()],
            command: "gopls".to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            command_env: Some("BONSAI_GOPLS".to_string()),
            install_hint:
                "Install with `go install golang.org/x/tools/gopls@latest`, or use built-in symbol_search/grep without LSP."
                    .to_string(),
            initialization_options: Value::Object(Default::default()),
            request_timeout_ms: 20_000,
        }
    }

    pub(crate) fn command(&self) -> String {
        self.command_with_env(|name| std::env::var(name).ok())
    }

    pub(crate) fn command_with_env(&self, lookup: impl Fn(&str) -> Option<String>) -> String {
        self.command_env
            .as_deref()
            .and_then(lookup)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| self.command.clone())
    }

    pub(crate) fn matches_file(&self, path: &Path) -> bool {
        self.language_id_for_path(path).is_some()
    }

    pub(crate) fn language_id_for_path(&self, path: &Path) -> Option<&str> {
        let extension = path.extension()?.to_str()?;
        self.file_types
            .iter()
            .find(|file_type| file_type.extension.eq_ignore_ascii_case(extension))
            .map(|file_type| file_type.language_id.as_str())
    }

    pub(crate) fn workspace_root(
        &self,
        file: &Path,
        project_root: &Path,
    ) -> Result<PathBuf, LspError> {
        let mut current = file.parent().unwrap_or(file);
        loop {
            if self
                .root_markers
                .iter()
                .any(|marker| current.join(marker).exists())
            {
                return Ok(current.to_path_buf());
            }
            let Some(parent) = current.parent() else {
                break;
            };
            current = parent;
        }
        project_root.canonicalize().map_err(|source| LspError::Io {
            action: "canonicalize project root".to_string(),
            source,
        })
    }
}

/// Registry of language-server descriptors.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LanguageServerRegistry {
    specs: Vec<LanguageServerSpec>,
}

impl LanguageServerRegistry {
    pub(crate) fn builtin() -> Self {
        Self {
            specs: vec![
                LanguageServerSpec::rust(),
                LanguageServerSpec::typescript_javascript(),
                LanguageServerSpec::python(),
                LanguageServerSpec::go(),
            ],
        }
    }

    #[cfg(test)]
    pub(crate) fn new(specs: Vec<LanguageServerSpec>) -> Self {
        Self { specs }
    }

    #[cfg(test)]
    pub(crate) fn specs(&self) -> &[LanguageServerSpec] {
        &self.specs
    }

    pub(crate) fn spec_for_path(&self, path: &Path) -> Option<&LanguageServerSpec> {
        self.specs.iter().find(|spec| spec.matches_file(path))
    }

    pub(crate) fn first_supported_file(&self, root: &Path) -> Option<PathBuf> {
        let mut stack = vec![root.to_path_buf()];
        while let Some(path) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&path) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let excluded =
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| {
                                name.starts_with('.')
                                    || matches!(
                                        name,
                                        "target"
                                            | "node_modules"
                                            | "__pycache__"
                                            | "venv"
                                            | "vendor"
                                            | "dist"
                                            | "build"
                                    )
                            });
                    if !excluded {
                        stack.push(path);
                    }
                } else if self.spec_for_path(&path).is_some() {
                    return Some(path);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_matches_release_language_files() {
        let registry = LanguageServerRegistry::builtin();
        assert_eq!(
            registry
                .spec_for_path(Path::new("src/main.rs"))
                .map(|spec| spec.id.as_str()),
            Some("rust")
        );
        assert_eq!(
            registry
                .spec_for_path(Path::new("src/main.ts"))
                .map(|spec| spec.id.as_str()),
            Some("typescript-javascript")
        );
        assert_eq!(
            registry
                .spec_for_path(Path::new("src/main.py"))
                .map(|spec| spec.id.as_str()),
            Some("python")
        );
        assert_eq!(
            registry
                .spec_for_path(Path::new("cmd/main.go"))
                .map(|spec| spec.id.as_str()),
            Some("go")
        );
        assert!(registry.spec_for_path(Path::new("README.md")).is_none());
    }

    #[test]
    fn registry_accepts_non_rust_descriptor() {
        let spec = LanguageServerSpec {
            id: "toy".to_string(),
            display_name: "Toy".to_string(),
            file_types: vec![LanguageFileType::new("toy", "toy")],
            root_markers: vec!["toy.toml".to_string()],
            command: "toy-lsp".to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            command_env: None,
            install_hint: "Install toy-lsp".to_string(),
            initialization_options: Value::Null,
            request_timeout_ms: 1000,
        };
        let registry = LanguageServerRegistry::new(vec![spec]);
        assert_eq!(registry.specs().len(), 1);
        assert_eq!(
            registry
                .spec_for_path(Path::new("src/example.toy"))
                .map(|spec| spec.id.as_str()),
            Some("toy")
        );
        assert!(registry.spec_for_path(Path::new("src/lib.rs")).is_none());
    }

    #[test]
    fn rust_command_honors_env_override() {
        assert_eq!(
            LanguageServerSpec::rust().command_with_env(|name| (name == "BONSAI_RUST_ANALYZER")
                .then(|| "/tmp/custom-rust-analyzer".to_string())),
            "/tmp/custom-rust-analyzer"
        );
    }

    #[test]
    fn typescript_descriptor_uses_extension_specific_language_ids() {
        let spec = LanguageServerSpec::typescript_javascript();
        assert_eq!(
            spec.language_id_for_path(Path::new("src/main.ts")),
            Some("typescript")
        );
        assert_eq!(
            spec.language_id_for_path(Path::new("src/view.tsx")),
            Some("typescriptreact")
        );
        assert_eq!(
            spec.language_id_for_path(Path::new("src/main.js")),
            Some("javascript")
        );
        assert_eq!(
            spec.language_id_for_path(Path::new("src/view.jsx")),
            Some("javascriptreact")
        );
    }

    #[test]
    fn language_server_commands_and_hints_are_actionable() {
        let typescript = LanguageServerSpec::typescript_javascript();
        assert_eq!(typescript.command, "typescript-language-server");
        assert_eq!(typescript.args, ["--stdio"]);
        assert!(typescript.install_hint.contains("npm install -g"));

        let python = LanguageServerSpec::python();
        assert_eq!(python.command, "pyright-langserver");
        assert_eq!(python.args, ["--stdio"]);
        assert!(python.install_hint.contains("pyright"));

        let go = LanguageServerSpec::go();
        assert_eq!(go.command, "gopls");
        assert!(go.args.is_empty());
        assert!(go.install_hint.contains("go install"));
    }

    #[test]
    fn workspace_root_uses_nearest_marker() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        let member = root.join("crates/a");
        std::fs::create_dir_all(member.join("src")).unwrap();
        std::fs::write(
            member.join("Cargo.toml"),
            "[package]\nname='a'\nversion='0.1.0'\n",
        )
        .unwrap();
        let file = member.join("src/lib.rs");
        std::fs::write(&file, "").unwrap();

        let spec = LanguageServerSpec::rust();
        assert_eq!(spec.workspace_root(&file, root).unwrap(), member);
    }

    #[test]
    fn language_roots_use_nearest_typescript_and_python_markers() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        std::fs::write(root.join("package.json"), "{}\n").unwrap();
        let web = root.join("packages/web");
        std::fs::create_dir_all(web.join("src")).unwrap();
        std::fs::write(web.join("tsconfig.json"), "{}\n").unwrap();
        let typescript_file = web.join("src/app.tsx");
        std::fs::write(&typescript_file, "").unwrap();

        let python = root.join("services/api");
        std::fs::create_dir_all(python.join("src")).unwrap();
        std::fs::write(python.join("pyproject.toml"), "[project]\nname='api'\n").unwrap();
        let python_file = python.join("src/main.py");
        std::fs::write(&python_file, "").unwrap();

        let go = root.join("services/worker");
        std::fs::create_dir_all(go.join("cmd")).unwrap();
        std::fs::write(go.join("go.mod"), "module example.com/worker\n").unwrap();
        let go_file = go.join("cmd/main.go");
        std::fs::write(&go_file, "package main\n").unwrap();

        assert_eq!(
            LanguageServerSpec::typescript_javascript()
                .workspace_root(&typescript_file, root)
                .unwrap(),
            web
        );
        assert_eq!(
            LanguageServerSpec::python()
                .workspace_root(&python_file, root)
                .unwrap(),
            python
        );
        assert_eq!(
            LanguageServerSpec::go()
                .workspace_root(&go_file, root)
                .unwrap(),
            go
        );
    }

    #[test]
    fn supported_file_scan_skips_dependency_and_cache_directories() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::write(root.join("node_modules/pkg/index.ts"), "").unwrap();
        std::fs::create_dir_all(root.join("__pycache__")).unwrap();
        std::fs::write(root.join("__pycache__/generated.py"), "").unwrap();

        assert!(
            LanguageServerRegistry::builtin()
                .first_supported_file(root)
                .is_none()
        );
    }
}
