use crate::tui::app::AppState;
use crate::tui::event::Focus;
use crate::tui::pickers::{ModelOption, ProviderOption};

pub(crate) fn app() -> AppState {
    AppState::new(
        "codex",
        "test-model".to_string(),
        "workspace".to_string(),
        None,
    )
}

pub(crate) fn input_app() -> AppState {
    let mut app = app();
    app.focus = Focus::Input;
    app
}

pub(crate) fn input_app_with_text(text: &str) -> AppState {
    let mut app = input_app();
    for ch in text.chars() {
        app.reduce(crate::tui::event::AppAction::InputChar(ch));
    }
    app
}

pub(crate) fn input_app_with_provider_choices(text: &str) -> AppState {
    let mut app = input_app();
    app.provider_choices = provider_options();
    for ch in text.chars() {
        app.reduce(crate::tui::event::AppAction::InputChar(ch));
    }
    app
}

pub(crate) fn provider_options() -> Vec<ProviderOption> {
    vec![
        ProviderOption {
            provider_id: "anthropic".to_string(),
            provider_label: "Anthropic".to_string(),
            authorized: false,
            current: false,
            uses_endpoint_auth_form: false,
        },
        ProviderOption {
            provider_id: "opencode".to_string(),
            provider_label: "OpenCode Go".to_string(),
            authorized: false,
            current: false,
            uses_endpoint_auth_form: false,
        },
    ]
}

pub(crate) fn model_option(provider_id: &str, provider_label: &str, model: &str) -> ModelOption {
    ModelOption {
        provider_id: provider_id.to_string(),
        connection_id: provider_id.to_string(),
        provider_label: provider_label.to_string(),
        model_id: None,
        model: model.to_string(),
        display_name: crate::tui::pickers::short_model_label(model).to_string(),
        reasoning: crate::provider::ReasoningSelection::default(),
        recommended_reasoning: None,
        discouraged_reasoning: Vec::new(),
        supported_reasoning: Vec::new(),
        shortcut_bindings: Vec::new(),
        parameter_preview: "default parameters".to_string(),
        pricing: None,
        context_window: None,
        features: Vec::new(),
        metadata_sources: crate::model_catalog::ResolvedModelMetadataSources::default(),
        catalog_drift: Vec::new(),
        unverified: false,
    }
}
