//! Durable user settings for compiled delegated subagents.
//!
//! Custom agents remain Markdown resources. These rows are deliberately keyed
//! by a typed built-in id so changing a model assignment or enabled state never
//! changes the subagent's identity or promotes it into a switchable persona.

use super::*;
use crate::subagent::{
    BuiltinSubagentId, BuiltinSubagentSettings, BuiltinSubagentSettingsRegistry,
};

impl Storage {
    /// Load every persisted built-in subagent setting, keyed by stable id.
    pub(crate) async fn load_builtin_subagent_settings(
        &self,
    ) -> Result<BuiltinSubagentSettingsRegistry> {
        let rows = sqlx::query(
            r#"
            SELECT
              subagent_id,
              enabled,
              primary_model,
              primary_effort,
              fallback_model,
              fallback_effort
            FROM builtin_subagent_settings
            ORDER BY subagent_id
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to load built-in subagent settings")?;

        let mut settings = BuiltinSubagentSettingsRegistry::new();
        for row in rows {
            let stored_id: String = row.try_get("subagent_id")?;
            let id = BuiltinSubagentId::parse(&stored_id)
                .with_context(|| format!("Unknown persisted built-in subagent id `{stored_id}`"))?;
            settings.insert(
                id,
                BuiltinSubagentSettings {
                    enabled: row.try_get::<i64, _>("enabled")? != 0,
                    primary_model: row.try_get("primary_model")?,
                    primary_effort: row.try_get("primary_effort")?,
                    fallback_model: row.try_get("fallback_model")?,
                    fallback_effort: row.try_get("fallback_effort")?,
                },
            );
        }
        Ok(settings)
    }

    /// Insert or replace one built-in subagent's global user settings.
    pub(crate) async fn upsert_builtin_subagent_settings(
        &self,
        id: BuiltinSubagentId,
        settings: &BuiltinSubagentSettings,
    ) -> Result<()> {
        validate_model_assignment(
            "primary",
            settings.primary_model.as_deref(),
            settings.primary_effort.as_deref(),
        )?;
        validate_model_assignment(
            "fallback",
            settings.fallback_model.as_deref(),
            settings.fallback_effort.as_deref(),
        )?;

        sqlx::query(
            r#"
            INSERT INTO builtin_subagent_settings (
              subagent_id,
              enabled,
              primary_model,
              primary_effort,
              fallback_model,
              fallback_effort,
              updated_at_ms
            )
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(subagent_id) DO UPDATE SET
              enabled = excluded.enabled,
              primary_model = excluded.primary_model,
              primary_effort = excluded.primary_effort,
              fallback_model = excluded.fallback_model,
              fallback_effort = excluded.fallback_effort,
              updated_at_ms = excluded.updated_at_ms
            "#,
        )
        .bind(id.as_str())
        .bind(i64::from(settings.enabled))
        .bind(settings.primary_model.as_deref())
        .bind(settings.primary_effort.as_deref())
        .bind(settings.fallback_model.as_deref())
        .bind(settings.fallback_effort.as_deref())
        .bind(now_ms())
        .execute(&self.pool)
        .await
        .with_context(|| format!("Failed to save built-in subagent settings for `{id}`"))?;
        Ok(())
    }

    /// Delete one override so the compiled built-in returns to its defaults.
    #[cfg(test)]
    pub(crate) async fn reset_builtin_subagent_settings(
        &self,
        id: BuiltinSubagentId,
    ) -> Result<bool> {
        let result = sqlx::query("DELETE FROM builtin_subagent_settings WHERE subagent_id = ?")
            .bind(id.as_str())
            .execute(&self.pool)
            .await
            .with_context(|| format!("Failed to reset built-in subagent settings for `{id}`"))?;
        Ok(result.rows_affected() > 0)
    }
}

fn validate_model_assignment(label: &str, model: Option<&str>, effort: Option<&str>) -> Result<()> {
    if model.is_none() && effort.is_some() {
        anyhow::bail!("{label} effort requires a {label} model");
    }
    if model.is_some_and(|value| value.trim().is_empty()) {
        anyhow::bail!("{label} model cannot be empty");
    }
    if effort.is_some_and(|value| value.trim().is_empty()) {
        anyhow::bail!("{label} effort cannot be empty");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_utils::TestStorage;

    fn assigned_settings() -> BuiltinSubagentSettings {
        BuiltinSubagentSettings {
            enabled: false,
            primary_model: Some("f".to_string()),
            primary_effort: Some("low".to_string()),
            fallback_model: Some("codex:openai/gpt-5.5".to_string()),
            fallback_effort: Some("high".to_string()),
        }
    }

    #[tokio::test]
    async fn settings_round_trip_update_and_reset_by_typed_id() {
        let fixture = TestStorage::new().await;
        let id = BuiltinSubagentId::Explore;
        assert!(
            fixture
                .storage
                .load_builtin_subagent_settings()
                .await
                .unwrap()
                .is_empty()
        );

        fixture
            .storage
            .upsert_builtin_subagent_settings(id, &assigned_settings())
            .await
            .unwrap();
        let loaded = fixture
            .storage
            .load_builtin_subagent_settings()
            .await
            .unwrap();
        assert_eq!(loaded.get(&id), Some(&assigned_settings()));

        let replacement = BuiltinSubagentSettings::default();
        fixture
            .storage
            .upsert_builtin_subagent_settings(id, &replacement)
            .await
            .unwrap();
        let loaded = fixture
            .storage
            .load_builtin_subagent_settings()
            .await
            .unwrap();
        assert_eq!(
            loaded.len(),
            1,
            "upsert must retain one row per built-in id"
        );
        assert_eq!(loaded.get(&id), Some(&replacement));

        assert!(
            fixture
                .storage
                .reset_builtin_subagent_settings(id)
                .await
                .unwrap()
        );
        assert!(
            !fixture
                .storage
                .reset_builtin_subagent_settings(id)
                .await
                .unwrap()
        );
        assert!(
            fixture
                .storage
                .load_builtin_subagent_settings()
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn effort_without_model_is_rejected_before_persistence() {
        let fixture = TestStorage::new().await;
        let settings = BuiltinSubagentSettings {
            primary_effort: Some("high".to_string()),
            ..BuiltinSubagentSettings::default()
        };

        let error = fixture
            .storage
            .upsert_builtin_subagent_settings(BuiltinSubagentId::Review, &settings)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("primary effort requires"));
        assert!(
            fixture
                .storage
                .load_builtin_subagent_settings()
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn shared_registry_replaces_and_mutates_without_changing_identity() {
        let id = BuiltinSubagentId::SecurityReview;
        let shared = crate::subagent::SharedBuiltinSubagentSettings::default();
        assert_eq!(shared.get(id), None);

        shared.upsert(id, assigned_settings());
        assert_eq!(shared.get(id), Some(assigned_settings()));

        let mut replacement = BuiltinSubagentSettingsRegistry::new();
        replacement.insert(
            BuiltinSubagentId::Research,
            BuiltinSubagentSettings::default(),
        );
        shared.replace(replacement.clone());
        assert_eq!(shared.snapshot(), replacement);
        assert_eq!(
            shared.reset(BuiltinSubagentId::Research),
            Some(BuiltinSubagentSettings::default())
        );
    }
}
