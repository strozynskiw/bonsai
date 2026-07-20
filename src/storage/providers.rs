use super::*;

impl Storage {
    /// Last selected autonomy level. Missing means use the normal startup
    /// default so existing installations retain their current behavior.
    pub(crate) async fn approval_level(&self) -> Result<Option<crate::tool::ApprovalLevel>> {
        Ok(self
            .preference("approval_level")
            .await?
            .and_then(|value| crate::tool::ApprovalLevel::parse(&value)))
    }

    pub(crate) async fn set_approval_level(&self, level: crate::tool::ApprovalLevel) -> Result<()> {
        self.set_preference("approval_level", level.label()).await
    }

    /// Last selected sandbox confinement toggle. Missing leaves the configured
    /// startup policy intact.
    pub(crate) async fn sandbox_enabled(&self) -> Result<Option<bool>> {
        Ok(parse_preference_bool(
            self.preference("sandbox_enabled").await?,
        ))
    }

    pub(crate) async fn set_sandbox_enabled(&self, enabled: bool) -> Result<()> {
        self.set_preference("sandbox_enabled", preference_bool(enabled))
            .await
    }

    /// Last selected sandbox network toggle. Missing leaves the configured
    /// startup policy intact.
    pub(crate) async fn sandbox_deny_network(&self) -> Result<Option<bool>> {
        Ok(parse_preference_bool(
            self.preference("sandbox_deny_network").await?,
        ))
    }

    pub(crate) async fn set_sandbox_deny_network(&self, deny_network: bool) -> Result<()> {
        self.set_preference("sandbox_deny_network", preference_bool(deny_network))
            .await
    }

    pub(crate) async fn credential_persistence(
        &self,
    ) -> Result<Option<crate::session::CredentialPersistence>> {
        let Some(value) = self.preference("credential_persistence").await? else {
            return Ok(None);
        };
        Ok(crate::session::CredentialPersistence::parse(&value))
    }

    pub(crate) async fn set_credential_persistence(
        &self,
        persistence: crate::session::CredentialPersistence,
    ) -> Result<()> {
        let mut tx = self
            .begin_write()
            .await
            .context("Failed to begin credential preference transaction")?;
        upsert_preference(&mut tx, "credential_persistence", persistence.as_str()).await?;
        tx.commit()
            .await
            .context("Failed to commit credential preference")?;
        Ok(())
    }

    /// Load the durable checkpoints for the guided first-run flow.
    pub(crate) async fn first_run_progress(&self) -> Result<crate::onboarding::FirstRunProgress> {
        Ok(crate::onboarding::FirstRunProgress {
            started: self
                .preference(crate::onboarding::FirstRunCheckpoint::Started.preference_key())
                .await?
                .as_deref()
                == Some("on"),
            model_confirmed: self
                .preference(crate::onboarding::FirstRunCheckpoint::ModelConfirmed.preference_key())
                .await?
                .as_deref()
                == Some("on"),
            sandbox_reviewed: self
                .preference(crate::onboarding::FirstRunCheckpoint::SandboxReviewed.preference_key())
                .await?
                .as_deref()
                == Some("on"),
            autonomy_reviewed: self
                .preference(
                    crate::onboarding::FirstRunCheckpoint::AutonomyReviewed.preference_key(),
                )
                .await?
                .as_deref()
                == Some("on"),
            completed: self
                .preference(crate::onboarding::FirstRunCheckpoint::Completed.preference_key())
                .await?
                .as_deref()
                == Some("on"),
        })
    }

    /// Persist the credential choice and wizard-start checkpoint atomically.
    pub(crate) async fn begin_first_run(
        &self,
        persistence: crate::session::CredentialPersistence,
    ) -> Result<()> {
        let mut tx = self
            .begin_write()
            .await
            .context("Failed to begin first-run preference transaction")?;
        upsert_preference(&mut tx, "credential_persistence", persistence.as_str()).await?;
        upsert_preference(
            &mut tx,
            crate::onboarding::FirstRunCheckpoint::Started.preference_key(),
            "on",
        )
        .await?;
        tx.commit()
            .await
            .context("Failed to commit first-run preferences")?;
        Ok(())
    }

    pub(crate) async fn mark_first_run_checkpoint(
        &self,
        checkpoint: crate::onboarding::FirstRunCheckpoint,
    ) -> Result<()> {
        self.set_preference(checkpoint.preference_key(), "on").await
    }

