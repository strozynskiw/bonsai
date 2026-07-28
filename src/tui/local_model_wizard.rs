use anyhow::{Context, Result};

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
            Self::OpenAiCompatible => "OpenAI compatible",
            Self::AnthropicCompatible => "Anthropic compatible",
        }
    }

    pub(crate) const fn base_url_prefill(self) -> &'static str {
        match self {
            Self::LmStudio => "http://localhost:1234/v1",
            Self::Ollama => "http://localhost:11434/v1",
            Self::OpenAiCompatible | Self::AnthropicCompatible => "",
        }
    }

    pub(crate) const fn transport(self) -> TransportProtocol {
        match self {
            Self::LmStudio | Self::Ollama | Self::OpenAiCompatible => TransportProtocol::OpenAiChat,
            Self::AnthropicCompatible => TransportProtocol::AnthropicMessages,
        }
    }

    pub(crate) const fn discovery(self) -> DiscoveryKind {
        match self {
            Self::LmStudio => DiscoveryKind::LmStudio,
            Self::Ollama => DiscoveryKind::Ollama,
            Self::OpenAiCompatible | Self::AnthropicCompatible => DiscoveryKind::Generic,
        }
    }

    pub(crate) const fn toggled(self) -> Self {
        match self {
            Self::LmStudio => Self::Ollama,
            Self::Ollama => Self::OpenAiCompatible,
            Self::OpenAiCompatible => Self::AnthropicCompatible,
            Self::AnthropicCompatible => Self::LmStudio,
        }
    }

    pub(crate) const fn toggled_back(self) -> Self {
        match self {
            Self::Ollama => Self::LmStudio,
            Self::OpenAiCompatible => Self::Ollama,
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
            (DiscoveryKind::Generic, TransportProtocol::OpenAiChat) => Some(Self::OpenAiCompatible),
            (DiscoveryKind::Generic, TransportProtocol::AnthropicMessages) => {
                Some(Self::AnthropicCompatible)
            }
            (DiscoveryKind::Generic, TransportProtocol::CodexResponses) => None,
            // The local wizard never creates provider-specific or curated
            // discovery connections.
            (DiscoveryKind::Gemini | DiscoveryKind::Mistral | DiscoveryKind::Static, _) => None,
        }
    }

    fn normalize_base_url(self, base_url: &str) -> Result<String> {
        match self.transport() {
            TransportProtocol::OpenAiChat => {
                crate::provider::normalize_openai_compatible_base_url(base_url)
            }
            TransportProtocol::AnthropicMessages | TransportProtocol::CodexResponses => {
                crate::provider::normalize_anthropic_compatible_base_url(base_url)
            }
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
    let current_index = items.iter().position(|item| *item == current).unwrap_or(0);
    let next = if delta == i16::MIN {
        0
    } else if delta == i16::MAX {
        items.len().saturating_sub(1)
    } else if delta.is_negative() {
        current_index.saturating_sub(delta.unsigned_abs() as usize)
    } else {
        current_index.saturating_add(delta as usize)
    }
    .min(items.len().saturating_sub(1));
    items[next]
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
            output_limit: String::new(),
            // An empty features list means the server didn't report
            // capabilities, not that tools are unsupported — keep the
            // optimistic default in that case.
            tool_call: model.features.is_empty()
                || model.features.contains(&ModelFeature::ToolCall),
        }
    }
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

    pub(crate) fn apply_fetch_success(&mut self, outcome: WizardFetchOutcome) {
        self.loading = false;
        self.active_fetch_request_id = None;
        self.step = LocalModelWizardStep::SelectModels;
        self.model_cursor = 0;
        let detection_note = outcome
            .detected
            .filter(|preset| *preset != self.preset)
            .map(|preset| {
                self.preset = preset;
                format!("Detected {}; using native metadata. ", preset.label())
            });
        self.models = outcome
            .models
            .iter()
            .map(LocalModelWizardModel::from_available)
            .collect::<Vec<_>>();
        let summary = if self.models.is_empty() {
            "No models returned. Enter a model id manually.".to_string()
        } else {
            format!("Selected {} fetched models.", self.models.len())
        };
        self.status = Some(format!("{}{summary}", detection_note.unwrap_or_default()));
        self.error = None;
    }

    pub(crate) fn apply_fetch_error(&mut self, message: String) {
        self.loading = false;
        self.active_fetch_request_id = None;
        self.step = LocalModelWizardStep::SelectModels;
        self.models.clear();
        self.model_cursor = 0;
        self.status = Some("Fetch failed. Enter model ids manually.".to_string());
        self.error = Some(message);
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

    fn setup_input_char(&mut self, ch: char) {
        match self.setup_field {
            LocalModelSetupField::DisplayName => {
                self.display_name.push(ch);
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
                self.provider_id.push(ch);
                self.provider_id_edited = true;
            }
            LocalModelSetupField::BaseUrl => {
                self.base_url.push(ch);
                self.base_url_edited = true;
            }
            LocalModelSetupField::ApiKey => self.api_key.push(ch),
            LocalModelSetupField::Preset | LocalModelSetupField::CredentialStorage => {}
        }
    }

    fn setup_backspace(&mut self) {
        match self.setup_field {
            LocalModelSetupField::DisplayName => {
                self.display_name.pop();
                if !self.provider_id_edited {
                    self.provider_id = slugify_provider_id(&self.display_name);
                }
            }
            LocalModelSetupField::ProviderId => {
                if self.editing_existing {
                    self.status = Some("Provider id is fixed while editing.".to_string());
                    return;
                }
                self.provider_id.pop();
                self.provider_id_edited = true;
            }
            LocalModelSetupField::BaseUrl => {
                self.base_url.pop();
                self.base_url_edited = true;
            }
            LocalModelSetupField::ApiKey => {
                self.api_key.pop();
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

fn parse_optional_u32(value: &str) -> Result<Option<u32>, String> {
    if value.is_empty() {
        return Ok(None);
    }
    value
        .parse::<u32>()
        .map(Some)
        .map_err(|err| err.to_string())
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
                ),
                crate::model_catalog::AvailableModel::remote("bare-model"),
            ],
            detected: Some(ProviderPreset::Ollama),
        });

        assert_eq!(state.preset, ProviderPreset::Ollama);
        assert_eq!(state.models[0].context_window, "131072");
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
}
