//! Canonical built-in tool profile descriptors.

use super::{ParallelPolicy, ToolEffectPolicy};

const ABSENT: u8 = u8::MAX;

/// A registry surface assembled from the canonical descriptor table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolProfile {
    Coding,
    Planning,
    Smol,
    BuiltinSubagent,
    CustomSubagent,
}

impl ToolProfile {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 5] = [
        Self::Coding,
        Self::Planning,
        Self::Smol,
        Self::BuiltinSubagent,
        Self::CustomSubagent,
    ];

    const fn index(self) -> usize {
        match self {
            Self::Coding => 0,
            Self::Planning => 1,
            Self::Smol => 2,
            Self::BuiltinSubagent => 3,
            Self::CustomSubagent => 4,
        }
    }
}

/// Construction key used by bootstrap to bind a descriptor to a live tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ToolFactoryKey {
    ProjectInfo,
    Read,
    Write,
    ApplyPatch,
    Edit,
    Bash,
    Terminal,
    Glob,
    Grep,
    SymbolSearch,
    Definition,
    References,
    Hover,
    WorkspaceSymbol,
    RenameSymbol,
    Git,
    ReadRegion,
    ReadSymbol,
    Skill,
    Agent,
    Diagnostics,
    TodoWrite,
    SetSessionTitle,
    Tasks,
    Peers,
    Question,
    EnterPlanMode,
    MemoryWrite,
    WebFetch,
    WebSearch,
    PlanReplaceDraft,
    PlanRemoveSection,
    PlanMoveSection,
    PlanPatchSection,
    PlanRemovePhase,
    PlanMovePhase,
    PlanInsertTask,
    PlanUpdateTask,
    PlanRemoveTask,
    PlanCheckTask,
    PlanUncheckTask,
    PlanAddQuestion,
    PlanRemoveQuestion,
    PlanAddFinding,
    PlanAssociateFinding,
    PlanResolveFinding,
    Recall,
}

/// Registration seam relative to dynamically discovered extension tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegistrationStage {
    Standard,
    AfterExtensions,
}

/// Static capability and ordering contract for one built-in tool.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ToolDescriptor {
    /// Stable table identity; never reuse an ordinal for a different tool.
    pub(crate) ordinal: u16,
    pub(crate) factory: ToolFactoryKey,
    pub(crate) name: &'static str,
    /// Authorization/mutation boundary enforced by the tool implementation.
    pub(crate) effect: ToolEffectPolicy,
    /// Batching/access classification consumed by the agent coordinator.
    pub(crate) parallel: ParallelPolicy,
    orders: [u8; 5],
    optional: [bool; 5],
    pub(crate) stage: RegistrationStage,
}

impl ToolDescriptor {
    pub(crate) const fn order(self, profile: ToolProfile) -> Option<u8> {
        let order = self.orders[profile.index()];
        if order == ABSENT { None } else { Some(order) }
    }

    pub(crate) const fn optional(self, profile: ToolProfile) -> bool {
        self.optional[profile.index()]
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "each argument is an independently audited descriptor-table column"
)]
const fn descriptor(
    ordinal: u16,
    factory: ToolFactoryKey,
    name: &'static str,
    effect: ToolEffectPolicy,
    parallel: ParallelPolicy,
    orders: [u8; 5],
    optional: [bool; 5],
    stage: RegistrationStage,
) -> ToolDescriptor {
    ToolDescriptor {
        ordinal,
        factory,
        name,
        effect,
        parallel,
        orders,
        optional,
        stage,
    }
}

use ParallelPolicy::{AlwaysSafe, PathScoped, Serialized};
use RegistrationStage::{AfterExtensions, Standard};
use ToolEffectPolicy::{Delegated, LocalState, ReadOnly, SelfAuthorized};
use ToolFactoryKey::*;

