//! Live model-availability cache for [`ModelCatalog`]: the per-connection remote
//! model list fetched from a provider, persisted under `live_models_dir` and held
//! in the catalog's `live_availability` RwLock. The lock itself and its guard
//! helpers (`live_availability_read`/`_write`) live on the catalog in `mod.rs`;
//! this module owns the higher-level read/write/snapshot operations over them.

use std::fs;
use std::time::Duration;

use super::{CatalogError, ConnectionId, LiveModelAvailability, ModelCatalog, atomic_write};

impl ModelCatalog {
    pub(crate) fn write_live_availability(
        &self,
        connection_id: &ConnectionId,
        mut availability: LiveModelAvailability,
    ) -> Result<(), CatalogError> {
        availability.mark_refreshed();
        self.fill_canonical_model_ids(connection_id, &mut availability);
        if let Some(live_models_dir) = &self.live_models_dir {
            let content = serde_json::to_string_pretty(&availability).map_err(|source| {
                CatalogError::LiveAvailabilitySerialize {
                    connection_id: connection_id.clone(),
                    source,
                }
            })?;
            atomic_write(
                &live_models_dir.join(format!("{connection_id}.json")),
                &content,
            )?;
        }
        self.live_availability_write()
            .insert(connection_id.clone(), availability);
        Ok(())
    }

    pub(crate) fn clear_live_availability(
        &self,
        connection_id: &ConnectionId,
    ) -> Result<(), CatalogError> {
        self.live_availability_write().remove(connection_id);
        if let Some(live_models_dir) = &self.live_models_dir {
            let path = live_models_dir.join(format!("{connection_id}.json"));
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(CatalogError::WriteFile { path, source: err }),
            }
        }
        Ok(())
    }

    pub(crate) fn has_fresh_live_availability(
        &self,
        connection_id: &ConnectionId,
        ttl: Duration,
    ) -> bool {
        self.live_availability_read()
            .get(connection_id)
            .is_some_and(|availability| availability.is_fresh(ttl))
    }

    pub(crate) fn has_current_live_availability_schema(
        &self,
        connection_id: &ConnectionId,
    ) -> bool {
        self.live_availability_read()
            .get(connection_id)
            .is_some_and(|availability| {
                availability.schema_version == super::LIVE_MODEL_CACHE_SCHEMA_VERSION
            })
    }

    pub(crate) fn live_context_window_for_connection_model(
        &self,
        connection_id: &ConnectionId,
        remote_model_id: &str,
    ) -> Option<u32> {
        self.live_model_for_connection_model(connection_id, remote_model_id)
            .and_then(|model| model.context_window)
    }

    pub(crate) fn live_model_for_connection_model(
        &self,
        connection_id: &ConnectionId,
        remote_model_id: &str,
    ) -> Option<super::AvailableModel> {
        self.live_availability_read()
            .get(connection_id)?
            .models
            .iter()
            .find(|model| model.remote_model_id.as_ref() == remote_model_id)
            .cloned()
    }

    pub(crate) fn live_availability_snapshot(&self) -> Vec<(ConnectionId, LiveModelAvailability)> {
        let mut entries = self
            .live_availability_read()
            .iter()
            .map(|(connection_id, availability)| (connection_id.clone(), availability.clone()))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        entries
    }

    fn fill_canonical_model_ids(
        &self,
        connection_id: &ConnectionId,
        availability: &mut LiveModelAvailability,
    ) {
        for model in &mut availability.models {
            if model.model_id.is_some() {
                continue;
            }
            let static_model_id = self
                .target_order
                .iter()
                .filter(|(target_connection_id, _model_id)| target_connection_id == connection_id)
                .filter_map(|(target_connection_id, model_id)| {
                    self.targets
                        .get(&(target_connection_id.clone(), model_id.clone()))
                        .map(|target| (model_id, target))
                })
                .find_map(|(model_id, target)| {
                    let remote_model = target
                        .remote_model
                        .as_deref()
                        .unwrap_or_else(|| target.model.model());
                    (remote_model == model.remote_model_id.as_ref()).then(|| model_id.clone())
                });
            model.model_id = static_model_id.or_else(|| {
                self.discovered_models_dev_match(connection_id, &model.remote_model_id)
                    .and_then(|_| {
                        format!("{connection_id}/{}", model.remote_model_id)
                            .parse()
                            .ok()
                    })
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writing_live_availability_marks_it_fresh() {
        let catalog = ModelCatalog::load_builtin().unwrap();
        let connection_id = "codex".parse::<ConnectionId>().unwrap();
        catalog
            .write_live_availability(
                &connection_id,
                LiveModelAvailability::from_remote_ids(["gpt-current".to_string()]),
            )
            .unwrap();

        assert!(catalog.has_fresh_live_availability(&connection_id, Duration::from_secs(300)));
    }

    #[test]
    fn legacy_live_availability_without_timestamp_is_stale() {
        let catalog = ModelCatalog::load_builtin().unwrap();
        let connection_id = "codex".parse::<ConnectionId>().unwrap();
        catalog.live_availability_write().insert(
            connection_id.clone(),
            LiveModelAvailability::from_remote_ids(["gpt-old".to_string()]),
        );

        assert!(!catalog.has_fresh_live_availability(&connection_id, Duration::from_secs(300)));
    }

    #[test]
    fn legacy_live_availability_schema_is_stale_even_with_timestamp() {
        let catalog = ModelCatalog::load_builtin().unwrap();
        let connection_id = "codex".parse::<ConnectionId>().unwrap();
        let legacy = serde_json::from_str::<LiveModelAvailability>(
            r#"{"fetched_at_unix_secs": 9999999999, "models": [{"remote_model_id": "gpt-old"}]}"#,
        )
        .unwrap();
        catalog
            .live_availability_write()
            .insert(connection_id.clone(), legacy);

        assert!(!catalog.has_current_live_availability_schema(&connection_id));
        assert!(!catalog.has_fresh_live_availability(&connection_id, Duration::from_secs(300)));
    }
}
