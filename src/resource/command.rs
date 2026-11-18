//! Custom slash commands: reusable prompts defined as markdown files.
//!
//! `<root>/commands/<name>.md` becomes `/<name>`, where `<root>` is the
//! project's `.bonsai`, `.claude` or `.agents` directory, or the global
//! `$BONSAI_HOME` (project wins over global, same precedence as skills). The
//! optional YAML frontmatter carries a `description` and an `argument-hint`;
//! the markdown body is the prompt. Running `/<name> some args` submits the
//! body as an ordinary user turn with every `$ARGUMENTS` replaced by
//! `some args` — or, when the body has no placeholder, with the arguments
//! appended — so the run goes through the normal permission path.
//!
//! Project commands are prompts the repository authors for the model, so they
//! are inert until the workspace is trusted, exactly like project skills.
//! Names that shadow a built-in command are rejected with a diagnostic instead
//! of silently overriding it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::Deserialize;

use crate::resource::discovery::{
    DiscoveredResource, Provenance, ResourceKind, disabled_names, disabled_names_global_only,
    discover, discover_global_only,
};
use crate::resource::frontmatter::{FrontmatterError, Parsed, parse_frontmatter};
use crate::resource::{MAX_RESOURCE_BODY_BYTES, cap_body};

/// Placeholder replaced by the text typed after the command name.
pub(crate) const ARGUMENTS_PLACEHOLDER: &str = "$ARGUMENTS";
/// Longest accepted command name (without the leading `/`).
const MAX_NAME_LEN: usize = 64;

/// Optional frontmatter of a command file.
///
/// Capability-shaped fields other tools use in their command files are
/// parsed so they can be rejected explicitly: Bonsai cannot scope a single
/// prompt's tools or model, and accepting them silently would imply a
/// boundary that does not exist.
#[derive(Debug, Default, Deserialize)]
struct CommandFrontmatter {
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "argument-hint", default)]
    argument_hint: Option<String>,
    #[serde(rename = "allowed-tools", default)]
    allowed_tools: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    model: Option<String>,
}

/// Why a command file was not registered.
#[derive(thiserror::Error, Debug)]
pub(crate) enum CommandLoadError {
    #[error("cannot read the file: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Frontmatter(#[from] FrontmatterError),
    #[error(
        "invalid name: use lowercase letters, digits, `-` or `_`, starting with a letter or digit (at most {MAX_NAME_LEN} characters)"
    )]
    InvalidName,
    #[error("shadows the built-in /{0} command; rename the file")]
    BuiltinClash(String),
    #[error(
        "frontmatter field `{0}` is recognized but unsupported; remove it because Bonsai does not scope {1} per command"
    )]
    Unsupported(&'static str, &'static str),
}

/// A discovered, parsed custom command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandDef {
    /// Name without the leading `/`.
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub body: String,
    pub source_path: PathBuf,
    pub provenance: Provenance,
    pub truncated: bool,
}

impl CommandDef {
    /// The slash form users type, e.g. `/review-pr`.
    pub(crate) fn slash_name(&self) -> String {
        format!("/{}", self.name)
    }

    /// Whether the definition lives in the global `$BONSAI_HOME`.
    #[cfg(test)]
    pub(crate) fn is_global(&self) -> bool {
        matches!(self.provenance, Provenance::Global { .. })
    }

    /// The prompt for an invocation with `arguments` (already trimmed).
    ///
    /// Every `$ARGUMENTS` is replaced; a body without the placeholder gets
    /// non-empty arguments appended on their own paragraph so they are never
    /// dropped silently.
    pub(crate) fn expand(&self, arguments: &str) -> String {
        let body = self.body.trim();
        if body.contains(ARGUMENTS_PLACEHOLDER) {
            body.replace(ARGUMENTS_PLACEHOLDER, arguments)
        } else if arguments.is_empty() {
            body.to_string()
        } else {
            format!("{body}\n\nArguments: {arguments}")
        }
    }
}

/// A registration problem worth showing the user (malformed file, clash).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandDiagnostic {
    pub path: PathBuf,
    pub message: String,
}

impl std::fmt::Display for CommandDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

