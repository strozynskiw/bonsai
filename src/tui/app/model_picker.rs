use super::AppState;
use crate::model_catalog::available_model_ids_for_provider;
use crate::provider::ReasoningSelection;
use crate::tui::event::{ModalKind, PlanOpenMode};
use crate::tui::pickers::{ModelOption, ProviderOption};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModelPickerPane {
    Provider,
    #[default]
    Model,
    Reasoning,
}

/// Destination for a selection made in the shared model picker.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModelPickerTarget {
    #[default]
    Session,
    Subagent,
    ComposerPrimary,
    ComposerBackup,
}

impl ModelPickerTarget {
    pub(crate) const fn reset_label(self) -> Option<&'static str> {
        match self {
            Self::ComposerPrimary => Some("Parent default"),
            Self::ComposerBackup => Some("No backup"),
            Self::Session | Self::Subagent => None,
        }
    }

    pub(crate) const fn is_composer(self) -> bool {
        matches!(self, Self::ComposerPrimary | Self::ComposerBackup)
    }
}

/// Cursor, filter, and viewport state for the model picker modal.
#[derive(Debug, Clone, Default)]
pub struct ModelPickerState {
    pub target: ModelPickerTarget,
    pub filter: String,
    pub active_pane: ModelPickerPane,
    pub provider_cursor: usize,
    pub provider_offset: usize,
    pub cursor: usize,
    pub model_offset: usize,
    pub reasoning_cursor: usize,
    pub reasoning_offset: usize,
}

impl ModelPickerState {
    pub(crate) fn reconcile_offsets(
        &mut self,
        provider_len: usize,
        provider_capacity: usize,
        model_len: usize,
        model_capacity: usize,
        reasoning_len: usize,
        reasoning_capacity: usize,
    ) {
        self.provider_offset = reconcile_viewport(
            self.provider_offset,
            self.provider_cursor,
            provider_len,
            provider_capacity,
        );
        self.model_offset =
            reconcile_viewport(self.model_offset, self.cursor, model_len, model_capacity);
        self.reasoning_offset = reconcile_viewport(
            self.reasoning_offset,
            self.reasoning_cursor,
            reasoning_len,
            reasoning_capacity,
        );
    }
}

/// Scroll-into-view for a windowed list: the cursor moves freely inside the
/// visible window and the window shifts only when the cursor crosses an edge.
/// Shared by the model picker panes and the provider manager list.
pub(crate) fn reconcile_viewport(
    offset: usize,
    cursor: usize,
    len: usize,
    capacity: usize,
) -> usize {
    if len == 0 {
        return 0;
    }
    let capacity = capacity.max(1).min(len);
    let cursor = cursor.min(len.saturating_sub(1));
    let offset = offset.min(len.saturating_sub(capacity));
    if cursor < offset {
        cursor
    } else if cursor >= offset.saturating_add(capacity) {
        cursor.saturating_add(1).saturating_sub(capacity)
    } else {
        offset
    }
}

/// The model picker shaped for one render: deduped provider rows, the models
/// under the selected provider (after filtering), and the selected model.
pub struct ModelPickerView<'a> {
    pub provider_rows: Vec<ModelOption>,
    pub filtered_models: Vec<&'a ModelOption>,
    pub selected_model: Option<&'a ModelOption>,
    pub reset_label: Option<&'static str>,
    pub reset_selected: bool,
}

impl ModelPickerPane {
    pub(super) fn moved(self, delta: i16) -> Self {
        let panes = [Self::Provider, Self::Model, Self::Reasoning];
        let current = panes.iter().position(|pane| *pane == self).unwrap_or(1);
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            current.saturating_add(delta as usize)
        }
        .min(panes.len().saturating_sub(1));
        panes[next]
    }
}

impl AppState {
    pub(crate) fn selected_session(&self) -> Option<crate::storage::SessionSummary> {
        let Some(ModalKind::SessionPicker {
            sessions, cursor, ..
        }) = self.modal.as_ref()
        else {
            return None;
        };
        sessions
            .get((*cursor).min(sessions.len().saturating_sub(1)))
            .cloned()
    }

