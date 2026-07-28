use super::AppState;
use crate::tui::event::ModalKind;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProviderAuthField {
    Origin,
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
    pub(crate) origins: Vec<crate::model_catalog::ServiceOrigin>,
    pub(crate) origin_cursor: usize,
}

impl ProviderAuthField {
    pub(super) fn moved(self, delta: i16, endpoint_form: bool, has_origins: bool) -> Self {
        let fields: &[Self] = if endpoint_form {
            &[
                Self::BaseUrl,
                Self::ApiKey,
                Self::Model,
                Self::ContextWindow,
            ]
        } else if has_origins {
            &[Self::Origin, Self::ApiKey]
        } else {
            &[Self::ApiKey]
        };
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
            origins: Vec::new(),
            origin_cursor: 0,
        }
    }

    pub(crate) fn from_provider_session(
        session: &crate::session::ProviderSession,
        default_persistence: crate::session::CredentialPersistence,
        mut origins: Vec<crate::model_catalog::ServiceOrigin>,
        endpoint_form: bool,
    ) -> Self {
        let mut form = if endpoint_form {
            Self::from_endpoint_session(session, default_persistence)
        } else {
            Self::with_persistence(default_persistence)
        };
        form.api_key_input = session.api_key.clone();
        let configured_base_url = session.base_url.trim();
        let mut origin_cursor = origins.iter().position(|origin| {
            origin.base_url.trim_end_matches('/') == configured_base_url.trim_end_matches('/')
        });
        if origin_cursor.is_none() && !configured_base_url.is_empty() && !origins.is_empty() {
            origins.push(crate::model_catalog::ServiceOrigin {
                id: "configured".into(),
                display_name: "Configured endpoint".into(),
                base_url: configured_base_url.to_string().into_boxed_str(),
            });
            origin_cursor = Some(origins.len() - 1);
        }
        form.origin_cursor = origin_cursor.unwrap_or(0);
        form.provider_auth_field = if !endpoint_form && !origins.is_empty() {
            ProviderAuthField::Origin
        } else if endpoint_form {
            ProviderAuthField::BaseUrl
        } else {
            ProviderAuthField::ApiKey
        };
        form.origins = origins;
        form
    }

    pub(crate) fn selected_origin(&self) -> Option<&crate::model_catalog::ServiceOrigin> {
        self.origins.get(self.origin_cursor)
    }

    pub(crate) fn cycle_origin(&mut self, delta: i16) {
        self.origin_cursor = super::move_index(
            self.origin_cursor,
            delta,
            self.origins.len().saturating_sub(1),
        );
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
        self.origins.clear();
        self.origin_cursor = 0;
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

    pub(crate) fn uses_structured_auth_form(&self) -> bool {
        self.uses_endpoint_auth_form() || !self.provider_auth_form.origins.is_empty()
    }

    pub(crate) fn origin_auth_field_active(&self) -> bool {
        self.provider_auth_form.provider_auth_field == ProviderAuthField::Origin
    }

    pub(super) fn active_auth_input_mut(&mut self) -> Option<&mut String> {
        if self.uses_endpoint_auth_form() {
            Some(match self.provider_auth_form.provider_auth_field {
                ProviderAuthField::Origin => return None,
                ProviderAuthField::BaseUrl => &mut self.provider_auth_form.provider_base_url_input,
                ProviderAuthField::ApiKey => &mut self.provider_auth_form.api_key_input,
                ProviderAuthField::Model => &mut self.provider_auth_form.provider_model_input,
                ProviderAuthField::ContextWindow => {
                    &mut self.provider_auth_form.context_window_input
                }
            })
        } else if self.origin_auth_field_active() {
            None
        } else {
            Some(&mut self.provider_auth_form.api_key_input)
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
    fn origin_auth_form_restores_and_cycles_the_saved_origin() {
        let mut app = app();
        let session = crate::session::ProviderSession::new(
            "sk-region".to_string(),
            "https://china.example/v1".to_string(),
            "example-model".to_string(),
        );
        let origins = vec![
            crate::model_catalog::ServiceOrigin {
                id: "global".into(),
                display_name: "Global".into(),
                base_url: "https://global.example/v1".into(),
            },
            crate::model_catalog::ServiceOrigin {
                id: "china".into(),
                display_name: "China".into(),
                base_url: "https://china.example/v1".into(),
            },
        ];
        let form = ProviderAuthForm::from_provider_session(
            &session,
            crate::session::CredentialPersistence::File,
            origins,
            false,
        );
        app.reduce(AppAction::OpenModal(ModalKind::ApiKeyPrompt {
            provider_id: "regional".to_string(),
            initial_form: Some(form),
        }));

        assert_eq!(
            app.provider_auth_form
                .selected_origin()
                .map(|origin| origin.id.as_ref()),
            Some("china")
        );
        app.reduce(AppAction::ApiKeyInputChar('x'));
        assert_eq!(app.provider_auth_form.api_key_input, "sk-region");

        app.reduce(AppAction::ApiKeyOriginCycle(-1));
        assert_eq!(
            app.provider_auth_form
                .selected_origin()
                .map(|origin| origin.id.as_ref()),
            Some("global")
        );
        app.reduce(AppAction::ApiKeyInputMoveField(1));
        app.reduce(AppAction::ApiKeyInputChar('x'));
        assert_eq!(app.provider_auth_form.api_key_input, "sk-regionx");
    }

    #[test]
    fn origin_auth_form_preserves_a_configured_non_catalog_endpoint() {
        let session = crate::session::ProviderSession::new(
            "sk-proxy".to_string(),
            "https://proxy.example/v1".to_string(),
            "example-model".to_string(),
        );
        let form = ProviderAuthForm::from_provider_session(
            &session,
            crate::session::CredentialPersistence::File,
            vec![crate::model_catalog::ServiceOrigin {
                id: "global".into(),
                display_name: "Global".into(),
                base_url: "https://global.example/v1".into(),
            }],
            false,
        );

        let selected = form.selected_origin().unwrap();
        assert_eq!(selected.display_name.as_ref(), "Configured endpoint");
        assert_eq!(selected.base_url.as_ref(), "https://proxy.example/v1");
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
