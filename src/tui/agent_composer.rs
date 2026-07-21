//! State for the **agent composer** — a multi-step modal (Details → Tools →
//! Prompt → Review) that creates or edits a custom agent definition and writes it to
//! `<home>/agents/<name>.md` (or the project `.bonsai/agents/`). Mirrors the
//! shape of [`crate::tui::local_model_wizard`]: pure state + transitions here,
//! disk I/O and the model call in `crate::tui::runtime_actions`, rendering in
//! `crate::tui::widgets::modal::agent_composer`.

use std::path::{Path, PathBuf};

use crate::resource::agent::AgentRegistry;
use crate::tui::app::Composer;
use crate::util::slug::slugify;

/// Effort choices offered in the composer. Index 0 (`default`) omits `effort:`;
/// the rest mirror [`crate::provider::ReasoningEffort`] / `parse_reasoning_effort`.
pub(crate) const EFFORT_CHOICES: [&str; 7] = [
    "default", "minimal", "low", "medium", "high", "xhigh", "max",
];

/// Palette color names the composer cycles through for the persona accent color.
/// These all resolve via [`crate::tui::theme::persona_color`]; a hand-authored
/// `#rrggbb` still round-trips (displayed and preserved, but not part of the cycle).
pub(crate) const COLOR_CHOICES: [&str; 26] = [
    "blue",
    "sky",
    "indigo",
    "cyan",
    "teal",
    "turquoise",
    "mint",
    "green",
    "lime",
    "olive",
    "yellow",
    "gold",
    "amber",
    "orange",
    "coral",
    "salmon",
    "red",
    "rose",
    "pink",
    "magenta",
    "purple",
    "violet",
    "lavender",
    "brown",
    "slate",
    "gray",
];

/// UI surface (`view:`) a custom agent renders when run as a persona. Index 0
/// (`default`) omits `view:` (which the persona system treats as `chat`).
pub(crate) const VIEW_CHOICES: [&str; 4] = ["default", "chat", "todo", "canvas"];

/// How a custom agent definition can be invoked.
///
/// This is the typed composer/UI counterpart of the on-disk `surface:` field:
/// a user-facing [`Self::Agent`] participates in the persona switcher, a
/// [`Self::Subagent`] is delegated work through the `agent` tool, and
/// [`Self::Both`] supports both entry points. New and legacy surface-less
/// definitions default to [`Self::Subagent`] so creating a helper never makes it
/// Shift+Tab-selectable by accident.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum AgentDefinitionKind {
    Agent,
    #[default]
    Subagent,
    Both,
}

impl AgentDefinitionKind {
    const ALL: [Self; 3] = [Self::Agent, Self::Subagent, Self::Both];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Subagent => "subagent",
            Self::Both => "both",
        }
    }

    pub(crate) const fn detail(self) -> &'static str {
        match self {
            Self::Agent => "user-facing; available in the Shift+Tab switcher",
            Self::Subagent => "delegated by an agent; hidden from the switcher",
            Self::Both => "switchable by the user and delegatable by an agent",
        }
    }

    pub(crate) fn from_surface(surface: Option<&[String]>) -> Self {
        let has = |expected: &str| {
            surface.is_some_and(|surfaces| {
                surfaces
                    .iter()
                    .any(|surface| surface.trim().eq_ignore_ascii_case(expected))
            })
        };
        match (has("mode"), has("subagent")) {
            (true, true) => Self::Both,
            (true, false) => Self::Agent,
            (false, true) | (false, false) => Self::Subagent,
        }
    }

    const fn surfaces(self) -> &'static [&'static str] {
        match self {
            Self::Agent => &["mode"],
            Self::Subagent => &["subagent"],
            Self::Both => &["mode", "subagent"],
        }
    }

    fn moved(self, delta: i16) -> Self {
        let index = Self::ALL.iter().position(|kind| *kind == self).unwrap_or(1);
        Self::ALL[cycle(index, Self::ALL.len(), delta)]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentComposerStep {
    Details,
    Tools,
    Prompt,
    Review,
}

impl AgentComposerStep {
    pub(crate) const fn title(self) -> &'static str {
        match self {
            Self::Details => "Details",
            Self::Tools => "Tools",
            Self::Prompt => "Prompt",
            Self::Review => "Review",
        }
    }
}

/// A caret motion within a focused text field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorMotion {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentComposerField {
    Name,
    Description,
    DefinitionKind,
    Location,
    Model,
    BackupModel,
    Color,
    View,
}

impl AgentComposerField {
    const ALL: [Self; 8] = [
        Self::Name,
        Self::Description,
        Self::DefinitionKind,
        Self::Location,
        Self::Model,
        Self::BackupModel,
        Self::Color,
        Self::View,
    ];

    fn moved(self, delta: i16) -> Self {
        let index = Self::ALL.iter().position(|f| *f == self).unwrap_or(0);
        let next = if delta.is_negative() {
            index.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            index.saturating_add(delta as usize)
        }
        .min(Self::ALL.len() - 1);
        Self::ALL[next]
    }

    /// Fields that accept free text. Model rows are picker-only.
    pub(crate) const fn is_text(self) -> bool {
        matches!(self, Self::Name | Self::Description)
    }

    pub(crate) const fn is_value(self) -> bool {
        matches!(
            self,
            Self::DefinitionKind | Self::Location | Self::Color | Self::View
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentLocation {
    /// `<BONSAI_HOME>/agents/` — available in every project.
    Global,
    /// `<project>/.bonsai/agents/` — scoped to this repository.
    Project,
}

impl AgentLocation {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Global => "global (~/.bonsai/agents)",
            Self::Project => "project (.bonsai/agents)",
        }
    }

    const fn toggled(self) -> Self {
        match self {
            Self::Global => Self::Project,
            Self::Project => Self::Global,
        }
    }
}

/// Durable identity of the definition being edited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentEditTarget {
    /// A custom definition that has not been saved yet.
    NewCustom,
    /// An existing custom Markdown definition.
    Custom { source_path: PathBuf },
    /// Settings layered over a compiled delegated subagent. `legacy_path` is a
    /// pre-DB same-name Markdown source retained as an inert compatibility backup
    /// after database settings take precedence.
    BuiltinSubagent {
        id: crate::subagent::BuiltinSubagentId,
        legacy_path: Option<PathBuf>,
    },
}

impl AgentEditTarget {
    pub(crate) const fn is_builtin_subagent(&self) -> bool {
        matches!(self, Self::BuiltinSubagent { .. })
    }

    pub(crate) const fn is_editing(&self) -> bool {
        !matches!(self, Self::NewCustom)
    }

