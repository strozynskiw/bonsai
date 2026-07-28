use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::model_catalog::{ConnectionId, ModelId};
use crate::model_catalog::{ModelCatalog, ResolvedModel};
use crate::provider::{ProviderRegistry, ReasoningSelection};
use crate::session::SessionStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ModelShortcutKey(char);

impl ModelShortcutKey {
    pub(crate) fn new(ch: char) -> Option<Self> {
        ch.is_ascii_alphabetic()
            .then_some(Self(ch.to_ascii_lowercase()))
    }

    pub(crate) fn from_command(command: &str) -> Option<Self> {
        let rest = command.strip_prefix('/')?;
        Self::from_str(rest).ok()
    }

    pub(crate) fn as_char(self) -> char {
        self.0
    }

    pub(crate) fn as_command(self) -> String {
        format!("/{}", self.0)
    }
}

impl fmt::Display for ModelShortcutKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.to_string())
    }
}

impl FromStr for ModelShortcutKey {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut chars = value.trim().chars();
        let Some(ch) = chars.next() else {
            return Err("model shortcut must be one ASCII letter".to_string());
        };
        if chars.next().is_some() {
            return Err("model shortcut must be one ASCII letter".to_string());
        }
        Self::new(ch).ok_or_else(|| "model shortcut must be one ASCII letter".to_string())
    }
}

impl Serialize for ModelShortcutKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for ModelShortcutKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_str(&value).map_err(serde::de::Error::custom)
    }
}

/// Legacy semantic shortcut roles kept only so old `model_roles` persistence can
/// migrate to the new arbitrary-letter shortcut map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum LegacyModelRole {
    Cheap,
    Fast,
    Smart,
}

impl LegacyModelRole {
    pub(crate) fn key(self) -> ModelShortcutKey {
        match self {
            Self::Cheap => ModelShortcutKey('c'),
            Self::Fast => ModelShortcutKey('f'),
            Self::Smart => ModelShortcutKey('s'),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ModelShortcutBinding {
    pub(crate) provider_id: ConnectionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) connection_id: Option<ConnectionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model_id: Option<ModelId>,
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) reasoning: ReasoningSelection,
}

impl ModelShortcutBinding {
    pub(crate) fn matches_model(
        &self,
        provider_id: &ConnectionId,
        model: &str,
        model_id: Option<&ModelId>,
    ) -> bool {
        let same_model = self.model == model
            || self
                .model_id
                .as_ref()
                .zip(model_id)
                .is_some_and(|(bound, selected)| bound == selected);
        self.provider_id == *provider_id && same_model
    }

    pub(crate) fn matches_selection(
        &self,
        provider_id: &ConnectionId,
        model: &str,
        model_id: Option<&ModelId>,
        reasoning: ReasoningSelection,
    ) -> bool {
        self.matches_model(provider_id, model, model_id) && self.reasoning == reasoning
    }

