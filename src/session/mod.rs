use std::collections::{BTreeMap, HashMap};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::model_catalog::{ConnectionId, ModelId, connection_id_for_provider_id};
use crate::model_role::{
    LegacyModelRole, ModelShortcutBinding, ModelShortcutKey, binding_matches_binding,
};
use crate::provider::ReasoningSelection;

mod credentials;
mod normalize;
mod persistence;

pub(crate) use credentials::CredentialStore;
pub use credentials::{CredentialPersistence, CredentialSource};

pub const DEFAULT_PROVIDER_ID: &str = "opencode";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderSession {
    /// Resolved runtime secret. Legacy files may deserialize this during
    /// one-time migration, but new persistence always omits it.
    #[serde(default, skip_serializing)]
    pub api_key: String,
    #[serde(default)]
    pub credential_source: CredentialSource,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub reasoning: ReasoningSelection,
    #[serde(default)]
    pub model_reasoning: HashMap<String, ReasoningSelection>,
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub is_fedramp_account: bool,
    #[serde(default)]
    pub authorized_at: Option<SystemTime>,
}

impl ProviderSession {
    pub fn new(api_key: String, base_url: String, model: String) -> Self {
        let credential_source = if api_key.trim().is_empty() {
            CredentialSource::None
        } else {
            CredentialSource::Session
        };
        Self {
            api_key,
            credential_source,
            base_url,
            model,
            context_window: None,
            reasoning: ReasoningSelection::default(),
            model_reasoning: HashMap::new(),
            account_id: String::new(),
            is_fedramp_account: false,
            authorized_at: None,
        }
    }

    pub fn reasoning_for_model(
        &self,
        metadata: &crate::provider::ProviderMetadata,
        model: &str,
    ) -> ReasoningSelection {
        let reasoning = self
            .model_reasoning
            .get(model)
            .copied()
            .unwrap_or(self.reasoning);
        metadata.normalize_reasoning_for_model(model, reasoning)
    }

    pub fn set_model_reasoning(
        &mut self,
        metadata: &crate::provider::ProviderMetadata,
        model: &str,
        reasoning: ReasoningSelection,
    ) -> ReasoningSelection {
        let reasoning = metadata.normalize_reasoning_for_model(model, reasoning);
        self.model_reasoning.insert(model.to_string(), reasoning);
        if self.model == model {
            self.reasoning = reasoning;
        }
        reasoning
    }