    /// Global serenity-mode preference. Missing key means off, so existing
    /// installs keep the normal presentation until the user opts in.
    pub(crate) async fn serenity_mode(&self) -> Result<bool> {
        Ok(self.preference("serenity").await?.as_deref() == Some("on"))
    }

    pub(crate) async fn set_serenity_mode(&self, on: bool) -> Result<()> {
        self.set_preference("serenity", if on { "on" } else { "off" })
            .await
    }

    /// Opt-in support lifecycle log (the redacted `bonsai::*` JSONL that
    /// `/bug` bundles can include). Missing key means off: no lifecycle file
    /// is ever written until the user explicitly enables it.
    pub(crate) async fn support_log_enabled(&self) -> Result<bool> {
        Ok(self.preference("support_log").await?.as_deref() == Some("on"))
    }

    pub(crate) async fn set_support_log_enabled(&self, on: bool) -> Result<()> {
        self.set_preference("support_log", if on { "on" } else { "off" })
            .await
    }

    /// Global SMOL preference. Missing, invalid, and legacy `auto` values are
    /// off so the reduced profile is entered only after an explicit user choice.
    pub(crate) async fn smol_preference(&self) -> Result<crate::smol::SmolPreference> {
        Ok(self
            .preference("smol")
            .await?
            .as_deref()
            .and_then(crate::smol::SmolPreference::parse)
            .unwrap_or_default())
    }

    pub(crate) async fn set_smol_preference(
        &self,
        preference: crate::smol::SmolPreference,
    ) -> Result<()> {
        self.set_preference("smol", preference.as_str()).await
    }

    /// User-selected limits for foreground runs. Missing means fully unset.
    pub(crate) async fn run_budget(&self) -> Result<crate::run_budget::RunBudget> {
        let Some(value) = self.preference("run_budget_json").await? else {
            return Ok(crate::run_budget::RunBudget::default());
        };
        crate::run_budget::RunBudget::from_json(&value)
    }

    pub(crate) async fn set_run_budget(&self, budget: crate::run_budget::RunBudget) -> Result<()> {
        let value = budget.validate()?.to_json()?;
        self.set_preference("run_budget_json", &value).await
    }

    async fn set_preference(&self, key: &str, value: &str) -> Result<()> {
        let mut tx = self
            .begin_write()
            .await
            .with_context(|| format!("Failed to begin {key} preference transaction"))?;
        upsert_preference(&mut tx, key, value).await?;
        tx.commit()
            .await
            .with_context(|| format!("Failed to commit {key} preference"))?;
        Ok(())
    }

