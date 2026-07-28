//! `/providers` manager state: the row model listing every catalog connection
//! (built-in and custom, including disabled built-ins) with its status, plus
//! the row builder shared by the TUI modal and the headless `/providers list`
//! output.

use crate::model_catalog::{DiscoveryKind, ModelCatalog, SourceKind};
use crate::provider::{ProviderMetadata, ProviderRegistry};
use crate::session::{CredentialSource, SessionStore};

const NO_CREDENTIAL_SOURCE: CredentialSource = CredentialSource::None;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderOrigin {
    BuiltIn,
    /// Defined by a user catalog file (`~/.bonsai/providers/<id>.toml`) —
    /// editable and removable through the manager.
    Custom,
    /// Defined by a trusted workspace under `.bonsai/providers`.
    Project,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderManagerRow {
    pub connection_id: String,
    pub display_name: String,
    pub origin: ProviderOrigin,
    pub enabled: bool,
    pub authorized: bool,
    pub current: bool,
    pub model_count: usize,
    pub discovery: DiscoveryKind,
    pub base_url: String,
    pub credential_label: Option<String>,
    pub auth_hint: Option<String>,
}

impl ProviderManagerRow {
    /// Case-insensitive substring match on the display name and connection id.
    pub(crate) fn matches_filter(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        query.is_empty()
            || self.display_name.to_lowercase().contains(&query)
            || self.connection_id.to_lowercase().contains(&query)
    }

    pub(crate) fn status_label(&self) -> String {
        let mut parts = Vec::new();
        if !self.enabled {
            parts.push("disabled".to_string());
        } else if self.authorized {
            parts.push("authorized".to_string());
        } else {
            parts.push("not authorized".to_string());
        }
        if self.authorized
            && let Some(credential_label) = self.credential_label.as_deref()
        {
            parts.push(format!("via {credential_label}"));
        }
        if self.enabled {
            parts.push(format!("{} models", self.model_count));
        }
        match self.discovery {
            DiscoveryKind::LmStudio => parts.push("LM Studio".to_string()),
            DiscoveryKind::Ollama => parts.push("Ollama".to_string()),
            DiscoveryKind::Gemini => parts.push("Gemini API".to_string()),
            DiscoveryKind::Mistral => parts.push("Mistral API".to_string()),
            DiscoveryKind::Static => parts.push("curated models".to_string()),
            DiscoveryKind::Generic => {}
        }
        parts.join(" · ")
    }
}

/// One environment variable that drives a provider, plus whether it is
/// currently present in the process environment. Only the name and presence are
/// captured — never the value, so a secret is never copied into UI state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderEnvVar {
    pub name: String,
    /// What the variable configures: "API key", "model", or "base URL".
    pub role: &'static str,
    pub is_set: bool,
}

/// The fully-resolved detail view of a single provider, opened from the manager
/// with Enter. Owned + render-ready so the widget stays a pure function of this
/// struct. `return_rows`/`return_cursor` carry the manager's list back so Esc
/// restores it losslessly at the same selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderDetail {
    pub display_name: String,
    pub connection_id: String,
    pub origin: ProviderOrigin,
    pub current: bool,
    pub status_label: String,
    pub auth_hint: Option<String>,
    pub base_url: String,
    pub endpoint_path: String,
    pub transport: String,
    pub default_model: String,
    pub env_vars: Vec<ProviderEnvVar>,
    pub models: Vec<String>,
    pub return_rows: Vec<ProviderManagerRow>,
    pub return_cursor: usize,
    /// The manager's active filter when this detail opened, restored on Esc so
    /// the narrowed list (and `return_cursor` within it) survives the round-trip.
    pub return_filter: String,
}

/// Assemble a [`ProviderDetail`] from a selected row, its provider metadata, and
/// its resolved model list. Env-var presence is read here via `std::env`, but
/// only the boolean is retained — the value is never touched.
pub(crate) fn build_provider_detail(
    row: &ProviderManagerRow,
    metadata: Option<&ProviderMetadata>,
    models: Vec<String>,
    return_rows: Vec<ProviderManagerRow>,
    return_cursor: usize,
    return_filter: String,
) -> ProviderDetail {
    let env_vars = metadata
        .map(|metadata| {
            [
                (metadata.env_var_api_key.as_deref(), "API key"),
                (metadata.env_var_model.as_deref(), "model"),
                (metadata.env_var_base_url.as_deref(), "base URL"),
            ]
            .into_iter()
            .filter_map(|(name, role)| {
                name.map(|name| ProviderEnvVar {
                    name: name.to_string(),
                    role,
                    is_set: std::env::var_os(name).is_some(),
                })
            })
            .collect()
        })
        .unwrap_or_default();
    ProviderDetail {
        display_name: row.display_name.clone(),
        connection_id: row.connection_id.clone(),
        origin: row.origin,
        current: row.current,
        status_label: row.status_label(),
        auth_hint: row.auth_hint.clone(),
        base_url: row.base_url.clone(),
        endpoint_path: metadata
            .map(|metadata| metadata.endpoint_path.to_string())
            .unwrap_or_default(),
        transport: metadata
            .map(|metadata| metadata.protocol.to_string())
            .unwrap_or_default(),
        default_model: metadata
            .map(|metadata| metadata.default_model.to_string())
            .unwrap_or_default(),
        env_vars,
        models,
        return_rows,
        return_cursor,
        return_filter,
    }
}

