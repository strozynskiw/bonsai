use std::sync::LazyLock;

use anyhow::Result;
use async_trait::async_trait;

use crate::model_catalog::LiveModelAvailability;
use crate::provider::{
    ANTHROPIC_PARAMETERS, NO_REASONING, Protocol, ProviderCapabilities, ProviderFactory,
    ProviderMetadata, TokenCounterKind,
};
use crate::session::ProviderSession;

pub static MINIMAX_CODING_PLAN_METADATA: LazyLock<ProviderMetadata> = LazyLock::new(|| {
    ProviderMetadata::new(
        "minimax-coding-plan",
        "MiniMax Coding Plan",
        "MiniMax-M3",
        "https://api.minimax.io/anthropic",
        Some("MINIMAX_CODING_PLAN_API_KEY"),
        Some("MINIMAX_CODING_PLAN_MODEL"),
        Some("MINIMAX_CODING_PLAN_BASE_URL"),
        &[
            "MiniMax-M3",
            "MiniMax-M2.5",
            "MiniMax-M2.5-highspeed",
            "MiniMax-M2.7",
            "MiniMax-M2",
            "MiniMax-M2.7-highspeed",
            "MiniMax-M2.1",
        ],
        Protocol::AnthropicMessages,
        // MiniMax's Anthropic endpoint supports explicit `cache_control`
        // breakpoints (docs: anthropic-api-compatible-cache; `{"type":"ephemeral"}`,
        // ≤4 breakpoints). Verified live: a warm request returns
        // `cache_read_input_tokens` (~99% on a repeated prefix). MiniMax reports the
        // breakdown on `message_delta`, which the usage parser now reads.
        ProviderCapabilities::new(NO_REASONING, ANTHROPIC_PARAMETERS).with_prompt_cache(),
        "v1/messages",
    )
    .with_context_window(200_000)
    .with_token_counter(TokenCounterKind::AnthropicCountTokens)
});

pub struct MiniMaxCodingPlanFactory;

#[async_trait]
impl ProviderFactory for MiniMaxCodingPlanFactory {
    fn metadata(&self) -> &ProviderMetadata {
        &MINIMAX_CODING_PLAN_METADATA
    }

    async fn list_models(&self, session: &ProviderSession) -> Result<Vec<String>> {
        Ok(self
            .list_available_models(session)
            .await?
            .remote_model_ids())
    }

    async fn list_available_models(
        &self,
        session: &ProviderSession,
    ) -> Result<LiveModelAvailability> {
        match crate::provider::endpoint::list_available_models_for_metadata(
            self.metadata(),
            session,
        )
        .await
        {
            Ok(models) => Ok(models),
            Err(err) => {
                tracing::warn!(
                    error = %format!("{err:#}"),
                    "MiniMax model list fetch failed; falling back to seed models (auth may be broken)"
                );
                Ok(LiveModelAvailability::from_remote_ids(
                    self.metadata().seed_model_list(),
                ))
            }
        }
    }

    async fn probe_reachability(&self, session: &ProviderSession) -> Result<()> {
        crate::provider::endpoint::probe_reachability_for_metadata(self.metadata(), session).await
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        unsafe_code,
        reason = "edition-2024 requires `unsafe` around std::env::{set_var,remove_var}; test-only"
    )]
    use super::*;
    use crate::provider::test_utils::provider_session_with_base;
    use crate::provider::{AuthInput, AuthRequirement};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_session(api_key: &str, base_url: String) -> ProviderSession {
        provider_session_with_base(api_key, &base_url)
    }

    #[test]
    fn metadata_matches_minimax_coding_plan() {
        let meta = &MINIMAX_CODING_PLAN_METADATA;
        assert_eq!(meta.id.as_ref(), "minimax-coding-plan");
        assert_eq!(meta.display_name.as_ref(), "MiniMax Coding Plan");
        assert_eq!(meta.default_model.as_ref(), "MiniMax-M3");
        assert_eq!(
            meta.env_var_api_key.as_deref(),
            Some("MINIMAX_CODING_PLAN_API_KEY")
        );
        assert_eq!(meta.auth_requirement, AuthRequirement::ApiKeyRequired);
        assert_eq!(meta.protocol, Protocol::AnthropicMessages);
        assert_eq!(meta.context_window, Some(200_000));
        assert!(
            meta.seed_models
                .iter()
                .any(|model| model.as_ref() == "MiniMax-M3")
        );
    }

    #[tokio::test]
    async fn list_models_uses_live_endpoint_when_available() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("x-api-key", "sk-mm-test"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"data":[{"id":"MiniMax-M3"},{"id":"MiniMax-M2.5"}]}"#),
            )
            .mount(&server)
            .await;

        let session = make_session("sk-mm-test", server.uri());
        let factory = MiniMaxCodingPlanFactory;
        let models = factory.list_models(&session).await.unwrap();
        assert_eq!(models, vec!["MiniMax-M2.5", "MiniMax-M3"]);
    }

    #[tokio::test]
    async fn list_models_falls_back_to_seed_when_endpoint_unreachable() {
        let session = make_session("sk-mm-test", "http://127.0.0.1:1".to_string());
        let factory = MiniMaxCodingPlanFactory;
        let models = factory.list_models(&session).await.unwrap();
        assert!(models.contains(&"MiniMax-M3".to_string()));
        assert_eq!(models.len(), factory.metadata().seed_models.len());
    }

    #[tokio::test]
    async fn list_models_falls_back_to_seed_on_error_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let session = make_session("sk-mm-test", server.uri());
        let factory = MiniMaxCodingPlanFactory;
        let models = factory.list_models(&session).await.unwrap();
        assert!(models.contains(&"MiniMax-M3".to_string()));
    }

    #[tokio::test]
    async fn authorize_from_env_reads_key() {
        crate::util::test_env::with_var_async(
            "MINIMAX_CODING_PLAN_API_KEY",
            Some("sk-mm-env"),
            async {
                let outcome = MiniMaxCodingPlanFactory
                    .authorize(AuthInput::FromEnv)
                    .await
                    .unwrap();
                assert_eq!(outcome.api_key, "sk-mm-env");
            },
        )
        .await;
    }

    #[tokio::test]
    async fn authorize_accepts_pasted_key() {
        let outcome = MiniMaxCodingPlanFactory
            .authorize(AuthInput::ApiKey {
                api_key: "sk-mm-pasted".to_string(),
                persistence: crate::session::CredentialPersistence::File,
            })
            .await
            .unwrap();
        assert_eq!(outcome.api_key, "sk-mm-pasted");
    }

    #[tokio::test]
    async fn authorize_rejects_empty_pasted_key() {
        let result = MiniMaxCodingPlanFactory
            .authorize(AuthInput::ApiKey {
                api_key: "".to_string(),
                persistence: crate::session::CredentialPersistence::File,
            })
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn authorize_rejects_codex_cache() {
        let result = MiniMaxCodingPlanFactory
            .authorize(AuthInput::FromCodexCache)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn is_authorized_checks_api_key_presence() {
        let factory = MiniMaxCodingPlanFactory;
        let session = make_session("", String::new());
        assert!(!factory.is_authorized(&session));
        let session = make_session("sk-mm-test", String::new());
        assert!(factory.is_authorized(&session));
    }

    #[tokio::test]
    async fn clear_authorization_wipes_api_key() {
        let factory = MiniMaxCodingPlanFactory;
        let mut session = make_session("sk-mm-test", String::new());
        factory.clear_authorization(&mut session);
        assert!(session.api_key.is_empty());
    }
}
