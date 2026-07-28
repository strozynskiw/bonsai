use std::borrow::Borrow;
use std::fmt;
use std::str::FromStr;

use serde::de;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ModelId(Box<str>);

impl ModelId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn catalog_provider(&self) -> &str {
        self.0
            .split_once('/')
            .map(|(provider, _model)| provider)
            .unwrap_or_default()
    }

    pub(crate) fn model(&self) -> &str {
        self.0
            .split_once('/')
            .map(|(_provider, model)| model)
            .unwrap_or_default()
    }

    pub(crate) fn fallback(connection_id: &ConnectionId) -> Self {
        Self(format!("{}/unknown", connection_id.as_str()).into())
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ModelId {
    type Err = IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.trim() != value || value.is_empty() {
            return Err(IdError::InvalidModel(value.to_string()));
        }
        let Some((provider, model)) = value.split_once('/') else {
            return Err(IdError::ModelMissingSeparator(value.to_string()));
        };
        if value.matches('/').count() != 1 {
            return Err(IdError::ModelExtraSeparator(value.to_string()));
        }
        if provider.is_empty() || model.is_empty() {
            return Err(IdError::ModelEmptyPart(value.to_string()));
        }
        if value.chars().any(char::is_whitespace) {
            return Err(IdError::InvalidModel(value.to_string()));
        }
        Ok(Self(value.into()))
    }
}

impl Serialize for ModelId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ModelId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub(crate) enum IdError {
    #[error("model id must be `provider/model`, got `{0}`")]
    ModelMissingSeparator(String),
    #[error("model id must contain exactly one `/`, got `{0}`")]
    ModelExtraSeparator(String),
    #[error("model id must have non-empty provider and model parts, got `{0}`")]
    ModelEmptyPart(String),
    #[error("model id is invalid: `{0}`")]
    InvalidModel(String),
    #[error("connection id is invalid: `{0}`")]
    InvalidConnection(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ConnectionId(Box<str>);

impl ConnectionId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn fallback(value: &str) -> Self {
        value.parse().unwrap_or_else(|_err| Self("unknown".into()))
    }
}

impl Borrow<str> for ConnectionId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ConnectionId {
    type Err = IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.trim() != value || value.is_empty() {
            return Err(IdError::InvalidConnection(value.to_string()));
        }
        if value.chars().any(char::is_whitespace) || value.contains(['/', ':']) {
            return Err(IdError::InvalidConnection(value.to_string()));
        }
        Ok(Self(value.into()))
    }
}

impl Serialize for ConnectionId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ConnectionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_id_accepts_provider_model_ids() {
        let model_id: ModelId = "anthropic/claude-sonnet-4-5".parse().unwrap();

        assert_eq!(model_id.as_str(), "anthropic/claude-sonnet-4-5");
        assert_eq!(model_id.catalog_provider(), "anthropic");
        assert_eq!(model_id.model(), "claude-sonnet-4-5");
    }

    #[test]
    fn model_id_rejects_bare_or_malformed_ids() {
        assert_eq!(
            "gpt-5.5".parse::<ModelId>(),
            Err(IdError::ModelMissingSeparator("gpt-5.5".to_string()))
        );
        assert_eq!(
            "/gpt-5.5".parse::<ModelId>(),
            Err(IdError::ModelEmptyPart("/gpt-5.5".to_string()))
        );
        assert_eq!(
            "openai/".parse::<ModelId>(),
            Err(IdError::ModelEmptyPart("openai/".to_string()))
        );
        assert_eq!(
            "a/b/c".parse::<ModelId>(),
            Err(IdError::ModelExtraSeparator("a/b/c".to_string()))
        );
        assert_eq!(
            " openai/gpt".parse::<ModelId>(),
            Err(IdError::InvalidModel(" openai/gpt".to_string()))
        );
    }

    #[test]
    fn connection_id_rejects_empty_whitespace_and_ambiguous_ids() {
        for value in ["", " ", "open ai", "codex/openai", "codex:openai"] {
            assert_eq!(
                value.parse::<ConnectionId>(),
                Err(IdError::InvalidConnection(value.to_string())),
                "{value} should be rejected"
            );
        }

        let id: ConnectionId = "openai-compatible".parse().unwrap();
        assert_eq!(id.as_str(), "openai-compatible");
    }
}