/// The manager rows narrowed to `filter` (matched on display name + id),
/// original order preserved. Shared by the keymap/reducer/render and the
/// runtime resolvers so `cursor` — which indexes the *filtered* view — always
/// maps back to the same underlying row.
pub(crate) fn provider_manager_filtered<'a>(
    rows: &'a [ProviderManagerRow],
    filter: &str,
) -> Vec<&'a ProviderManagerRow> {
    rows.iter()
        .filter(|row| row.matches_filter(filter))
        .collect()
}

/// Build the manager rows: enabled connections in catalog order, then
/// built-ins hidden by a disable patch (the live catalog filters those out,
/// so they are recovered from the built-in catalog).
pub(crate) fn provider_manager_rows(
    registry: &ProviderRegistry,
    session: &SessionStore,
    catalog: &ModelCatalog,
) -> Vec<ProviderManagerRow> {
    let current_id = session.current_kind_id().to_string();
    let builtin_ids = ModelCatalog::load_builtin()
        .map(|catalog| {
            catalog
                .connections()
                .into_iter()
                .map(|connection| connection.id.clone())
                .collect::<std::collections::HashSet<_>>()
        })
        .unwrap_or_default();
    let mut rows: Vec<ProviderManagerRow> = catalog
        .connections()
        .into_iter()
        .map(|connection| {
            let id = connection.id.as_str();
            let provider_session = session.providers.get(id);
            let authorized = registry
                .lookup(id)
                .zip(provider_session)
                .is_some_and(|(factory, provider_session)| factory.is_authorized(provider_session));
            let credential_source = provider_session
                .map(|session| &session.credential_source)
                .unwrap_or(&NO_CREDENTIAL_SOURCE);
            let model_count = catalog
                .available_models_for_connection(&connection.id, Vec::new())
                .len();
            ProviderManagerRow {
                connection_id: id.to_string(),
                display_name: connection.display_name.to_string(),
                origin: match (
                    builtin_ids.contains(&connection.id),
                    catalog.connection_source(&connection.id),
                ) {
                    (_, SourceKind::Project) => ProviderOrigin::Project,
                    (true, _) | (false, SourceKind::BuiltIn) => ProviderOrigin::BuiltIn,
                    (false, SourceKind::User) => ProviderOrigin::Custom,
                },
                enabled: true,
                authorized,
                current: current_id.eq_ignore_ascii_case(id),
                model_count,
                discovery: connection.discovery,
                base_url: connection.default_base_url.to_string(),
                credential_label: credential_label(credential_source),
                auth_hint: authorization_hint(id, credential_source, authorized),
            }
        })
        .collect();

    // Disabled built-ins: present in the built-in catalog but filtered from
    // the live one by a user disable patch.
    if let Ok(builtin) = ModelCatalog::load_builtin() {
        for connection in builtin.connections() {
            if catalog.connection(&connection.id).is_some() {
                continue;
            }
            rows.push(ProviderManagerRow {
                connection_id: connection.id.to_string(),
                display_name: connection.display_name.to_string(),
                origin: ProviderOrigin::BuiltIn,
                enabled: false,
                authorized: false,
                current: false,
                model_count: 0,
                discovery: connection.discovery,
                base_url: connection.default_base_url.to_string(),
                credential_label: None,
                auth_hint: None,
            });
        }
    }
    rows
}

fn credential_label(source: &CredentialSource) -> Option<String> {
    match source {
        CredentialSource::None => None,
        CredentialSource::Environment(variable) => Some(format!("environment {variable}")),
        CredentialSource::Keyring
        | CredentialSource::File
        | CredentialSource::CodexCache
        | CredentialSource::Session => Some(source.label().to_string()),
    }
}

