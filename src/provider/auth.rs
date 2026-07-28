use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::provider::{AuthRequirement, ProviderMetadata};
use crate::session::{CredentialPersistence, ProviderSession};

#[derive(Debug, Clone)]
pub enum AuthInput {
    FromEnv,
    FromCodexCache,
    ApiKey {
        api_key: String,
        persistence: CredentialPersistence,
        /// Catalog-selected service origin. `None` keeps the connection's
        /// default base URL (or an environment override).
        base_url: Option<String>,
    },
    OpenAiCompatible {
        base_url: String,
        api_key: Option<String>,
        model: Option<String>,
        context_window: Option<u32>,
        credential_persistence: CredentialPersistence,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthorizeOutcome {
    pub api_key: String,
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub clear_existing_api_key: bool,
    pub account_id: String,
    pub is_fedramp: bool,
}

impl AuthorizeOutcome {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            ..Self::default()
        }
    }
}

/// Shared `authorize` for the plain API-key providers (Anthropic, MiniMax
/// Coding Plan, OpenCode). Codex is the outlier and implements its own. Error
/// messages name the provider via `metadata.display_name`.
pub(crate) fn authorize_api_key(
    metadata: &ProviderMetadata,
    input: AuthInput,
) -> Result<AuthorizeOutcome> {
    let (api_key, base_url) = match input {
        AuthInput::FromEnv => {
            let var = metadata
                .env_var_api_key
                .as_deref()
                .with_context(|| format!("{} has no env var configured", metadata.display_name))?;
            (
                std::env::var(var).with_context(|| format!("{var} is not set"))?,
                None,
            )
        }
        AuthInput::ApiKey {
            api_key: key,
            base_url,
            ..
        } => {
            if key.trim().is_empty() {
                anyhow::bail!("{} API key cannot be empty", metadata.display_name);
            }
            (key, base_url)
        }
        AuthInput::FromCodexCache => {
            anyhow::bail!(
                "{} does not support codex auth cache",
                metadata.display_name
            )
        }
        AuthInput::OpenAiCompatible { .. } => {
            anyhow::bail!(
                "{} does not support OpenAI-compatible endpoint setup",
                metadata.display_name
            )
        }
    };
    let mut outcome = AuthorizeOutcome::new(api_key);
    outcome.base_url = base_url;
    Ok(outcome)
}

/// Shared `is_authorized` for API-key providers: a non-empty key.
pub(crate) fn api_key_is_authorized(session: &ProviderSession) -> bool {
    !session.api_key.trim().is_empty()
}

pub(crate) fn openai_compatible_is_authorized(session: &ProviderSession) -> bool {
    !session.base_url.trim().is_empty() && !session.model.trim().is_empty()
}

pub(crate) fn codex_cache_is_authorized(session: &ProviderSession) -> bool {
    !session.api_key.trim().is_empty() && !session.account_id.trim().is_empty()
}

pub(crate) fn is_authorized(metadata: &ProviderMetadata, session: &ProviderSession) -> bool {
    match metadata.auth_requirement {
        AuthRequirement::ApiKeyRequired => api_key_is_authorized(session),
        AuthRequirement::ApiKeyOptional => openai_compatible_is_authorized(session),
        AuthRequirement::CodexCache => codex_cache_is_authorized(session),
    }
}

/// Shared `clear_authorization` for API-key providers.
pub(crate) fn clear_api_key(session: &mut ProviderSession) {
    session.api_key.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_outcome_defaults_are_empty_strings() {
        let outcome = AuthorizeOutcome::new("token-123");

        assert_eq!(outcome.api_key, "token-123");
        assert!(outcome.base_url.is_none());
        assert!(outcome.account_id.is_empty());
        assert!(!outcome.is_fedramp);
    }

    #[test]
    fn api_key_authorization_preserves_selected_service_origin() {
        let metadata = crate::provider::metadata_for("anthropic").unwrap();
        let outcome = authorize_api_key(
            metadata,
            AuthInput::ApiKey {
                api_key: "token-123".to_string(),
                persistence: CredentialPersistence::Session,
                base_url: Some("https://regional.example/v1".to_string()),
            },
        )
        .unwrap();

        assert_eq!(
            outcome.base_url.as_deref(),
            Some("https://regional.example/v1")
        );
    }
}