    pub(crate) fn source_path(&self) -> Option<&Path> {
        match self {
            Self::Custom { source_path } => Some(source_path.as_path()),
            Self::BuiltinSubagent { legacy_path, .. } => legacy_path.as_deref(),
            Self::NewCustom => None,
        }
    }

    pub(crate) const fn builtin_subagent_id(&self) -> Option<crate::subagent::BuiltinSubagentId> {
        match self {
            Self::BuiltinSubagent { id, .. } => Some(*id),
            Self::NewCustom | Self::Custom { .. } => None,
        }
    }
}

/// A tool the composer can grant, paired with whether it's currently selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposerTool {
    pub name: &'static str,
    pub selected: bool,
}

fn default_tools() -> Vec<ComposerTool> {
    crate::tool::GRANTABLE_AGENT_TOOLS
        .iter()
        .map(|name| ComposerTool {
            name,
            selected: false,
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentComposerState {
    pub step: AgentComposerStep,
    pub field: AgentComposerField,
    /// Editable text fields, each backed by the full [`Composer`] editor (cursor,
    /// insert/delete mid-text, wrapping). `name`/`description` are single-line;
    pub name: Composer,
    pub description: Composer,
    /// Invocation type selected in the composer. Existing explicit `surface:`
    /// frontmatter loaded through [`Self::preserved_surface`] takes precedence
    /// until the user changes this field.
    definition_kind: AgentDefinitionKind,
    pub location: AgentLocation,
    /// The `model:` selector override. Empty means parent default; otherwise it
    /// may be a one-letter shortcut or any `/model`-style concrete selector.
    pub model: Composer,
    /// The optional `fallback_model:` selector used after the primary model.
    pub fallback_model: Composer,
    /// The persona accent `color:` (a [`COLOR_CHOICES`] name or `#rrggbb`), or
    /// `None` for the default accent.
    pub color: Option<String>,
    /// Index into [`VIEW_CHOICES`]; 0 (`default`) omits `view:`.
    pub view_index: usize,
    /// Index into [`EFFORT_CHOICES`]; 0 (`default`) omits `effort:`.
    pub effort_index: usize,
    /// Index into [`EFFORT_CHOICES`]; 0 (`default`) omits `fallback_effort:`.
    pub fallback_effort_index: usize,
    pub tools: Vec<ComposerTool>,
    pub tools_cursor: usize,
    /// The system prompt / instructions body (multi-line editor).
    pub prompt: Composer,
    /// Custom definitions preserve their current enabled state across edits.
    /// Built-in enablement is edited from the browser and stored separately.
    pub enabled: bool,
    /// True while a background prompt-generation call is in flight.
    pub generating: bool,
    pub active_request_id: Option<u64>,
    /// Whether this is a new/custom file or a DB-backed compiled subagent.
    pub edit_target: AgentEditTarget,
    /// Enabled state loaded with a compiled subagent's durable settings. Custom
    /// definitions keep this as `None` because their state lives in Markdown.
    builtin_enabled: Option<bool>,
    /// Reserved frontmatter the composer doesn't edit (`permission:`/`max_turns:`),
    /// preserved on an edit round-trip so saving never drops it.
    pub preserved_permission: Option<String>,
    /// Compatibility handoff for the current disk loader. The composer exposes
    /// this as [`Self::definition_kind`] and canonicalizes it when saving.
    pub preserved_surface: Option<Vec<String>>,
    pub preserved_max_turns: Option<usize>,
    pub status: Option<String>,
    pub error: Option<String>,
}

impl AgentComposerState {
    /// A fresh composer for creating an agent.
    pub(crate) fn new() -> Self {
        Self {
            step: AgentComposerStep::Details,
            field: AgentComposerField::Name,
            name: Composer::default(),
            description: Composer::default(),
            definition_kind: AgentDefinitionKind::default(),
            location: AgentLocation::Global,
            model: Composer::default(),
            fallback_model: Composer::default(),
            color: None,
            view_index: 0,
            effort_index: 0,
            fallback_effort_index: 0,
            tools: default_tools(),
            tools_cursor: 0,
            prompt: Composer::default(),
            enabled: true,
            generating: false,
            active_request_id: None,
            edit_target: AgentEditTarget::NewCustom,
            builtin_enabled: None,
            preserved_permission: None,
            preserved_surface: None,
            preserved_max_turns: None,
            status: None,
            error: None,
        }
    }

    /// A composer pre-populated from an existing agent, for editing. `prompt` is
    /// the full body read fresh from disk. `source_path` is overwritten on save.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn edit(
        name: String,
        description: String,
        is_global: bool,
        tools: Option<&[String]>,
        model: Option<&str>,
        effort: Option<&str>,
        fallback_model: Option<&str>,
        fallback_effort: Option<&str>,
        color: Option<&str>,
        view: Option<&str>,
        prompt: String,
        source_path: PathBuf,
    ) -> Self {
        let effort_index = effort
            .and_then(|effort| EFFORT_CHOICES.iter().position(|choice| *choice == effort))
            .unwrap_or(0);
        let fallback_effort_index = fallback_effort
            .and_then(|effort| EFFORT_CHOICES.iter().position(|choice| *choice == effort))
            .unwrap_or(0);
        let view_index = view
            .and_then(|view| VIEW_CHOICES.iter().position(|choice| *choice == view))
            .unwrap_or(0);
        let mut tool_rows = default_tools();
        if let Some(declared) = tools {
            for row in &mut tool_rows {
                row.selected = declared
                    .iter()
                    .filter_map(|name| crate::tool::canonical_agent_tool(name))
                    .any(|canonical| canonical == row.name);
            }
        }
        Self {
            step: AgentComposerStep::Details,
            field: AgentComposerField::Name,
            name: composer_with(name),
            description: composer_with(description),
            definition_kind: AgentDefinitionKind::default(),
            location: if is_global {
                AgentLocation::Global
            } else {
                AgentLocation::Project
            },
            model: model
                .map(str::to_string)
                .map(composer_with)
                .unwrap_or_default(),
            fallback_model: fallback_model
                .map(str::to_string)
                .map(composer_with)
                .unwrap_or_default(),
            color: color.map(str::to_string),
            view_index,
            effort_index,
            fallback_effort_index,
            tools: tool_rows,
            tools_cursor: 0,
            prompt: composer_with(prompt),
            enabled: true,
            generating: false,
            active_request_id: None,
            edit_target: AgentEditTarget::Custom { source_path },
            builtin_enabled: None,
            // Populated by the caller from the parsed frontmatter (passthrough).
            preserved_permission: None,
            preserved_surface: None,
            preserved_max_turns: None,
            status: None,
            error: None,
        }
    }

    /// A model-settings editor for one compiled delegated subagent.
    pub(crate) fn edit_builtin_subagent(
        id: crate::subagent::BuiltinSubagentId,
        description: &str,
        instructions: &str,
        settings: &crate::subagent::BuiltinSubagentSettings,
        legacy_path: Option<PathBuf>,
    ) -> Self {
        let mut state = Self::edit(
            id.as_str().to_string(),
            description.to_string(),
            true,
            None,
            settings.primary_model.as_deref(),
            settings.primary_effort.as_deref(),
            settings.fallback_model.as_deref(),
            settings.fallback_effort.as_deref(),
            None,
            None,
            instructions.to_string(),
            PathBuf::new(),
        );
        state.field = AgentComposerField::Model;
        state.edit_target = AgentEditTarget::BuiltinSubagent { id, legacy_path };
        state.builtin_enabled = Some(settings.enabled);
        state.definition_kind = AgentDefinitionKind::Subagent;
        state.preserved_surface = Some(vec!["subagent".to_string()]);
        state
    }

    pub(crate) fn is_builtin_subagent(&self) -> bool {
        self.edit_target.is_builtin_subagent()
    }

    /// Build the durable settings represented by this built-in editor.
    pub(crate) fn builtin_subagent_settings(
        &self,
    ) -> Option<(
        crate::subagent::BuiltinSubagentId,
        crate::subagent::BuiltinSubagentSettings,
    )> {
        let id = self.edit_target.builtin_subagent_id()?;
        Some((
            id,
            crate::subagent::BuiltinSubagentSettings {
                enabled: self.builtin_enabled.unwrap_or(true),
                primary_model: self.selected_model().map(str::to_string),
                primary_effort: self.selected_effort().map(str::to_string),
                fallback_model: self.selected_fallback_model().map(str::to_string),
                fallback_effort: self.selected_fallback_effort().map(str::to_string),
            },
        ))
    }

    /// The [`Composer`] for the currently focused text field, if any (the Name /
    /// Description fields on the Details step, or the Prompt step's body).
    pub(crate) fn active_text_composer_mut(&mut self) -> Option<&mut Composer> {
        match self.step {
            AgentComposerStep::Details => match self.field {
                AgentComposerField::Name => Some(&mut self.name),
                AgentComposerField::Description => Some(&mut self.description),
                AgentComposerField::Model => Some(&mut self.model),
                AgentComposerField::BackupModel => Some(&mut self.fallback_model),
                _ => None,
            },
            AgentComposerStep::Prompt => Some(&mut self.prompt),
            AgentComposerStep::Tools | AgentComposerStep::Review => None,
        }
    }

    pub(crate) fn input_char(&mut self, ch: char) {
        self.clear_feedback();
        // Single-line fields never take a raw newline (Enter advances there).
        if let Some(composer) = self.active_text_composer_mut() {
            composer.insert_char(ch);
        }
    }

    pub(crate) fn insert_newline(&mut self) {
        self.clear_feedback();
        // Only the multi-line Prompt inserts newlines; Enter advances elsewhere.
        if matches!(self.step, AgentComposerStep::Prompt) {
            self.prompt.insert_newline();
        }
    }

    pub(crate) fn backspace(&mut self) {
        self.clear_feedback();
        if let Some(composer) = self.active_text_composer_mut() {
            composer.backspace();
            return;
        }
        // Backspace clears the Color override to its default.
        if matches!(self.step, AgentComposerStep::Details)
            && self.field == AgentComposerField::Color
        {
            self.color = None;
        }
    }

    pub(crate) fn delete_forward(&mut self) {
        self.clear_feedback();
        if let Some(composer) = self.active_text_composer_mut() {
            composer.delete_forward();
        }
    }

    pub(crate) fn cursor(&mut self, motion: CursorMotion) {
        self.clear_feedback();
        let single_line = !matches!(self.step, AgentComposerStep::Prompt);
        if let Some(composer) = self.active_text_composer_mut() {
            match motion {
                CursorMotion::Left => composer.move_left(),
                CursorMotion::Right => composer.move_right(),
                // Single-line fields have no vertical motion.
                CursorMotion::Up if !single_line => composer.move_up(),
                CursorMotion::Down if !single_line => composer.move_down(),
                CursorMotion::Up | CursorMotion::Down => {}
                CursorMotion::Home => composer.move_to_start(),
                CursorMotion::End => composer.move_to_end(),
            }
        }
    }

    pub(crate) fn paste(&mut self, text: &str) {
        self.clear_feedback();
        let single_line = !matches!(self.step, AgentComposerStep::Prompt);
        if let Some(composer) = self.active_text_composer_mut() {
            if single_line {
                // Name/Description are single-line: fold newlines into spaces.
                composer.insert_text(&text.replace(['\n', '\r'], " "));
            } else {
                composer.insert_text(text);
            }
        }
    }

    /// Move the focused field (Details) or the tools cursor (Tools).
    pub(crate) fn move_selection(&mut self, delta: i16) {
        self.clear_feedback();
        match self.step {
            AgentComposerStep::Details if self.is_builtin_subagent() => {
                const FIELDS: [AgentComposerField; 2] =
                    [AgentComposerField::Model, AgentComposerField::BackupModel];
                let index = FIELDS
                    .iter()
                    .position(|field| *field == self.field)
                    .unwrap_or(0);
                self.field = FIELDS[cycle(index, FIELDS.len(), delta)];
            }
            AgentComposerStep::Details => self.field = self.field.moved(delta),
            AgentComposerStep::Tools => {
                if self.tools.is_empty() {
                    return;
                }
                let max = self.tools.len() - 1;
                self.tools_cursor = if delta.is_negative() {
                    self.tools_cursor
                        .saturating_sub(delta.unsigned_abs() as usize)
                } else {
                    self.tools_cursor.saturating_add(delta as usize)
                }
                .min(max);
            }
            AgentComposerStep::Prompt | AgentComposerStep::Review => {}
        }
    }

    /// Cycle the focused value field (Details) or toggle the tool under the cursor
    /// (Tools). `delta` selects the cycle direction; ignored for toggles.
    pub(crate) fn toggle(&mut self, delta: i16) {
        self.clear_feedback();
        match self.step {
            AgentComposerStep::Details => match self.field {
                AgentComposerField::Location if !self.is_builtin_subagent() => {
                    self.location = self.location.toggled();
                }
                AgentComposerField::DefinitionKind => {
                    if !self.is_builtin_subagent() {
                        let kind = self.definition_kind().moved(delta);
                        self.set_definition_kind(kind);
                    }
                }
                AgentComposerField::Color => {
                    self.color = cycle_color(self.color.as_deref(), delta);
                }
                AgentComposerField::View => {
                    self.view_index = cycle(self.view_index, VIEW_CHOICES.len(), delta);
                }
                AgentComposerField::Name
                | AgentComposerField::Description
                | AgentComposerField::Location
                | AgentComposerField::Model
                | AgentComposerField::BackupModel => {}
            },
            AgentComposerStep::Tools => {
                if let Some(tool) = self.tools.get_mut(self.tools_cursor) {
                    tool.selected = !tool.selected;
                }
            }
            AgentComposerStep::Prompt | AgentComposerStep::Review => {}
        }
    }

    pub(crate) fn back(&mut self) {
        self.clear_feedback();
        if self.is_builtin_subagent() {
            self.step = AgentComposerStep::Details;
            return;
        }
        self.step = match self.step {
            AgentComposerStep::Details => AgentComposerStep::Details,
            AgentComposerStep::Tools => AgentComposerStep::Details,
            AgentComposerStep::Prompt => AgentComposerStep::Tools,
            AgentComposerStep::Review => AgentComposerStep::Prompt,
        };
    }

    /// Advance to the next step, validating first. `Review` is handled by the
    /// runtime layer (it writes the file), so this is a no-op there.
    pub(crate) fn submit(&mut self) {
        self.clear_feedback();
        if self.is_builtin_subagent() {
            if matches!(self.step, AgentComposerStep::Details) {
                self.step = AgentComposerStep::Review;
            }
            return;
        }
        match self.step {
            AgentComposerStep::Details => match self.validate_details() {
                Ok(()) => self.step = AgentComposerStep::Tools,
                Err(message) => self.error = Some(message),
            },
            AgentComposerStep::Tools => self.step = AgentComposerStep::Prompt,
            AgentComposerStep::Prompt => {
                if self.prompt.text.trim().is_empty() {
                    self.error = Some(
                        "The prompt can't be empty — type, paste, or generate one.".to_string(),
                    );
                } else {
                    self.step = AgentComposerStep::Review;
                }
            }
            AgentComposerStep::Review => {}
        }
    }

    /// Move to the next page without validating incomplete fields. Final
    /// validation still runs when the definition is saved.
    pub(crate) fn next_page(&mut self) {
        self.clear_feedback();
        if self.is_builtin_subagent() {
            self.step = AgentComposerStep::Review;
            return;
        }
        self.step = match self.step {
            AgentComposerStep::Details => AgentComposerStep::Tools,
            AgentComposerStep::Tools => AgentComposerStep::Prompt,
            AgentComposerStep::Prompt => AgentComposerStep::Review,
            AgentComposerStep::Review => AgentComposerStep::Review,
        };
    }

    pub(crate) fn mark_generating(&mut self, request_id: u64) {
        self.generating = true;
        self.active_request_id = Some(request_id);
        self.status = Some("Generating prompt…".to_string());
        self.error = None;
    }

    pub(crate) fn apply_generated(&mut self, prompt: String) {
        self.generating = false;
        self.active_request_id = None;
        self.prompt.set_text(prompt.trim().to_string());
        self.status = Some("Prompt generated — edit or continue.".to_string());
        self.error = None;
    }

    pub(crate) fn apply_generate_error(&mut self, message: String) {
        self.generating = false;
        self.active_request_id = None;
        self.status = None;
        self.error = Some(message);
    }

    pub(crate) fn validate_details(&self) -> Result<(), String> {
        if self.is_builtin_subagent() {
            return Ok(());
        }
        if self.name.text.trim().is_empty() {
            return Err("Name is required.".to_string());
        }
        if slugify(&self.name.text).is_empty() {
            return Err("Name must contain a letter or digit.".to_string());
        }
        if crate::tool::is_builtin_agent(&self.slug()) {
            return Err(format!(
                "'{}' is a reserved built-in subagent id. Choose another name.",
                self.slug()
            ));
        }
        if self.description.text.trim().is_empty() {
            return Err("Description is required.".to_string());
        }
        Ok(())
    }

    /// Validate every invariant required for a durable custom definition.
    pub(crate) fn validate_for_save(&self) -> Result<(), String> {
        self.validate_details()?;
        if !self.is_builtin_subagent() && self.prompt.text.trim().is_empty() {
            return Err("The prompt can't be empty — type, paste, or generate one.".to_string());
        }
        Ok(())
    }

    /// The original, human-readable name kept verbatim in the frontmatter `name:`
    /// and shown as the persona label (e.g. `Grumpy Senior`).
    pub(crate) fn display_name(&self) -> &str {
        self.name.text.trim()
    }

    /// The `<slug>.md` filename stem (lower-cased, dashed). The frontmatter `name:`
    /// keeps the original text; only the file on disk is slugified.
    pub(crate) fn slug(&self) -> String {
        slugify(&self.name.text)
    }

    /// Whether this definition is a user-facing agent, delegated subagent, or
    /// both. An explicit surface loaded by the runtime wins over the default.
    pub(crate) fn definition_kind(&self) -> AgentDefinitionKind {
        if self.is_builtin_subagent() {
            return AgentDefinitionKind::Subagent;
        }
        self.preserved_surface
            .as_deref()
            .map(|surface| AgentDefinitionKind::from_surface(Some(surface)))
            .unwrap_or(self.definition_kind)
    }

    /// Select an invocation type and replace any raw surface loaded from disk
    /// with its canonical spelling.
    pub(crate) fn set_definition_kind(&mut self, kind: AgentDefinitionKind) {
        if self.is_builtin_subagent() {
            return;
        }
        self.definition_kind = kind;
        self.preserved_surface = Some(
            kind.surfaces()
                .iter()
                .map(|surface| (*surface).to_string())
                .collect(),
        );
        self.clear_feedback();
    }

    /// The selected model, or `None` for the parent default.
    pub(crate) fn selected_model(&self) -> Option<&str> {
        let model = self.model.text.trim();
        (!model.is_empty()).then_some(model)
    }

    /// Set the `model:` selector from manual text or a `/model` picker
    /// selection (`None` = parent default). Empty strings collapse to default.
    pub(crate) fn set_model(&mut self, model: Option<String>) {
        let model = model
            .map(|model| model.trim().to_string())
            .filter(|model| !model.is_empty());
        if model.is_none() {
            self.effort_index = 0;
        }
        self.model.set_text(model.unwrap_or_default());
        self.clear_feedback();
    }

    /// The selected backup model, or `None` when failover is disabled.
    pub(crate) fn selected_fallback_model(&self) -> Option<&str> {
        let model = self.fallback_model.text.trim();
        (!model.is_empty()).then_some(model)
    }

    /// Set the backup model selector (`None` disables persisted failover).
    pub(crate) fn set_fallback_model(&mut self, model: Option<String>) {
        let model = model
            .map(|model| model.trim().to_string())
            .filter(|model| !model.is_empty());
        if model.is_none() {
            self.fallback_effort_index = 0;
        }
        self.fallback_model.set_text(model.unwrap_or_default());
        self.clear_feedback();
    }

    /// Apply a canonical picker selection to whichever model slot is focused.
    pub(crate) fn set_focused_model_selection(
        &mut self,
        selector: String,
        effort: crate::provider::ReasoningSelection,
    ) {
        let effort_index = reasoning_effort_index(effort);
        match self.field {
            AgentComposerField::Model => {
                self.set_model(Some(selector));
                self.effort_index = effort_index;
            }
            AgentComposerField::BackupModel => {
                self.set_fallback_model(Some(selector));
                self.fallback_effort_index = effort_index;
            }
            _ => {}
        }
    }

    /// Clear whichever model slot is focused: the primary reverts to inheriting
    /// the parent's model, the backup disables persisted failover. No-op on any
    /// other field.
    pub(crate) fn clear_focused_model(&mut self) {
        match self.field {
            AgentComposerField::Model => self.set_model(None),
            AgentComposerField::BackupModel => self.set_fallback_model(None),
            _ => {}
        }
    }

    /// The persona `color:` spec, or `None` for the default accent.
    pub(crate) fn selected_color(&self) -> Option<&str> {
        self.color.as_deref()
    }

    /// The persona `view:` (`chat`/`todo`/`canvas`), or `None` for the default.
    pub(crate) fn selected_view(&self) -> Option<&str> {
        if self.view_index == 0 {
            None
        } else {
            VIEW_CHOICES.get(self.view_index).copied()
        }
    }

    /// The selected effort string, or `None` for the default.
    pub(crate) fn selected_effort(&self) -> Option<&str> {
        if self.effort_index == 0 {
            None
        } else {
            EFFORT_CHOICES.get(self.effort_index).copied()
        }
    }

    /// The selected backup effort string, or `None` for the model default.
    pub(crate) fn selected_fallback_effort(&self) -> Option<&str> {
        if self.fallback_effort_index == 0 {
            None
        } else {
            EFFORT_CHOICES.get(self.fallback_effort_index).copied()
        }
    }

    /// The selected tools, or `None` when none are ticked (the read-only default).
    pub(crate) fn selected_tools(&self) -> Option<Vec<&'static str>> {
        let selected: Vec<&'static str> = self
            .tools
            .iter()
            .filter(|tool| tool.selected)
            .map(|tool| tool.name)
            .collect();
        (!selected.is_empty()).then_some(selected)
    }

    /// The prompt (system framing) sent to the model to draft `prompt`.
    pub(crate) fn generation_messages(&self) -> (String, String) {
        let system = "You write concise, effective system prompts for custom coding agents and subagents. \
An agent can read, search, and inspect a codebase; depending on the tools it is granted it may \
also edit files or run shell commands (which prompt for approval under the current policy). Given a \
short description of its job, invocation type, and granted tools, output ONLY the prompt body (no \
frontmatter, no markdown fences, no preamble). Write in the second person ('You are…'), state the \
goal, how to work (use only the granted tools), and how to report findings (prefer file:line \
references). Keep it tight — a few short paragraphs at most."
            .to_string();
        let tools = match self.selected_tools() {
            Some(tools) => tools.join(", "),
            None => "read-only tools only (read, grep, glob, symbol_search, project_info, git)"
                .to_string(),
        };
        // A canvas persona also drives the plan canvas via the plan_* tools.
        let canvas_note = if self.selected_view() == Some("canvas") {
            "\nThis agent runs on the plan canvas and can build/edit the plan with the plan_* tools."
        } else {
            ""
        };
        let user = format!(
            "Name: {}\nType: {} ({})\nDescription: {}\nAvailable tools: {}{}\n\nWrite the system prompt body.",
            self.display_name(),
            self.definition_kind().label(),
            self.definition_kind().detail(),
            self.description.text.trim(),
            tools,
            canvas_note,
        );
        (system, user)
    }

    /// Render the on-disk `.md` (YAML frontmatter + body). No serializer exists for
    /// `AgentFrontmatter`, so format by hand (mirrors `example_agent_md`); covered
    /// by a parse-back round-trip test.
    pub(crate) fn render_agent_md(&self) -> String {
        let mut front = String::from("---\n");
        // Keep the original name verbatim (the file is slugified separately).
        front.push_str(&format!("name: {}\n", yaml_quote(self.display_name())));
        front.push_str(&format!(
            "description: {}\n",
            yaml_quote(self.description.text.trim())
        ));
        front.push_str(&format!("enabled: {}\n", self.enabled));
        if let Some(tools) = self.selected_tools() {
            front.push_str(&format!("tools: [{}]\n", tools.join(", ")));
        }
        if let Some(model) = self.selected_model() {
            front.push_str(&format!("model: {model}\n"));
            if let Some(effort) = self.selected_effort() {
                front.push_str(&format!("effort: {effort}\n"));
            }
        }
        if let Some(model) = self.selected_fallback_model() {
            front.push_str(&format!("fallback_model: {model}\n"));
            if let Some(effort) = self.selected_fallback_effort() {
                front.push_str(&format!("fallback_effort: {effort}\n"));
            }
        }
        if let Some(color) = self.selected_color() {
            front.push_str(&format!("color: {color}\n"));
        }
        if let Some(view) = self.selected_view() {
            front.push_str(&format!("view: {view}\n"));
        }
        // Surface is always explicit so the definition's entry points remain
        // visible and stable across Bonsai and cross-tool loaders.
        front.push_str(&format!(
            "surface: [{}]\n",
            self.definition_kind().surfaces().join(", ")
        ));
        // Preserve reserved fields the composer doesn't edit (set only on edit).
        if let Some(permission) = &self.preserved_permission {
            front.push_str(&format!("permission: {}\n", yaml_quote(permission)));
        }
        if let Some(max_turns) = self.preserved_max_turns {
            front.push_str(&format!("max_turns: {max_turns}\n"));
        }
        front.push_str("---\n");
        let body = self.prompt.text.trim_end();
        format!("{front}{body}\n")
    }

    fn clear_feedback(&mut self) {
        self.status = None;
        self.error = None;
    }
}

fn reasoning_effort_index(reasoning: crate::provider::ReasoningSelection) -> usize {
    use crate::provider::ReasoningSelection;

    let effort = match reasoning {
        ReasoningSelection::Minimal => "minimal",
        ReasoningSelection::Low => "low",
        ReasoningSelection::Medium => "medium",
        ReasoningSelection::High => "high",
        ReasoningSelection::XHigh => "xhigh",
        ReasoningSelection::Max | ReasoningSelection::Ultra => "max",
        ReasoningSelection::Default
        | ReasoningSelection::Off
        | ReasoningSelection::On
        | ReasoningSelection::BudgetTokens(_) => "default",
    };
    EFFORT_CHOICES
        .iter()
        .position(|choice| *choice == effort)
        .unwrap_or(0)
}

/// A [`Composer`] seeded with `text`, cursor at the end.
fn composer_with(text: String) -> Composer {
    let mut composer = Composer::default();
    composer.set_text(text);
    composer
}

/// Cycle the persona color through `default → COLOR_CHOICES → default`. A current
/// value outside the palette (e.g. a hand-authored `#rrggbb`) is treated as the
/// `default` slot, so cycling forward lands on the first palette color.
fn cycle_color(current: Option<&str>, delta: i16) -> Option<String> {
    // Slot 0 is the default (no `color:`); slots 1..=N are `COLOR_CHOICES`.
    let current_slot = match current {
        Some(name) => COLOR_CHOICES
            .iter()
            .position(|choice| *choice == name)
            .map_or(0, |index| index + 1),
        None => 0,
    };
    let slot = cycle(current_slot, COLOR_CHOICES.len() + 1, delta);
    (slot > 0).then(|| COLOR_CHOICES[slot - 1].to_string())
}

fn cycle(index: usize, len: usize, delta: i16) -> usize {
    if len == 0 {
        return 0;
    }
    let len = len as i64;
    let mut next = index as i64 + delta as i64;
    next = ((next % len) + len) % len;
    next as usize
}

/// Quote a YAML scalar defensively so free-text descriptions (colons, quotes,
/// leading specials) always parse back to the same string.
fn yaml_quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Where the composer writes: the resolved directory + `<slug>.md` file path.
pub(crate) fn agent_file_path(
    state: &AgentComposerState,
    home_dir: &std::path::Path,
    project_root: &std::path::Path,
) -> PathBuf {
    let dir = match state.location {
        AgentLocation::Global => home_dir.join("agents"),
        AgentLocation::Project => project_root.join(".bonsai/agents"),
    };
    dir.join(format!("{}.md", state.slug()))
}

/// One row in the agents browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentBrowserRow {
    pub name: String,
    pub description: String,
    pub origin: AgentOrigin,
    pub(crate) definition_kind: AgentDefinitionKind,
    pub(crate) builtin_id: Option<crate::subagent::BuiltinSubagentId>,
    pub enabled: bool,
    /// The agent's pinned model (definition `model:` or built-in settings
    /// `primary_model`), shown in the row tag; `None` follows the session.
    pub model: Option<String>,
    /// Custom source file, or a legacy pre-DB built-in settings file.
    pub source_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentOrigin {
    Builtin,
    Global,
    Project,
}

impl AgentOrigin {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Builtin => "built-in",
            Self::Global => "global",
            Self::Project => "project",
        }
    }
}