    pub async fn load_session_store_raw(&self) -> Result<Option<SessionStore>> {
        let provider_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM provider_settings")
            .fetch_one(&self.pool)
            .await
            .context("Failed to count persisted provider settings")?;
        if provider_count == 0 {
            return Ok(None);
        }

        let current_provider = self
            .preference("current_provider")
            .await?
            .unwrap_or_else(|| crate::session::DEFAULT_PROVIDER_ID.to_string());
        let active_connection_id =
            parse_active_connection_id(self.preference("active_connection_id").await?)?;
        let active_model_id = parse_active_model_id(self.preference("active_model_id").await?)?;
        let model_shortcuts =
            parse_model_shortcuts(self.preference("model_shortcuts_json").await?)?;
        let theme = self.preference("theme").await?.unwrap_or_default();

        let rows = sqlx::query(
            r#"
            SELECT
              provider_settings.provider_id,
              provider_settings.base_url,
              provider_settings.model,
              provider_settings.reasoning_json,
              provider_settings.model_reasoning_json,
              provider_settings.account_id,
              provider_settings.is_fedramp_account,
              provider_settings.authorized_at_ms,
              provider_settings.context_window,
              COALESCE(provider_credentials.source, 'none') AS credential_source,
              COALESCE(provider_credentials.reference, '') AS credential_reference
            FROM provider_settings
            LEFT JOIN provider_credentials
              ON provider_credentials.provider_id = provider_settings.provider_id
            ORDER BY provider_settings.provider_id
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to load persisted provider settings")?;

        let mut providers = HashMap::new();
        for row in rows {
            let provider_id: String = row.try_get("provider_id")?;
            let reasoning_json: String = row.try_get("reasoning_json")?;
            let model_reasoning_json: String = row.try_get("model_reasoning_json")?;
            let authorized_at_ms: Option<i64> = row.try_get("authorized_at_ms")?;
            providers.insert(
                provider_id,
                ProviderSession {
                    api_key: String::new(),
                    credential_source: crate::session::CredentialSource::from_db(
                        row.try_get::<String, _>("credential_source")?.as_str(),
                        row.try_get("credential_reference")?,
                    ),
                    base_url: row.try_get("base_url")?,
                    model: row.try_get("model")?,
                    context_window: optional_i64_to_u32(row.try_get("context_window")?),
                    reasoning: serde_json::from_str(&reasoning_json)
                        .context("Failed to parse persisted provider reasoning")?,
                    model_reasoning: serde_json::from_str(&model_reasoning_json)
                        .context("Failed to parse persisted model reasoning")?,
                    account_id: row.try_get("account_id")?,
                    is_fedramp_account: row.try_get::<i64, _>("is_fedramp_account")? != 0,
                    authorized_at: authorized_at_ms.and_then(system_time_from_ms),
                },
            );
        }

        Ok(Some(SessionStore::from_persistent_parts(
            current_provider,
            active_connection_id,
            active_model_id,
            providers,
            Default::default(),
            model_shortcuts,
            theme,
        )))
    }

    pub async fn save_session_store_with_auth_policy(
        &self,
        store: &SessionStore,
        auth_policy: SaveAuthPolicy,
    ) -> Result<()> {
        let mut store = store.clone_for_persistence();
        if matches!(auth_policy, SaveAuthPolicy::PreserveExisting)
            && let Some(existing) = self.load_session_store_raw().await?
        {
            store.preserve_existing_auth_from_store(&existing);
        }

        let active_connection_id = store
            .active_connection_id()
            .map(ToString::to_string)
            .unwrap_or_default();
        let active_model_id = store
            .active_model_id()
            .map(ToString::to_string)
            .unwrap_or_default();
        let model_shortcuts_json = serde_json::to_string(&store.model_shortcuts)
            .context("Failed to serialize model shortcut bindings")?;

        // The whole session-store save is one transaction: preferences and every
        // provider's settings + credentials must persist together or not at all,
        // otherwise a mid-save failure leaves auth state half-written (e.g.
        // settings without matching credentials).
        let mut tx = self
            .begin_write()
            .await
            .context("Failed to begin session store save transaction")?;

        upsert_preference(&mut tx, "current_provider", &store.current_provider).await?;
        upsert_preference(&mut tx, "active_connection_id", &active_connection_id).await?;
        upsert_preference(&mut tx, "active_model_id", &active_model_id).await?;
        upsert_preference(&mut tx, "model_shortcuts_json", &model_shortcuts_json).await?;
        upsert_preference(&mut tx, "theme", &store.theme).await?;

        for (provider_id, session) in &store.providers {
            let reasoning_json = serde_json::to_string(&session.reasoning)
                .context("Failed to serialize provider reasoning")?;
            let model_reasoning_json = serde_json::to_string(&session.model_reasoning)
                .context("Failed to serialize provider model reasoning")?;
            let authorized_at_ms = session.authorized_at.and_then(system_time_to_ms);
            let context_window = session.context_window.map(i64::from);

            sqlx::query(
                r#"
                INSERT INTO provider_settings (
                  provider_id, base_url, model, reasoning_json, model_reasoning_json,
                  account_id, is_fedramp_account, authorized_at_ms, context_window
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT(provider_id) DO UPDATE SET
                  base_url = excluded.base_url,
                  model = excluded.model,
                  reasoning_json = excluded.reasoning_json,
                  model_reasoning_json = excluded.model_reasoning_json,
                  account_id = excluded.account_id,
                  is_fedramp_account = excluded.is_fedramp_account,
                  authorized_at_ms = excluded.authorized_at_ms,
                  context_window = excluded.context_window
                "#,
            )
            .bind(provider_id)
            .bind(&session.base_url)
            .bind(&session.model)
            .bind(reasoning_json)
            .bind(model_reasoning_json)
            .bind(&session.account_id)
            .bind(if session.is_fedramp_account {
                1_i64
            } else {
                0_i64
            })
            .bind(authorized_at_ms)
            .bind(context_window)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("Failed to save provider settings for {provider_id}"))?;

            sqlx::query(
                r#"
                INSERT INTO provider_credentials (
                  provider_id, source, reference
                )
                VALUES (?, ?, ?)
                ON CONFLICT(provider_id) DO UPDATE SET
                  source = excluded.source,
                  reference = excluded.reference
                "#,
            )
            .bind(provider_id)
            .bind(session.credential_source.as_db_str())
            .bind(session.credential_source.reference())
            .execute(&mut *tx)
            .await
            .with_context(|| format!("Failed to save provider credentials for {provider_id}"))?;
        }

        if matches!(auth_policy, SaveAuthPolicy::AllowClear) {
            // PreserveExisting is intentionally additive: it may be saving a
            // catalog-normalized view that omitted an unavailable provider.
            // Explicit auth-clearing/removal saves, however, make the supplied
            // store authoritative and must delete rows that disappeared from
            // it in this same transaction.
            let persisted_provider_ids: Vec<String> = sqlx::query_scalar(
                r#"
                SELECT provider_id FROM provider_settings
                UNION
                SELECT provider_id FROM provider_credentials
                "#,
            )
            .fetch_all(&mut *tx)
            .await
            .context("Failed to list persisted providers before authoritative save")?;
            for provider_id in persisted_provider_ids {
                if store.providers.contains_key(&provider_id) {
                    continue;
                }
                // provider_credentials cascades from provider_settings, but an
                // explicit delete keeps this robust to legacy/orphaned rows.
                sqlx::query("DELETE FROM provider_credentials WHERE provider_id = ?")
                    .bind(&provider_id)
                    .execute(&mut *tx)
                    .await
                    .with_context(|| {
                        format!("Failed to delete removed provider credential {provider_id}")
                    })?;
                sqlx::query("DELETE FROM provider_settings WHERE provider_id = ?")
                    .bind(&provider_id)
                    .execute(&mut *tx)
                    .await
                    .with_context(|| {
                        format!("Failed to delete removed provider settings {provider_id}")
                    })?;
            }
        }

        tx.commit()
            .await
            .context("Failed to commit session store save")?;

        tracing::info!(
            db_path = %self.db_path().display(),
            current_provider = %store.current_provider,
            providers = store.providers.len(),
            provider_state = %store.provider_state_summary(),
            "session store saved to SQLite"
        );
        Ok(())
    }

    async fn preference(&self, key: &str) -> Result<Option<String>> {
        let value = sqlx::query_scalar("SELECT value FROM user_preferences WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .with_context(|| format!("Failed to load preference {key}"))?;
        Ok(value)
    }
}

fn preference_bool(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn parse_preference_bool(value: Option<String>) -> Option<bool> {
    match value.as_deref() {
        Some("on") => Some(true),
        Some("off") => Some(false),
        _ => None,
    }
}

fn optional_i64_to_u32(value: Option<i64>) -> Option<u32> {
    value.and_then(|value| u32::try_from(value).ok())
}

async fn upsert_preference(tx: &mut Transaction<'_, Sqlite>, key: &str, value: &str) -> Result<()> {
    sqlx::query(
        r#"
            INSERT INTO user_preferences (key, value)
            VALUES (?, ?)
            ON CONFLICT(key) DO UPDATE SET
              value = excluded.value
            "#,
    )
    .bind(key)
    .bind(value)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("Failed to save preference {key}"))?;
    Ok(())
}

fn parse_active_connection_id(
    value: Option<String>,
) -> Result<Option<crate::model_catalog::ConnectionId>> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            value.parse().with_context(|| {
                format!("Failed to parse active_connection_id preference `{value}`")
            })
        })
        .transpose()
}

fn parse_active_model_id(value: Option<String>) -> Result<Option<crate::model_catalog::ModelId>> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            value
                .parse()
                .with_context(|| format!("Failed to parse active_model_id preference `{value}`"))
        })
        .transpose()
}

fn parse_model_shortcuts(
    value: Option<String>,
) -> Result<
    std::collections::BTreeMap<
        crate::model_role::ModelShortcutKey,
        crate::model_role::ModelShortcutBinding,
    >,
> {
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        return Ok(Default::default());
    };
    serde_json::from_str(&value).context("Failed to parse model_shortcuts_json preference")
}
