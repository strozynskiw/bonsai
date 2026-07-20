use super::AppState;
use crate::tui::event::ModalKind;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProviderAuthField {
    #[default]
    BaseUrl,
    ApiKey,
    Model,
    ContextWindow,
}

/// Inline editing state for the endpoint-auth modal (`/authorize` of providers
/// that take a base URL + API key + model). Grouped into one struct so the
/// form lives in a single place instead of four parallel `AppState` fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderAuthForm {
    pub api_key_input: String,
    pub provider_base_url_input: String,
    pub provider_model_input: String,
    pub context_window_input: String,
    pub provider_auth_field: ProviderAuthField,
    pub credential_persistence: crate::session::CredentialPersistence,
}

impl ProviderAuthField {
    pub(super) fn moved(self, delta: i16) -> Self {
        let fields = [
            Self::BaseUrl,
            Self::ApiKey,
            Self::Model,
            Self::ContextWindow,
        ];
        let current = fields.iter().position(|field| *field == self).unwrap_or(0);
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            current.saturating_add(delta as usize)
        }
        .min(fields.len().saturating_sub(1));
        fields[next]
    }
}

impl ProviderAuthForm {
    pub(crate) fn with_persistence(
        credential_persistence: crate::session::CredentialPersistence,
    ) -> Self {
        Self {
            credential_persistence,
            ..Self::default()
        }
    }

    pub(crate) fn from_endpoint_session(
        session: &crate::session::ProviderSession,
        default_persistence: crate::session::CredentialPersistence,
    ) -> Self {
        let credential_persistence = match session.credential_source {
            crate::session::CredentialSource::File => crate::session::CredentialPersistence::File,
            crate::session::CredentialSource::Keyring => {
                crate::session::CredentialPersistence::Keyring
            }
            crate::session::CredentialSource::Session => {
                crate::session::CredentialPersistence::Session
            }
            crate::session::CredentialSource::None
            | crate::session::CredentialSource::Environment(_)
            | crate::session::CredentialSource::CodexCache => default_persistence,
        };
        Self {
            api_key_input: session.api_key.clone(),
            provider_base_url_input: session.base_url.clone(),
            provider_model_input: session.model.clone(),
            context_window_input: session
                .context_window
                .map(|value| value.to_string())
                .unwrap_or_default(),
            provider_auth_field: ProviderAuthField::default(),
            credential_persistence,
        }
    }

    pub(crate) fn parsed_context_window(&self) -> Result<Option<u32>, String> {
        let value = self.context_window_input.trim();
        if value.is_empty() {
            return Ok(None);
        }
        let parsed = value
            .parse::<u32>()
            .map_err(|err| format!("Invalid context window: {err}"))?;
        if parsed == 0 {
            return Err("Context window must be greater than zero.".to_string());
        }
        Ok(Some(parsed))
    }

    pub(crate) fn clear(&mut self, credential_persistence: crate::session::CredentialPersistence) {
        self.api_key_input.clear();
        self.provider_base_url_input.clear();
        self.provider_model_input.clear();
        self.context_window_input.clear();
        self.provider_auth_field = ProviderAuthField::default();
        self.credential_persistence = credential_persistence;
    }
}

impl AppState {
    pub fn provider_uses_endpoint_auth_form(&self, provider_id: &str) -> bool {
        self.provider_choices
            .iter()
            .find(|provider| provider.provider_id == provider_id)
            .map(|provider| provider.uses_endpoint_auth_form)
            .or_else(|| {
                crate::provider::metadata_for(provider_id)
                    .map(|metadata| metadata.auth_requirement.uses_endpoint_setup())
            })
            .unwrap_or(false)
    }

    pub fn uses_endpoint_auth_form(&self) -> bool {
        matches!(
            self.modal.as_ref(),
            Some(ModalKind::ApiKeyPrompt { provider_id, .. })
                if self.provider_uses_endpoint_auth_form(provider_id)
        )
    }

