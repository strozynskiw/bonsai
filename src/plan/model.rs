//! Plain plan entities and domain-level read-only invariants.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// A normalized section patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SectionPatch {
    Replace(String),
    Append(String),
    Prepend(String),
    ReplaceText { old: String, new: String },
}

/// Where to place an item when inserting or reordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertPosition {
    Start,
    End,
    Before,
    After,
}

/// A titled Markdown section in a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSection {
    pub heading: String,
    pub body: String,
}

/// A single checklist item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanTask {
    pub text: String,
    pub done: bool,
}

/// A named, ordered group that owns its checklist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanPhase {
    pub name: String,
    pub tasks: Vec<PlanTask>,
}

/// Severity of a review finding, ordered most-to-least urgent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Blocker,
    Major,
    Minor,
    Nit,
}

impl Severity {
    /// Returns the lowercase persisted and wire representation.
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Blocker => "blocker",
            Self::Major => "major",
            Self::Minor => "minor",
            Self::Nit => "nit",
        }
    }

    /// Parses the persisted and wire representation.
    pub fn from_db_str(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "blocker" => Some(Self::Blocker),
            "major" => Some(Self::Major),
            "minor" => Some(Self::Minor),
            "nit" => Some(Self::Nit),
            _ => None,
        }
    }

    /// Returns the uppercase canvas and handoff label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Blocker => "BLOCKER",
            Self::Major => "MAJOR",
            Self::Minor => "MINOR",
            Self::Nit => "NIT",
        }
    }
}

/// A structured review finding recorded on a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub issue: String,
    pub required_fix: String,
    pub acceptance_tests: Vec<String>,
    pub source_ids: Vec<String>,
    pub task: Option<String>,
    pub resolved: bool,
}

impl Finding {
    /// Returns `file:line`, `file`, `line N`, or `None` without a location.
    pub fn location_label(&self) -> Option<String> {
        match (&self.file, self.line) {
            (Some(file), Some(line)) => Some(format!("{file}:{line}")),
            (Some(file), None) => Some(file.clone()),
            (None, Some(line)) => Some(format!("line {line}")),
            (None, None) => None,
        }
    }
}

/// Persistent plan document state. Serialized as the immutable JSON snapshot
/// format of the saved-plan library (`saved_plans.doc_json`); `serde(default)`
/// keeps older snapshots loadable when fields are added.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlanDoc {
    pub title: String,
    pub sections: Vec<PlanSection>,
    pub questions: Vec<String>,
    pub tasks: Vec<PlanTask>,
    pub phases: Vec<PlanPhase>,
    pub findings: Vec<Finding>,
    /// Monotonic content revision used by render and persistence consumers.
    pub revision: u64,
}

impl PlanDoc {
    /// Returns whether the document contains no user-visible plan content.
    pub fn is_empty(&self) -> bool {
        self.title.is_empty()
            && self.sections.is_empty()
            && self.questions.is_empty()
            && self.tasks.is_empty()
            && self.phases.is_empty()
            && self.findings.is_empty()
    }

    /// Returns whether work is grouped into phases.
    pub fn is_phased(&self) -> bool {
        !self.phases.is_empty()
    }
}

/// Shared mutable plan state used by the agent, tools, and TUI.
pub type SharedPlanStore = Arc<Mutex<PlanDoc>>;