    fn matches_binding(&self, other: &Self) -> bool {
        let same_model = self.model == other.model
            || self
                .model_id
                .as_ref()
                .zip(other.model_id.as_ref())
                .is_some_and(|(left, right)| left == right);
        self.provider_id == other.provider_id && same_model && self.reasoning == other.reasoning
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelShortcutSelection {
    pub(crate) key: ModelShortcutKey,
    pub(crate) provider_id: ConnectionId,
    pub(crate) connection_id: Option<ConnectionId>,
    pub(crate) model_id: Option<ModelId>,
    pub(crate) model: String,
    pub(crate) reasoning: ReasoningSelection,
    pub(crate) context_window: Option<u32>,
}

pub(crate) fn resolve_model_shortcut(
    registry: &ProviderRegistry,
    session_store: &SessionStore,
    catalog: Option<&ModelCatalog>,
    key: ModelShortcutKey,
) -> Option<ModelShortcutSelection> {
    let binding = session_store.model_shortcut_binding(key)?;
    selection_from_binding(registry, catalog, key, binding)
}

fn selection_from_binding(
    registry: &ProviderRegistry,
    catalog: Option<&ModelCatalog>,
    key: ModelShortcutKey,
    binding: &ModelShortcutBinding,
) -> Option<ModelShortcutSelection> {
    let factory = registry.lookup(binding.provider_id.as_str())?;
    let metadata = factory.metadata();
    let resolved = binding
        .connection_id
        .as_ref()
        .zip(binding.model_id.as_ref())
        .and_then(|(connection_id, model_id)| catalog?.resolve(connection_id, model_id).ok())
        .or_else(|| {
            let connection_id = binding.provider_id.clone();
            catalog?.resolve_connection_model(&connection_id, &binding.model)
        });
    if let Some(resolved) = resolved {
        return Some(selection_from_resolved(
            key,
            &binding.provider_id,
            resolved.model_id.to_string(),
            &resolved,
            resolved.normalize_reasoning(binding.reasoning),
        ));
    }
    let context_window = metadata.context_window;
    Some(ModelShortcutSelection {
        key,
        provider_id: binding.provider_id.clone(),
        connection_id: binding.connection_id.clone(),
        model_id: binding.model_id.clone(),
        model: binding.model.clone(),
        reasoning: metadata.normalize_reasoning_for_model(&binding.model, binding.reasoning),
        context_window,
    })
}

fn selection_from_resolved(
    key: ModelShortcutKey,
    provider_id: &ConnectionId,
    model: String,
    resolved: &ResolvedModel,
    reasoning: ReasoningSelection,
) -> ModelShortcutSelection {
    ModelShortcutSelection {
        key,
        provider_id: provider_id.clone(),
        connection_id: Some(resolved.connection_id.clone()),
        model_id: Some(resolved.model_id.clone()),
        model,
        reasoning: resolved.normalize_reasoning(reasoning),
        context_window: resolved.context_window,
    }
}

/// Resolve the reasoning selection for a resolved model: a per-model override
/// (`model_reasoning[model_id]`), else the current-model override
/// (`model_reasoning[session.model]`), else the session default. Shared with
/// bootstrap's provider/target resolution.
pub(crate) fn reasoning_for_resolved(
    session: &crate::session::ProviderSession,
    resolved: &ResolvedModel,
) -> ReasoningSelection {
    let model_id = resolved.model_id.to_string();
    session
        .model_reasoning
        .get(model_id.as_str())
        .or_else(|| session.model_reasoning.get(session.model.as_str()))
        .copied()
        .unwrap_or(session.reasoning)
}

pub(crate) fn binding_matches_binding(
    left: &ModelShortcutBinding,
    right: &ModelShortcutBinding,
) -> bool {
    left.matches_binding(right)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_shortcut_keeps_exact_gemini_context_profile_and_reasoning() {
        let catalog = ModelCatalog::load_builtin().unwrap();
        let registry = ProviderRegistry::from_catalog(&catalog);
        let key = ModelShortcutKey::new('g').unwrap();
        let mut store = SessionStore::default();
        store.set_model_shortcut_binding(
            key,
            ModelShortcutBinding {
                provider_id: "gemini".parse().unwrap(),
                connection_id: Some("gemini".parse().unwrap()),
                model_id: Some(
                    "gemini/gemini-3.1-pro-preview-customtools-1m"
                        .parse()
                        .unwrap(),
                ),
                model: "gemini/gemini-3.1-pro-preview-customtools-1m".to_string(),
                reasoning: ReasoningSelection::High,
            },
        );
        let encoded = serde_json::to_string(&store).unwrap();
        let restored: SessionStore = serde_json::from_str(&encoded).unwrap();

        let selection = resolve_model_shortcut(&registry, &restored, Some(&catalog), key).unwrap();

        assert_eq!(
            selection.model_id.as_ref().map(ModelId::as_str),
            Some("gemini/gemini-3.1-pro-preview-customtools-1m")
        );
        assert_eq!(selection.context_window, Some(1_048_576));
        assert_eq!(selection.reasoning, ReasoningSelection::High);
    }
}