/// Why a matched custom command cannot run.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub(crate) enum InvokeError {
    #[error("/{0} has an empty prompt body")]
    EmptyPrompt(String),
    #[error("/{0} expands to a slash command; custom commands cannot invoke other commands")]
    ExpandsToCommand(String),
}

/// A resolved invocation of a custom command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Expansion {
    /// Name without the leading `/`.
    pub name: String,
    /// The prompt submitted to the model.
    pub prompt: String,
}

/// All custom commands for one project, keyed by name (sorted, byte-stable).
#[derive(Debug, Default)]
pub(crate) struct CommandRegistry {
    commands: BTreeMap<String, CommandDef>,
    diagnostics: Vec<CommandDiagnostic>,
}

impl CommandRegistry {
    /// Discover commands for `project_root`, resolving the global directory
    /// from `$BONSAI_HOME`. `trusted` decides whether project-owned commands
    /// are read at all.
    pub(crate) fn load(project_root: &Path, trusted: bool) -> Self {
        let home = crate::storage::BonsaiPaths::discover()
            .map(|paths| paths.home_dir().to_path_buf())
            .unwrap_or_default();
        Self::load_from(
            project_root,
            &home,
            trusted,
            crate::commands::is_builtin_command,
        )
    }

    /// Pure variant of [`load`](Self::load) for tests: the global home and the
    /// built-in predicate are explicit.
    pub(crate) fn load_from(
        project_root: &Path,
        bonsai_home: &Path,
        trusted: bool,
        is_builtin: impl Fn(&str) -> bool,
    ) -> Self {
        let (resources, disabled) = if trusted {
            (
                discover(project_root, bonsai_home, ResourceKind::Commands),
                disabled_names(project_root, bonsai_home, ResourceKind::Commands),
            )
        } else {
            (
                discover_global_only(project_root, bonsai_home, ResourceKind::Commands),
                disabled_names_global_only(project_root, bonsai_home, ResourceKind::Commands),
            )
        };
        let mut registry = Self::default();
        for resource in resources {
            if disabled.contains(&resource.name) {
                continue;
            }
            match parse_command(&resource, &is_builtin) {
                Ok(def) => {
                    registry.commands.insert(def.name.clone(), def);
                }
                Err(err) => {
                    tracing::warn!(
                        command = %resource.name,
                        path = %resource.path.display(),
                        error = %err,
                        "skipping custom command"
                    );
                    registry.diagnostics.push(CommandDiagnostic {
                        path: resource.path,
                        message: err.to_string(),
                    });
                }
            }
        }
        registry
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Registered commands sorted by name.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &CommandDef> {
        self.commands.values()
    }

    /// Files that were found but not registered, with the reason.
    pub(crate) fn diagnostics(&self) -> &[CommandDiagnostic] {
        &self.diagnostics
    }

    /// Look a command up by its slash form (`/name`) or bare name.
    #[cfg(test)]
    pub(crate) fn get(&self, name: &str) -> Option<&CommandDef> {
        self.commands.get(name.strip_prefix('/').unwrap_or(name))
    }

    /// Resolve `input` when it invokes a custom command: the first
    /// whitespace-delimited token must be `/<name>` exactly. Returns `None`
    /// for anything else, including built-ins and plain prompts.
    pub(crate) fn expand(&self, input: &str) -> Option<Expansion> {
        let input = input.trim();
        let rest = input.strip_prefix('/')?;
        let (name, arguments) = match rest.split_once(char::is_whitespace) {
            Some((name, arguments)) => (name, arguments.trim()),
            None => (rest, ""),
        };
        let def = self.commands.get(name)?;
        Some(Expansion {
            name: def.name.clone(),
            prompt: def.expand(arguments),
        })
    }

    /// [`expand`](Self::expand) plus the checks every surface applies before
    /// submitting: the prompt must be non-empty and must not itself start with
    /// `/` (commands do not chain).
    ///
    /// # Errors
    ///
    /// Returns [`InvokeError`] when `input` names a custom command whose
    /// expansion cannot be submitted.
    pub(crate) fn resolve(&self, input: &str) -> Result<Option<Expansion>, InvokeError> {
        let Some(expansion) = self.expand(input) else {
            return Ok(None);
        };
        let prompt = expansion.prompt.trim();
        if prompt.is_empty() {
            return Err(InvokeError::EmptyPrompt(expansion.name));
        }
        if prompt.starts_with('/') {
            return Err(InvokeError::ExpandsToCommand(expansion.name));
        }
        Ok(Some(expansion))
    }
}

/// A hot-swappable handle so a long-lived surface (the TUI) can pick up
/// added or edited command files without a restart. The trust boundary is
/// fixed when the handle is created; reloads never widen it.
#[derive(Clone, Debug)]
pub(crate) struct SharedCommandRegistry {
    registry: Arc<RwLock<Arc<CommandRegistry>>>,
    project_root: PathBuf,
    trusted: bool,
}

impl SharedCommandRegistry {
    /// Load the registry for `project_root` and wrap it.
    pub(crate) fn load(project_root: &Path, trusted: bool) -> Self {
        Self::from_registry(
            CommandRegistry::load(project_root, trusted),
            project_root,
            trusted,
        )
    }