    pub(super) fn active_auth_input_mut(&mut self) -> &mut String {
        if self.uses_endpoint_auth_form() {
            match self.provider_auth_form.provider_auth_field {
                ProviderAuthField::BaseUrl => &mut self.provider_auth_form.provider_base_url_input,
                ProviderAuthField::ApiKey => &mut self.provider_auth_form.api_key_input,
                ProviderAuthField::Model => &mut self.provider_auth_form.provider_model_input,
                ProviderAuthField::ContextWindow => {
                    &mut self.provider_auth_form.context_window_input
                }
            }
        } else {
            &mut self.provider_auth_form.api_key_input
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::event::AppAction;
    use crate::tui::pickers::ProviderOption;

    fn app() -> AppState {
        AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        )
    }

    #[test]
    fn endpoint_auth_form_routes_input_to_active_field() {
        let mut app = app();
        // Endpoint-form providers are catalog connections now; the form flag
        // rides on the provider choices seeded by `/authorize`.
        app.provider_choices = vec![ProviderOption {
            provider_id: "local-endpoint".to_string(),
            provider_label: "Local Endpoint".to_string(),
            authorized: false,
            current: false,
            uses_endpoint_auth_form: true,
        }];
        app.reduce(AppAction::OpenModal(ModalKind::ApiKeyPrompt {
            provider_id: "local-endpoint".to_string(),
            initial_form: None,
        }));

        app.reduce(AppAction::ApiKeyInputPaste(
            "localhost:11434/v1\n".to_string(),
        ));
        assert_eq!(
            app.provider_auth_form.provider_base_url_input,
            "localhost:11434/v1"
        );
        assert_eq!(
            app.provider_auth_form.provider_auth_field,
            ProviderAuthField::BaseUrl
        );

        app.reduce(AppAction::ApiKeyInputMoveField(1));
        app.reduce(AppAction::ApiKeyInputChar('k'));
        assert_eq!(app.provider_auth_form.api_key_input, "k");
        assert_eq!(
            app.provider_auth_form.provider_auth_field,
            ProviderAuthField::ApiKey
        );

        app.reduce(AppAction::ApiKeyInputMoveField(1));
        app.reduce(AppAction::ApiKeyInputPaste("llama-local".to_string()));
        assert_eq!(app.provider_auth_form.provider_model_input, "llama-local");

        app.reduce(AppAction::ApiKeyInputMoveField(1));
        app.reduce(AppAction::ApiKeyInputPaste("32768".to_string()));
        assert_eq!(app.provider_auth_form.context_window_input, "32768");
        assert_eq!(
            app.provider_auth_form.parsed_context_window().unwrap(),
            Some(32_768)
        );

        app.reduce(AppAction::CloseModal);
        assert!(app.provider_auth_form.provider_base_url_input.is_empty());
        assert!(app.provider_auth_form.api_key_input.is_empty());
        assert!(app.provider_auth_form.provider_model_input.is_empty());
        assert!(app.provider_auth_form.context_window_input.is_empty());
        assert_eq!(
            app.provider_auth_form.provider_auth_field,
            ProviderAuthField::BaseUrl
        );
    }

    #[test]
    fn catalog_only_endpoint_auth_form_uses_provider_choice_metadata() {
        let mut app = app();
        app.provider_choices = vec![ProviderOption {
            provider_id: "local-example".to_string(),
            provider_label: "Local Example".to_string(),
            authorized: false,
            current: false,
            uses_endpoint_auth_form: true,
        }];
        app.reduce(AppAction::OpenModal(ModalKind::ApiKeyPrompt {
            provider_id: "local-example".to_string(),
            initial_form: None,
        }));

        assert!(app.uses_endpoint_auth_form());
        app.reduce(AppAction::ApiKeyInputPaste(
            "http://localhost:11434/v1".to_string(),
        ));
        assert_eq!(
            app.provider_auth_form.provider_base_url_input,
            "http://localhost:11434/v1"
        );
    }

    #[test]
    fn api_key_prompt_can_open_with_prefilled_endpoint_form() {
        let mut app = app();
        let mut session = crate::session::ProviderSession::new(
            "sk-local".to_string(),
            "http://localhost:11434/v1".to_string(),
            "llama-local".to_string(),
        );
        session.context_window = Some(65_536);

        app.reduce(AppAction::OpenModal(ModalKind::ApiKeyPrompt {
            provider_id: "openai-compatible".to_string(),
            initial_form: Some(ProviderAuthForm::from_endpoint_session(
                &session,
                crate::session::CredentialPersistence::File,
            )),
        }));

        assert_eq!(
            app.provider_auth_form.provider_base_url_input,
            "http://localhost:11434/v1"
        );
        assert_eq!(app.provider_auth_form.api_key_input, "sk-local");
        assert_eq!(app.provider_auth_form.provider_model_input, "llama-local");
        assert_eq!(app.provider_auth_form.context_window_input, "65536");
    }

    #[test]
    fn context_window_input_rejects_zero_and_non_numeric_values() {
        let mut form = ProviderAuthForm {
            context_window_input: "0".to_string(),
            ..ProviderAuthForm::default()
        };
        assert!(form.parsed_context_window().is_err());

        form.context_window_input = "abc".to_string();
        assert!(form.parsed_context_window().is_err());

        form.context_window_input.clear();
        assert_eq!(form.parsed_context_window().unwrap(), None);
    }

    #[test]
    fn new_authorization_uses_global_default_and_cycles_per_credential() {
        let mut app = app();
        app.credential_persistence = crate::session::CredentialPersistence::Session;
        app.reduce(AppAction::OpenModal(ModalKind::ApiKeyPrompt {
            provider_id: "opencode".to_string(),
            initial_form: None,
        }));
        assert_eq!(
            app.provider_auth_form.credential_persistence,
            crate::session::CredentialPersistence::Session
        );

        app.reduce(AppAction::ApiKeyPersistenceToggle);
        assert_eq!(
            app.provider_auth_form.credential_persistence,
            crate::session::CredentialPersistence::File
        );
    }
}