    /// Store an already-resolved reasoning selection verbatim. Use when the
    /// caller validated it through the catalog-aware path
    /// (`model_resolution::normalize_reasoning_for_provider_model`): the
    /// metadata-only [`set_model_reasoning`] would clamp it back to `Default`
    /// for a catalog-defined provider, whose per-model reasoning options are not
    /// mirrored onto the flat provider metadata (`capabilities_for_model` is
    /// model-agnostic).
    pub fn store_model_reasoning(&mut self, model: &str, reasoning: ReasoningSelection) {
        self.model_reasoning.insert(model.to_string(), reasoning);
        if self.model == model {
            self.reasoning = reasoning;
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LegacySessionFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opencode_go: Option<ProviderSession>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<ProviderSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStore {
    #[serde(default = "default_current_provider")]
    pub current_provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_connection_id: Option<ConnectionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_model_id: Option<ModelId>,
    #[serde(default)]
    pub providers: HashMap<String, ProviderSession>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) model_shortcuts: BTreeMap<ModelShortcutKey, ModelShortcutBinding>,
    #[serde(default, rename = "model_roles", skip_serializing)]
    legacy_model_roles: BTreeMap<LegacyModelRole, ModelShortcutBinding>,
    /// Selected UI theme name; empty means the default theme.
    #[serde(default)]
    pub theme: String,
    #[serde(default, flatten)]
    legacy: LegacySessionFields,
    #[serde(skip)]
    persistence: SessionPersistence,
    #[serde(skip)]
    credential_store: CredentialStore,
}

#[derive(Debug, Clone, Default)]
enum SessionPersistence {
    #[default]
    Toml,
    Sqlite(crate::storage::Storage),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SaveAuthPolicy {
    PreserveExisting,
    AllowClear,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self {
            current_provider: default_current_provider(),
            active_connection_id: None,
            active_model_id: None,
            providers: HashMap::new(),
            model_shortcuts: BTreeMap::new(),
            legacy_model_roles: BTreeMap::new(),
            theme: String::new(),
            legacy: LegacySessionFields::default(),
            persistence: SessionPersistence::Toml,
            credential_store: CredentialStore::default(),
        }
    }
}

impl SessionStore {
    pub fn current_kind_id(&self) -> &str {
        if self.providers.contains_key(&self.current_provider) {
            self.current_provider.as_str()
        } else {
            DEFAULT_PROVIDER_ID
        }
    }

    pub fn set_current_kind_id(&mut self, id: impl Into<String>) {
        self.current_provider = id.into();
        self.sync_active_target_from_current_session();
    }

    pub(crate) fn active_target_ids(&self) -> Option<(&ConnectionId, &ModelId)> {
        Some((
            self.active_connection_id.as_ref()?,
            self.active_model_id.as_ref()?,
        ))
    }

    pub(crate) fn active_connection_id(&self) -> Option<&ConnectionId> {
        self.active_connection_id.as_ref()
    }

    pub(crate) fn active_model_id(&self) -> Option<&ModelId> {
        self.active_model_id.as_ref()
    }

    pub(crate) fn set_active_model_target(
        &mut self,
        provider_id: &str,
        connection_id: Option<ConnectionId>,
        model_id: Option<ModelId>,
        model: &str,
    ) {
        self.ensure_provider(provider_id);
        self.current_provider = provider_id.to_string();
        self.active_connection_id =
            connection_id.or_else(|| connection_id_for_provider_id(provider_id));
        self.active_model_id = model_id;
        let persisted_model = self
            .active_model_id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| model.to_string());
        self.session_mut(provider_id).model = persisted_model;
    }

    pub fn current_session(&self) -> &ProviderSession {
        self.providers
            .get(self.current_kind_id())
            .expect("current provider session must exist after normalize")
    }

    pub fn session(&self, id: &str) -> &ProviderSession {
        self.providers
            .get(id)
            .expect("requested provider session must exist after normalize")
    }

    pub fn session_mut(&mut self, id: &str) -> &mut ProviderSession {
        self.providers
            .get_mut(id)
            .expect("requested provider session must exist after normalize")
    }

    pub fn provider_label(&self) -> &str {
        self.current_kind_id()
    }

    pub(crate) fn model_shortcut_binding(
        &self,
        key: ModelShortcutKey,
    ) -> Option<&ModelShortcutBinding> {
        self.model_shortcuts.get(&key)
    }

    pub(crate) fn set_model_shortcut_binding(
        &mut self,
        key: ModelShortcutKey,
        binding: ModelShortcutBinding,
    ) {
        self.ensure_provider(&binding.provider_id);
        self.model_shortcuts.retain(|existing_key, existing| {
            *existing_key == key || !binding_matches_binding(existing, &binding)
        });
        self.model_shortcuts.insert(key, binding);
    }

    /// Remove the binding for `key`, returning whether one was present. Lets
    /// the picker toggle a shortcut off when its key is pressed on the exact
    /// model+effort that already holds it.
    pub(crate) fn clear_model_shortcut_binding(&mut self, key: ModelShortcutKey) -> bool {
        self.model_shortcuts.remove(&key).is_some()
    }

    pub(crate) fn model_shortcuts_for_selection(
        &self,
        provider_id: &str,
        model: &str,
        model_id: Option<&str>,
    ) -> Vec<(ModelShortcutKey, ReasoningSelection)> {
        self.model_shortcuts
            .iter()
            .filter_map(|(key, binding)| {
                binding
                    .matches_model(provider_id, model, model_id)
                    .then_some((*key, binding.reasoning))
            })
            .collect()
    }

    pub(crate) fn migrate_legacy_model_roles(&mut self) {
        let legacy = std::mem::take(&mut self.legacy_model_roles);
        for (role, binding) in legacy {
            self.model_shortcuts.entry(role.key()).or_insert(binding);
        }
    }

    #[cfg(test)]
    pub fn authorized_provider_ids(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self
            .providers
            .iter()
            .filter(|(_, session)| !session.api_key.trim().is_empty())
            .map(|(id, _)| id.as_str())
            .collect();
        ids.sort();
        ids
    }

    pub(crate) fn provider_state_summary(&self) -> String {
        let mut provider_ids = self.providers.keys().collect::<Vec<_>>();
        provider_ids.sort();
        provider_ids
            .into_iter()
            .filter_map(|id| {
                let session = self.providers.get(id)?;
                Some(format!(
                    "{}:model={},credential={},runtime_key={},account_id={},authorized_at={}",
                    id,
                    if session.model.trim().is_empty() {
                        "<empty>"
                    } else {
                        session.model.as_str()
                    },
                    session.credential_source.label(),
                    presence(session.api_key.as_str()),
                    presence(session.account_id.as_str()),
                    session.authorized_at.is_some()
                ))
            })
            .collect::<Vec<_>>()
            .join(";")
    }

    pub fn ensure_provider(&mut self, id: &str) {
        if !self.providers.contains_key(id) {
            self.providers
                .insert(id.to_string(), default_session_for(id));
        }
    }

    fn sync_active_target_from_current_session(&mut self) {
        let current_provider = self.current_kind_id().to_string();
        self.active_connection_id = connection_id_for_provider_id(&current_provider);
        self.active_model_id = self
            .providers
            .get(&current_provider)
            .and_then(|session| session.model.parse::<ModelId>().ok());
    }
}

fn default_current_provider() -> String {
    DEFAULT_PROVIDER_ID.to_string()
}

fn defaults_for(id: &str) -> Option<(&'static str, &'static str)> {
    crate::provider::metadata_for(id)
        .map(|meta| (meta.default_base_url.as_ref(), meta.default_model.as_ref()))
}

fn default_session_for(id: &str) -> ProviderSession {
    if let Some((base_url, model)) = defaults_for(id) {
        ProviderSession::new(String::new(), base_url.to_string(), model.to_string())
    } else {
        ProviderSession::default()
    }
}

fn presence(value: &str) -> &'static str {
    if value.trim().is_empty() {
        "missing"
    } else {
        "set"
    }
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use super::*;