/// Build one immutable-identity row per built-in, then explicit custom agent
/// and subagent definitions. A legacy same-name file supplies only fallback
/// model/enabled settings and remains attached for migration on the next save.
pub(crate) fn browser_rows_with_settings(
    custom: &AgentRegistry,
    builtin_settings: &crate::subagent::BuiltinSubagentSettingsRegistry,
) -> Vec<AgentBrowserRow> {
    let mut rows: Vec<AgentBrowserRow> = crate::tool::builtin_agents()
        .iter()
        .map(|spec| {
            let legacy = custom.get(spec.name);
            let enabled = builtin_settings
                .get(&spec.id)
                .map(|settings| settings.enabled)
                .or_else(|| legacy.map(|def| def.enabled))
                .unwrap_or(true);
            let model = builtin_settings
                .get(&spec.id)
                .and_then(|settings| settings.primary_model.clone())
                .or_else(|| legacy.and_then(|def| def.model.clone()));
            AgentBrowserRow {
                name: spec.name.to_string(),
                description: spec.description.to_string(),
                origin: AgentOrigin::Builtin,
                definition_kind: AgentDefinitionKind::Subagent,
                builtin_id: Some(spec.id),
                enabled,
                model,
                source_path: legacy.map(|def| def.source_path.clone()),
            }
        })
        .collect();
    for def in custom
        .iter()
        .filter(|def| !crate::tool::is_builtin_agent(&def.name))
    {
        rows.push(AgentBrowserRow {
            name: def.name.clone(),
            description: def.description.clone(),
            origin: if def.is_global() {
                AgentOrigin::Global
            } else {
                AgentOrigin::Project
            },
            definition_kind: AgentDefinitionKind::from_surface(def.surface.as_deref()),
            builtin_id: None,
            enabled: def.enabled,
            model: def.model.clone(),
            source_path: Some(def.source_path.clone()),
        });
    }
    rows
}