    /// Wrap an already-loaded registry.
    pub(crate) fn from_registry(
        registry: CommandRegistry,
        project_root: &Path,
        trusted: bool,
    ) -> Self {
        Self {
            registry: Arc::new(RwLock::new(Arc::new(registry))),
            project_root: project_root.to_path_buf(),
            trusted,
        }
    }

    /// An empty handle that never loads anything (tests, surfaces without a
    /// project).
    pub(crate) fn empty() -> Self {
        Self::from_registry(CommandRegistry::default(), Path::new(""), false)
    }

    /// Cheap snapshot; the lock is never held past this call.
    pub(crate) fn snapshot(&self) -> Arc<CommandRegistry> {
        self.registry
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Re-scan the command directories and swap the result in place, then
    /// return the fresh snapshot. An empty handle stays empty.
    pub(crate) fn reload(&self) -> Arc<CommandRegistry> {
        if self.project_root.as_os_str().is_empty() {
            return self.snapshot();
        }
        let fresh = Arc::new(CommandRegistry::load(&self.project_root, self.trusted));
        *self
            .registry
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = fresh.clone();
        fresh
    }
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && name.len() <= MAX_NAME_LEN
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_')
}

fn parse_command(
    resource: &DiscoveredResource,
    is_builtin: &impl Fn(&str) -> bool,
) -> Result<CommandDef, CommandLoadError> {
    let name = resource.name.clone();
    if !valid_name(&name) {
        return Err(CommandLoadError::InvalidName);
    }
    if is_builtin(&format!("/{name}")) {
        return Err(CommandLoadError::BuiltinClash(name));
    }
    let content = std::fs::read_to_string(&resource.path)?;
    // Frontmatter is optional: a plain markdown file is a valid command.
    let Parsed { frontmatter, body } = match parse_frontmatter::<CommandFrontmatter>(&content) {
        Ok(parsed) => parsed,
        Err(FrontmatterError::Missing) => Parsed {
            frontmatter: CommandFrontmatter::default(),
            body: content
                .strip_prefix('\u{feff}')
                .unwrap_or(&content)
                .to_string(),
        },
        Err(err) => return Err(err.into()),
    };
    if frontmatter.allowed_tools.is_some() {
        return Err(CommandLoadError::Unsupported(
            "allowed-tools",
            "tool access",
        ));
    }
    if frontmatter.model.is_some() {
        return Err(CommandLoadError::Unsupported("model", "model selection"));
    }
    let description = frontmatter
        .description
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| default_description(&body));
    let (body, truncated) = cap_body(body);
    debug_assert!(body.len() <= MAX_RESOURCE_BODY_BYTES);
    Ok(CommandDef {
        name,
        description,
        argument_hint: frontmatter
            .argument_hint
            .map(|hint| hint.trim().to_string())
            .filter(|hint| !hint.is_empty()),
        body,
        source_path: resource.path.clone(),
        provenance: resource.provenance.clone(),
        truncated,
    })
}

/// The first non-empty body line, stripped of markdown heading markers and
/// shortened, so a frontmatter-less command still has something to show.
fn default_description(body: &str) -> String {
    const MAX_CHARS: usize = 80;
    let line = body
        .lines()
        .map(|line| line.trim().trim_start_matches('#').trim())
        .find(|line| !line.is_empty())
        .unwrap_or("Custom command");
    if line.chars().count() <= MAX_CHARS {
        line.to_string()
    } else {
        let cut: String = line.chars().take(MAX_CHARS - 1).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn no_builtins(_: &str) -> bool {
        false
    }

    fn load(root: &Path, home: &Path, trusted: bool) -> CommandRegistry {
        CommandRegistry::load_from(root, home, trusted, no_builtins)
    }

    #[test]
    fn project_command_parses_frontmatter_and_body() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/commands/review-pr.md"),
            "---\ndescription: Review a pull request\nargument-hint: <pr-number>\n---\nReview PR $ARGUMENTS carefully.\n",
        );

        let registry = load(root.path(), home.path(), true);
        let def = registry.get("/review-pr").expect("command registered");
        assert_eq!(def.description, "Review a pull request");
        assert_eq!(def.argument_hint.as_deref(), Some("<pr-number>"));
        assert!(!def.is_global());
        assert_eq!(def.slash_name(), "/review-pr");
        assert!(registry.diagnostics().is_empty());
    }