    fn make_store() -> SessionStore {
        let mut store = SessionStore::default();
        store.ensure_provider("opencode");
        store.ensure_provider("codex");
        store.ensure_provider("anthropic");
        store.ensure_provider("minimax-coding-plan");
        store
    }

    fn shortcut_binding(provider_id: &str, model: &str) -> ModelShortcutBinding {
        ModelShortcutBinding {
            provider_id: provider_id.to_string(),
            connection_id: None,
            model_id: None,
            model: model.to_string(),
            reasoning: crate::provider::ReasoningSelection::default(),
        }
    }

    #[test]
    fn model_shortcut_toggle_clears_when_the_same_model_and_effort_hold_it() {
        let mut store = make_store();
        let key = ModelShortcutKey::new('f').unwrap();
        store.set_model_shortcut_binding(key, shortcut_binding("codex", "gpt-5.5"));

        // Held by this model → the picker would clear it.
        let held = store
            .model_shortcut_binding(key)
            .expect("shortcut should be bound");
        assert!(held.matches_selection(
            "codex",
            "gpt-5.5",
            None,
            crate::provider::ReasoningSelection::default()
        ));

        assert!(store.clear_model_shortcut_binding(key));
        assert!(store.model_shortcut_binding(key).is_none());
        // Clearing an already-empty shortcut reports nothing removed.
        assert!(!store.clear_model_shortcut_binding(key));
    }

    #[test]
    fn model_shortcut_reassigns_across_models_and_respects_reasoning() {
        let mut store = make_store();
        let key = ModelShortcutKey::new('s').unwrap();
        store.set_model_shortcut_binding(key, shortcut_binding("codex", "gpt-5.5"));
        let binding = store.model_shortcut_binding(key).unwrap();

        // A different model does not hold the shortcut, so the key would reassign
        // rather than clear.
        assert!(!binding.matches_model("codex", "gpt-5.4-mini", None));
        // Model-row badges still match by model, but exact toggles include reasoning.
        assert!(binding.matches_model("codex", "gpt-5.5", None));
        assert!(!binding.matches_selection(
            "codex",
            "gpt-5.5",
            None,
            crate::provider::ReasoningSelection::High
        ));
        // Wrong provider, same model string, still not a match.
        assert!(!binding.matches_model("anthropic", "gpt-5.5", None));
    }

    #[test]
    fn model_shortcut_replaces_existing_key_for_same_model_and_effort() {
        let mut store = make_store();
        let first = ModelShortcutKey::new('m').unwrap();
        let second = ModelShortcutKey::new('x').unwrap();
        store.set_model_shortcut_binding(first, shortcut_binding("codex", "gpt-5.5"));
        store.set_model_shortcut_binding(second, shortcut_binding("codex", "gpt-5.5"));

        assert!(store.model_shortcut_binding(first).is_none());
        assert!(store.model_shortcut_binding(second).is_some());
    }

    #[test]
    fn model_shortcut_key_accepts_only_single_ascii_letters() {
        assert_eq!(ModelShortcutKey::new('S').unwrap().as_char(), 's');
        assert!("s".parse::<ModelShortcutKey>().is_ok());
        assert!("ss".parse::<ModelShortcutKey>().is_err());
        assert!("1".parse::<ModelShortcutKey>().is_err());
    }

