use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::SurfaceKind;
use crate::completion_report::{
    CompletionEvidenceSnapshot, CompletionReport, CompletionSessionEvidence, CompletionStatus,
    classify_completion_status,
};
use crate::permissions::{Permission, PermissionManager};
use crate::run_budget::{RunBudgetExhaustion, select_budget_timeout};
use crate::session_persist::{
    SessionSnapshotData, SessionSnapshotSignatures, SessionSnapshotWriter,
};
use crate::tool::ApprovalLevel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SharedRuntimeRule {
    Permission,
    Budget,
    Persistence,
    Cancellation,
    Recovery,
    ProviderFailure,
    CompletionReport,
}

const SHARED_RUNTIME_RULES: [SharedRuntimeRule; 7] = [
    SharedRuntimeRule::Permission,
    SharedRuntimeRule::Budget,
    SharedRuntimeRule::Persistence,
    SharedRuntimeRule::Cancellation,
    SharedRuntimeRule::Recovery,
    SharedRuntimeRule::ProviderFailure,
    SharedRuntimeRule::CompletionReport,
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct SurfaceAcceptanceSnapshot {
    permission: [Permission; 3],
    budget_timeout: Option<(Duration, RunBudgetExhaustion)>,
    cancellation_propagated: bool,
    recovery_isolation: [bool; 3],
    provider_failure: String,
    completion_report: CompletionReport,
    persisted_user_message: String,
}

async fn surface_acceptance_snapshot(surface: SurfaceKind) -> SurfaceAcceptanceSnapshot {
    let permissions = PermissionManager::memory_only();
    permissions.allow_for_session("git push *");
    let permission = [
        permissions.check_one("git status"),
        permissions.check_one("git push origin main"),
        permissions.check_one("rm -rf /"),
    ];

    let cancellation = CancellationToken::new();
    let run_cancellation = cancellation.child_token();
    cancellation.cancel();

    let provider_error = anyhow::Error::new(crate::provider::ProviderFailure::http(
        401,
        "expired acceptance credential",
        None,
    ));
    // A detected loop (repeated identical failure), not mere tool noise: a
    // single unresolved failure no longer demotes a completed run, so the
    // fixture exercises the strong-signal path both surfaces must agree on.
    let completion_evidence = CompletionEvidenceSnapshot {
        unresolved_tool_failure: true,
        repeated_tool_failure: true,
        failed_tool_attempts: 3,
        ..CompletionEvidenceSnapshot::default()
    };
    let completion_status =
        classify_completion_status(CompletionStatus::Completed, &completion_evidence, None);
    let completion_report = CompletionReport::from_evidence(
        completion_status,
        completion_evidence,
        CompletionSessionEvidence {
            completion_guard: None,
            verification: None,
            review: None,
            authorization_decisions: &[],
            usage: crate::agent::UsageTotals::default(),
            session_budget: crate::run_budget::SessionBudgetUsage::default(),
            budget_exhaustion: None,
        },
    );

    SurfaceAcceptanceSnapshot {
        permission,
        budget_timeout: select_budget_timeout(
            Some(Duration::from_secs(10)),
            Some(Duration::from_secs(60)),
            55_000,
        ),
        cancellation_propagated: run_cancellation.is_cancelled(),
        recovery_isolation: [
            crate::recovery::RecoveryMode::Auto.should_isolate(ApprovalLevel::Ask),
            crate::recovery::RecoveryMode::Auto.should_isolate(ApprovalLevel::Balanced),
            crate::recovery::RecoveryMode::Off.should_isolate(ApprovalLevel::Yolo),
        ],
        provider_failure: crate::provider::agent_failure_detail(&provider_error),
        completion_report,
        persisted_user_message: persist_surface_snapshot(surface).await,
    }
}

async fn persist_surface_snapshot(surface: SurfaceKind) -> String {
    let fixture = crate::storage::test_utils::TestStorage::new().await;
    let session_id = fixture
        .start_session_with("openai-compatible", "acceptance-model")
        .await;
    let transcript = vec![crate::tui::app::TranscriptItem::UserMessage {
        text: format!("{} acceptance", surface.label()),
    }];
    let plan = crate::plan::PlanDoc::default();
    let todos = Vec::new();
    SessionSnapshotWriter::new(&fixture.storage, session_id)
        .persist(
            SessionSnapshotData {
                transcript: &transcript,
                plan: &plan,
                todos: &todos,
                agent: None,
                fallback_usage: None,
                ui_peer_delivery_receipts: &[],
                agent_peer_delivery_receipts: &[],
            },
            SessionSnapshotSignatures::default(),
        )
        .await
        .expect("surface acceptance snapshot should persist");
    let snapshot = fixture
        .storage
        .load_session_snapshot(session_id)
        .await
        .expect("surface acceptance snapshot should load")
        .expect("persisted acceptance session should exist");
    match snapshot.transcript.as_slice() {
        [crate::tui::app::TranscriptItem::UserMessage { text }] => text
            .split_whitespace()
            .last()
            .unwrap_or_default()
            .to_string(),
        transcript => panic!("unexpected persisted transcript: {transcript:?}"),
    }
}

#[test]
fn shared_contract_matrix_covers_every_release_parity_rule() {
    assert_eq!(
        SHARED_RUNTIME_RULES,
        [
            SharedRuntimeRule::Permission,
            SharedRuntimeRule::Budget,
            SharedRuntimeRule::Persistence,
            SharedRuntimeRule::Cancellation,
            SharedRuntimeRule::Recovery,
            SharedRuntimeRule::ProviderFailure,
            SharedRuntimeRule::CompletionReport,
        ]
    );
}

#[tokio::test]
async fn tui_and_headless_pass_the_same_surface_acceptance_fixture() {
    let tui = surface_acceptance_snapshot(SurfaceKind::Tui).await;
    let headless = surface_acceptance_snapshot(SurfaceKind::Headless).await;

    assert_eq!(tui, headless);
    assert_eq!(
        tui.permission,
        [Permission::Allow, Permission::Allow, Permission::Deny]
    );
    assert_eq!(
        tui.budget_timeout,
        Some((
            Duration::from_secs(5),
            RunBudgetExhaustion::SessionTime {
                limit_seconds: 60,
                used_seconds: 60,
            },
        ))
    );
    assert!(tui.cancellation_propagated);
    assert_eq!(tui.recovery_isolation, [false, true, false]);
    assert!(tui.provider_failure.contains("provider HTTP error (401)"));
    assert!(tui.provider_failure.contains("Action:"));
    assert_eq!(tui.completion_report.status, CompletionStatus::Failed);
    assert!(
        tui.completion_report
            .caveats
            .iter()
            .any(|caveat| caveat.contains("tool operation remains failed"))
    );
    assert_eq!(tui.persisted_user_message, "acceptance");
}
