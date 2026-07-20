use crate::memory::entry::{MemoryEntryType, MemoryTier, normalize_description};
use crate::tui::app::Composer;
use crate::util::slug::slugify;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemoryWizardStep {
    Details,
    Body,
    Review,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemoryWizardField {
    Tier,
    Type,
    Enabled,
    Description,
}

impl MemoryWizardField {
    const ALL: [Self; 4] = [Self::Tier, Self::Type, Self::Enabled, Self::Description];

    pub(crate) fn moved(self, delta: i16) -> Self {
        let index = Self::ALL
            .iter()
            .position(|field| *field == self)
            .unwrap_or(0);
        let next = if delta.is_negative() {
            index.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            index.saturating_add(delta as usize)
        }
        .min(Self::ALL.len() - 1);
        Self::ALL[next]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemoryAddWizardState {
    pub step: MemoryWizardStep,
    pub field: MemoryWizardField,
    pub tier: MemoryTier,
    pub entry_type: MemoryEntryType,
    pub enabled: bool,
    pub description: String,
    pub body: Composer,
    pub status: Option<String>,
    pub error: Option<String>,
    /// `Some((tier, name))` = edit mode: the original identity being edited.
    /// The name never changes on edit (description edits don't re-slug); a
    /// tier change moves the file between stores under the same name.
    pub editing: Option<(MemoryTier, String)>,
}

impl Default for MemoryAddWizardState {
    fn default() -> Self {
        Self {
            step: MemoryWizardStep::Details,
            field: MemoryWizardField::Tier,
            tier: MemoryTier::User,
            entry_type: MemoryEntryType::Preference,
            enabled: true,
            description: String::new(),
            body: Composer::default(),
            status: None,
            error: None,
            editing: None,
        }
    }
}

impl MemoryAddWizardState {
    /// An edit-mode wizard prefilled from an existing entry.
    pub(crate) fn for_edit(entry: &crate::memory::entry::MemoryEntry) -> Self {
        let mut body = Composer::default();
        body.set_text(entry.body.clone());
        Self {
            tier: entry.tier,
            entry_type: entry.entry_type,
            enabled: entry.enabled,
            description: entry.description.clone(),
            body,
            editing: Some((entry.tier, entry.name.clone())),
            ..Self::default()
        }
    }

    pub(crate) fn preview_slug(&self) -> String {
        slugify(&normalize_description(&self.description))
    }

    /// The name the save will use: the original identity in edit mode (stable
    /// even when the description changes), the description slug otherwise.
    pub(crate) fn effective_name(&self) -> String {
        match &self.editing {
            Some((_, name)) => name.clone(),
            None => self.preview_slug(),
        }
    }

    pub(crate) fn move_field(&mut self, delta: i16) {
        self.field = self.field.moved(delta);
    }

    pub(crate) fn cycle_tier(&mut self, delta: i16) {
        self.tier = match delta {
            i16::MIN => MemoryTier::User,
            i16::MAX => MemoryTier::Project,
            _ => match self.tier {
                MemoryTier::User => MemoryTier::Project,
                MemoryTier::Project => MemoryTier::User,
            },
        };
    }

    pub(crate) fn cycle_type(&mut self, delta: i16) {
        let types = [
            MemoryEntryType::User,
            MemoryEntryType::Preference,
            MemoryEntryType::Project,
            MemoryEntryType::Reference,
        ];
        let index = types
            .iter()
            .position(|kind| *kind == self.entry_type)
            .unwrap_or(0);
        let next = if delta == i16::MIN {
            0
        } else if delta == i16::MAX {
            types.len().saturating_sub(1)
        } else if delta.is_negative() {
            index.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            index.saturating_add(delta as usize)
        }
        .min(types.len().saturating_sub(1));
        self.entry_type = types[next];
    }

    pub(crate) fn toggle_enabled(&mut self) {
        self.enabled = !self.enabled;
    }

    pub(crate) fn input_char(&mut self, ch: char) {
        match self.step {
            MemoryWizardStep::Details => {
                if matches!(self.field, MemoryWizardField::Description) {
                    self.description.push(ch);
                }
            }
            MemoryWizardStep::Body => self.body.insert_char(ch),
            MemoryWizardStep::Review => {}
        }
    }

    pub(crate) fn backspace(&mut self) {
        match self.step {
            MemoryWizardStep::Details => {
                if matches!(self.field, MemoryWizardField::Description) {
                    self.description.pop();
                }
            }
            MemoryWizardStep::Body => self.body.backspace(),
            MemoryWizardStep::Review => {}
        }
    }

    pub(crate) fn submit(&mut self) {
        self.step = match self.step {
            MemoryWizardStep::Details => MemoryWizardStep::Body,
            MemoryWizardStep::Body => MemoryWizardStep::Review,
            MemoryWizardStep::Review => MemoryWizardStep::Review,
        };
    }

    pub(crate) fn back(&mut self) {
        self.step = match self.step {
            MemoryWizardStep::Details => MemoryWizardStep::Details,
            MemoryWizardStep::Body => MemoryWizardStep::Details,
            MemoryWizardStep::Review => MemoryWizardStep::Body,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> crate::memory::entry::MemoryEntry {
        crate::memory::entry::MemoryEntry {
            tier: MemoryTier::Project,
            name: "tests-run-with-quiet".to_string(),
            description: "tests run with quiet".to_string(),
            entry_type: MemoryEntryType::Project,
            enabled: false,
            created: "2026-07-01".to_string(),
            updated: "2026-07-09".to_string(),
            body: "Use --quiet.".to_string(),
            content_hash: String::new(),
            path: std::path::PathBuf::from("tests-run-with-quiet.md"),
        }
    }

    #[test]
    fn for_edit_prefills_fields_and_identity() {
        let state = MemoryAddWizardState::for_edit(&entry());
        assert_eq!(state.step, MemoryWizardStep::Details);
        assert_eq!(state.tier, MemoryTier::Project);
        assert_eq!(state.entry_type, MemoryEntryType::Project);
        assert!(!state.enabled);
        assert_eq!(state.description, "tests run with quiet");
        assert_eq!(state.body.text, "Use --quiet.");
        assert_eq!(
            state.editing,
            Some((MemoryTier::Project, "tests-run-with-quiet".to_string()))
        );
    }

    #[test]
    fn effective_name_keeps_original_identity_when_editing() {
        let mut state = MemoryAddWizardState::for_edit(&entry());
        state.description = "a completely different description".to_string();
        assert_eq!(state.effective_name(), "tests-run-with-quiet");

        let create = MemoryAddWizardState {
            description: "tests run with quiet".to_string(),
            ..MemoryAddWizardState::default()
        };
        assert_eq!(create.effective_name(), create.preview_slug());
    }
}