    pub(crate) fn selected_saved_plan(&self) -> Option<crate::storage::SavedPlanSummary> {
        let Some(ModalKind::PlanPicker {
            plans,
            query,
            cursor,
        }) = self.modal.as_ref()
        else {
            return None;
        };
        let filtered = Self::plan_picker_filtered_plans(plans, query);
        filtered
            .get((*cursor).min(filtered.len().saturating_sub(1)))
            .map(|plan| (*plan).clone())
    }

    pub(crate) fn selected_plan_open_mode(
        &self,
    ) -> Option<(crate::storage::SavedPlanSummary, PlanOpenMode)> {
        let Some(ModalKind::PlanOpenChoice { plan, cursor }) = self.modal.as_ref() else {
            return None;
        };
        let mode = match (*cursor).min(1) {
            0 => PlanOpenMode::CleanContext,
            _ => PlanOpenMode::ResumeSourceSession,
        };
        Some((plan.clone(), mode))
    }

    pub(crate) fn selected_start_plan_mode(&self) -> Option<crate::tui::event::StartPlanMode> {
        let Some(ModalKind::StartPlanChoice { cursor }) = self.modal.as_ref() else {
            return None;
        };
        Some(match (*cursor).min(1) {
            0 => crate::tui::event::StartPlanMode::CleanContext,
            _ => crate::tui::event::StartPlanMode::KeepContext,
        })
    }

    pub fn sync_provider_choices(&mut self, registry: &crate::provider::ProviderRegistry) {
        self.provider_choices = registry
            .all()
            .iter()
            .map(|factory| ProviderOption {
                provider_id: factory.metadata().id.to_string(),
                provider_label: factory.metadata().display_name.to_string(),
                authorized: false,
                current: false,
                uses_endpoint_auth_form: factory.metadata().auth_requirement.uses_endpoint_setup(),
            })
            .collect();
    }

    pub fn sync_cached_model_choices(
        &mut self,
        session: &crate::session::SessionStore,
        registry: &crate::provider::ProviderRegistry,
        catalog: Option<&crate::model_catalog::ModelCatalog>,
    ) {
        self.cached_model_choices = session
            .providers
            .iter()
            .filter_map(|(provider_id, provider_session)| {
                let factory = registry.lookup(provider_id)?;
                factory.is_authorized(provider_session).then_some((
                    provider_id,
                    provider_session,
                    factory,
                ))
            })
            .flat_map(|(provider_id, provider_session, factory)| {
                let metadata = factory.metadata();
                let provider_label = metadata.display_name.to_string();
                available_model_ids_for_provider(
                    catalog,
                    provider_id,
                    metadata,
                    &provider_session.model,
                )
                .into_iter()
                .map(move |model| {
                    ModelOption::from_provider_model(
                        catalog,
                        provider_id,
                        &provider_label,
                        session,
                        metadata,
                        model,
                    )
                })
            })
            .collect();
    }

    pub fn model_picker_provider_rows(entries: &[ModelOption]) -> Vec<ModelOption> {
        // First entry per provider, in first-seen order — an O(n) seen-set
        // instead of the old O(n²) rescan.
        let mut seen = std::collections::HashSet::new();
        entries
            .iter()
            .filter(|entry| seen.insert(entry.provider_id.as_str()))
            .cloned()
            .collect()
    }

