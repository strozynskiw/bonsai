use anyhow::{Context, Result};
use unicode_segmentation::UnicodeSegmentation;

use crate::model_catalog::{
    AvailableModel, ConnectionId, ConnectionSpec, DiscoveryKind, LocalCatalogConnectionInput,
    LocalCatalogEntryInput, LocalCatalogTargetInput, ModelFeature, TargetSpec, TransportProtocol,
};
use crate::provider::{DetectedServer, Protocol};

/// Server preset for the add-provider flow: picks the transport, the metadata
/// discovery kind, and sensible prefills so the common cases (a local
/// LM Studio or Ollama) are one Enter away.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ProviderPreset {
    LmStudio,
    Ollama,
    LlamaCpp,
    Vllm,
    Unsloth,
    /// Default: the most general server kind — a fresh wizard should not
    /// presume a specific local app is installed.
    #[default]
    OpenAiCompatible,
    AnthropicCompatible,
}

impl ProviderPreset {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::LmStudio => "LM Studio",
            Self::Ollama => "Ollama",
            Self::LlamaCpp => "llama.cpp",
            Self::Vllm => "vLLM",
            Self::Unsloth => "Unsloth",
            Self::OpenAiCompatible => "OpenAI compatible",
            Self::AnthropicCompatible => "Anthropic compatible",
        }
    }

    pub(crate) const fn base_url_prefill(self) -> &'static str {
        match self {
            Self::LmStudio => "http://localhost:1234/v1",
            Self::Ollama => "http://localhost:11434/v1",
            Self::LlamaCpp => "http://localhost:8080/v1",
            Self::Vllm | Self::Unsloth => "http://localhost:8000/v1",
            Self::OpenAiCompatible | Self::AnthropicCompatible => "",
        }
    }

    pub(crate) const fn transport(self) -> TransportProtocol {
        match self {
            Self::LmStudio
            | Self::Ollama
            | Self::LlamaCpp
            | Self::Vllm
            | Self::Unsloth
            | Self::OpenAiCompatible => TransportProtocol::OpenAiChat,
            Self::AnthropicCompatible => TransportProtocol::AnthropicMessages,
        }
    }

    pub(crate) const fn discovery(self) -> DiscoveryKind {
        match self {
            Self::LmStudio => DiscoveryKind::LmStudio,
            Self::Ollama => DiscoveryKind::Ollama,
            Self::LlamaCpp
            | Self::Vllm
            | Self::Unsloth
            | Self::OpenAiCompatible
            | Self::AnthropicCompatible => DiscoveryKind::Generic,
        }
    }

    pub(crate) const fn toggled(self) -> Self {
        match self {
            Self::LmStudio => Self::Ollama,
            Self::Ollama => Self::LlamaCpp,
            Self::LlamaCpp => Self::Vllm,
            Self::Vllm => Self::Unsloth,
            Self::Unsloth => Self::OpenAiCompatible,
            Self::OpenAiCompatible => Self::AnthropicCompatible,
            Self::AnthropicCompatible => Self::LmStudio,
        }
    }

    pub(crate) const fn toggled_back(self) -> Self {
        match self {
            Self::Ollama => Self::LmStudio,
            Self::LlamaCpp => Self::Ollama,
            Self::Vllm => Self::LlamaCpp,
            Self::Unsloth => Self::Vllm,
            Self::OpenAiCompatible => Self::Unsloth,
            Self::AnthropicCompatible => Self::OpenAiCompatible,
            Self::LmStudio => Self::AnthropicCompatible,
        }
    }

    /// The preset for an existing connection being edited: recover the
    /// server-specific preset from its discovery kind, else fall back to the
    /// generic preset for its transport.
    fn for_connection(transport: TransportProtocol, discovery: DiscoveryKind) -> Option<Self> {
        match (discovery, transport) {
            (DiscoveryKind::LmStudio, _) => Some(Self::LmStudio),
            (DiscoveryKind::Ollama, _) => Some(Self::Ollama),
            (DiscoveryKind::Generic | DiscoveryKind::Auto, TransportProtocol::OpenAiChat) => {
                Some(Self::OpenAiCompatible)
            }
            (
                DiscoveryKind::Generic | DiscoveryKind::Auto,
                TransportProtocol::AnthropicMessages,
            ) => Some(Self::AnthropicCompatible),
            (DiscoveryKind::Generic | DiscoveryKind::Auto, TransportProtocol::CodexResponses) => {
                None
            }
            // The local wizard never creates provider-specific or curated
            // discovery connections.
            (
                DiscoveryKind::Gemini
                | DiscoveryKind::Mistral
                | DiscoveryKind::OpenRouter
                | DiscoveryKind::Tencent
                | DiscoveryKind::QwenCloud
                | DiscoveryKind::Zai
                | DiscoveryKind::Static,
                _,
            ) => None,
        }
    }

    fn normalize_base_url(self, base_url: &str) -> Result<String> {
        let normalized = match self.transport() {
            TransportProtocol::OpenAiChat => {
                crate::provider::normalize_openai_compatible_base_url(base_url)
            }
            TransportProtocol::AnthropicMessages | TransportProtocol::CodexResponses => {
                crate::provider::normalize_anthropic_compatible_base_url(base_url)
            }
        }?;
        Ok(self.with_openai_mount(normalized))
    }

    /// Local servers mount their OpenAI surface at `/v1`, but their native
    /// probes ignore the path — so `http://localhost:1234` fetches models fine
    /// and then sends chat requests to `/chat/completions`, which 404s. Only
    /// presets with a known mount get the suffix; the generic compatible kinds
    /// keep exactly what the user typed.
    fn with_openai_mount(self, base_url: String) -> String {
        let known_mount = matches!(
            self,
            Self::LmStudio | Self::Ollama | Self::LlamaCpp | Self::Vllm | Self::Unsloth
        );
        if !known_mount {
            return base_url;
        }
        let Ok(url) = reqwest::Url::parse(&base_url) else {
            return base_url;
        };
        if url.path().trim_matches('/').is_empty() {
            format!("{base_url}/v1")
        } else {
            base_url
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalModelWizardStep {
    Setup,
    SelectModels,
    Metadata,
    Review,
}

impl LocalModelWizardStep {
    pub(crate) const fn title(self) -> &'static str {
        match self {
            Self::Setup => "Setup",
            Self::SelectModels => "Models",
            Self::Metadata => "Metadata",
            Self::Review => "Review",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalModelSetupField {
    Preset,
    DisplayName,
    ProviderId,
    BaseUrl,
    ApiKey,
    CredentialStorage,
}

impl LocalModelSetupField {
    fn moved(self, delta: i16) -> Self {
        let fields = [
            Self::Preset,
            Self::DisplayName,
            Self::ProviderId,
            Self::BaseUrl,
            Self::ApiKey,
            Self::CredentialStorage,
        ];
        move_enum(fields.as_slice(), self, delta)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalModelMetadataField {
    ContextWindow,
    OutputLimit,
    ToolCalls,
}

impl LocalModelMetadataField {
    fn moved(self, delta: i16) -> Self {
        let fields = [Self::ContextWindow, Self::OutputLimit, Self::ToolCalls];
        move_enum(fields.as_slice(), self, delta)
    }
}

fn move_enum<T: Copy + PartialEq>(items: &[T], current: T, delta: i16) -> T {
    let Some(last) = items.len().checked_sub(1) else {
        return current;
    };
    let current_index = items.iter().position(|item| *item == current).unwrap_or(0);
    let next = if delta == i16::MIN {
        0
    } else if delta == i16::MAX {
        last
    } else {
        // Tab and Shift-Tab wrap around the field list. Clamping on the last
        // row strands the cursor there with no forward key left.
        (current_index as isize + delta as isize).rem_euclid(items.len() as isize) as usize
    };
    items[next.min(last)]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalModelWizardModel {
    pub remote_model: String,
    pub display_name: Option<String>,
    pub selected: bool,
    pub context_window: String,
    pub output_limit: String,
    pub tool_call: bool,
}

impl LocalModelWizardModel {
    fn new(remote_model: String) -> Self {
        Self {
            remote_model,
            display_name: None,
            selected: true,
            // Unknown until the user fills it in; an empty field persists as
            // "no context window" rather than a fabricated number.
            context_window: String::new(),
            output_limit: String::new(),
            tool_call: true,
        }
    }

    fn from_available(model: &AvailableModel) -> Self {
        Self {
            remote_model: model.remote_model_id.to_string(),
            display_name: model.display_name.as_deref().map(str::to_string),
            selected: true,
            context_window: model
                .context_window
                .map(|value| value.to_string())
                .unwrap_or_default(),
            output_limit: model
                .output_limit
                .map(|value| value.to_string())
                .unwrap_or_default(),
            // An empty features list means the server didn't report
            // capabilities, not that tools are unsupported — keep the
            // optimistic default in that case.
            tool_call: model.features.is_empty()
                || model.features.contains(&ModelFeature::ToolCall),
        }
    }
}

/// Merge a fresh discovery listing into the wizard's model list.
///
/// A refetch is a *refresh*, not a reset: entries already in the list keep the
/// user's selection and typed metadata, and models the server no longer lists
/// stay (the user can still untick them). Newly discovered models are appended
/// unchecked once the list is non-empty, so re-running the wizard on an existing
/// provider never silently enables every model installed on the box. The return
/// value is how many new entries were added unchecked.
fn merge_fetched_models(
    mut existing: Vec<LocalModelWizardModel>,
    fetched: &[AvailableModel],
    already_curated: bool,
) -> (Vec<LocalModelWizardModel>, usize) {
    let select_new = !already_curated;
    let mut new_unchecked = 0;
    for model in fetched {
        let remote_model = model.remote_model_id.to_string();
        if let Some(current) = existing
            .iter_mut()
            .find(|entry| entry.remote_model == remote_model)
        {
            // Fill only blank fields: a server-reported window must not
            // overwrite a value the user set deliberately.
            if current.display_name.is_none() {
                current.display_name = model.display_name.as_deref().map(str::to_string);
            }
            if current.context_window.trim().is_empty()
                && let Some(window) = model.context_window
            {
                current.context_window = window.to_string();
            }
            if current.output_limit.trim().is_empty()
                && let Some(limit) = model.output_limit
            {
                current.output_limit = limit.to_string();
            }
            continue;
        }
        let mut entry = LocalModelWizardModel::from_available(model);
        entry.selected = select_new;
        if !select_new {
            new_unchecked += 1;
        }
        existing.push(entry);
    }
    (existing, new_unchecked)
}

/// Model the wizard should make active after writing the catalog, or `None`
/// when the write must leave the active target alone.
///
/// Creating a provider activates it — that is the point of adding one. Editing
/// must not pull the user off the model they are working in, so an edit of a
/// provider that is *not* active returns `None`, and an edit of the active
/// provider keeps the model the user is on unless the edit dropped it.
pub(crate) fn activation_after_write(
    editing_existing: bool,
    was_current: bool,
    current_model: &str,
    targets: &[&TargetSpec],
    fallback: &crate::model_catalog::ModelId,
) -> Option<crate::model_catalog::ModelId> {
    if !editing_existing || was_current {
        Some(
            target_matching(targets, current_model)
                .map(|target| target.model.clone())
                .unwrap_or_else(|| fallback.clone()),
        )
    } else {
        None
    }
}

/// Provider-local model to record when the active target is left alone: keeps
/// `stored` while it still resolves, else falls back to the first written
/// target so a later switch to the provider does not land on a dropped model.
pub(crate) fn retained_provider_model(
    targets: &[&TargetSpec],
    stored: &str,
    fallback: &crate::model_catalog::ModelId,
) -> crate::model_catalog::ModelId {
    target_matching(targets, stored)
        .map(|target| target.model.clone())
        .unwrap_or_else(|| fallback.clone())
}

/// Apply a just-written provider to the session.
///
/// Creating activates the provider; editing another provider leaves the active
/// target untouched. Whenever the active model selector actually changes, the
/// session's per-mode selections are re-linked (the same rule as any other
/// provider switch) — otherwise the next `Tab` re-applies the old provider's
/// recorded model and silently undoes the write.
pub(crate) fn apply_written_provider(
    session: &mut crate::session::SessionStore,
    provider_id: &str,
    connection_id: &ConnectionId,
    editing_existing: bool,
    targets: &[&TargetSpec],
    fallback: &crate::model_catalog::ModelId,
) {
    session.ensure_provider(provider_id);
    let was_current = session.current_kind_id().eq_ignore_ascii_case(provider_id);
    let current_model = session.current_session().model.clone();
    match activation_after_write(
        editing_existing,
        was_current,
        &current_model,
        targets,
        fallback,
    ) {
        Some(model) => {
            if model.as_str() != current_model {
                session.set_current_kind_id(provider_id);
            }
            session.set_active_model_target(
                provider_id,
                Some(connection_id.clone()),
                Some(model.clone()),
                model.as_str(),
            );
        }
        None => {
            // Editing a provider the user is not working in; only realign that
            // provider's own stored model when the edit dropped it.
            let stored = session.session(provider_id).model.clone();
            let realigned = retained_provider_model(targets, &stored, fallback);
            if realigned.as_str() != stored {
                session.session_mut(provider_id).model = realigned.to_string();
            }
        }
    }
}

/// Resolve a persisted session selector — a canonical model id or the wire id
/// the server reports — to the catalog target that backs it.
fn target_matching<'a>(targets: &[&'a TargetSpec], selector: &str) -> Option<&'a TargetSpec> {
    targets.iter().copied().find(|target| {
        target.model.as_str() == selector || target.remote_model.as_deref() == Some(selector)
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalModelWizardState {
    pub step: LocalModelWizardStep,
    pub setup_field: LocalModelSetupField,
    pub metadata_field: LocalModelMetadataField,
    pub display_name: String,
    pub provider_id: String,
    pub provider_id_edited: bool,
    pub preset: ProviderPreset,
    pub base_url: String,
    pub base_url_edited: bool,
    pub api_key: String,
    pub text_cursor: Option<usize>,
    pub credential_persistence: crate::session::CredentialPersistence,
    pub models: Vec<LocalModelWizardModel>,
    pub model_cursor: usize,
    pub manual_model_input: String,
    pub loading: bool,
    pub active_fetch_request_id: Option<u64>,
    pub status: Option<String>,
    pub error: Option<String>,
    /// `true` when the wizard was opened on an existing wizard-managed
    /// provider (`/wizard <id>`): the id is locked and commit replaces the
    /// provider's catalog files instead of refusing to overwrite them.
    pub editing_existing: bool,
}

impl Default for LocalModelWizardState {
    fn default() -> Self {
        let preset = ProviderPreset::default();
        Self {
            step: LocalModelWizardStep::Setup,
            setup_field: LocalModelSetupField::Preset,
            metadata_field: LocalModelMetadataField::ContextWindow,
            display_name: String::new(),
            provider_id: String::new(),
            provider_id_edited: false,
            preset,
            base_url: preset.base_url_prefill().to_string(),
            base_url_edited: false,
            api_key: String::new(),
            text_cursor: None,
            credential_persistence: crate::session::CredentialPersistence::default(),
            models: Vec::new(),
            model_cursor: 0,
            manual_model_input: String::new(),
            loading: false,
            active_fetch_request_id: None,
            status: None,
            error: None,
            editing_existing: false,
        }
    }
}

impl LocalModelWizardState {
    pub(crate) fn with_persistence(
        credential_persistence: crate::session::CredentialPersistence,
    ) -> Self {
        Self {
            credential_persistence,
            ..Self::default()
        }
    }

    /// Prefill the wizard from an existing catalog connection so `/wizard <id>`
    /// edits it in place. Only wizard-manageable transports are supported.
    pub(crate) fn for_edit(
        connection: &ConnectionSpec,
        targets: &[&TargetSpec],
        credential_persistence: crate::session::CredentialPersistence,
    ) -> Result<Self, String> {
        let Some(preset) =
            ProviderPreset::for_connection(connection.transport, connection.discovery)
        else {
            return Err(format!(
                "Provider `{}` uses the codex-responses transport, which the wizard cannot edit.",
                connection.id
            ));
        };
        let models = targets
            .iter()
            .map(|target| LocalModelWizardModel {
                remote_model: target
                    .remote_model
                    .as_deref()
                    .unwrap_or_else(|| target.model.model())
                    .to_string(),
                display_name: target.display_name.as_deref().map(str::to_string),
                selected: true,
                context_window: target
                    .context_window
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                output_limit: target
                    .output_limit
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                tool_call: target.features.contains(&ModelFeature::ToolCall),
            })
            .collect::<Vec<_>>();
        Ok(Self {
            display_name: connection.display_name.to_string(),
            provider_id: connection.id.to_string(),
            provider_id_edited: true,
            preset,
            base_url: connection.default_base_url.to_string(),
            base_url_edited: true,
            models,
            status: Some(format!(
                "Editing provider `{}`; saving replaces its catalog files.",
                connection.id
            )),
            editing_existing: true,
            ..Self::with_persistence(credential_persistence)
        })
    }

    pub(crate) fn input_char(&mut self, ch: char) {
        self.clear_feedback();
        match self.step {
            LocalModelWizardStep::Setup => self.setup_input_char(ch),
            LocalModelWizardStep::SelectModels => self.manual_model_input.push(ch),
            LocalModelWizardStep::Metadata => {
                if let Some(value) = self.metadata_input_mut() {
                    value.push(ch);
                }
            }
            LocalModelWizardStep::Review => {}
        };
    }

    pub(crate) fn backspace(&mut self) {
        self.clear_feedback();
        match self.step {
            LocalModelWizardStep::Setup => self.setup_backspace(),
            LocalModelWizardStep::SelectModels => {
                self.manual_model_input.pop();
            }
            LocalModelWizardStep::Metadata => {
                if let Some(value) = self.metadata_input_mut() {
                    value.pop();
                }
            }
            LocalModelWizardStep::Review => {}
        }
    }

    pub(crate) fn paste(&mut self, text: &str) {
        let cleaned = text.replace("\r\n", "\n").replace('\r', "\n");
        for ch in cleaned.chars().filter(|ch| *ch != '\n') {
            self.input_char(ch);
        }
    }

    pub(crate) fn move_field(&mut self, delta: i16) {
        self.clear_feedback();
        self.text_cursor = None;
        match self.step {
            LocalModelWizardStep::Setup => {
                self.setup_field = self.setup_field.moved(delta);
            }
            LocalModelWizardStep::Metadata => {
                self.metadata_field = self.metadata_field.moved(delta);
            }
            LocalModelWizardStep::SelectModels | LocalModelWizardStep::Review => {}
        }
    }

    pub(crate) fn move_model(&mut self, delta: i16) {
        self.clear_feedback();
        let len = self.visible_model_indices().len();
        if len == 0 {
            self.model_cursor = 0;
            return;
        }
        self.model_cursor = if delta == i16::MIN {
            0
        } else if delta == i16::MAX {
            len.saturating_sub(1)
        } else if delta.is_negative() {
            self.model_cursor
                .saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.model_cursor.saturating_add(delta as usize)
        }
        .min(len.saturating_sub(1));
    }

    fn cycle_preset(&mut self, delta: i16) {
        if !matches!(self.step, LocalModelWizardStep::Setup)
            || !matches!(self.setup_field, LocalModelSetupField::Preset)
        {
            return;
        }
        self.preset = if delta.is_negative() {
            self.preset.toggled_back()
        } else {
            self.preset.toggled()
        };
        // Refresh the URL prefill only while the user hasn't touched it, so
        // cycling presets never clobbers manual input.
        if !self.base_url_edited {
            self.base_url = self.preset.base_url_prefill().to_string();
        }
        self.clear_feedback();
    }

    pub(crate) fn toggle_selected_model(&mut self) {
        if !matches!(self.step, LocalModelWizardStep::SelectModels) {
            return;
        }
        let Some(index) = self.visible_model_indices().get(self.model_cursor).copied() else {
            return;
        };
        if let Some(model) = self.models.get_mut(index) {
            model.selected = !model.selected;
        }
        self.clear_feedback();
    }

    pub(crate) fn toggle_tool_calls(&mut self) {
        if !matches!(self.step, LocalModelWizardStep::Metadata)
            || !matches!(self.metadata_field, LocalModelMetadataField::ToolCalls)
        {
            return;
        }
        let Some(index) = self.visible_model_indices().get(self.model_cursor).copied() else {
            return;
        };
        if let Some(model) = self.models.get_mut(index) {
            model.tool_call = !model.tool_call;
        }
        self.clear_feedback();
    }

    pub(crate) fn mark_fetch_started(&mut self, request_id: u64) {
        self.loading = true;
        self.active_fetch_request_id = Some(request_id);
        self.status = Some("Fetching models...".to_string());
        self.error = None;
    }

    pub(crate) fn mark_commit_started(&mut self) {
        self.loading = true;
        self.status = Some("Writing catalog...".to_string());
        self.error = None;
    }

    /// Adopt a discovered endpoint: prefill the form and jump straight to the
    /// model list, so adding a running local server is "pick models, Enter"
    /// instead of typing a URL and a provider id.
    pub(crate) fn apply_discovered_endpoint(
        &mut self,
        endpoint: DiscoveredEndpoint,
        taken_ids: &[String],
    ) {
        let DiscoveredEndpoint {
            preset,
            base_url,
            models,
        } = endpoint;
        if self.display_name.trim().is_empty() {
            self.display_name = preset.label().to_string();
        }
        self.provider_id = unique_provider_id(&slugify_provider_id(&self.display_name), taken_ids);
        self.provider_id_edited = true;
        self.preset = preset;
        self.base_url = base_url;
        self.base_url_edited = true;
        let model_count = models.len();
        self.models = models
            .iter()
            .map(LocalModelWizardModel::from_available)
            .collect();
        self.model_cursor = 0;
        self.step = LocalModelWizardStep::SelectModels;
        self.status = Some(format!(
            "Found {} at {} — {model_count} model(s). Space toggles, Enter continues.",
            preset.label(),
            self.base_url
        ));
        self.error = None;
    }

    pub(crate) fn apply_fetch_success(&mut self, outcome: WizardFetchOutcome) {
        self.loading = false;
        self.active_fetch_request_id = None;
        self.step = LocalModelWizardStep::SelectModels;
        // The merged list restarts at the top; the cursor clamp below belongs to
        // the failure path, which keeps the surviving list.
        self.model_cursor = 0;
        let detection_note = outcome
            .detected
            .filter(|preset| *preset != self.preset)
            .map(|preset| {
                self.preset = preset;
                format!("Detected {}; using native metadata. ", preset.label())
            });
        let fetched_count = outcome.models.len();
        // Merge rather than replace: editing a provider must not discard the
        // models and metadata the user already curated, and a new listing must
        // not silently re-select everything on disk. A list the user has
        // already curated — an edit, or a retry after a failed fetch where they
        // typed a model by hand — is not a fresh selection either.
        let already_curated = self.editing_existing || !self.models.is_empty();
        let (models, new_unchecked) = merge_fetched_models(
            std::mem::take(&mut self.models),
            &outcome.models,
            already_curated,
        );
        self.models = models;
        let summary = if self.models.is_empty() {
            "No models returned. Enter a model id manually.".to_string()
        } else {
            let selected = self.selected_model_count();
            let mut summary = format!("Fetched {fetched_count} models; {selected} selected.");
            if new_unchecked > 0 {
                summary.push_str(&format!(" {new_unchecked} new left unchecked."));
            }
            summary
        };
        self.status = Some(format!("{}{summary}", detection_note.unwrap_or_default()));
        self.error = None;
    }

    pub(crate) fn apply_fetch_error(&mut self, message: String) {
        self.loading = false;
        self.active_fetch_request_id = None;
        if credentials_required(&message) {
            // The URL is right; the server just wants a key. Land on the field
            // that fixes it instead of leaving the cursor on the address.
            self.step = LocalModelWizardStep::Setup;
            self.setup_field = LocalModelSetupField::ApiKey;
            self.text_cursor = None;
            self.status =
                Some("This server needs an API key. Type it above and press Enter.".to_string());
            self.error = Some(message);
            return;
        }
        self.step = LocalModelWizardStep::SelectModels;
        // Keep the cursor inside the list that survives the failure.
        self.model_cursor = self.model_cursor.min(self.models.len().saturating_sub(1));
        if self.models.is_empty() {
            self.status = Some(format!("Fetch failed. {}", self.port_hints()));
        } else {
            // Keep whatever the list already holds — an unreachable server must
            // not throw away a provider the user is part-way through editing.
            self.status = Some(format!(
                "Fetch failed; keeping the models already listed. {}",
                self.port_hints()
            ));
        }
        self.error = Some(message);
    }

    /// Guidance for a failed fetch: a loopback URL that refuses connections is
    /// usually the wrong port, and the common local servers each ship a default.
    fn port_hints(&self) -> &'static str {
        let loopback = reqwest::Url::parse(self.base_url.trim())
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
            .is_some_and(|host| {
                matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]")
            });
        if loopback {
            "Enter model ids manually, or check the port: LM Studio 1234, \
             Ollama 11434, llama.cpp 8080, vLLM 8000."
        } else {
            "Enter model ids manually."
        }
    }

    pub(crate) fn submit_selection(&mut self) {
        self.clear_feedback();
        self.add_manual_model_if_present();
        if self.selected_model_count() == 0 {
            self.error = Some("Select or enter at least one model.".to_string());
            return;
        }
        self.step = LocalModelWizardStep::Metadata;
        self.model_cursor = 0;
        self.metadata_field = LocalModelMetadataField::ContextWindow;
    }

    pub(crate) fn submit_metadata(&mut self) {
        self.clear_feedback();
        match self.validate_targets() {
            Ok(()) => {
                self.step = LocalModelWizardStep::Review;
                self.model_cursor = 0;
            }
            Err(message) => self.error = Some(message),
        }
    }

    pub(crate) fn back(&mut self) {
        self.clear_feedback();
        match self.step {
            LocalModelWizardStep::Setup => {}
            LocalModelWizardStep::SelectModels => {
                self.step = LocalModelWizardStep::Setup;
                self.model_cursor = 0;
            }
            LocalModelWizardStep::Metadata => {
                self.step = LocalModelWizardStep::SelectModels;
                self.model_cursor = 0;
            }
            LocalModelWizardStep::Review => {
                self.step = LocalModelWizardStep::Metadata;
                self.model_cursor = 0;
            }
        }
    }

    pub(crate) fn validate_setup(&self) -> Result<(), String> {
        if self.display_name.trim().is_empty() {
            return Err("Provider display name is required.".to_string());
        }
        self.provider_id
            .trim()
            .parse::<ConnectionId>()
            .map_err(|err| err.to_string())?;
        self.preset
            .normalize_base_url(&self.base_url)
            .map(|_| ())
            .map_err(|err| format!("{err:#}"))
    }

    pub(crate) fn catalog_input(&self) -> Result<LocalCatalogEntryInput, String> {
        self.validate_setup()?;
        self.validate_targets()?;
        let id = self
            .provider_id
            .trim()
            .parse::<ConnectionId>()
            .map_err(|err| err.to_string())?;
        let base_url = self
            .preset
            .normalize_base_url(&self.base_url)
            .map_err(|err| format!("{err:#}"))?;
        let targets = self
            .models
            .iter()
            .filter(|model| model.selected)
            .map(|model| {
                let context_window =
                    parse_optional_u32(model.context_window.trim()).map_err(|err| {
                        format!("Invalid context window for {}: {err}", model.remote_model)
                    })?;
                let output_limit =
                    parse_optional_u32(model.output_limit.trim()).map_err(|err| {
                        format!("Invalid output limit for {}: {err}", model.remote_model)
                    })?;
                Ok(LocalCatalogTargetInput {
                    remote_model: model.remote_model.trim().to_string(),
                    display_name: model
                        .display_name
                        .as_deref()
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(str::to_string),
                    context_window,
                    output_limit,
                    tool_call: model.tool_call,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(LocalCatalogEntryInput {
            connection: LocalCatalogConnectionInput {
                id,
                display_name: self.display_name.trim().to_string(),
                transport: self.preset.transport(),
                base_url,
                discovery: self.preset.discovery(),
            },
            targets,
        })
    }

    pub(crate) fn selected_model_count(&self) -> usize {
        self.models.iter().filter(|model| model.selected).count()
    }

    pub(crate) fn visible_model_indices(&self) -> Vec<usize> {
        match self.step {
            LocalModelWizardStep::Metadata | LocalModelWizardStep::Review => self
                .models
                .iter()
                .enumerate()
                .filter_map(|(index, model)| model.selected.then_some(index))
                .collect(),
            LocalModelWizardStep::Setup | LocalModelWizardStep::SelectModels => {
                (0..self.models.len()).collect()
            }
        }
    }

    pub(crate) fn active_model(&self) -> Option<&LocalModelWizardModel> {
        let index = self
            .visible_model_indices()
            .get(self.model_cursor)
            .copied()?;
        self.models.get(index)
    }

    pub(crate) fn move_text_cursor(&mut self, delta: i16) {
        let value = match self.setup_field {
            LocalModelSetupField::DisplayName => &self.display_name,
            LocalModelSetupField::ProviderId => &self.provider_id,
            LocalModelSetupField::BaseUrl => &self.base_url,
            LocalModelSetupField::ApiKey => &self.api_key,
            _ => return,
        };
        if self.step != LocalModelWizardStep::Setup {
            return;
        }
        let len = value.graphemes(true).count();
        let current = self.text_cursor.unwrap_or(len).min(len);
        self.text_cursor = Some(match delta {
            i16::MIN => 0,
            i16::MAX => len,
            _ => current.saturating_add_signed(delta as isize).min(len),
        });
    }

    fn setup_input_char(&mut self, ch: char) {
        match self.setup_field {
            LocalModelSetupField::DisplayName => {
                insert_at_cursor(&mut self.display_name, &mut self.text_cursor, ch);
                if !self.provider_id_edited {
                    self.provider_id = slugify_provider_id(&self.display_name);
                }
            }
            LocalModelSetupField::ProviderId => {
                // The id names the catalog files; editing it mid-edit would
                // fork the provider instead of updating it.
                if self.editing_existing {
                    self.status = Some("Provider id is fixed while editing.".to_string());
                    return;
                }
                insert_at_cursor(&mut self.provider_id, &mut self.text_cursor, ch);
                self.provider_id_edited = true;
            }
            LocalModelSetupField::BaseUrl => {
                insert_at_cursor(&mut self.base_url, &mut self.text_cursor, ch);
                self.base_url_edited = true;
            }
            LocalModelSetupField::ApiKey => {
                insert_at_cursor(&mut self.api_key, &mut self.text_cursor, ch)
            }
            LocalModelSetupField::Preset | LocalModelSetupField::CredentialStorage => {}
        }
    }

    fn setup_backspace(&mut self) {
        match self.setup_field {
            LocalModelSetupField::DisplayName => {
                backspace_at_cursor(&mut self.display_name, &mut self.text_cursor);
                if !self.provider_id_edited {
                    self.provider_id = slugify_provider_id(&self.display_name);
                }
            }
            LocalModelSetupField::ProviderId => {
                if self.editing_existing {
                    self.status = Some("Provider id is fixed while editing.".to_string());
                    return;
                }
                backspace_at_cursor(&mut self.provider_id, &mut self.text_cursor);
                self.provider_id_edited = true;
            }
            LocalModelSetupField::BaseUrl => {
                backspace_at_cursor(&mut self.base_url, &mut self.text_cursor);
                self.base_url_edited = true;
            }
            LocalModelSetupField::ApiKey => {
                backspace_at_cursor(&mut self.api_key, &mut self.text_cursor);
            }
            LocalModelSetupField::Preset | LocalModelSetupField::CredentialStorage => {}
        }
    }

    pub(crate) fn toggle_setup_choice(&mut self) {
        self.cycle_setup_choice(1);
    }

    /// Cycle the active choice field forward (`delta > 0`) or backward. Text
    /// fields ignore it — Left/Right belong to their cursor semantics.
    pub(crate) fn cycle_setup_choice(&mut self, delta: i16) {
        match self.setup_field {
            LocalModelSetupField::Preset => self.cycle_preset(delta),
            LocalModelSetupField::CredentialStorage => {
                self.credential_persistence = if delta.is_negative() {
                    self.credential_persistence.cycled_back()
                } else {
                    self.credential_persistence.cycled()
                };
            }
            LocalModelSetupField::DisplayName
            | LocalModelSetupField::ProviderId
            | LocalModelSetupField::BaseUrl
            | LocalModelSetupField::ApiKey => {}
        }
    }

    /// Whether the active Setup field is an option toggle (Server preset or
    /// credential storage) — the fields whose choices ←/→ cycle through.
    pub(crate) fn setup_field_is_choice(&self) -> bool {
        matches!(self.step, LocalModelWizardStep::Setup)
            && matches!(
                self.setup_field,
                LocalModelSetupField::Preset | LocalModelSetupField::CredentialStorage
            )
    }

    fn metadata_input_mut(&mut self) -> Option<&mut String> {
        let index = self
            .visible_model_indices()
            .get(self.model_cursor)
            .copied()?;
        let model = self.models.get_mut(index)?;
        match self.metadata_field {
            LocalModelMetadataField::ContextWindow => Some(&mut model.context_window),
            LocalModelMetadataField::OutputLimit => Some(&mut model.output_limit),
            LocalModelMetadataField::ToolCalls => None,
        }
    }

    fn add_manual_model_if_present(&mut self) {
        let model = self.manual_model_input.trim();
        if model.is_empty() {
            return;
        }
        if !self.models.iter().any(|entry| entry.remote_model == model) {
            self.models
                .push(LocalModelWizardModel::new(model.to_string()));
        }
        self.manual_model_input.clear();
        self.model_cursor = self.models.len().saturating_sub(1);
    }

    fn validate_targets(&self) -> Result<(), String> {
        if self.selected_model_count() == 0 {
            return Err("Select at least one model.".to_string());
        }
        for model in self.models.iter().filter(|model| model.selected) {
            let context = parse_optional_u32(model.context_window.trim()).map_err(|err| {
                format!("Invalid context window for {}: {err}", model.remote_model)
            })?;
            if context == Some(0) {
                return Err(format!(
                    "Context window for {} must be greater than zero.",
                    model.remote_model
                ));
            }
            if parse_optional_u32(model.output_limit.trim())? == Some(0) {
                return Err(format!(
                    "Output limit for {} must be greater than zero.",
                    model.remote_model
                ));
            }
        }
        Ok(())
    }

    fn clear_feedback(&mut self) {
        self.status = None;
        self.error = None;
    }
}

#[cfg(test)]
#[test]
fn combining_character_cursor_stays_on_grapheme_boundaries() {
    let mut state = LocalModelWizardState::default();
    state.move_field(1);
    state.paste("a\u{301}b");
    state.move_text_cursor(-1);
    assert_eq!(state.text_cursor, Some(1));
    state.input_char('x');
    assert_eq!(state.display_name, "a\u{301}xb");
    state.backspace();
    state.backspace();
    assert_eq!(state.display_name, "b");
    assert_eq!(state.text_cursor, Some(0));
}

fn insert_at_cursor(value: &mut String, cursor: &mut Option<usize>, ch: char) {
    let index = cursor
        .unwrap_or(value.graphemes(true).count())
        .min(value.graphemes(true).count());
    let byte = value
        .grapheme_indices(true)
        .nth(index)
        .map_or(value.len(), |(byte, _)| byte);
    value.insert(byte, ch);
    *cursor = Some(value[..byte + ch.len_utf8()].graphemes(true).count());
}

fn backspace_at_cursor(value: &mut String, cursor: &mut Option<usize>) {
    let index = cursor
        .unwrap_or(value.graphemes(true).count())
        .min(value.graphemes(true).count());
    if index > 0
        && let Some((byte, grapheme)) = value.grapheme_indices(true).nth(index - 1)
    {
        let end = byte + grapheme.len();
        value.replace_range(byte..end, "");
    }
    *cursor = Some(index.saturating_sub(1));
}

fn parse_optional_u32(value: &str) -> Result<Option<u32>, String> {
    if value.is_empty() {
        return Ok(None);
    }
    value
        .parse::<u32>()
        .map(Some)
        .map_err(|err| err.to_string())
}

/// Whether a model-fetch failure means "this server wants credentials". The
/// message is what the transport rendered; matching it keeps the hint honest
/// across transports that report 401/403 in different shapes.
fn credentials_required(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("(401)")
        || lower.contains("(403)")
        || lower.contains("unauthorized")
        || lower.contains("not authenticated")
        || lower.contains("invalid api key")
        || lower.contains("invalid_api_key")
        || lower.contains("authentication")
}

/// A provider id that is free in the catalog: the name slug, else `-2`, `-3`…
/// so adopting a discovered server never fails on a name collision.
fn unique_provider_id(base: &str, taken: &[String]) -> String {
    let base = if base.is_empty() { "local" } else { base };
    let taken_id = |candidate: &str| taken.iter().any(|id| id.eq_ignore_ascii_case(candidate));
    if !taken_id(base) {
        return base.to_string();
    }
    (2..)
        .map(|suffix| format!("{base}-{suffix}"))
        .find(|candidate| !taken_id(candidate))
        .unwrap_or_else(|| base.to_string())
}

fn slugify_provider_id(display_name: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;
    for ch in display_name.trim().chars() {
        let next = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if next == '-' {
            if !previous_dash && !slug.is_empty() {
                slug.push(next);
            }
            previous_dash = true;
        } else {
            slug.push(next);
            previous_dash = false;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

/// Result of the wizard's model fetch: the discovered models plus the preset
/// the server was detected to be, when auto-detection upgraded a generic
/// OpenAI-compatible setup to a server-specific one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WizardFetchOutcome {
    pub models: Vec<AvailableModel>,
    pub detected: Option<ProviderPreset>,
}

/// A local endpoint the add flow found by probing, with the models it serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscoveredEndpoint {
    pub preset: ProviderPreset,
    pub base_url: String,
    pub models: Vec<AvailableModel>,
}

/// Endpoints the add flow probes before falling back to an empty form: the
/// well-known loopback ports, then every loopback/private endpoint the catalog
/// already knows (a server this machine has talked to before).
pub(crate) fn local_endpoint_candidates(
    known_base_urls: impl IntoIterator<Item = String>,
) -> Vec<(ProviderPreset, String)> {
    let mut candidates: Vec<(ProviderPreset, String)> = [
        ProviderPreset::LmStudio,
        ProviderPreset::Ollama,
        ProviderPreset::LlamaCpp,
        ProviderPreset::Vllm,
    ]
    .into_iter()
    .map(|preset| (preset, preset.base_url_prefill().to_string()))
    .collect();
    for base_url in known_base_urls {
        let base_url = base_url.trim().trim_end_matches('/').to_string();
        if base_url.is_empty() || !crate::provider::is_local_endpoint_url(&base_url) {
            continue;
        }
        if candidates.iter().any(|(_, known)| *known == base_url) {
            continue;
        }
        candidates.push((ProviderPreset::OpenAiCompatible, base_url));
    }
    candidates
}

/// How long probing may take before the wizard shows the empty form instead.
const LOCAL_ENDPOINT_PROBE_BUDGET: std::time::Duration = std::time::Duration::from_millis(1500);

/// Probe the candidate endpoints concurrently and return the first that lists
/// models, so `/providers add` can skip the blank form entirely.
pub(crate) async fn discover_local_endpoint(
    known_base_urls: impl IntoIterator<Item = String>,
) -> Option<DiscoveredEndpoint> {
    use futures::StreamExt;

    let probes = local_endpoint_candidates(known_base_urls).into_iter().map(
        |(preset, base_url)| async move {
            let availability = crate::provider::fetch_models_with_discovery(
                preset.discovery(),
                Protocol::from(preset.transport()),
                preset.label(),
                &base_url,
                "",
                None,
            )
            .await
            .ok()?;
            (!availability.models.is_empty()).then_some(DiscoveredEndpoint {
                preset,
                base_url,
                models: availability.models,
            })
        },
    );
    let mut found = Box::pin(
        futures::stream::iter(probes)
            .buffer_unordered(6)
            .filter_map(|endpoint| async move { endpoint }),
    );
    tokio::time::timeout(LOCAL_ENDPOINT_PROBE_BUDGET, found.next())
        .await
        .ok()
        .flatten()
}

pub(crate) async fn fetch_wizard_models(
    state: &LocalModelWizardState,
) -> Result<WizardFetchOutcome> {
    state.validate_setup().map_err(anyhow::Error::msg)?;
    let base_url = state
        .preset
        .normalize_base_url(&state.base_url)
        .context("Invalid base URL")?;
    let api_key = state.api_key.trim();

    // A local endpoint entered under the generic preset is often really an
    // LM Studio or Ollama; detect and upgrade so metadata comes from the
    // native API. Loopback-only: remote OpenAI-compatible hosts are what the
    // generic preset is for.
    let mut preset = state.preset;
    let mut detected = None;
    if preset == ProviderPreset::OpenAiCompatible && is_loopback_url(&base_url) {
        match crate::provider::detect_server(&base_url, api_key).await {
            crate::provider::DetectedServer::LmStudio => {
                preset = ProviderPreset::LmStudio;
                detected = Some(preset);
            }
            crate::provider::DetectedServer::Ollama => {
                preset = ProviderPreset::Ollama;
                detected = Some(preset);
            }
            DetectedServer::OpenAiCompatible
            | DetectedServer::AnthropicCompatible
            | DetectedServer::Unreachable => {}
        }
    }

    let availability = crate::provider::fetch_models_with_discovery(
        preset.discovery(),
        Protocol::from(preset.transport()),
        preset.label(),
        &base_url,
        api_key,
        // Local-model presets use the transport-default auth header.
        None,
    )
    .await?;
    Ok(WizardFetchOutcome {
        models: availability.models,
        detected,
    })
}

/// Build the wizard state for editing the provider named by `id_arg`: a
/// prefilled edit flow when the id names an existing wizard-managed provider
/// (one whose definition file lives in the user catalog directory).
pub(crate) fn wizard_state_for_provider_id(
    id_arg: &str,
    model_catalog: &crate::model_catalog::ModelCatalog,
    home_dir: &std::path::Path,
    credential_persistence: crate::session::CredentialPersistence,
) -> Result<LocalModelWizardState, String> {
    let connection_id = id_arg
        .parse::<crate::model_catalog::ConnectionId>()
        .map_err(|err| format!("Invalid provider id `{id_arg}`: {err}"))?;
    let Some(connection) = model_catalog.connection(&connection_id) else {
        return Err(format!(
            "Unknown provider `{id_arg}`. Run /providers add to create one."
        ));
    };
    let provider_file = crate::model_catalog::CatalogPaths::from_home_dir(home_dir)
        .provider_dir
        .join(format!("{}.toml", connection_id.as_str()));
    if !provider_file.exists() {
        return Err(format!(
            "Provider `{id_arg}` is built in; only custom providers (files under {}) can be edited.",
            provider_file.parent().unwrap_or(&provider_file).display()
        ));
    }
    let targets = model_catalog.targets_for_connection(&connection_id);
    LocalModelWizardState::for_edit(connection, &targets, credential_persistence)
}

fn is_loopback_url(base_url: &str) -> bool {
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn setup_state(base_url: String, preset: ProviderPreset) -> LocalModelWizardState {
        LocalModelWizardState {
            display_name: "Local Test".to_string(),
            provider_id: "local-test".to_string(),
            base_url,
            base_url_edited: true,
            api_key: "test-key".to_string(),
            preset,
            ..LocalModelWizardState::default()
        }
    }

    fn remote_ids(outcome: &WizardFetchOutcome) -> Vec<String> {
        outcome
            .models
            .iter()
            .map(|model| model.remote_model_id.to_string())
            .collect()
    }

    #[tokio::test]
    async fn fetches_openai_compatible_models() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "zeta"}, {"id": "alpha"}]
            })))
            .mount(&server)
            .await;
        let state = setup_state(
            format!("{}/v1", server.uri()),
            ProviderPreset::OpenAiCompatible,
        );

        let outcome = fetch_wizard_models(&state).await.unwrap();

        assert_eq!(remote_ids(&outcome), vec!["alpha", "zeta"]);
        assert_eq!(outcome.detected, None);
    }

    #[tokio::test]
    async fn fetches_anthropic_compatible_models() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("x-api-key", "test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "claude-local"}]
            })))
            .mount(&server)
            .await;
        let state = setup_state(server.uri(), ProviderPreset::AnthropicCompatible);

        let outcome = fetch_wizard_models(&state).await.unwrap();

        assert_eq!(remote_ids(&outcome), vec!["claude-local"]);
        assert_eq!(outcome.detected, None);
    }

    #[tokio::test]
    async fn generic_preset_detects_lm_studio_and_uses_native_metadata() {
        let server = MockServer::start().await;
        // Detection probe: LM Studio's distinctive v0 endpoint answers.
        Mock::given(method("GET"))
            .and(path("/api/v0/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"id": "qwen3-coder", "type": "llm", "max_context_length": 131_072}]
            })))
            .mount(&server)
            .await;
        // Native listing: the newer v1 endpoint with rich metadata.
        Mock::given(method("GET"))
            .and(path("/api/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{
                    "type": "llm",
                    "key": "qwen3-coder",
                    "display_name": "Qwen3 Coder",
                    "max_context_length": 131_072,
                    "capabilities": ["tool_use"]
                }]
            })))
            .mount(&server)
            .await;
        let state = setup_state(
            format!("{}/v1", server.uri()),
            ProviderPreset::OpenAiCompatible,
        );

        let outcome = fetch_wizard_models(&state).await.unwrap();

        assert_eq!(outcome.detected, Some(ProviderPreset::LmStudio));
        assert_eq!(outcome.models.len(), 1);
        assert_eq!(outcome.models[0].context_window, Some(131_072));
        assert_eq!(
            outcome.models[0].display_name.as_deref(),
            Some("Qwen3 Coder")
        );
    }

    #[test]
    fn preset_toggle_refreshes_untouched_base_url_prefill() {
        let mut state = LocalModelWizardState::default();
        // A fresh wizard starts on the general preset, not a specific app.
        assert_eq!(state.preset, ProviderPreset::OpenAiCompatible);
        assert_eq!(state.base_url, "");

        state.cycle_setup_choice(-1);

        assert_eq!(state.preset, ProviderPreset::Unsloth);
        assert_eq!(state.base_url, "http://localhost:8000/v1");
        assert_eq!(state.preset.transport(), TransportProtocol::OpenAiChat);
        assert_eq!(state.preset.discovery(), DiscoveryKind::Generic);
        assert!(state.models.is_empty());

        // vLLM shares Unsloth's 8000 default; llama.cpp serves on 8080.
        state.cycle_setup_choice(-1);
        assert_eq!(state.preset, ProviderPreset::Vllm);
        assert_eq!(state.base_url, "http://localhost:8000/v1");

        state.cycle_setup_choice(-1);
        assert_eq!(state.preset, ProviderPreset::LlamaCpp);
        assert_eq!(state.base_url, "http://localhost:8080/v1");
        assert_eq!(state.preset.discovery(), DiscoveryKind::Generic);

        state.cycle_setup_choice(-1);
        assert_eq!(state.preset, ProviderPreset::Ollama);
        assert_eq!(state.base_url, "http://localhost:11434/v1");

        state.cycle_setup_choice(-1);
        assert_eq!(state.preset, ProviderPreset::LmStudio);
        assert_eq!(state.base_url, "http://localhost:1234/v1");

        // Manual edits pin the URL across further preset cycles.
        state.setup_field = LocalModelSetupField::BaseUrl;
        state.input_char('x');
        state.setup_field = LocalModelSetupField::Preset;
        state.cycle_setup_choice(1);
        assert_eq!(state.preset, ProviderPreset::Ollama);
        assert!(state.base_url.ends_with('x'));
    }

    #[test]
    fn setup_text_cursor_edits_unicode_and_preserves_locked_id() {
        let mut state = LocalModelWizardState::default();
        state.move_field(1);
        state.paste("Zażółć");
        state.move_text_cursor(-2);
        state.input_char('!');
        assert_eq!(state.display_name, "Zażó!łć");
        state.backspace();
        assert_eq!(state.display_name, "Zażółć");
        state.move_text_cursor(i16::MIN);
        state.backspace();
        state.input_char('X');
        assert_eq!(state.display_name, "XZażółć");
        state.move_text_cursor(i16::MAX);
        state.input_char(' ');
        assert_eq!(state.display_name, "XZażółć ");
        state.move_field(1);
        assert_eq!(state.text_cursor, None);
        state.editing_existing = true;
        let id = state.provider_id.clone();
        state.move_text_cursor(0);
        state.input_char('x');
        state.backspace();
        assert_eq!(state.provider_id, id);
    }

    #[test]
    fn known_local_presets_mount_the_openai_surface_at_v1() {
        // Native discovery probes ignore the path, so a bare host fetches
        // models fine and then 404s on /chat/completions. Known local servers
        // get their documented mount; anything else keeps what was typed.
        assert_eq!(
            ProviderPreset::LmStudio
                .normalize_base_url("localhost:1234")
                .unwrap(),
            "http://localhost:1234/v1"
        );
        assert_eq!(
            ProviderPreset::Ollama
                .normalize_base_url("http://127.0.0.1:11434/")
                .unwrap(),
            "http://127.0.0.1:11434/v1"
        );
        assert_eq!(
            ProviderPreset::Vllm
                .normalize_base_url("http://localhost:8000/v1")
                .unwrap(),
            "http://localhost:8000/v1"
        );
        assert_eq!(
            ProviderPreset::LmStudio
                .normalize_base_url("http://localhost:1234/custom")
                .unwrap(),
            "http://localhost:1234/custom",
            "an explicit path is the user's call"
        );
        assert_eq!(
            ProviderPreset::OpenAiCompatible
                .normalize_base_url("http://gpu-box:9000")
                .unwrap(),
            "http://gpu-box:9000",
            "the generic compatible kind never invents a path"
        );
    }

    #[test]
    fn discovered_endpoint_prefills_a_writable_provider() {
        let mut state = LocalModelWizardState::default();
        state.apply_discovered_endpoint(
            DiscoveredEndpoint {
                preset: ProviderPreset::LmStudio,
                base_url: "http://localhost:1234/v1".to_string(),
                models: vec![
                    crate::model_catalog::AvailableModel::with_metadata(
                        "qwen3-coder",
                        Some(131_072),
                        None,
                        vec![ModelFeature::ToolCall],
                    ),
                    crate::model_catalog::AvailableModel::remote("granite-3b"),
                ],
            },
            &[],
        );

        assert_eq!(state.step, LocalModelWizardStep::SelectModels);
        assert_eq!(state.preset, ProviderPreset::LmStudio);
        assert_eq!(state.display_name, "LM Studio");
        assert_eq!(state.provider_id, "lm-studio");
        assert_eq!(state.base_url, "http://localhost:1234/v1");
        assert_eq!(state.selected_model_count(), 2);
        assert_eq!(state.models[0].context_window, "131072");
        assert!(
            state
                .status
                .as_deref()
                .is_some_and(|status| status.contains("Found LM Studio")),
            "status should name the discovered server: {:?}",
            state.status
        );
        assert!(
            state.catalog_input().is_ok(),
            "a discovered endpoint must be writable without further input"
        );
    }

    #[test]
    fn discovered_provider_id_avoids_catalog_collisions() {
        let mut state = LocalModelWizardState::default();
        state.apply_discovered_endpoint(
            DiscoveredEndpoint {
                preset: ProviderPreset::Ollama,
                base_url: "http://localhost:11434/v1".to_string(),
                models: vec![crate::model_catalog::AvailableModel::remote("qwen3:8b")],
            },
            &["ollama".to_string(), "ollama-2".to_string()],
        );

        assert_eq!(state.provider_id, "ollama-3");
    }

    #[test]
    fn local_endpoint_candidates_probe_loopback_then_known_local_hosts() {
        let candidates = local_endpoint_candidates(vec![
            "http://localhost:1234/v1".to_string(),
            "http://192.168.50.123:8888/v1".to_string(),
            "https://api.vendor.com/v1".to_string(),
        ]);

        assert_eq!(
            candidates
                .iter()
                .map(|(_, base_url)| base_url.as_str())
                .collect::<Vec<_>>(),
            vec![
                "http://localhost:1234/v1",
                "http://localhost:11434/v1",
                "http://localhost:8080/v1",
                "http://localhost:8000/v1",
                "http://192.168.50.123:8888/v1",
            ],
            "well-known ports first, then known LAN endpoints, never public hosts"
        );
    }

    #[test]
    fn credentials_rejection_sends_the_user_to_the_api_key_field() {
        let mut state = LocalModelWizardState {
            preset: ProviderPreset::OpenAiCompatible,
            base_url: "http://192.168.50.123:8888/v1".to_string(),
            display_name: "LAN Local".to_string(),
            provider_id: "lan-local".to_string(),
            step: LocalModelWizardStep::Setup,
            ..LocalModelWizardState::default()
        };

        state.apply_fetch_error("provider HTTP error (401): Not authenticated".to_string());

        assert_eq!(state.step, LocalModelWizardStep::Setup);
        assert_eq!(state.setup_field, LocalModelSetupField::ApiKey);
        assert!(
            state
                .status
                .as_deref()
                .is_some_and(|status| status.contains("needs an API key")),
            "status should say what to do next: {:?}",
            state.status
        );

        // A plain unreachable-server failure still lands on manual model entry.
        state.apply_fetch_error("error sending request for url".to_string());
        assert_eq!(state.step, LocalModelWizardStep::SelectModels);
    }

    #[test]
    fn field_navigation_wraps_at_both_ends() {
        let mut state = LocalModelWizardState::default();
        assert_eq!(state.setup_field, LocalModelSetupField::Preset);

        // Six forward steps visit every Setup field and return to the first.
        for _ in 0..6 {
            state.move_field(1);
        }
        assert_eq!(state.setup_field, LocalModelSetupField::Preset);

        // Backward from the first field lands on the last.
        state.move_field(-1);
        assert_eq!(state.setup_field, LocalModelSetupField::CredentialStorage);

        // The metadata fields wrap the same way.
        state.step = LocalModelWizardStep::Metadata;
        state.metadata_field = LocalModelMetadataField::ToolCalls;
        state.move_field(1);
        assert_eq!(state.metadata_field, LocalModelMetadataField::ContextWindow);
        state.move_field(-1);
        assert_eq!(state.metadata_field, LocalModelMetadataField::ToolCalls);
    }

    #[test]
    fn arrow_cycling_only_applies_to_choice_fields() {
        let mut state = LocalModelWizardState::default();
        assert!(state.setup_field_is_choice(), "Server field is a choice");
        state.setup_field = LocalModelSetupField::BaseUrl;
        assert!(!state.setup_field_is_choice(), "text fields are not");
        state.setup_field = LocalModelSetupField::CredentialStorage;
        assert!(state.setup_field_is_choice(), "Store key is a choice");

        // Backward cycle on credential storage: File -> Session.
        state.cycle_setup_choice(-1);
        assert_eq!(
            state.credential_persistence,
            crate::session::CredentialPersistence::Session
        );
        state.cycle_setup_choice(1);
        assert_eq!(
            state.credential_persistence,
            crate::session::CredentialPersistence::File
        );
    }

    #[test]
    fn credential_storage_toggle_is_explicit_and_session_safe() {
        let mut state = LocalModelWizardState {
            setup_field: LocalModelSetupField::CredentialStorage,
            ..LocalModelWizardState::default()
        };
        assert_eq!(
            state.credential_persistence,
            crate::session::CredentialPersistence::File
        );

        state.toggle_setup_choice();

        assert_eq!(
            state.credential_persistence,
            crate::session::CredentialPersistence::Keyring
        );
        state.toggle_setup_choice();
        assert_eq!(
            state.credential_persistence,
            crate::session::CredentialPersistence::Session
        );
    }

    #[test]
    fn fetched_metadata_prefills_model_rows() {
        let mut state = LocalModelWizardState::default();

        state.apply_fetch_success(WizardFetchOutcome {
            models: vec![
                crate::model_catalog::AvailableModel::with_metadata(
                    "qwen3-coder",
                    Some(131_072),
                    Some("Qwen3 Coder".to_string()),
                    vec![ModelFeature::ToolCall],
                )
                .with_output_limit(Some(32_768)),
                crate::model_catalog::AvailableModel::remote("bare-model"),
            ],
            detected: Some(ProviderPreset::Ollama),
        });

        assert_eq!(state.preset, ProviderPreset::Ollama);
        assert_eq!(state.models[0].context_window, "131072");
        assert_eq!(state.models[0].output_limit, "32768");
        assert_eq!(state.models[0].display_name.as_deref(), Some("Qwen3 Coder"));
        assert!(state.models[0].tool_call);
        assert_eq!(state.models[1].context_window, "");
        assert!(
            state.models[1].tool_call,
            "unreported capabilities keep the optimistic tools default"
        );
        assert!(
            state
                .status
                .as_deref()
                .is_some_and(|status| status.contains("Detected Ollama")),
            "status should surface the detection: {:?}",
            state.status
        );
    }

    #[test]
    fn for_edit_prefills_state_and_locks_provider_id() {
        let home = tempfile::TempDir::new().unwrap();
        let report = crate::model_catalog::write_local_catalog_entry(
            home.path(),
            crate::model_catalog::LocalCatalogEntryInput {
                connection: crate::model_catalog::LocalCatalogConnectionInput {
                    id: "my-local".parse().unwrap(),
                    display_name: "My Local".to_string(),
                    transport: TransportProtocol::OpenAiChat,
                    base_url: "http://localhost:11434/v1".to_string(),
                    discovery: DiscoveryKind::Ollama,
                },
                targets: vec![crate::model_catalog::LocalCatalogTargetInput {
                    remote_model: "qwen3:32b".to_string(),
                    display_name: None,
                    context_window: Some(65_536),
                    output_limit: Some(8_192),
                    tool_call: true,
                }],
            },
        )
        .unwrap();
        let catalog = crate::model_catalog::load_catalog_from_home(home.path()).unwrap();
        let connection_id = report.connection_id;
        let connection = catalog.connection(&connection_id).unwrap();
        let targets = catalog.targets_for_connection(&connection_id);

        let mut state = LocalModelWizardState::for_edit(
            connection,
            &targets,
            crate::session::CredentialPersistence::File,
        )
        .unwrap();

        assert!(state.editing_existing);
        assert_eq!(state.display_name, "My Local");
        assert_eq!(state.provider_id, "my-local");
        assert_eq!(
            state.preset,
            ProviderPreset::Ollama,
            "preset must be recovered from the persisted discovery kind"
        );
        assert_eq!(state.base_url, "http://localhost:11434/v1");
        assert_eq!(state.models.len(), 1);
        assert_eq!(state.models[0].remote_model, "qwen3:32b");
        assert_eq!(state.models[0].context_window, "65536");
        assert_eq!(state.models[0].output_limit, "8192");
        assert!(state.models[0].tool_call);
        assert!(state.catalog_input().is_ok());

        state.setup_field = LocalModelSetupField::ProviderId;
        state.input_char('x');
        state.backspace();
        assert_eq!(
            state.provider_id, "my-local",
            "provider id must stay fixed while editing"
        );
    }

    #[test]
    fn fetch_failure_moves_state_to_manual_entry() {
        let mut state = LocalModelWizardState::default();

        state.apply_fetch_error("boom".to_string());

        assert_eq!(state.step, LocalModelWizardStep::SelectModels);
        assert_eq!(
            state.status.as_deref(),
            Some("Fetch failed. Enter model ids manually.")
        );
        assert_eq!(state.error.as_deref(), Some("boom"));
    }

    fn target_spec(model: &str, remote: Option<&str>) -> TargetSpec {
        let mut source = format!("connection = \"my-local\"\nmodel = \"{model}\"\n");
        if let Some(remote) = remote {
            source.push_str(&format!("remote_model = \"{remote}\"\n"));
        }
        toml::from_str(&source).expect("target spec should parse")
    }

    #[test]
    fn fetch_failure_suggests_local_ports_for_a_loopback_url() {
        let mut state = LocalModelWizardState {
            preset: ProviderPreset::LmStudio,
            base_url: "http://127.0.0.1:9/v1".to_string(),
            ..LocalModelWizardState::default()
        };

        state.apply_fetch_error("connection refused".to_string());

        let status = state.status.clone().expect("status should be set");
        assert!(
            status.contains("LM Studio 1234")
                && status.contains("Ollama 11434")
                && status.contains("llama.cpp 8080")
                && status.contains("vLLM 8000"),
            "a refused loopback connection is usually a wrong port: {status}"
        );
    }

    #[test]
    fn writing_activates_new_providers_but_never_steals_the_active_target() {
        let target = target_spec("my-local/qwen3-coder", Some("qwen3-coder"));
        let targets = vec![&target];
        let fallback = "my-local/qwen3-coder"
            .parse::<crate::model_catalog::ModelId>()
            .unwrap();
        let model_name = |model: Option<crate::model_catalog::ModelId>| {
            model.map(|model| model.as_str().to_string())
        };

        // A brand-new provider takes over, even though the user sits on another
        // provider — adding a provider is meant to switch to it.
        assert_eq!(
            model_name(activation_after_write(
                false,
                false,
                "anthropic/anthropic/claude-sonnet-4-6",
                &targets,
                &fallback,
            )),
            Some("my-local/qwen3-coder".to_string())
        );

        // Editing the provider you are using keeps the model you are on.
        assert_eq!(
            model_name(activation_after_write(
                true,
                true,
                "my-local/qwen3-coder",
                &targets,
                &fallback,
            )),
            Some("my-local/qwen3-coder".to_string())
        );

        // The wire id resolves to the same target.
        assert_eq!(
            model_name(activation_after_write(
                true,
                true,
                "qwen3-coder",
                &targets,
                &fallback,
            )),
            Some("my-local/qwen3-coder".to_string())
        );

        // Editing a provider you are not using leaves the active target alone.
        assert_eq!(
            model_name(activation_after_write(
                true,
                false,
                "anthropic/anthropic/claude-sonnet-4-6",
                &targets,
                &fallback,
            )),
            None
        );
    }

    #[test]
    fn dropped_model_falls_back_without_touching_a_live_selection() {
        let kept = target_spec("my-local/qwen3-coder", Some("qwen3-coder"));
        let targets = vec![&kept];
        let fallback = "my-local/qwen3-coder"
            .parse::<crate::model_catalog::ModelId>()
            .unwrap();

        assert_eq!(
            retained_provider_model(&targets, "qwen3-coder", &fallback).as_str(),
            "my-local/qwen3-coder"
        );
        assert_eq!(
            retained_provider_model(&targets, "my-local/retired-model", &fallback).as_str(),
            "my-local/qwen3-coder",
            "an edit that dropped the stored model must not leave it dangling"
        );
        // An active provider whose current model vanished also falls back.
        assert_eq!(
            activation_after_write(true, true, "my-local/retired-model", &targets, &fallback)
                .map(|model| model.as_str().to_string()),
            Some("my-local/qwen3-coder".to_string())
        );
    }

    #[test]
    fn a_hand_entered_model_counts_as_curation_before_the_next_fetch() {
        let mut state = LocalModelWizardState::default();
        state.apply_fetch_error("server not up yet".to_string());
        state.manual_model_input = "qwen3:8b".to_string();
        state.submit_selection();
        state.step = LocalModelWizardStep::SelectModels;

        state.apply_fetch_success(WizardFetchOutcome {
            models: vec![
                crate::model_catalog::AvailableModel::remote("qwen3:8b"),
                crate::model_catalog::AvailableModel::remote("llama3:8b"),
            ],
            detected: None,
        });

        assert!(
            state.models[0].selected,
            "the hand-entered model stays picked"
        );
        assert!(
            !state.models[1].selected,
            "a later discovery must not tick models the user has not reviewed"
        );
    }

    #[test]
    fn writing_a_provider_relinks_mode_models_only_when_the_active_model_moves() {
        use crate::session::SessionStore;

        let target = target_spec("my-local/qwen3-coder", Some("qwen3-coder"));
        let targets = vec![&target];
        let fallback = "my-local/qwen3-coder"
            .parse::<crate::model_catalog::ModelId>()
            .unwrap();
        let connection = "my-local"
            .parse::<crate::model_catalog::ConnectionId>()
            .unwrap();

        let mut session = SessionStore::default();
        session.ensure_provider(crate::session::DEFAULT_PROVIDER_ID);
        session.set_current_kind_id(crate::session::DEFAULT_PROVIDER_ID);
        session.record_mode_model("plan", None);
        assert!(session.mode_model("plan").is_some());

        // Creating a provider activates it — and re-links the working modes, so
        // the next mode switch cannot snap back to the old provider.
        apply_written_provider(
            &mut session,
            "my-local",
            &connection,
            false,
            &targets,
            &fallback,
        );
        assert_eq!(session.current_kind_id(), "my-local");
        assert_eq!(session.current_session().model, "my-local/qwen3-coder");
        assert_eq!(session.mode_model("plan"), None);

        // Editing the active provider while keeping its model touches nothing.
        session.record_mode_model("plan", None);
        apply_written_provider(
            &mut session,
            "my-local",
            &connection,
            true,
            &targets,
            &fallback,
        );
        assert_eq!(session.current_kind_id(), "my-local");
        assert!(
            session.mode_model("plan").is_some(),
            "an edit that keeps the model must not drop per-mode selections"
        );

        // Editing a provider the session is not on never moves the active target.
        session.set_current_kind_id(crate::session::DEFAULT_PROVIDER_ID);
        apply_written_provider(
            &mut session,
            "my-local",
            &connection,
            true,
            &targets,
            &fallback,
        );
        assert_eq!(
            session.current_kind_id(),
            crate::session::DEFAULT_PROVIDER_ID
        );
    }

    #[test]
    fn refetch_merges_and_preserves_curated_selection() {
        let mut state = LocalModelWizardState {
            editing_existing: true,
            models: vec![
                LocalModelWizardModel {
                    remote_model: "qwen3:32b".to_string(),
                    display_name: None,
                    selected: true,
                    context_window: "65536".to_string(),
                    output_limit: "8192".to_string(),
                    tool_call: true,
                },
                LocalModelWizardModel {
                    remote_model: "tiny".to_string(),
                    display_name: None,
                    selected: false,
                    context_window: String::new(),
                    output_limit: String::new(),
                    tool_call: false,
                },
                LocalModelWizardModel {
                    remote_model: "manual-model".to_string(),
                    display_name: None,
                    selected: true,
                    context_window: String::new(),
                    output_limit: String::new(),
                    tool_call: true,
                },
            ],
            ..LocalModelWizardState::default()
        };

        state.apply_fetch_success(WizardFetchOutcome {
            models: vec![
                crate::model_catalog::AvailableModel::with_metadata(
                    "qwen3:32b",
                    Some(4096),
                    None,
                    vec![ModelFeature::ToolCall],
                ),
                crate::model_catalog::AvailableModel::with_metadata(
                    "tiny",
                    Some(2048),
                    None,
                    vec![],
                )
                .with_output_limit(Some(512)),
                crate::model_catalog::AvailableModel::with_metadata(
                    "fresh:7b",
                    Some(32768),
                    None,
                    vec![ModelFeature::ToolCall],
                ),
            ],
            detected: None,
        });

        assert_eq!(
            state
                .models
                .iter()
                .map(|model| model.remote_model.as_str())
                .collect::<Vec<_>>(),
            vec!["qwen3:32b", "tiny", "manual-model", "fresh:7b"],
            "curated entries keep their order, server-only additions append"
        );
        assert_eq!(
            state.models[0].context_window, "65536",
            "a server-reported window must not overwrite a typed one"
        );
        assert_eq!(state.models[0].output_limit, "8192");
        assert_eq!(
            state.models[1].context_window, "2048",
            "blank fields take the server value"
        );
        assert_eq!(state.models[1].output_limit, "512");
        assert!(
            !state.models[1].selected,
            "an unchecked model stays unchecked after a refetch"
        );
        assert!(
            state.models[2].selected,
            "a manually entered model the server does not list survives a refetch"
        );
        assert!(
            !state.models[3].selected,
            "newly pulled models start unchecked so saving never enables the whole library"
        );
        assert_eq!(state.models[3].context_window, "32768");
        assert_eq!(state.selected_model_count(), 2);
        assert!(
            state
                .status
                .as_deref()
                .is_some_and(|status| status.contains("1 new left unchecked")),
            "status should say new models were added unchecked: {:?}",
            state.status
        );
    }

    #[test]
    fn first_fetch_selects_every_listed_model() {
        let mut state = LocalModelWizardState::default();

        state.apply_fetch_success(WizardFetchOutcome {
            models: vec![
                crate::model_catalog::AvailableModel::remote("alpha"),
                crate::model_catalog::AvailableModel::remote("beta"),
            ],
            detected: None,
        });

        assert!(state.models.iter().all(|model| model.selected));
        assert_eq!(state.selected_model_count(), 2);
        assert!(
            !state
                .status
                .as_deref()
                .is_some_and(|status| status.contains("unchecked")),
            "nothing is left unchecked on a first fetch: {:?}",
            state.status
        );
    }

    #[test]
    fn fetch_failure_keeps_curated_models_for_offline_edits() {
        let mut state = LocalModelWizardState {
            editing_existing: true,
            models: vec![
                LocalModelWizardModel {
                    remote_model: "qwen3:32b".to_string(),
                    display_name: None,
                    selected: true,
                    context_window: "65536".to_string(),
                    output_limit: String::new(),
                    tool_call: true,
                },
                LocalModelWizardModel {
                    remote_model: "qwen3:8b".to_string(),
                    display_name: None,
                    selected: true,
                    context_window: String::new(),
                    output_limit: String::new(),
                    tool_call: true,
                },
                LocalModelWizardModel {
                    remote_model: "llama3:8b".to_string(),
                    display_name: None,
                    selected: false,
                    context_window: String::new(),
                    output_limit: String::new(),
                    tool_call: true,
                },
            ],
            model_cursor: 2,
            ..LocalModelWizardState::default()
        };

        state.apply_fetch_error("connection refused".to_string());

        assert_eq!(state.models.len(), 3);
        assert_eq!(state.models[0].context_window, "65536");
        assert_eq!(
            state.model_cursor, 2,
            "the cursor stays inside the surviving list instead of resetting"
        );
        assert!(
            state
                .status
                .as_deref()
                .is_some_and(|status| status.contains("keeping the models already listed")),
            "status should say the listed models were kept: {:?}",
            state.status
        );
        assert_eq!(state.error.as_deref(), Some("connection refused"));
    }
}