/// Canonical built-in tool catalog. Profile-specific ordinals preserve the
/// existing provider-wire order even where historical profiles differ.
pub(crate) const TOOL_DESCRIPTORS: &[ToolDescriptor] = &[
    descriptor(
        0,
        ProjectInfo,
        "project_info",
        ReadOnly,
        AlwaysSafe,
        [0, 0, ABSENT, 0, 0],
        [false; 5],
        Standard,
    ),
    descriptor(
        1,
        Read,
        "read",
        ReadOnly,
        PathScoped,
        [1, 1, 0, 1, 1],
        [false; 5],
        Standard,
    ),
    descriptor(
        2,
        Write,
        "write",
        SelfAuthorized,
        PathScoped,
        [12, ABSENT, 1, ABSENT, 12],
        [false; 5],
        Standard,
    ),
    descriptor(
        3,
        ApplyPatch,
        "apply_patch",
        SelfAuthorized,
        Serialized,
        [13, ABSENT, ABSENT, ABSENT, 13],
        [false; 5],
        Standard,
    ),
    descriptor(
        4,
        Edit,
        "edit",
        SelfAuthorized,
        PathScoped,
        [14, ABSENT, 2, ABSENT, 14],
        [false; 5],
        Standard,
    ),
    descriptor(
        5,
        Bash,
        "bash",
        SelfAuthorized,
        Serialized,
        [15, ABSENT, 3, ABSENT, 15],
        [false; 5],
        Standard,
    ),
    descriptor(
        6,
        Terminal,
        "terminal",
        LocalState,
        Serialized,
        [16, ABSENT, 4, ABSENT, 16],
        [false; 5],
        Standard,
    ),
    descriptor(
        7,
        Glob,
        "glob",
        ReadOnly,
        PathScoped,
        [2, 2, ABSENT, 2, 2],
        [false; 5],
        Standard,
    ),
    descriptor(
        8,
        Grep,
        "grep",
        ReadOnly,
        PathScoped,
        [3, 3, ABSENT, 3, 3],
        [false; 5],
        Standard,
    ),
    descriptor(
        9,
        SymbolSearch,
        "symbol_search",
        ReadOnly,
        PathScoped,
        [4, 4, ABSENT, 4, 4],
        [false; 5],
        Standard,
    ),
    descriptor(
        10,
        Definition,
        "definition",
        ReadOnly,
        PathScoped,
        [5, 5, ABSENT, 5, 5],
        [false; 5],
        Standard,
    ),
    descriptor(
        11,
        References,
        "references",
        ReadOnly,
        PathScoped,
        [6, 6, ABSENT, 6, 6],
        [false; 5],
        Standard,
    ),
    descriptor(
        12,
        Hover,
        "hover",
        ReadOnly,
        PathScoped,
        [7, 7, ABSENT, 7, 7],
        [false; 5],
        Standard,
    ),
    descriptor(
        13,
        WorkspaceSymbol,
        "workspace_symbol",
        ReadOnly,
        PathScoped,
        [8, 8, ABSENT, 8, 8],
        [false; 5],
        Standard,
    ),
    descriptor(
        14,
        RenameSymbol,
        "rename_symbol",
        SelfAuthorized,
        Serialized,
        [17, ABSENT, ABSENT, ABSENT, 17],
        [false; 5],
        Standard,
    ),
    descriptor(
        15,
        Git,
        "git",
        ReadOnly,
        AlwaysSafe,
        [9, 9, ABSENT, 9, 9],
        [false; 5],
        Standard,
    ),
    descriptor(
        16,
        ReadRegion,
        "read_region",
        ReadOnly,
        PathScoped,
        [10, 10, ABSENT, 10, 10],
        [false; 5],
        Standard,
    ),
    descriptor(
        17,
        ReadSymbol,
        "read_symbol",
        ReadOnly,
        PathScoped,
        [11, 11, ABSENT, 11, 11],
        [false; 5],
        Standard,
    ),
    descriptor(
        18,
        Skill,
        "skill",
        ReadOnly,
        AlwaysSafe,
        [18, 13, 7, ABSENT, 18],
        [false, false, true, false, false],
        Standard,
    ),
    descriptor(
        19,
        Agent,
        "agent",
        Delegated,
        Serialized,
        [19, 14, ABSENT, ABSENT, ABSENT],
        [true, true, false, false, false],
        Standard,
    ),
    descriptor(
        20,
        Diagnostics,
        "diagnostics",
        SelfAuthorized,
        Serialized,
        [20, ABSENT, ABSENT, ABSENT, ABSENT],
        [false; 5],
        Standard,
    ),
    descriptor(
        21,
        TodoWrite,
        "todowrite",
        LocalState,
        Serialized,
        [21, ABSENT, 5, ABSENT, ABSENT],
        [false; 5],
        Standard,
    ),
    descriptor(
        22,
        SetSessionTitle,
        "set_session_title",
        LocalState,
        Serialized,
        [22, ABSENT, 6, ABSENT, ABSENT],
        [false; 5],
        Standard,
    ),
    descriptor(
        23,
        Tasks,
        "tasks",
        LocalState,
        Serialized,
        [23, ABSENT, ABSENT, ABSENT, ABSENT],
        [false; 5],
        Standard,
    ),
    descriptor(
        24,
        Peers,
        "peers",
        LocalState,
        Serialized,
        [24, ABSENT, ABSENT, ABSENT, ABSENT],
        [true, false, false, false, false],
        Standard,
    ),
    descriptor(
        25,
        Question,
        "question",
        LocalState,
        Serialized,
        [25, 12, ABSENT, ABSENT, 19],
        [false; 5],
        Standard,
    ),
    // Coding-only: the lever a coding turn uses to move planning work onto the
    // plan canvas. Absent from the planning profile (already there) and from
    // subagents/smol.
    descriptor(
        46,
        EnterPlanMode,
        "enter_plan_mode",
        LocalState,
        Serialized,
        [30, ABSENT, ABSENT, ABSENT, ABSENT],
        [false; 5],
        Standard,
    ),
    descriptor(
        26,
        MemoryWrite,
        "memory_write",
        LocalState,
        Serialized,
        [26, 33, ABSENT, ABSENT, ABSENT],
        [true, true, false, false, false],
        Standard,
    ),
    descriptor(
        27,
        WebFetch,
        "webfetch",
        SelfAuthorized,
        AlwaysSafe,
        [27, 31, ABSENT, ABSENT, 20],
        [false; 5],
        Standard,
    ),
    descriptor(
        28,
        WebSearch,
        "websearch",
        SelfAuthorized,
        AlwaysSafe,
        [28, 32, ABSENT, ABSENT, ABSENT],
        [true, true, false, false, false],
        Standard,
    ),
    descriptor(
        29,
        PlanReplaceDraft,
        "plan_replace_draft",
        LocalState,
        Serialized,
        [ABSENT, 15, ABSENT, ABSENT, ABSENT],
        [false; 5],
        Standard,
    ),
    descriptor(
        30,
        PlanRemoveSection,
        "plan_remove_section",
        LocalState,
        Serialized,
        [ABSENT, 16, ABSENT, ABSENT, ABSENT],
        [false; 5],
        Standard,
    ),
    descriptor(
        31,
        PlanMoveSection,
        "plan_move_section",
        LocalState,
        Serialized,
        [ABSENT, 17, ABSENT, ABSENT, ABSENT],
        [false; 5],
        Standard,
    ),
    descriptor(
        32,
        PlanPatchSection,
        "plan_patch_section",
        LocalState,
        Serialized,
        [ABSENT, 18, ABSENT, ABSENT, ABSENT],
        [false; 5],
        Standard,
    ),
    descriptor(
        33,
        PlanRemovePhase,
        "plan_remove_phase",
        LocalState,
        Serialized,
        [ABSENT, 19, ABSENT, ABSENT, ABSENT],
        [false; 5],
        Standard,
    ),
    descriptor(
        34,
        PlanMovePhase,
        "plan_move_phase",
        LocalState,
        Serialized,
        [ABSENT, 20, ABSENT, ABSENT, ABSENT],
        [false; 5],
        Standard,
    ),
    descriptor(
        35,
        PlanInsertTask,
        "plan_insert_task",
        LocalState,
        Serialized,
        [ABSENT, 21, ABSENT, ABSENT, ABSENT],
        [false; 5],
        Standard,
    ),
    descriptor(
        36,
        PlanUpdateTask,
        "plan_update_task",
        LocalState,
        Serialized,
        [ABSENT, 22, ABSENT, ABSENT, ABSENT],
        [false; 5],
        Standard,
    ),
    descriptor(
        37,
        PlanRemoveTask,
        "plan_remove_task",
        LocalState,
        Serialized,
        [ABSENT, 23, ABSENT, ABSENT, ABSENT],
        [false; 5],
        Standard,
    ),
    descriptor(
        38,
        PlanCheckTask,
        "plan_check_task",
        LocalState,
        Serialized,
        [ABSENT, 24, ABSENT, ABSENT, ABSENT],
        [false; 5],
        Standard,
    ),
    descriptor(
        39,
        PlanUncheckTask,
        "plan_uncheck_task",
        LocalState,
        Serialized,
        [ABSENT, 25, ABSENT, ABSENT, ABSENT],
        [false; 5],
        Standard,
    ),
    descriptor(
        40,
        PlanAddQuestion,
        "plan_add_question",
        LocalState,
        Serialized,
        [ABSENT, 26, ABSENT, ABSENT, ABSENT],
        [false; 5],
        Standard,
    ),
    descriptor(
        41,
        PlanRemoveQuestion,
        "plan_remove_question",
        LocalState,
        Serialized,
        [ABSENT, 27, ABSENT, ABSENT, ABSENT],
        [false; 5],
        Standard,
    ),
    descriptor(
        42,
        PlanAddFinding,
        "plan_add_finding",
        LocalState,
        Serialized,
        [ABSENT, 28, ABSENT, ABSENT, ABSENT],
        [false; 5],
        Standard,
    ),
    descriptor(
        43,
        PlanAssociateFinding,
        "plan_associate_finding",
        LocalState,
        Serialized,
        [ABSENT, 29, ABSENT, ABSENT, ABSENT],
        [false; 5],
        Standard,
    ),
    descriptor(
        44,
        PlanResolveFinding,
        "plan_resolve_finding",
        LocalState,
        Serialized,
        [ABSENT, 30, ABSENT, ABSENT, ABSENT],
        [false; 5],
        Standard,
    ),
    descriptor(
        45,
        Recall,
        "recall",
        LocalState,
        Serialized,
        [29, 34, ABSENT, ABSENT, ABSENT],
        [true, true, false, false, false],
        AfterExtensions,
    ),
];