    pub fn model_picker_selected_provider<'a>(
        &'a self,
        entries: &'a [ModelOption],
    ) -> Option<&'a ModelOption> {
        let providers = Self::model_picker_provider_rows(entries);
        let selected = self
            .model_picker
            .provider_cursor
            .min(providers.len().saturating_sub(1));
        let provider_id = providers.get(selected)?.provider_id.as_str();
        entries
            .iter()
            .find(|entry| entry.provider_id == provider_id)
    }

    pub fn model_picker_filtered_models<'a>(
        &'a self,
        entries: &'a [ModelOption],
    ) -> Vec<&'a ModelOption> {
        let Some(provider) = self.model_picker_selected_provider(entries) else {
            return Vec::new();
        };
        let filter = self.model_picker.filter.to_lowercase();
        entries
            .iter()
            .filter(|entry| entry.provider_id == provider.provider_id)
            .filter(|entry| entry.matches_filter(&filter))
            .collect()
    }

    pub fn model_picker_selected_model<'a>(
        &'a self,
        entries: &'a [ModelOption],
    ) -> Option<&'a ModelOption> {
        let models = self.model_picker_filtered_models(entries);
        let reset_rows = usize::from(self.model_picker.target.reset_label().is_some());
        let selected = self.model_picker.cursor.saturating_sub(reset_rows);
        (!self.model_picker_reset_selected())
            .then(|| models.get(selected).copied())
            .flatten()
    }

    /// Shape the whole picker in one pass: the deduped provider rows, the models
    /// filtered to the selected provider, and the selected model. The render
    /// path builds this once per frame instead of re-deriving provider rows and
    /// the filtered list through three chained calls.
    pub fn model_picker_view<'a>(&self, entries: &'a [ModelOption]) -> ModelPickerView<'a> {
        let provider_rows = Self::model_picker_provider_rows(entries);
        let provider_cursor = self
            .model_picker
            .provider_cursor
            .min(provider_rows.len().saturating_sub(1));
        let filtered_models = match provider_rows.get(provider_cursor) {
            Some(provider) => {
                let filter = self.model_picker.filter.to_lowercase();
                entries
                    .iter()
                    .filter(|entry| entry.provider_id == provider.provider_id)
                    .filter(|entry| entry.matches_filter(&filter))
                    .collect::<Vec<_>>()
            }
            None => Vec::new(),
        };
        let reset_label = self.model_picker.target.reset_label();
        let reset_selected = reset_label.is_some() && self.model_picker.cursor == 0;
        let model_cursor = self
            .model_picker
            .cursor
            .saturating_sub(usize::from(reset_label.is_some()));
        let selected_model = (!reset_selected)
            .then(|| filtered_models.get(model_cursor).copied())
            .flatten();
        ModelPickerView {
            provider_rows,
            filtered_models,
            selected_model,
            reset_label,
            reset_selected,
        }
    }

    pub(crate) fn model_picker_reset_selected(&self) -> bool {
        self.model_picker.target.reset_label().is_some() && self.model_picker.cursor == 0
    }

    pub(crate) fn model_picker_model_row_count(&self, entries: &[ModelOption]) -> usize {
        self.model_picker_filtered_models(entries).len()
            + usize::from(self.model_picker.target.reset_label().is_some())
    }

    pub(crate) fn model_picker_first_model_row(&self, entries: &[ModelOption]) -> usize {
        usize::from(
            self.model_picker.target.reset_label().is_some()
                && !self.model_picker_filtered_models(entries).is_empty(),
        )
    }

    pub fn model_picker_reasoning_choices(entry: &ModelOption) -> Vec<ReasoningSelection> {
        let mut values = vec![ReasoningSelection::Default];
        values.extend(entry.supported_reasoning.iter().copied());
        values.sort_by_cached_key(|selection| (selection.sort_rank(), selection.label()));
        // `ReasoningSelection::label` is unique for all current selections,
        // including token budgets, so adjacent equal labels are equal values
        // after sorting.
        values.dedup();
        values
    }

    pub fn model_picker_selected_reasoning(&self, entry: &ModelOption) -> ReasoningSelection {
        let choices = Self::model_picker_reasoning_choices(entry);
        let selected = self
            .model_picker
            .reasoning_cursor
            .min(choices.len().saturating_sub(1));
        choices
            .get(selected)
            .copied()
            .unwrap_or(ReasoningSelection::Default)
    }

    pub fn plan_picker_filtered_plans<'a>(
        plans: &'a [crate::storage::SavedPlanSummary],
        query: &str,
    ) -> Vec<&'a crate::storage::SavedPlanSummary> {
        let query = query.trim().to_lowercase();
        plans
            .iter()
            .filter(|plan| {
                query.is_empty()
                    || plan.title.to_lowercase().contains(&query)
                    || plan.status.label().to_lowercase().contains(&query)
                    || plan
                        .branch
                        .as_deref()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&query)
                    || plan.id.to_string().contains(&query)
            })
            .collect()
    }

    pub(super) fn sync_model_picker_reasoning_cursor(&mut self, entries: &[ModelOption]) {
        let selected = self.model_picker_selected_model(entries).cloned();
        let reasoning = selected
            .as_ref()
            .map(|entry| entry.reasoning)
            .unwrap_or_default();
        let choices = selected
            .as_ref()
            .map(Self::model_picker_reasoning_choices)
            .unwrap_or_else(|| vec![ReasoningSelection::Default]);
        self.model_picker.reasoning_cursor = choices
            .iter()
            .position(|candidate| *candidate == reasoning)
            .unwrap_or(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ReasoningEffort, ReasoningSelection};
    use crate::storage::{SavedPlanId, SavedPlanStatus, SessionId};
    use crate::tui::event::AppAction;

    fn app() -> AppState {
        AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        )
    }

    fn model_entry(provider_id: &str, provider_label: &str, model: &str) -> ModelOption {
        ModelOption {
            provider_id: provider_id.to_string(),
            connection_id: provider_id.to_string(),
            provider_label: provider_label.to_string(),
            model_id: None,
            model: model.to_string(),
            display_name: crate::tui::pickers::short_model_label(model).to_string(),
            reasoning: ReasoningSelection::default(),
            recommended_reasoning: None,
            discouraged_reasoning: Vec::new(),
            supported_reasoning: vec![
                ReasoningSelection::Low,
                ReasoningSelection::Medium,
                ReasoningSelection::High,
            ],
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

    #[test]
    fn provider_rows_keep_first_seen_order_with_interleaved_duplicates() {
        let entries = vec![
            model_entry("openai", "OpenAI", "gpt-a"),
            model_entry("anthropic", "Anthropic", "claude-a"),
            model_entry("openai", "OpenAI", "gpt-b"),
            model_entry("anthropic", "Anthropic", "claude-b"),
            model_entry("google", "Google", "gemini-a"),
        ];
        let rows = AppState::model_picker_provider_rows(&entries);
        let ids: Vec<&str> = rows.iter().map(|r| r.provider_id.as_str()).collect();
        assert_eq!(ids, ["openai", "anthropic", "google"]);
    }

    fn saved_plan(
        id: i64,
        title: &str,
        branch: Option<&str>,
        status: &str,
    ) -> crate::storage::SavedPlanSummary {
        crate::storage::SavedPlanSummary {
            id: SavedPlanId::from_raw(id),
            project_path: "/tmp/project".to_string(),
            title: title.to_string(),
            source_session_id: Some(SessionId::from_raw(id)),
            branch: branch.map(str::to_string),
            status: SavedPlanStatus::from_db_str(status),
            execution_session_id: None,
            saved_at_ms: 1_000,
            updated_at_ms: 1_000,
            section_count: 1,
            task_count: 2,
        }
    }

    #[test]
    fn model_picker_move_clamps_to_filtered_size() {
        let mut app = app();
        app.reduce(AppAction::OpenModal(ModalKind::ModelPicker {
            entries: vec![
                model_entry("opencode", "OpenCode Go", "qwen3.7-max"),
                model_entry("anthropic", "Anthropic", "claude"),
            ],
        }));
        // Filter down to a single entry, then overshoot the cursor.
        for ch in "qwen".chars() {
            app.reduce(AppAction::ModelPickerInputChar(ch));
        }
        for _ in 0..40 {
            app.reduce(AppAction::ModelPickerMove(1));
        }
        assert_eq!(
            app.model_picker.cursor, 0,
            "cursor should clamp to the single filtered entry"
        );
    }

    #[test]
    fn opening_model_picker_keeps_modal_and_caches_choices() {
        let mut app = app();

        app.reduce(AppAction::OpenModal(ModalKind::ModelPicker {
            entries: vec![model_entry("opencode", "OpenCode Go", "qwen3.7-max")],
        }));

        assert!(matches!(app.modal, Some(ModalKind::ModelPicker { .. })));
        assert_eq!(app.cached_model_choices.len(), 1);
        assert_eq!(app.cached_model_choices[0].model, "qwen3.7-max");
    }

    #[test]
    fn plan_picker_search_filters_and_keeps_selected_plan() {
        let mut app = app();
        app.reduce(AppAction::OpenModal(ModalKind::PlanPicker {
            plans: vec![
                saved_plan(1, "Parser cleanup", Some("main"), "draft"),
                saved_plan(2, "Plan library", Some("feature/plans"), "started"),
            ],
            query: String::new(),
            cursor: 0,
        }));

        for ch in "library".chars() {
            app.reduce(AppAction::PlanPickerInputChar(ch));
        }

        assert!(matches!(
            app.modal,
            Some(ModalKind::PlanPicker { ref query, cursor: 0, .. }) if query == "library"
        ));
        let selected = app
            .selected_saved_plan()
            .expect("filtered plan should be selected");
        assert_eq!(selected.id, SavedPlanId::from_raw(2));
        assert_eq!(selected.title, "Plan library");

        app.reduce(AppAction::PlanPickerInputBackspace);
        assert!(matches!(
            app.modal,
            Some(ModalKind::PlanPicker { ref query, cursor: 0, .. }) if query == "librar"
        ));
    }

    #[test]
    fn plan_open_choice_defaults_to_clean_context_and_moves() {
        let mut app = app();
        app.reduce(AppAction::OpenModal(ModalKind::PlanOpenChoice {
            plan: saved_plan(7, "Saved workflow", Some("main"), "draft"),
            cursor: 0,
        }));

        let (_, mode) = app
            .selected_plan_open_mode()
            .expect("open choice should be selected");
        assert_eq!(mode, PlanOpenMode::CleanContext);

        app.reduce(AppAction::PlanOpenChoiceMove(1));

        let (_, mode) = app
            .selected_plan_open_mode()
            .expect("open choice should still be selected");
        assert_eq!(mode, PlanOpenMode::ResumeSourceSession);
    }

    #[test]
    fn start_plan_choice_defaults_to_clean_context_and_moves() {
        let mut app = app();
        app.reduce(AppAction::OpenModal(ModalKind::StartPlanChoice {
            cursor: 0,
        }));

        assert_eq!(
            app.selected_start_plan_mode(),
            Some(crate::tui::event::StartPlanMode::CleanContext)
        );
        app.reduce(AppAction::StartPlanChoiceMove(1));
        assert_eq!(
            app.selected_start_plan_mode(),
            Some(crate::tui::event::StartPlanMode::KeepContext)
        );
    }

    #[test]
    fn model_picker_pane_navigation_and_effort_selection_are_separate() {
        let mut app = app();
        let mut anthropic = model_entry("anthropic", "Anthropic", "claude");
        anthropic.supported_reasoning.clear();
        app.reduce(AppAction::OpenModal(ModalKind::ModelPicker {
            entries: vec![model_entry("codex", "Codex", "gpt-5.5"), anthropic],
        }));

        app.reduce(AppAction::ModelPickerMovePane(-1));
        assert_eq!(app.model_picker.active_pane, ModelPickerPane::Provider);
        app.reduce(AppAction::ModelPickerMove(1));
        assert_eq!(app.model_picker.provider_cursor, 1);
        app.reduce(AppAction::ModelPickerMovePane(1));
        app.reduce(AppAction::ModelPickerMovePane(1));
        assert_eq!(app.model_picker.active_pane, ModelPickerPane::Reasoning);
        app.reduce(AppAction::ModelPickerMove(1));
        assert_eq!(
            app.model_picker.reasoning_cursor, 0,
            "unsupported reasoning choices must not become selected"
        );
    }

    #[test]
    fn model_picker_uses_saved_effort_for_selected_model() {
        fn selected_reasoning(app: &AppState) -> ReasoningSelection {
            let entries = match &app.modal {
                Some(ModalKind::ModelPicker { entries }) => entries,
                _ => panic!("model picker must be open"),
            };
            let entry = app
                .model_picker_selected_model(entries)
                .expect("model should be selected");
            AppState::model_picker_reasoning_choices(entry)[app.model_picker.reasoning_cursor]
        }

        let mut app = app();
        app.model = "gpt-5.4-mini".to_string();
        let mut high = model_entry("codex", "Codex", "gpt-5.5");
        high.reasoning = ReasoningSelection::from_effort(ReasoningEffort::High);
        let mut low = model_entry("codex", "Codex", "gpt-5.4-mini");
        low.reasoning = ReasoningSelection::from_effort(ReasoningEffort::Low);

        app.reduce(AppAction::OpenModal(ModalKind::ModelPicker {
            entries: vec![high, low],
        }));
        assert_eq!(app.model_picker.cursor, 1);
        assert_eq!(selected_reasoning(&app), ReasoningSelection::Low);

        app.reduce(AppAction::ModelPickerMove(i16::MIN));
        assert_eq!(app.model_picker.cursor, 0);
        assert_eq!(selected_reasoning(&app), ReasoningSelection::High);
    }

    #[test]
    fn viewport_follows_only_after_cursor_crosses_an_edge() {
        let mut state = ModelPickerState {
            cursor: 3,
            ..ModelPickerState::default()
        };
        state.reconcile_offsets(0, 5, 12, 5, 0, 5);
        assert_eq!(
            state.model_offset, 0,
            "movement within the pane must not scroll"
        );

        state.cursor = 5;
        state.reconcile_offsets(0, 5, 12, 5, 0, 5);
        assert_eq!(
            state.model_offset, 1,
            "crossing the bottom advances one row"
        );
        state.cursor = 7;
        state.reconcile_offsets(0, 5, 12, 5, 0, 5);
        assert_eq!(state.model_offset, 3);

        state.cursor = 4;
        state.reconcile_offsets(0, 5, 12, 5, 0, 5);
        assert_eq!(
            state.model_offset, 3,
            "reverse movement within the pane is stable"
        );
        state.cursor = 2;
        state.reconcile_offsets(0, 5, 12, 5, 0, 5);
        assert_eq!(state.model_offset, 2, "crossing the top retreats one row");
    }

    #[test]
    fn viewport_handles_page_home_end_resize_and_result_shrink() {
        let mut state = ModelPickerState {
            cursor: 14,
            model_offset: 4,
            ..ModelPickerState::default()
        };
        state.reconcile_offsets(0, 5, 20, 5, 0, 5);
        assert_eq!(
            state.model_offset, 10,
            "page movement makes its result visible"
        );

        state.cursor = 0;
        state.reconcile_offsets(0, 5, 20, 5, 0, 5);
        assert_eq!(state.model_offset, 0, "Home reveals the first row");
        state.cursor = 19;
        state.reconcile_offsets(0, 5, 20, 5, 0, 5);
        assert_eq!(state.model_offset, 15, "End reveals the last row");

        state.reconcile_offsets(0, 5, 20, 10, 0, 5);
        assert_eq!(
            state.model_offset, 10,
            "a taller terminal clamps the offset"
        );
        state.cursor = 2;
        state.reconcile_offsets(0, 5, 3, 10, 0, 5);
        assert_eq!(state.model_offset, 0, "shrinking results clamps the offset");
    }

    #[test]
    fn opening_filtering_and_provider_changes_reset_affected_offsets() {
        let mut app = app();
        app.model_picker.provider_offset = 4;
        app.model_picker.model_offset = 5;
        app.model_picker.reasoning_offset = 2;
        let entries = vec![
            model_entry("codex", "Codex", "gpt-a"),
            model_entry("codex", "Codex", "gpt-b"),
            model_entry("anthropic", "Anthropic", "claude"),
        ];
        app.reduce(AppAction::OpenModal(ModalKind::ModelPicker { entries }));
        assert_eq!(app.model_picker.provider_offset, 0);
        assert_eq!(app.model_picker.model_offset, 0);
        assert_eq!(app.model_picker.reasoning_offset, 0);

        app.model_picker.model_offset = 1;
        app.model_picker.reasoning_offset = 1;
        app.reduce(AppAction::ModelPickerInputChar('g'));
        assert_eq!(app.model_picker.model_offset, 0);
        assert_eq!(app.model_picker.reasoning_offset, 0);

        app.model_picker.active_pane = ModelPickerPane::Provider;
        app.model_picker.model_offset = 1;
        app.model_picker.reasoning_offset = 1;
        app.reduce(AppAction::ModelPickerMove(1));
        assert_eq!(app.model_picker.model_offset, 0);
        assert_eq!(app.model_picker.reasoning_offset, 0);
    }

    #[test]
    fn composer_primary_open_preselects_current_model_and_effort() {
        let mut app = app();
        let mut state = crate::tui::agent_composer::AgentComposerState::new();
        state.field = crate::tui::agent_composer::AgentComposerField::Model;
        state.set_focused_model_selection(
            "anthropic:claude-b".to_string(),
            ReasoningSelection::High,
        );
        app.pending_composer_state = Some(Box::new(state));
        app.model_picker.target = ModelPickerTarget::ComposerPrimary;

        app.reduce(AppAction::OpenModal(ModalKind::ModelPicker {
            entries: vec![
                model_entry("codex", "Codex", "gpt-a"),
                model_entry("anthropic", "Anthropic", "claude-a"),
                model_entry("anthropic", "Anthropic", "claude-b"),
            ],
        }));

        assert_eq!(app.model_picker.provider_cursor, 1);
        assert_eq!(app.model_picker.cursor, 2, "reset row occupies cursor zero");
        let entries = match app.modal.as_ref() {
            Some(ModalKind::ModelPicker { entries }) => entries,
            _ => panic!("model picker should be open"),
        };
        let selected = app
            .model_picker_selected_model(entries)
            .expect("configured primary should be selected");
        assert_eq!(selected.model, "claude-b");
        assert_eq!(
            app.model_picker_selected_reasoning(selected),
            ReasoningSelection::High
        );
    }

    #[test]
    fn composer_backup_open_is_independent_and_cancel_restores_state() {
        let mut app = app();
        let mut state = crate::tui::agent_composer::AgentComposerState::new();
        state.field = crate::tui::agent_composer::AgentComposerField::BackupModel;
        state.model.set_text("codex:gpt-a".to_string());
        state.effort_index = 4;
        state
            .set_focused_model_selection("anthropic:claude-b".to_string(), ReasoningSelection::Low);
        app.pending_composer_state = Some(Box::new(state));
        app.model_picker.target = ModelPickerTarget::ComposerBackup;
        app.reduce(AppAction::OpenModal(ModalKind::ModelPicker {
            entries: vec![
                model_entry("codex", "Codex", "gpt-a"),
                model_entry("anthropic", "Anthropic", "claude-b"),
            ],
        }));

        app.reduce(AppAction::CloseModal);

        let Some(ModalKind::AgentComposer { state }) = app.modal.as_ref() else {
            panic!("cancel should restore the composer");
        };
        assert_eq!(state.selected_model(), Some("codex:gpt-a"));
        assert_eq!(state.selected_effort(), Some("high"));
        assert_eq!(state.selected_fallback_model(), Some("anthropic:claude-b"));
        assert_eq!(state.selected_fallback_effort(), Some("low"));
    }

    #[test]
    fn composer_without_model_entries_keeps_reset_row() {
        let mut app = app();
        app.pending_composer_state = Some(Box::new(
            crate::tui::agent_composer::AgentComposerState::new(),
        ));
        app.model_picker.target = ModelPickerTarget::ComposerPrimary;

        app.reduce(AppAction::OpenModal(ModalKind::ModelPicker {
            entries: Vec::new(),
        }));

        assert!(app.model_picker_reset_selected());
        assert_eq!(app.model_picker_model_row_count(&[]), 1);
    }
}
