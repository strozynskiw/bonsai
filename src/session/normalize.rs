use std::collections::HashMap;

use crate::model_catalog::{
    ConnectionId, ConnectionSpec, ModelCatalog, ModelId, connection_id_for_provider_id,
};
use crate::provider::ReasoningSelection;

use super::*;

impl ProviderSession {
    fn normalize_reasoning(&mut self, metadata: &crate::provider::ProviderMetadata) {
        let mut normalized = HashMap::new();
        for (model, reasoning) in self.model_reasoning.drain() {
            let reasoning = metadata.normalize_reasoning_for_model(&model, reasoning);
            normalized.insert(model, reasoning);
        }

        let current_reasoning = normalized
            .get(&self.model)
            .copied()
            .unwrap_or_else(|| metadata.normalize_reasoning_for_model(&self.model, self.reasoning));
        normalized.insert(self.model.clone(), current_reasoning);
        self.reasoning = current_reasoning;
        self.model_reasoning = normalized;
    }
}

impl SessionStore {
    fn ensure_provider_for_connection(&mut self, provider_id: &str, connection: &ConnectionSpec) {
        if !self.providers.contains_key(provider_id) {
            self.providers.insert(
                ConnectionId::fallback(provider_id),
                default_session_for_connection(connection),
            );
        }
    }

    pub(super) fn apply_env_overrides_with_catalog(&mut self, catalog: Option<&ModelCatalog>) {
        self.apply_provider_env_overrides_with_catalog(catalog, |var| std::env::var(var).ok());
    }

    #[cfg(test)]
    fn apply_provider_env_overrides(&mut self, mut value_for: impl FnMut(&str) -> Option<String>) {
        self.apply_provider_env_overrides_with_catalog(None, |var| value_for(var));
    }

    fn apply_provider_env_overrides_with_catalog(
        &mut self,
        catalog: Option<&ModelCatalog>,
        mut value_for: impl FnMut(&str) -> Option<String>,
    ) {
        if let Some(catalog) = catalog {
            for connection in catalog.connections() {
                let provider_id = connection.id.as_str();
                self.ensure_provider_for_connection(provider_id, connection);
                let session = self.providers.get_mut(provider_id).unwrap();
                apply_connection_env_overrides(session, connection, &mut value_for);
            }
            return;
        }

        // Driven by provider metadata: each provider declares its env-var names,
        // so adding a provider needs no edit here. Codex carries no API-key var.
        for meta in crate::provider::all_metadata() {
            self.ensure_provider(&meta.id);
            let session = self.providers.get_mut(meta.id.as_ref()).unwrap();
            if let Some(var) = meta.env_var_api_key.as_deref()
                && let Some(value) = value_for(var)
                && !value.is_empty()
            {
                session.api_key = value;
                session.credential_source = CredentialSource::Environment(var.to_string());
            }
            if let Some(var) = meta.env_var_model.as_deref()
                && let Some(value) = value_for(var)
                && !value.is_empty()
            {
                session.model = value;
            }
            if let Some(var) = meta.env_var_base_url.as_deref()
                && let Some(value) = value_for(var)
                && !value.is_empty()
            {
                session.base_url = value;
            }
        }
    }

    #[cfg(test)]
    fn normalize_sessions(&mut self) {
        self.normalize_sessions_with_catalog(None);
    }