    #[test]
    fn provider_state_summary_does_not_include_secret_values() {
        let mut store = make_store();
        store.session_mut("codex").api_key = "codex-token-secret".to_string();
        store.session_mut("codex").account_id = "codex-account-secret".to_string();

        let summary = store.provider_state_summary();

        assert!(
            summary.contains("codex:model=gpt-5.5,credential=none,runtime_key=set,account_id=set")
        );
        assert!(!summary.contains("codex-token-secret"));
        assert!(!summary.contains("codex-account-secret"));
    }

    #[test]
    fn defaults_match_per_provider() {
        let store = make_store();
        let session = store.session("opencode");
        assert_eq!(session.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(session.model, "qwen3.7-max");

        let codex = store.session("codex");
        assert_eq!(codex.base_url, "https://chatgpt.com/backend-api/codex");
        assert_eq!(codex.model, "gpt-5.5");

        let anthropic = store.session("anthropic");
        assert_eq!(anthropic.base_url, "https://api.anthropic.com");
        assert_eq!(anthropic.model, "claude-sonnet-4-5");

        let minimax = store.session("minimax-coding-plan");
        assert_eq!(minimax.base_url, "https://api.minimax.io/anthropic");
        assert_eq!(minimax.model, "MiniMax-M3");
    }

    #[test]
    fn current_kind_falls_back_to_default_when_unknown() {
        let mut store = make_store();
        store.set_current_kind_id("does-not-exist");
        assert_eq!(store.current_kind_id(), DEFAULT_PROVIDER_ID);
    }

    #[test]
    fn authorized_provider_ids_returns_only_authorized_sessions() {
        let mut store = make_store();
        store.session_mut("opencode").api_key = "sk-oc".to_string();
        store.session_mut("anthropic").api_key = "sk-ant".to_string();
        let mut authorized = store.authorized_provider_ids();
        authorized.sort();
        assert_eq!(authorized, vec!["anthropic", "opencode"]);
    }

    #[test]
    fn available_models_falls_back_to_provider_seed() {
        let store = make_store();
        let session = store.session("minimax-coding-plan");
        let metadata = crate::provider::metadata_for("minimax-coding-plan").unwrap();

        assert_eq!(
            crate::model_catalog::available_model_ids_for_provider(
                None,
                "minimax-coding-plan",
                metadata,
                &session.model
            ),
            metadata.seed_model_list()
        );
    }

    #[test]
    fn available_models_uses_configured_model_when_provider_has_no_seed() {
        // A seedless endpoint-style metadata, standing in for a custom
        // OpenAI-compatible catalog connection.
        static SEEDLESS_METADATA: LazyLock<crate::provider::ProviderMetadata> =
            LazyLock::new(|| {
                crate::provider::ProviderMetadata::new(
                    "local-endpoint",
                    "Local Endpoint",
                    "",
                    "",
                    None,
                    None,
                    None,
                    &[],
                    crate::provider::Protocol::OpenAiChat,
                    crate::provider::ProviderCapabilities::new(
                        crate::provider::NO_REASONING,
                        crate::provider::NO_PARAMETERS,
                    ),
                    "chat/completions",
                )
            });
        let mut store = make_store();
        store.ensure_provider("local-endpoint");
        store.session_mut("local-endpoint").model = "llama-local".to_string();
        let session = store.session("local-endpoint");

        assert_eq!(
            crate::model_catalog::available_model_ids_for_provider(
                None,
                "local-endpoint",
                &SEEDLESS_METADATA,
                &session.model
            ),
            vec!["llama-local".to_string()]
        );
    }

    #[test]
    fn available_models_prefers_catalog_live_models() {
        let store = make_store();
        let catalog = crate::model_catalog::ModelCatalog::load_builtin().unwrap();
        let minimax = "minimax-coding-plan"
            .parse::<crate::model_catalog::ConnectionId>()
            .unwrap();
        catalog
            .write_live_availability(
                &minimax,
                crate::model_catalog::LiveModelAvailability::from_remote_ids([
                    "MiniMax-live".to_string()
                ]),
            )
            .unwrap();
        let session = store.session("minimax-coding-plan");
        let metadata = crate::provider::metadata_for("minimax-coding-plan").unwrap();

        assert_eq!(
            crate::model_catalog::available_model_ids_for_provider(
                Some(&catalog),
                "minimax-coding-plan",
                metadata,
                &session.model
            ),
            vec!["MiniMax-live".to_string()]
        );
    }
}