/// Flip (or insert) the `enabled:` field in an agent `.md`'s frontmatter, leaving
/// the body and other fields untouched. Used by the browser's enable/disable
/// toggle so a user can turn an agent off without deleting it.
pub(crate) fn set_enabled_in_markdown(content: &str, enabled: bool) -> String {
    let value = if enabled { "true" } else { "false" };
    let had_trailing_newline = content.ends_with('\n');
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();

    // Locate the frontmatter block (first two `---` fences).
    let mut fences = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.trim() == "---")
        .map(|(idx, _)| idx);
    if let (Some(start), Some(end)) = (fences.next(), fences.next()) {
        let existing = (start + 1..end).find(|&idx| {
            lines[idx]
                .split_once(':')
                .is_some_and(|(key, _)| key.trim().eq_ignore_ascii_case("enabled"))
        });
        match existing {
            Some(idx) => lines[idx] = format!("enabled: {value}"),
            None => lines.insert(end, format!("enabled: {value}")),
        }
    }

    let mut out = lines.join("\n");
    if had_trailing_newline {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AgentComposerState {
        let mut state = AgentComposerState::new();
        state.name.set_text("API Explorer".to_string());
        state
            .description
            .set_text("Maps HTTP routes: handlers".to_string());
        state.prompt.set_text("You map routes.".to_string());
        state
    }

    #[test]
    fn set_enabled_flips_existing_field() {
        let md = "---\nname: bot\ndescription: d\nenabled: true\n---\nbody\n";
        let off = set_enabled_in_markdown(md, false);
        assert!(off.contains("enabled: false"));
        assert!(!off.contains("enabled: true"));
        assert!(off.ends_with("body\n"), "body preserved: {off:?}");
        let on = set_enabled_in_markdown(&off, true);
        assert!(on.contains("enabled: true"));
    }

    #[test]
    fn set_enabled_inserts_when_absent() {
        let md = "---\nname: bot\ndescription: d\n---\nbody";
        let off = set_enabled_in_markdown(md, false);
        assert!(off.contains("enabled: false"));
        // Inserted inside the frontmatter, before the closing fence.
        let fm_end = off.find("\n---").unwrap();
        assert!(off[..fm_end].contains("enabled: false"));
        assert!(!off.ends_with('\n'), "no trailing newline added: {off:?}");
    }

    #[test]
    fn cycle_wraps_both_directions() {
        assert_eq!(cycle(0, 3, 1), 1);
        assert_eq!(cycle(2, 3, 1), 0);
        assert_eq!(cycle(0, 3, -1), 2);
    }

    #[test]
    fn definition_kind_defaults_to_subagent_and_parses_explicit_surfaces() {
        let state = AgentComposerState::new();
        assert_eq!(state.definition_kind(), AgentDefinitionKind::Subagent);
        assert_eq!(
            AgentDefinitionKind::from_surface(None),
            AgentDefinitionKind::Subagent
        );
        assert_eq!(
            AgentDefinitionKind::from_surface(Some(&["mode".to_string()])),
            AgentDefinitionKind::Agent
        );
        assert_eq!(
            AgentDefinitionKind::from_surface(Some(&["subagent".to_string(), "MODE".to_string(),])),
            AgentDefinitionKind::Both
        );
    }

    #[test]
    fn definition_kind_participates_in_field_navigation_and_cycles() {
        let mut state = AgentComposerState::new();
        state.field = AgentComposerField::Description;

        state.move_selection(1);
        assert_eq!(state.field, AgentComposerField::DefinitionKind);
        state.toggle(1);
        assert_eq!(state.definition_kind(), AgentDefinitionKind::Both);
        state.toggle(1);
        assert_eq!(state.definition_kind(), AgentDefinitionKind::Agent);
        state.toggle(-1);
        assert_eq!(state.definition_kind(), AgentDefinitionKind::Both);
    }

    #[test]
    fn details_validation_requires_name_and_description() {
        let mut state = AgentComposerState::new();
        assert!(state.validate_details().is_err());
        state.name.set_text("  ".to_string());
        assert!(state.validate_details().is_err());
        state.name.set_text("explorer".to_string());
        assert!(state.validate_details().is_err());
        state.description.set_text("does things".to_string());
        assert!(state.validate_details().is_ok());
    }

    #[test]
    fn reserved_builtin_id_and_empty_prompt_cannot_be_saved_as_custom() {
        let mut state = AgentComposerState::new();
        state.name.set_text("Explore".to_string());
        state.description.set_text("custom shadow".to_string());
        state.prompt.set_text("prompt".to_string());
        let error = state.validate_for_save().unwrap_err();
        assert!(error.contains("reserved built-in subagent id"), "{error}");

        state.name.set_text("helper".to_string());
        state.prompt.set_text(String::new());
        let error = state.validate_for_save().unwrap_err();
        assert!(error.contains("prompt can't be empty"), "{error}");
    }

    #[test]
    fn tab_navigation_advances_without_details_validation() {
        let mut state = AgentComposerState::new();
        state.name.set_text(String::new());
        state.description.set_text(String::new());

        state.next_page();

        assert_eq!(state.step, AgentComposerStep::Tools);
        assert!(state.error.is_none());
    }

    #[test]
    fn submit_advances_and_blocks_empty_prompt() {
        let mut state = sample();
        state.submit();
        assert_eq!(state.step, AgentComposerStep::Tools);
        state.submit();
        assert_eq!(state.step, AgentComposerStep::Prompt);
        state.prompt.set_text(String::new());
        state.submit();
        assert_eq!(state.step, AgentComposerStep::Prompt, "empty prompt blocks");
        assert!(state.error.is_some());
        state.prompt.set_text("You do X.".to_string());
        state.submit();
        assert_eq!(state.step, AgentComposerStep::Review);
    }

    #[test]
    fn tools_toggle_and_selected_set() {
        let mut state = AgentComposerState::new();
        state.step = AgentComposerStep::Tools;
        assert_eq!(state.selected_tools(), None, "none = read-only default");
        state.toggle(1); // toggle first tool (project_info)
        state.tools_cursor = 1;
        state.toggle(1); // read
        let selected = state.selected_tools().expect("some selected");
        assert!(selected.contains(&"project_info"));
        assert!(selected.contains(&"read"));
    }

    #[test]
    fn model_and_effort_selection() {
        let mut state = AgentComposerState::new();
        assert_eq!(state.selected_model(), None);
        state.set_model(Some("openai/gpt-5.5".to_string()));
        assert_eq!(state.selected_model(), Some("openai/gpt-5.5"));
        // The Model field is editable selector text: users can type either a
        // one-letter shortcut or a full model selector.
        state.step = AgentComposerStep::Details;
        state.field = AgentComposerField::Model;
        state.set_model(None);
        state.input_char('f');
        assert_eq!(state.selected_model(), Some("f"));
        state.backspace();
        assert_eq!(state.selected_model(), None);
        state.paste("codex:openai/gpt-5.5");
        assert_eq!(state.selected_model(), Some("codex:openai/gpt-5.5"));
        state.backspace();
        assert_eq!(state.selected_model(), Some("codex:openai/gpt-5."));
        // Effort is selected together with the model in the picker.
        state.set_focused_model_selection(
            "codex:openai/gpt-5.5".to_string(),
            crate::provider::ReasoningSelection::Minimal,
        );
        assert_eq!(state.selected_effort(), Some("minimal"));
    }

    #[test]
    fn primary_and_backup_picker_selections_are_canonical_and_independent() {
        let mut state = AgentComposerState::new();
        state.field = AgentComposerField::Model;
        state.set_focused_model_selection(
            "codex:openai/gpt-5.5".to_string(),
            crate::provider::ReasoningSelection::High,
        );
        state.field = AgentComposerField::BackupModel;
        state.set_focused_model_selection(
            "anthropic:anthropic/claude-sonnet-4-6".to_string(),
            crate::provider::ReasoningSelection::Medium,
        );

        assert_eq!(state.selected_model(), Some("codex:openai/gpt-5.5"));
        assert_eq!(state.selected_effort(), Some("high"));
        assert_eq!(
            state.selected_fallback_model(),
            Some("anthropic:anthropic/claude-sonnet-4-6")
        );
        assert_eq!(state.selected_fallback_effort(), Some("medium"));

        state.set_model(None);
        state.set_fallback_model(None);
        assert_eq!(state.selected_model(), None);
        assert_eq!(state.selected_fallback_model(), None);
    }

    #[test]
    fn prompt_paste_keeps_newlines() {
        let mut state = AgentComposerState::new();
        state.step = AgentComposerStep::Prompt;
        state.paste("line 1\r\nline 2");
        assert_eq!(state.prompt.text, "line 1\nline 2");
    }

    #[test]
    fn mid_text_editing_with_cursor() {
        let mut state = AgentComposerState::new();
        // Details/Name is a text field: type, then move the caret and insert.
        for ch in "helo".chars() {
            state.input_char(ch);
        }
        state.cursor(CursorMotion::Left); // caret between 'l' and 'o'
        state.input_char('l'); // -> "hello"
        assert_eq!(state.name.text, "hello");
        state.cursor(CursorMotion::Home);
        state.delete_forward(); // removes 'h'
        assert_eq!(state.name.text, "ello");
    }

    #[test]
    fn render_round_trips_through_frontmatter() {
        use crate::resource::frontmatter::{AgentFrontmatter, parse_frontmatter};

        let mut state = sample();
        state.set_model(Some("f".to_string()));
        state.set_fallback_model(Some("codex:openai/gpt-5.5".to_string()));
        state.fallback_effort_index = 4;
        state.color = Some("amber".to_string());
        state.step = AgentComposerStep::Tools;
        state.toggle(1); // project_info
        let md = state.render_agent_md();

        let parsed = parse_frontmatter::<AgentFrontmatter>(&md).expect("parses");
        // The original name is preserved verbatim; the file uses the slug.
        assert_eq!(parsed.frontmatter.name, "API Explorer");
        assert_eq!(state.slug(), "api-explorer");
        assert_eq!(parsed.frontmatter.description, "Maps HTTP routes: handlers");
        assert!(parsed.frontmatter.enabled);
        assert_eq!(
            parsed.frontmatter.tools,
            Some(vec!["project_info".to_string()])
        );
        assert_eq!(parsed.frontmatter.model.as_deref(), Some("f"));
        assert_eq!(
            parsed.frontmatter.fallback_model.as_deref(),
            Some("codex:openai/gpt-5.5")
        );
        assert_eq!(parsed.frontmatter.fallback_effort.as_deref(), Some("high"));
        assert_eq!(parsed.frontmatter.color.as_deref(), Some("amber"));
        assert_eq!(
            parsed.frontmatter.surface,
            Some(vec!["subagent".to_string()])
        );
        assert_eq!(parsed.body.trim(), "You map routes.");
    }

    #[test]
    fn disabled_custom_definition_stays_disabled_when_rendered() {
        use crate::resource::frontmatter::{AgentFrontmatter, parse_frontmatter};

        let mut state = sample();
        state.enabled = false;
        let parsed = parse_frontmatter::<AgentFrontmatter>(&state.render_agent_md())
            .expect("definition parses");

        assert!(!parsed.frontmatter.enabled);
    }

    #[test]
    fn definition_kinds_serialize_to_canonical_explicit_surfaces() {
        use crate::resource::frontmatter::{AgentFrontmatter, parse_frontmatter};

        for (kind, expected) in [
            (AgentDefinitionKind::Agent, vec!["mode".to_string()]),
            (AgentDefinitionKind::Subagent, vec!["subagent".to_string()]),
            (
                AgentDefinitionKind::Both,
                vec!["mode".to_string(), "subagent".to_string()],
            ),
        ] {
            let mut state = sample();
            state.set_definition_kind(kind);
            let parsed = parse_frontmatter::<AgentFrontmatter>(&state.render_agent_md())
                .expect("definition parses");
            assert_eq!(parsed.frontmatter.surface, Some(expected));
        }
    }

    #[test]
    fn reserved_frontmatter_is_preserved_on_edit() {
        use crate::resource::frontmatter::{AgentFrontmatter, parse_frontmatter};

        let mut state = sample();
        state.preserved_permission = Some("read-only".to_string());
        state.preserved_surface = Some(vec!["mode".to_string(), "subagent".to_string()]);
        state.preserved_max_turns = Some(72);
        let md = state.render_agent_md();

        let parsed = parse_frontmatter::<AgentFrontmatter>(&md).expect("parses");
        assert_eq!(parsed.frontmatter.permission.as_deref(), Some("read-only"));
        assert_eq!(
            parsed.frontmatter.surface,
            Some(vec!["mode".to_string(), "subagent".to_string()])
        );
        assert_eq!(state.definition_kind(), AgentDefinitionKind::Both);
        assert_eq!(parsed.frontmatter.max_turns, Some(72));
    }

    #[test]
    fn browser_keeps_legacy_builtin_shadow_typed_as_builtin_subagent() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        let agents_dir = root.path().join(".bonsai/agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("explore.md"),
            "---\nname: explore\ndescription: legacy shadow\nmodel: f\nsurface: [mode]\n---\nCUSTOM PROMPT",
        )
        .unwrap();
        std::fs::write(
            agents_dir.join("interactive.md"),
            "---\nname: interactive\ndescription: user-facing\nsurface: [mode]\n---\nprompt",
        )
        .unwrap();
        std::fs::write(
            agents_dir.join("helper.md"),
            "---\nname: helper\ndescription: delegated\n---\nprompt",
        )
        .unwrap();
        let custom = AgentRegistry::load_from(root.path(), home.path());
        let settings = crate::subagent::BuiltinSubagentSettingsRegistry::from([(
            crate::subagent::BuiltinSubagentId::Explore,
            crate::subagent::BuiltinSubagentSettings {
                enabled: false,
                ..crate::subagent::BuiltinSubagentSettings::default()
            },
        )]);

        let rows = browser_rows_with_settings(&custom, &settings);
        let explore = rows
            .iter()
            .filter(|row| row.name == "explore")
            .collect::<Vec<_>>();
        assert_eq!(explore.len(), 1, "reserved id must produce one row");
        assert_eq!(explore[0].origin, AgentOrigin::Builtin);
        assert_eq!(explore[0].definition_kind, AgentDefinitionKind::Subagent);
        assert_eq!(
            explore[0].builtin_id,
            Some(crate::subagent::BuiltinSubagentId::Explore)
        );
        assert!(!explore[0].enabled, "database setting wins");
        assert!(explore[0].source_path.is_some(), "legacy path retained");

        let interactive = rows
            .iter()
            .find(|row| row.name == "interactive")
            .expect("mode agent row");
        assert_eq!(interactive.definition_kind, AgentDefinitionKind::Agent);
        let helper = rows
            .iter()
            .find(|row| row.name == "helper")
            .expect("subagent row");
        assert_eq!(helper.definition_kind, AgentDefinitionKind::Subagent);
    }

    #[test]
    fn color_cycles_through_palette_and_default() {
        assert_eq!(cycle_color(None, 1).as_deref(), Some("blue"));
        assert_eq!(cycle_color(Some("blue"), 1).as_deref(), Some("sky"));
        assert_eq!(cycle_color(None, -1).as_deref(), Some("gray"));
        // The last palette color wraps back to the default (no `color:`).
        assert_eq!(cycle_color(Some("gray"), 1), None);
        // A hand-authored hex is treated as the default slot for cycling.
        assert_eq!(cycle_color(Some("#e0af68"), 1).as_deref(), Some("blue"));
    }
}