    pub(super) fn normalize_sessions_with_catalog(&mut self, catalog: Option<&ModelCatalog>) {
        if let Some(catalog) = catalog {
            for connection in catalog.connections() {
                let provider_id = connection.id.as_str();
                self.ensure_provider_for_connection(provider_id, connection);
            }
        }

        let ids: Vec<ConnectionId> = self.providers.keys().cloned().collect();
        for id in ids {
            let session = self.providers.get_mut(&id).unwrap();
            if let Some((connection_id, connection)) =
                catalog_connection_for_provider(catalog, id.as_str())
            {
                if session.base_url.trim().is_empty() {
                    session.base_url = connection.default_base_url.to_string();
                } else {
                    let trimmed = session.base_url.trim().trim_end_matches('/').to_string();
                    session.base_url = trimmed;
                }
                if session.model.trim().is_empty()
                    && let Some(default_model) = &connection.default_model
                {
                    session.model = default_model.to_string();
                }
                if let Some(catalog) = catalog {
                    normalize_reasoning_for_connection(session, catalog, &connection_id);
                } else if let Some(metadata) = crate::provider::metadata_for(id.as_str()) {
                    session.normalize_reasoning(metadata);
                }
            } else if let Some((default_base_url, default_model)) = defaults_for(id.as_str()) {
                let metadata = crate::provider::metadata_for(id.as_str())
                    .expect("defaults_for returned metadata-backed provider");
                if session.base_url.trim().is_empty() {
                    session.base_url = default_base_url.to_string();
                } else {
                    let trimmed = session.base_url.trim().trim_end_matches('/').to_string();
                    session.base_url = trimmed;
                }
                if session.model.trim().is_empty() {
                    session.model = default_model.to_string();
                }
                session.normalize_reasoning(metadata);
            } else {
                session.base_url = session.base_url.trim().trim_end_matches('/').to_string();
            }
        }

        if !known_provider_with_catalog(catalog, &self.current_provider)
            || !self.providers.contains_key(self.current_provider.as_str())
        {
            self.ensure_provider(DEFAULT_PROVIDER_ID);
            self.current_provider = DEFAULT_PROVIDER_ID.to_string();
        }
        self.normalize_active_target();
    }

    fn normalize_active_target(&mut self) {
        let current_provider = self.current_kind_id().to_string();
        let expected_connection = connection_id_for_provider_id(&current_provider);
        let connection_changed = self.active_connection_id.as_ref() != expected_connection.as_ref();
        if connection_changed {
            self.active_connection_id = expected_connection;
            self.active_model_id = None;
        }

        let session_model = self
            .providers
            .get(current_provider.as_str())
            .map(|session| session.model.trim().to_string())
            .unwrap_or_default();
        if let Some(active_model_id) = &self.active_model_id
            && !session_model.is_empty()
            && session_model != active_model_id.as_str()
        {
            self.active_model_id = session_model.parse::<ModelId>().ok();
        }

        if self.active_model_id.is_none()
            && let Ok(model_id) = session_model.parse::<ModelId>()
        {
            self.active_model_id = Some(model_id);
        }

        if let Some(model_id) = &self.active_model_id
            && let Some(session) = self.providers.get_mut(current_provider.as_str())
        {
            session.model = model_id.to_string();
        }
    }
}

fn catalog_connection_for_provider<'a>(
    catalog: Option<&'a ModelCatalog>,
    provider_id: &str,
) -> Option<(ConnectionId, &'a ConnectionSpec)> {
    let catalog = catalog?;
    let connection_id = provider_id.parse::<ConnectionId>().ok()?;
    let connection = catalog.connection(&connection_id)?;
    Some((connection_id, connection))
}

fn known_provider_with_catalog(catalog: Option<&ModelCatalog>, provider_id: &str) -> bool {
    crate::provider::metadata_for(provider_id).is_some()
        || catalog_connection_for_provider(catalog, provider_id).is_some()
}

fn apply_connection_env_overrides(
    session: &mut ProviderSession,
    connection: &ConnectionSpec,
    value_for: &mut impl FnMut(&str) -> Option<String>,
) {
    if let Some(var) = connection.api_key_env.as_deref()
        && let Some(value) = value_for(var)
        && !value.is_empty()
    {
        session.api_key = value;
        session.credential_source = CredentialSource::Environment(var.to_string());
    }
    if let Some(var) = connection.model_env.as_deref()
        && let Some(value) = value_for(var)
        && !value.is_empty()
    {
        session.model = value;
    }
    if let Some(var) = connection.base_url_env.as_deref()
        && let Some(value) = value_for(var)
        && !value.is_empty()
    {
        session.base_url = value;
    }
}

fn normalize_reasoning_for_connection(
    session: &mut ProviderSession,
    catalog: &ModelCatalog,
    connection_id: &ConnectionId,
) {
    let mut normalized = HashMap::new();
    for (model, reasoning) in session.model_reasoning.drain() {
        let reasoning = normalize_catalog_reasoning(catalog, connection_id, &model, reasoning);
        normalized.insert(model, reasoning);
    }

    let current_reasoning = normalized.get(&session.model).copied().unwrap_or_else(|| {
        normalize_catalog_reasoning(catalog, connection_id, &session.model, session.reasoning)
    });
    normalized.insert(session.model.clone(), current_reasoning);
    session.reasoning = current_reasoning;
    session.model_reasoning = normalized;
}