pub(crate) fn descriptors_for(
    profile: ToolProfile,
    stage: RegistrationStage,
) -> Vec<&'static ToolDescriptor> {
    let mut descriptors: Vec<_> = TOOL_DESCRIPTORS
        .iter()
        .filter(|descriptor| descriptor.stage == stage && descriptor.order(profile).is_some())
        .collect();
    descriptors.sort_by_key(|descriptor| (descriptor.order(profile), descriptor.ordinal));
    descriptors
}

#[cfg(test)]
pub(crate) fn descriptor_for(profile: ToolProfile, name: &str) -> Option<&'static ToolDescriptor> {
    TOOL_DESCRIPTORS
        .iter()
        .find(|descriptor| descriptor.name == name && descriptor.order(profile).is_some())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn descriptors_have_stable_unique_ordinals_and_profile_orders() {
        let mut ordinals = HashSet::new();
        let mut factories = HashSet::new();
        let mut names = HashSet::new();
        for descriptor in TOOL_DESCRIPTORS {
            assert!(ordinals.insert(descriptor.ordinal));
            assert!(factories.insert(descriptor.factory));
            assert!(names.insert(descriptor.name));
            assert!(
                ToolProfile::ALL
                    .into_iter()
                    .any(|profile| descriptor.order(profile).is_some())
            );
        }

        for profile in ToolProfile::ALL {
            let descriptors = descriptors_for(profile, Standard)
                .into_iter()
                .chain(descriptors_for(profile, AfterExtensions));
            let mut orders = HashSet::new();
            let mut profile_names = HashSet::new();
            for descriptor in descriptors {
                assert!(orders.insert(descriptor.order(profile).unwrap()));
                assert!(profile_names.insert(descriptor.name));
            }
        }
    }

    #[test]
    fn coding_and_sub_lanes_share_the_read_only_schema_prefix() {
        let names = |profile| {
            descriptors_for(profile, Standard)
                .into_iter()
                .map(|descriptor| descriptor.name)
                .collect::<Vec<_>>()
        };
        let coding = names(ToolProfile::Coding);
        let planning = names(ToolProfile::Planning);
        let builtin_subagent = names(ToolProfile::BuiltinSubagent);
        let custom_subagent = names(ToolProfile::CustomSubagent);

        assert_eq!(&coding[..12], &planning[..12]);
        assert_eq!(&coding[..12], builtin_subagent.as_slice());
        assert_eq!(&coding[..12], &custom_subagent[..12]);
    }
}