fn authorization_hint(
    provider_id: &str,
    source: &CredentialSource,
    authorized: bool,
) -> Option<String> {
    match (authorized, source) {
        (true, CredentialSource::CodexCache) => Some(
            "Uses the Codex auth cache; after `codex login` changes, run /authorize codex again."
                .to_string(),
        ),
        (true, _) => None,
        (false, CredentialSource::CodexCache) => Some(
            "Codex login is missing or changed; run `codex login`, then /authorize codex."
                .to_string(),
        ),
        (false, CredentialSource::Environment(variable)) => Some(format!(
            "{variable} is unavailable or rejected; set it and restart, or run /authorize {provider_id}."
        )),
        (false, CredentialSource::File | CredentialSource::Keyring) => Some(format!(
            "The stored credential is unavailable or rejected; run /authorize {provider_id}."
        )),
        (false, CredentialSource::Session) => Some(format!(
            "The session-only credential is unavailable; run /authorize {provider_id}."
        )),
        (false, CredentialSource::None) => Some(format!("Run /authorize {provider_id}.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(connection_id: &str, display_name: &str) -> ProviderManagerRow {
        ProviderManagerRow {
            connection_id: connection_id.to_string(),
            display_name: display_name.to_string(),
            origin: ProviderOrigin::BuiltIn,
            enabled: true,
            authorized: false,
            current: false,
            model_count: 0,
            discovery: DiscoveryKind::Generic,
            base_url: String::new(),
            credential_label: None,
            auth_hint: None,
        }
    }

    #[test]
    fn provider_manager_filtered_matches_name_and_id_case_insensitively() {
        let rows = vec![
            row("opencode", "OpenCode Go"),
            row("qwencloud", "Qwen Cloud API"),
            row("deepseek", "DeepSeek API"),
        ];

        assert_eq!(provider_manager_filtered(&rows, "").len(), 3);
        assert_eq!(provider_manager_filtered(&rows, "QWEN").len(), 1);
        // Id fragment absent from the display name still matches.
        let deep = provider_manager_filtered(&rows, "deepseek");
        assert_eq!(deep.len(), 1);
        assert_eq!(deep[0].display_name, "DeepSeek API");
        assert!(provider_manager_filtered(&rows, "mistral").is_empty());
    }

    #[test]
    fn rows_mark_custom_providers_and_disabled_builtins() {
        let home = tempfile::TempDir::new().unwrap();
        crate::model_catalog::write_local_catalog_entry(
            home.path(),
            crate::model_catalog::LocalCatalogEntryInput {
                connection: crate::model_catalog::LocalCatalogConnectionInput {
                    id: "lm-local".parse().unwrap(),
                    display_name: "LM Local".to_string(),
                    transport: crate::model_catalog::TransportProtocol::OpenAiChat,
                    base_url: "http://localhost:1234/v1".to_string(),
                    discovery: DiscoveryKind::LmStudio,
                },
                targets: vec![crate::model_catalog::LocalCatalogTargetInput {
                    remote_model: "qwen3-coder".to_string(),
                    display_name: None,
                    context_window: Some(131_072),
                    output_limit: None,
                    tool_call: true,
                }],
            },
        )
        .unwrap();
        crate::model_catalog::set_builtin_connection_enabled(
            home.path(),
            &"codex".parse().unwrap(),
            false,
        )
        .unwrap();
        let catalog = crate::model_catalog::load_catalog_from_home(home.path()).unwrap();
        let registry = ProviderRegistry::from_catalog(&catalog);
        let session = SessionStore::default();

        let rows = provider_manager_rows(&registry, &session, &catalog);

        let custom = rows
            .iter()
            .find(|row| row.connection_id == "lm-local")
            .expect("custom row");
        assert_eq!(custom.origin, ProviderOrigin::Custom);
        assert!(custom.enabled);
        assert_eq!(custom.discovery, DiscoveryKind::LmStudio);
        assert_eq!(custom.model_count, 1);

        let disabled = rows
            .iter()
            .find(|row| row.connection_id == "codex")
            .expect("disabled builtin row");
        assert_eq!(disabled.origin, ProviderOrigin::BuiltIn);
        assert!(!disabled.enabled);

        let builtin = rows
            .iter()
            .find(|row| row.connection_id == "opencode")
            .expect("builtin row");
        assert_eq!(builtin.origin, ProviderOrigin::BuiltIn);
        assert!(builtin.enabled);
    }

    #[test]
    fn rows_mark_trusted_project_providers_as_project_managed() {
        let home = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join(".bonsai/providers")).unwrap();
        std::fs::write(
            project.path().join(".bonsai/providers/team.toml"),
            r#"[[connections]]
id = "team-endpoint"
display_name = "Team Endpoint"
auth = "optional-api-key"
transport = "openai-chat"
default_base_url = "https://team.example/v1"
"#,
        )
        .unwrap();
        let catalog = crate::model_catalog::load_catalog_from_home_and_project(
            home.path(),
            Some(project.path()),
        )
        .unwrap();
        let registry = ProviderRegistry::from_catalog(&catalog);

        let rows = provider_manager_rows(&registry, &SessionStore::default(), &catalog);
        let project = rows
            .iter()
            .find(|row| row.connection_id == "team-endpoint")
            .expect("project provider row");

        assert_eq!(project.origin, ProviderOrigin::Project);
    }

    #[test]
    fn user_patch_of_builtin_remains_builtin_managed() {
        let home = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join("providers")).unwrap();
        std::fs::write(
            home.path().join("providers/openai-override.toml"),
            r#"[[connections]]
id = "openai"
default_base_url = "https://proxy.example/v1"
"#,
        )
        .unwrap();
        let catalog = crate::model_catalog::load_catalog_from_home(home.path()).unwrap();
        let registry = ProviderRegistry::from_catalog(&catalog);

        let rows = provider_manager_rows(&registry, &SessionStore::default(), &catalog);
        let openai = rows
            .iter()
            .find(|row| row.connection_id == "openai")
            .expect("OpenAI provider row");

        assert_eq!(openai.origin, ProviderOrigin::BuiltIn);
    }

    #[test]
    fn provider_detail_captures_env_presence_and_models_without_values() {
        use crate::provider::{ProviderCapabilities, ProviderMetadata};

        let metadata = ProviderMetadata::new(
            "acme",
            "Acme",
            "acme-large",
            "https://acme.example/v1",
            Some("BONSAI_TEST_DEFINITELY_UNSET_ENV_9271"),
            None,
            Some("PATH"),
            &["acme-large", "acme-small"],
            crate::provider::Protocol::OpenAiChat,
            ProviderCapabilities::new(&[], &[]),
            "/chat/completions",
        );
        let row = ProviderManagerRow {
            connection_id: "acme".to_string(),
            display_name: "Acme".to_string(),
            origin: ProviderOrigin::BuiltIn,
            enabled: true,
            authorized: false,
            current: false,
            model_count: 2,
            discovery: DiscoveryKind::Generic,
            base_url: "https://acme.example/v1".to_string(),
            credential_label: None,
            auth_hint: None,
        };
        let models = vec!["acme-large".to_string(), "acme-small".to_string()];

        let detail = build_provider_detail(
            &row,
            Some(&metadata),
            models.clone(),
            vec![row.clone()],
            3,
            "acme".to_string(),
        );

        assert_eq!(detail.return_cursor, 3);
        assert_eq!(detail.return_filter, "acme");
        assert_eq!(detail.models, models);
        assert_eq!(detail.endpoint_path, "/chat/completions");
        assert_eq!(detail.transport, "openai-chat");
        assert_eq!(detail.default_model, "acme-large");

        // Only the two `Some` env vars appear, in api-key/base-url order, and the
        // value is never captured — presence only (`PATH` is set in the test env,
        // the synthetic key is not).
        assert_eq!(detail.env_vars.len(), 2);
        assert_eq!(
            detail.env_vars[0].name,
            "BONSAI_TEST_DEFINITELY_UNSET_ENV_9271"
        );
        assert_eq!(detail.env_vars[0].role, "API key");
        assert!(!detail.env_vars[0].is_set);
        assert_eq!(detail.env_vars[1].name, "PATH");
        assert_eq!(detail.env_vars[1].role, "base URL");
        assert!(detail.env_vars[1].is_set);
    }

    #[test]
    fn credential_diagnostics_explain_codex_cache_and_environment_recovery() {
        let codex = authorization_hint("codex", &CredentialSource::CodexCache, false)
            .expect("Codex cache failure should have a hint");
        let environment = authorization_hint(
            "anthropic",
            &CredentialSource::Environment("ANTHROPIC_API_KEY".to_string()),
            false,
        )
        .expect("missing environment credential should have a hint");

        assert!(codex.contains("codex login"));
        assert!(codex.contains("/authorize codex"));
        assert!(environment.contains("ANTHROPIC_API_KEY"));
        assert!(environment.contains("/authorize anthropic"));
        assert_eq!(
            credential_label(&CredentialSource::File).as_deref(),
            Some("protected file")
        );
    }
}