fn normalize_catalog_reasoning(
    catalog: &ModelCatalog,
    connection_id: &ConnectionId,
    model: &str,
    reasoning: ReasoningSelection,
) -> ReasoningSelection {
    if let Some(resolved) = catalog.resolve_connection_model(connection_id, model) {
        resolved.normalize_reasoning(reasoning)
    } else {
        reasoning
    }
}

fn default_session_for_connection(connection: &ConnectionSpec) -> ProviderSession {
    ProviderSession::new(
        String::new(),
        connection.default_base_url.to_string(),
        connection
            .default_model
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store() -> SessionStore {
        let mut store = SessionStore::default();
        store.ensure_provider("opencode");
        store.ensure_provider("codex");
        store.ensure_provider("anthropic");
        store.ensure_provider("minimax-coding-plan");
        store
    }

    #[test]
    fn provider_env_overrides_replace_saved_settings() {
        let mut store = make_store();
        store.session_mut("anthropic").api_key = "sk-ant-saved".to_string();
        store.session_mut("anthropic").model = "claude-saved".to_string();
        store.session_mut("anthropic").base_url = "https://saved.example".to_string();

        store.apply_provider_env_overrides(|var| match var {
            "ANTHROPIC_API_KEY" => Some("sk-ant-env".to_string()),
            "ANTHROPIC_MODEL" => Some("claude-env".to_string()),
            "ANTHROPIC_BASE_URL" => Some("https://env.example".to_string()),
            _ => None,
        });

        let session = store.session("anthropic");
        assert_eq!(session.api_key, "sk-ant-env");
        assert_eq!(session.model, "claude-env");
        assert_eq!(session.base_url, "https://env.example");
    }

    #[test]
    fn catalog_keeps_api_and_coding_plan_credentials_independent() {
        let catalog = ModelCatalog::load_builtin().unwrap();
        let mut store = SessionStore::default();
        store.apply_provider_env_overrides_with_catalog(Some(&catalog), |var| match var {
            "MINIMAX_API_KEY" => Some("minimax-api".to_string()),
            "MINIMAX_CODING_PLAN_API_KEY" => Some("minimax-plan".to_string()),
            "ZAI_API_KEY" => Some("zai-api".to_string()),
            "ZAI_CODING_PLAN_API_KEY" => Some("zai-plan".to_string()),
            "MOONSHOT_API_KEY" => Some("moonshot-api".to_string()),
            "KIMI_CODING_PLAN_API_KEY" => Some("kimi-plan".to_string()),
            _ => None,
        });

        for (provider_id, api_key, default_model) in [
            ("minimax", "minimax-api", "minimax/MiniMax-M3"),
            (
                "minimax-coding-plan",
                "minimax-plan",
                "minimax-coding-plan/MiniMax-M3",
            ),
            ("zai", "zai-api", "zai/glm-5.2"),
            ("zai-coding-plan", "zai-plan", "zai-coding-plan/glm-5.2"),
            ("moonshotai", "moonshot-api", "moonshotai/kimi-k2.7-code"),
            (
                "kimi-coding-plan",
                "kimi-plan",
                "kimi-coding-plan/kimi-for-coding",
            ),
        ] {
            let session = store.session(provider_id);
            assert_eq!(session.api_key, api_key, "{provider_id}");
            assert_eq!(session.model, default_model, "{provider_id}");
        }
    }

    #[test]
    fn active_model_target_does_not_clobber_env_model_override() {
        let mut store = make_store();
        store.set_active_model_target(
            "anthropic",
            Some("anthropic".parse().unwrap()),
            Some("anthropic/claude-sonnet-4-5".parse().unwrap()),
            "claude-sonnet-4-5",
        );

        store.apply_provider_env_overrides(|var| match var {
            "ANTHROPIC_MODEL" => Some("claude-env".to_string()),
            _ => None,
        });
        store.normalize_sessions();

        assert_eq!(store.session("anthropic").model, "claude-env");
        assert_eq!(
            store.active_connection_id().map(|id| id.as_str()),
            Some("anthropic")
        );
        assert_eq!(store.active_model_id().map(|id| id.as_str()), None);
    }
}