    #[test]
    fn arguments_placeholder_is_substituted_everywhere() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/commands/fix.md"),
            "Fix issue $ARGUMENTS. Close $ARGUMENTS when done.",
        );

        let registry = load(root.path(), home.path(), true);
        let expansion = registry.expand("/fix   #42  ").expect("custom command");
        assert_eq!(expansion.name, "fix");
        assert_eq!(expansion.prompt, "Fix issue #42. Close #42 when done.");
        assert_eq!(
            registry.expand("/fix").unwrap().prompt,
            "Fix issue . Close  when done."
        );
    }

    #[test]
    fn arguments_are_appended_when_body_has_no_placeholder() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/commands/tidy.md"),
            "Tidy the imports.\n",
        );

        let registry = load(root.path(), home.path(), true);
        assert_eq!(
            registry.expand("/tidy").unwrap().prompt,
            "Tidy the imports."
        );
        assert_eq!(
            registry.expand("/tidy src/main.rs").unwrap().prompt,
            "Tidy the imports.\n\nArguments: src/main.rs"
        );
    }

    #[test]
    fn plain_markdown_without_frontmatter_uses_first_line_as_description() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/commands/explain.md"),
            "\n# Explain the selected code\n\nWalk through it step by step.",
        );

        let registry = load(root.path(), home.path(), true);
        assert_eq!(
            registry.get("explain").unwrap().description,
            "Explain the selected code"
        );
    }

    #[test]
    fn untrusted_workspace_loads_only_global_commands() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(&root.path().join(".bonsai/commands/project.md"), "project");
        write(&home.path().join("commands/global.md"), "global");

        let untrusted = load(root.path(), home.path(), false);
        let names: Vec<&str> = untrusted.iter().map(|def| def.name.as_str()).collect();
        assert_eq!(names, vec!["global"]);
        assert!(untrusted.expand("/project").is_none());
        assert!(untrusted.get("global").unwrap().is_global());

        let trusted = load(root.path(), home.path(), true);
        let names: Vec<&str> = trusted.iter().map(|def| def.name.as_str()).collect();
        assert_eq!(names, vec!["global", "project"]);
    }

    #[test]
    fn project_command_shadows_global_of_same_name() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/commands/ship.md"),
            "project ship",
        );
        write(&home.path().join("commands/ship.md"), "global ship");

        let registry = load(root.path(), home.path(), true);
        assert_eq!(registry.expand("/ship").unwrap().prompt, "project ship");
    }

    #[test]
    fn disabled_list_suppresses_a_command() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(&home.path().join("commands/noisy.md"), "noisy");
        write(&home.path().join("commands/kept.md"), "kept");
        write(
            &root.path().join(".bonsai/commands/.disabled"),
            "noisy # too chatty\n",
        );

        let names: Vec<String> = load(root.path(), home.path(), true)
            .iter()
            .map(|def| def.name.clone())
            .collect();
        assert_eq!(names, vec!["kept"]);
        // An untrusted project's `.disabled` is ignored along with its commands.
        assert_eq!(load(root.path(), home.path(), false).iter().count(), 2);
    }

    #[test]
    fn claude_commands_directory_is_discovered() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".claude/commands/deploy.md"),
            "Deploy it.",
        );

        let registry = load(root.path(), home.path(), true);
        assert!(registry.get("deploy").is_some());
    }

    #[test]
    fn builtin_name_clash_is_rejected_with_a_diagnostic() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(&root.path().join(".bonsai/commands/help.md"), "my help");
        write(&root.path().join(".bonsai/commands/mine.md"), "mine");

        let registry =
            CommandRegistry::load_from(root.path(), home.path(), true, |name| name == "/help");
        assert!(registry.get("help").is_none());
        assert!(registry.expand("/help").is_none());
        assert!(registry.get("mine").is_some());
        let [diagnostic] = registry.diagnostics() else {
            panic!("expected one diagnostic, got {:?}", registry.diagnostics());
        };
        assert!(diagnostic.path.ends_with("help.md"));
        assert!(
            diagnostic.message.contains("built-in /help"),
            "{diagnostic}"
        );
    }

    #[test]
    fn real_builtin_table_rejects_core_commands() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        for name in ["model", "review", "subtasks", "f"] {
            write(
                &root.path().join(format!(".bonsai/commands/{name}.md")),
                "x",
            );
        }

        let registry = CommandRegistry::load_from(
            root.path(),
            home.path(),
            true,
            crate::commands::is_builtin_command,
        );
        assert!(
            registry.is_empty(),
            "{:?}",
            registry.iter().collect::<Vec<_>>()
        );
        assert_eq!(registry.diagnostics().len(), 4);
    }

    #[test]
    fn invalid_names_and_unsupported_fields_are_diagnosed() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(&root.path().join(".bonsai/commands/Bad Name.md"), "x");
        write(
            &root.path().join(".bonsai/commands/scoped.md"),
            "---\nallowed-tools: [bash]\n---\nx",
        );
        write(
            &root.path().join(".bonsai/commands/pinned.md"),
            "---\nmodel: opus\n---\nx",
        );
        write(
            &root.path().join(".bonsai/commands/broken.md"),
            "---\ndescription: unterminated\n",
        );

        let registry = load(root.path(), home.path(), true);
        assert!(registry.is_empty());
        let messages: Vec<String> = registry
            .diagnostics()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(messages.len(), 4, "{messages:?}");
        assert!(messages.iter().any(|m| m.contains("invalid name")));
        assert!(messages.iter().any(|m| m.contains("`allowed-tools`")));
        assert!(messages.iter().any(|m| m.contains("`model`")));
        assert!(messages.iter().any(|m| m.contains("unterminated")));
    }

    #[test]
    fn resolve_rejects_empty_and_chained_prompts() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/commands/blank.md"),
            "---\ndescription: d\n---\n\n",
        );
        write(&root.path().join(".bonsai/commands/chain.md"), "/model gpt");
        write(&root.path().join(".bonsai/commands/ok.md"), "Do $ARGUMENTS");

        let registry = load(root.path(), home.path(), true);
        assert_eq!(
            registry.resolve("/blank"),
            Err(InvokeError::EmptyPrompt("blank".into()))
        );
        assert_eq!(
            registry.resolve("/chain"),
            Err(InvokeError::ExpandsToCommand("chain".into()))
        );
        assert_eq!(registry.resolve("/model x"), Ok(None));
        assert_eq!(registry.resolve("/ok it").unwrap().unwrap().prompt, "Do it");
    }

    #[test]
    fn non_command_input_does_not_expand() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(&root.path().join(".bonsai/commands/fix.md"), "Fix it.");

        let registry = load(root.path(), home.path(), true);
        assert!(registry.expand("fix it").is_none());
        assert!(registry.expand("/fixit").is_none());
        assert!(registry.expand("please /fix").is_none());
        assert!(registry.expand("/").is_none());
    }

    #[test]
    fn shared_handle_reload_picks_up_new_files_within_its_trust_boundary() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        let handle = SharedCommandRegistry::from_registry(
            load(root.path(), home.path(), false),
            root.path(),
            false,
        );
        assert!(handle.snapshot().is_empty());
        write(&root.path().join(".bonsai/commands/later.md"), "later");
        // Untrusted handles never read project commands, even after a reload.
        assert!(handle.reload().get("later").is_none());
        assert!(SharedCommandRegistry::empty().reload().is_empty());
    }
}
