use super::*;
use crate::permissions::Permission;
use crate::provider::{EstimateConfidence, ReasoningEffort, TokenCounterKind};
use crate::session::{CredentialSource, CredentialStore};
use crate::storage::test_utils::TestStorage;
use crate::tool::read_evidence::{
    InspectionEventRecord, InspectionOutcome, InspectionReason, ReadAdmissionMetadata,
    ReadEvidenceRecord, ReadProvenance,
};
use crate::tool::{ReadCoverage, ReadEvidence, ReadWindow, digest_content};
use async_openai::types::chat::{
    ChatCompletionRequestMessage, ChatCompletionRequestUserMessageArgs,
};

fn make_store() -> SessionStore {
    let mut store = SessionStore::default();
    store.ensure_provider("anthropic");
    store.set_current_kind_id("anthropic");
    store.session_mut("anthropic").api_key = "sk-ant-test".to_string();
    store.session_mut("anthropic").credential_source = crate::session::CredentialSource::Keyring;
    store.session_mut("anthropic").model = "claude-sonnet-4-5".to_string();
    store.session_mut("anthropic").context_window = Some(200_000);
    store.session_mut("anthropic").reasoning =
        ReasoningSelection::from_effort(ReasoningEffort::High);
    store
}

async fn insert_active_recovery(fixture: &TestStorage, id: &str) -> RecoveryId {
    let id = RecoveryId::parse(id).unwrap();
    fixture
        .storage
        .insert_recovery_point(NewRecoveryPoint {
            id: &id,
            project_path: fixture.project_path(),
            repository_path: fixture.project_path(),
            worktree_path: fixture.project_path(),
            baseline_ref: "refs/bonsai/recovery/test",
            source_index_tree: "test-tree",
        })
        .await
        .unwrap();
    id
}

const NOISY_CONTEXT_STATUS: &str = "[context] prompt estimate 3760 + reserve 16000 exceeds context window 19000 (heuristic, low confidence); continuing with heuristic fallback";

#[tokio::test]
async fn conversation_cache_keys_are_stable_and_unique_per_session() {
    let fixture = TestStorage::new().await;
    let first = fixture.start_session().await;
    let second = fixture.start_session().await;

    let first_key = fixture.storage.conversation_cache_key(first).await.unwrap();
    assert_eq!(
        fixture.storage.conversation_cache_key(first).await.unwrap(),
        first_key,
    );
    assert_eq!(
        uuid::Uuid::parse_str(&first_key).unwrap().get_version_num(),
        7,
    );
    assert_ne!(
        first_key,
        fixture
            .storage
            .conversation_cache_key(second)
            .await
            .unwrap(),
    );
}

#[tokio::test]
async fn verification_runs_roundtrip_with_check_and_workspace_evidence() {
    use crate::verification::{
        VerificationBinding, VerificationCheckRecord, VerificationCheckStatus, VerificationKind,
        VerificationRunRecord, VerificationRunStatus, VerificationWorkspaceIdentity,
    };

    let fixture = TestStorage::new().await;
    let session_id = fixture.start_session().await;
    let identity = VerificationWorkspaceIdentity {
        repository_root: Some("/repo/.git".to_string()),
        worktree_root: "/repo".to_string(),
        project_root: "/repo".to_string(),
        head_oid: Some("abc123".to_string()),
        index_digest: "index".to_string(),
        tracked_worktree_digest: "tracked".to_string(),
        untracked_inputs: std::collections::BTreeMap::new(),
        command_cwd: "/repo".to_string(),
        command_fingerprint: "command".to_string(),
        toolchain_environment_fingerprint: "toolchain".to_string(),
    };
    let binding = VerificationBinding::Bound {
        digest: identity.digest().unwrap(),
        identity: Box::new(identity),
    };
    let runs = vec![VerificationRunRecord {
        kind: VerificationKind::Test,
        status: VerificationRunStatus::Stale,
        checks: vec![VerificationCheckRecord {
            name: "Rust tests".to_string(),
            command: "cargo test --locked".to_string(),
            status: VerificationCheckStatus::Passed,
            tool_call_id: Some("call-test".to_string()),
            exit_code: Some(0),
            completed_at_ms: Some(1_700_000_000_100),
            attempt_count: 2,
            last_failure_signature: Some("failure-a".to_string()),
            binding: Some(binding.clone()),
            delivered_binding: Some(binding.clone()),
            attempt_timestamps_ms: vec![1_700_000_000_050, 1_700_000_000_100],
            failure_signatures: vec!["failure-a".to_string()],
            terminal_reason_kind: None,
        }],
        started_at_ms: 1_700_000_000_000,
        finished_at_ms: Some(1_700_000_000_200),
        observed_final_workspace: Some(false),
        workspace_changes_after_last_check: vec!["src/main.rs".to_string()],
        repair_attempts: 1,
        reasoning_escalations: vec![crate::verification::VerificationReasoningEscalation {
            from: ReasoningSelection::Medium,
            to: ReasoningSelection::High,
            repair_attempt: 1,
            failure_signature: "failure-a".to_string(),
            occurred_at_ms: 1_700_000_000_150,
        }],
        terminal_reason: Some("workspace changed after verification".to_string()),
        terminal_reason_kind: Some(crate::verification::VerificationTerminalReason::UserSkipped),
        delivered_workspace_binding: Some(binding),
    }];

    fixture
        .storage
        .replace_verification_runs_snapshot(session_id, &runs)
        .await
        .unwrap();

    let snapshot = fixture
        .storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.verification_runs, runs);
}

#[tokio::test]
async fn verification_terminal_reason_kinds_all_roundtrip() {
    use crate::verification::{
        VerificationCheck, VerificationKind, VerificationRunRecord, VerificationRunStatus,
        VerificationTerminalReason,
    };

    let fixture = TestStorage::new().await;
    let session_id = fixture.start_session().await;
    let reasons = [
        VerificationTerminalReason::Irrelevant,
        VerificationTerminalReason::PolicyDisabled,
        VerificationTerminalReason::UserSkipped,
        VerificationTerminalReason::Delegated,
        VerificationTerminalReason::Cancelled,
        VerificationTerminalReason::EnvironmentBlocked,
        VerificationTerminalReason::Interrupted,
        VerificationTerminalReason::RepeatedDeterministicFailure,
        VerificationTerminalReason::RepairBudgetExhausted,
        VerificationTerminalReason::UnstableFailure,
    ];
    let runs = reasons
        .into_iter()
        .enumerate()
        .map(|(index, reason)| {
            let mut run = VerificationRunRecord::running(
                VerificationKind::Test,
                &[VerificationCheck {
                    name: format!("check-{index}"),
                    command: format!("cargo test check_{index}"),
                }],
            );
            run.status = match reason {
                VerificationTerminalReason::EnvironmentBlocked
                | VerificationTerminalReason::RepeatedDeterministicFailure
                | VerificationTerminalReason::RepairBudgetExhausted
                | VerificationTerminalReason::UnstableFailure => VerificationRunStatus::Blocked,
                VerificationTerminalReason::Cancelled | VerificationTerminalReason::Interrupted => {
                    VerificationRunStatus::Interrupted
                }
                VerificationTerminalReason::Irrelevant
                | VerificationTerminalReason::PolicyDisabled
                | VerificationTerminalReason::UserSkipped
                | VerificationTerminalReason::Delegated => VerificationRunStatus::Incomplete,
            };
            run.finished_at_ms = Some(run.started_at_ms);
            run.terminal_reason_kind = Some(reason);
            run.checks[0].terminal_reason_kind = Some(reason);
            run
        })
        .collect::<Vec<_>>();

    fixture
        .storage
        .replace_verification_runs_snapshot(session_id, &runs)
        .await
        .unwrap();
    let snapshot = fixture
        .storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(snapshot.verification_runs, runs);
}

#[tokio::test]
async fn self_review_runs_roundtrip_and_roll_up_effectiveness() {
    use crate::self_review::{
        SelfReviewDisposition, SelfReviewFindingCounts, SelfReviewMode, SelfReviewRunRecord,
        SelfReviewScope,
    };

    let fixture = TestStorage::new().await;
    let session_id = fixture.start_session().await;
    let runs = vec![
        SelfReviewRunRecord {
            started_at_ms: 1_700_000_000_000,
            mode: SelfReviewMode::Auto,
            scope: SelfReviewScope::Scoped,
            diff_line_count: 42,
            reviewer_duration_ms: 1_250,
            reviewer_prompt_tokens: 1_000,
            reviewer_completion_tokens: 200,
            reviewer_cost_micros: Some(321),
            findings: SelfReviewFindingCounts {
                blocker: 0,
                major: 1,
                minor: 1,
                nit: 0,
            },
            disposition: Some(SelfReviewDisposition::Fixed),
        },
        SelfReviewRunRecord {
            started_at_ms: 1_700_000_001_000,
            mode: SelfReviewMode::On,
            scope: SelfReviewScope::Unscoped,
            diff_line_count: 8,
            reviewer_duration_ms: 750,
            reviewer_prompt_tokens: 500,
            reviewer_completion_tokens: 50,
            reviewer_cost_micros: Some(79),
            findings: SelfReviewFindingCounts::default(),
            disposition: Some(SelfReviewDisposition::NoneNeeded),
        },
    ];

    fixture
        .storage
        .replace_self_review_runs_snapshot(session_id, &runs)
        .await
        .unwrap();
    let snapshot = fixture
        .storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.self_review_runs, runs);

    let stats = fixture.storage.load_self_review_stats().await.unwrap();
    assert_eq!(stats.runs, 2);
    assert_eq!(stats.runs_with_findings, 1);
    assert_eq!(stats.fixed, 1);
    assert_eq!(stats.none_needed, 1);
    assert_eq!(stats.findings, 2);
    assert_eq!(stats.reviewer_duration_ms, 2_000);
    assert_eq!(stats.reviewer_cost_micros, 400);
}

#[tokio::test]
async fn cross_session_duplicate_quality_evidence_is_quarantined_from_aggregates() {
    let fixture = TestStorage::new().await;
    let first = fixture.start_session().await;
    let second = fixture.start_session().await;
    let third = fixture.start_session().await;
    let fourth = fixture.start_session().await;

    let duplicated_verification =
        sample_verification_evidence(1_700_000_000_000, "cargo test --locked");
    let duplicated_review = sample_self_review_evidence(1_700_000_000_100, 42);
    for session_id in [first, second] {
        fixture
            .storage
            .replace_verification_runs_snapshot(
                session_id,
                std::slice::from_ref(&duplicated_verification),
            )
            .await
            .unwrap();
        fixture
            .storage
            .replace_self_review_runs_snapshot(session_id, std::slice::from_ref(&duplicated_review))
            .await
            .unwrap();
    }

    // Same timestamp is only a cheap duplicate candidate. Different complete
    // records in separate sessions remain trusted.
    fixture
        .storage
        .replace_verification_runs_snapshot(
            third,
            &[sample_verification_evidence(
                1_700_000_001_000,
                "cargo test --workspace",
            )],
        )
        .await
        .unwrap();
    fixture
        .storage
        .replace_verification_runs_snapshot(
            fourth,
            &[sample_verification_evidence(
                1_700_000_001_000,
                "cargo test --doc",
            )],
        )
        .await
        .unwrap();
    fixture
        .storage
        .replace_self_review_runs_snapshot(
            third,
            &[sample_self_review_evidence(1_700_000_001_100, 8)],
        )
        .await
        .unwrap();
    fixture
        .storage
        .replace_self_review_runs_snapshot(
            fourth,
            &[sample_self_review_evidence(1_700_000_001_100, 9)],
        )
        .await
        .unwrap();

    let dashboard = fixture.storage.load_usage_dashboard().await.unwrap();
    assert_eq!(dashboard.quality_evidence.quarantined_verification_runs, 2);
    assert_eq!(dashboard.quality_evidence.quarantined_self_review_runs, 2);
    assert_eq!(
        dashboard.self_review.runs, 2,
        "both members of the ambiguous duplicate group must be excluded"
    );
    assert_eq!(dashboard.self_review.findings, 4);
}

fn sample_verification_evidence(
    started_at_ms: i64,
    command: &str,
) -> crate::verification::VerificationRunRecord {
    crate::verification::VerificationRunRecord {
        kind: crate::verification::VerificationKind::Test,
        status: crate::verification::VerificationRunStatus::Passed,
        checks: vec![crate::verification::VerificationCheckRecord {
            name: "Rust tests".to_string(),
            command: command.to_string(),
            status: crate::verification::VerificationCheckStatus::Passed,
            tool_call_id: Some("call-test".to_string()),
            exit_code: Some(0),
            completed_at_ms: Some(started_at_ms + 10),
            attempt_count: 1,
            last_failure_signature: None,
            binding: None,
            delivered_binding: None,
            attempt_timestamps_ms: vec![started_at_ms + 10],
            failure_signatures: Vec::new(),
            terminal_reason_kind: None,
        }],
        started_at_ms,
        finished_at_ms: Some(started_at_ms + 20),
        observed_final_workspace: Some(true),
        workspace_changes_after_last_check: Vec::new(),
        repair_attempts: 0,
        reasoning_escalations: Vec::new(),
        terminal_reason: None,
        terminal_reason_kind: None,
        delivered_workspace_binding: None,
    }
}

fn sample_self_review_evidence(
    started_at_ms: i64,
    diff_line_count: u32,
) -> crate::self_review::SelfReviewRunRecord {
    crate::self_review::SelfReviewRunRecord {
        started_at_ms,
        mode: crate::self_review::SelfReviewMode::Auto,
        scope: crate::self_review::SelfReviewScope::Scoped,
        diff_line_count,
        reviewer_duration_ms: 1_250,
        reviewer_prompt_tokens: 1_000,
        reviewer_completion_tokens: 200,
        reviewer_cost_micros: Some(321),
        findings: crate::self_review::SelfReviewFindingCounts {
            blocker: 0,
            major: 1,
            minor: 1,
            nit: 0,
        },
        disposition: Some(crate::self_review::SelfReviewDisposition::Fixed),
    }
}

#[tokio::test]
async fn read_evidence_snapshot_roundtrips_without_file_content_duplication() {
    let fixture = TestStorage::new().await;
    let session_id = fixture.start_session().await;
    let path = fixture.project_path().join("sample.rs");
    let file_content = b"fn sample() {}\n";
    tokio::fs::write(&path, file_content).await.unwrap();
    let canonical_path = tokio::fs::canonicalize(&path).await.unwrap();
    let metadata = tokio::fs::metadata(&canonical_path).await.unwrap();
    let modified = metadata
        .modified()
        .ok()
        .and_then(system_time_to_ms)
        .and_then(system_time_from_ms);
    let rendered = "1: fn sample() {}\n";
    let file_digest = digest_content(file_content);
    let evidence = ReadEvidence::new(
        "sample.rs",
        canonical_path,
        ReadWindow {
            requested_offset: 1,
            requested_limit: 100,
            start_line: 1,
            end_line: Some(1),
            total_lines: Some(1),
        },
        ReadCoverage::Full,
        rendered,
        modified,
        metadata.len(),
        Some(file_digest),
    );
    let record = ReadEvidenceRecord {
        source_id: "tool:call-read".to_string(),
        provenance: ReadProvenance::ParentVisible,
        target_message_id: "msg-2".to_string(),
        target_content_digest: digest_content(rendered.as_bytes()),
        target_tool_call_id: Some("call-read".to_string()),
        tool_name: Some("read".to_string()),
        tool_arguments: Some(r#"{"path":"sample.rs"}"#.to_string()),
        target_live: true,
        target_stubbed: false,
        evidence,
        admission_outcome: "executed".to_string(),
        admission_reason: "direct_read".to_string(),
        requested_chars: rendered.chars().count(),
        returned_chars: rendered.chars().count(),
        avoided_chars: 0,
    };
    let inspection = InspectionEventRecord {
        call_id: "call-read".to_string(),
        target_message_id: "msg-2".to_string(),
        target_content_digest: digest_content(rendered.as_bytes()),
        tool_name: "read".to_string(),
        tool_arguments: r#"{"path":"sample.rs"}"#.to_string(),
        target_live: true,
        target_stubbed: false,
        admission: ReadAdmissionMetadata {
            outcome: InspectionOutcome::Executed,
            reason: InspectionReason::NoFreshVisibleCoverage,
            reuse_target_tool_call_id: None,
            requested_chars: rendered.chars().count(),
            returned_chars: rendered.chars().count(),
            avoided_chars: 0,
        },
    };
    let rejected_inspection = InspectionEventRecord {
        call_id: "call-read-repeat".to_string(),
        target_message_id: "msg-3".to_string(),
        target_content_digest: digest_content(b"Error: repeated inspection"),
        tool_name: "read".to_string(),
        tool_arguments: r#"{"path":"sample.rs"}"#.to_string(),
        target_live: true,
        target_stubbed: false,
        admission: ReadAdmissionMetadata {
            outcome: InspectionOutcome::Rejected,
            reason: InspectionReason::RepeatedFreshReuse,
            reuse_target_tool_call_id: Some("call-read".to_string()),
            requested_chars: rendered.chars().count(),
            returned_chars: 26,
            avoided_chars: rendered.chars().count().saturating_sub(26),
        },
    };
    let inspections = vec![inspection.clone(), rejected_inspection.clone()];

    fixture
        .storage
        .replace_agent_context_snapshot(
            session_id,
            &ContextMessageSnapshot {
                messages: Vec::new(),
                ids: Vec::new(),
            },
            std::slice::from_ref(&record),
            &inspections,
            &[],
        )
        .await
        .unwrap();

    let snapshot = fixture
        .storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.read_evidence, vec![record]);
    assert_eq!(
        snapshot.inspection_events,
        vec![inspection, rejected_inspection]
    );
}

#[test]
fn session_status_db_strings_roundtrip() {
    for (status, db) in [
        (SessionStatus::Active, "active"),
        (SessionStatus::Completed, "completed"),
        (SessionStatus::Forgotten, "forgotten"),
        (SessionStatus::Interrupted, "interrupted"),
        (SessionStatus::Failed, "failed"),
    ] {
        assert_eq!(status.as_db_str(), db);
        assert_eq!(SessionStatus::from_db_str(db), status);
    }
    assert_eq!(
        SessionStatus::from_db_str("paused"),
        SessionStatus::Other("paused".to_string())
    );
}

#[test]
fn saved_plan_status_db_strings_roundtrip() {
    for (status, db) in [
        (SavedPlanStatus::Draft, "draft"),
        (SavedPlanStatus::Started, "started"),
    ] {
        assert_eq!(status.as_db_str(), db);
        assert_eq!(SavedPlanStatus::from_db_str(db), status);
    }
    assert_eq!(
        SavedPlanStatus::from_db_str("archived"),
        SavedPlanStatus::Other("archived".to_string())
    );
}

#[test]
fn permission_scope_db_strings_roundtrip() {
    for (scope, db) in [
        (PermissionScope::Project, "project"),
        (PermissionScope::Global, "global"),
    ] {
        assert_eq!(scope.as_db_str(), db);
        assert_eq!(PermissionScope::from_db_str(db), scope);
    }
    assert_eq!(
        PermissionScope::from_db_str("weird"),
        PermissionScope::Project
    );
}

#[tokio::test]
async fn permission_rules_persist_filter_dedup_and_delete() {
    let ts = TestStorage::new().await;
    let pid = ts.storage.ensure_project(ts.project_path()).await.unwrap();

    let project_rule = ts
        .storage
        .upsert_permission_rule(
            Some(pid),
            "git push *",
            Permission::Allow,
            PermissionScope::Project,
            RuleKind::Bash,
        )
        .await
        .unwrap();
    ts.storage
        .upsert_permission_rule(
            None,
            "npm install *",
            Permission::Allow,
            PermissionScope::Global,
            RuleKind::Bash,
        )
        .await
        .unwrap();

    // Both apply to the project; project rule sorts before global.
    let rules = ts
        .storage
        .permission_rules_for_project(pid, RuleKind::Bash)
        .await
        .unwrap();
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0].pattern, "git push *");
    assert_eq!(rules[0].scope, PermissionScope::Project);
    assert_eq!(rules[0].decision, Permission::Allow);
    assert_eq!(rules[1].scope, PermissionScope::Global);

    // Upserting the same (scope, project, pattern) updates in place, no dup.
    let again = ts
        .storage
        .upsert_permission_rule(
            Some(pid),
            "git push *",
            Permission::Deny,
            PermissionScope::Project,
            RuleKind::Bash,
        )
        .await
        .unwrap();
    assert_eq!(project_rule, again);
    let rules = ts
        .storage
        .permission_rules_for_project(pid, RuleKind::Bash)
        .await
        .unwrap();
    assert_eq!(rules.len(), 2);
    assert_eq!(
        rules
            .iter()
            .find(|r| r.pattern == "git push *")
            .unwrap()
            .decision,
        Permission::Deny
    );

    // Delete drops only that rule.
    assert!(
        ts.storage
            .delete_permission_rule(project_rule, pid)
            .await
            .unwrap()
    );
    let rules = ts
        .storage
        .permission_rules_for_project(pid, RuleKind::Bash)
        .await
        .unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].pattern, "npm install *");

    // A rule owned by another project can't be deleted from this one, even by id.
    let other_pid = ts
        .storage
        .ensure_project(std::path::Path::new("/tmp/bonsai-other-project"))
        .await
        .unwrap();
    let foreign_rule = ts
        .storage
        .upsert_permission_rule(
            Some(other_pid),
            "rm *",
            Permission::Allow,
            PermissionScope::Project,
            RuleKind::Bash,
        )
        .await
        .unwrap();
    assert!(
        !ts.storage
            .delete_permission_rule(foreign_rule, pid)
            .await
            .unwrap(),
        "another project's rule must not be deletable from this project"
    );

    // A domain rule with the same (scope, project, pattern) as a bash rule is a
    // distinct row — `kind` partitions the dedup index.
    let bash_star = ts
        .storage
        .upsert_permission_rule(
            Some(pid),
            "*",
            Permission::Allow,
            PermissionScope::Project,
            RuleKind::Bash,
        )
        .await
        .unwrap();
    let domain_star = ts
        .storage
        .upsert_permission_rule(
            Some(pid),
            "*",
            Permission::Allow,
            PermissionScope::Project,
            RuleKind::Domain,
        )
        .await
        .unwrap();
    assert_ne!(
        bash_star, domain_star,
        "same pattern in two kinds are separate rows"
    );
    let domains = ts
        .storage
        .permission_rules_for_project(pid, RuleKind::Domain)
        .await
        .unwrap();
    assert_eq!(domains.len(), 1, "domain query sees only domain rules");
    assert_eq!(domains[0].pattern, "*");
}

#[test]
fn rule_kind_db_strings_are_stable() {
    // Only the encoder exists (rows are never decoded back to a kind); these
    // strings are the on-disk contract in the canonical schema.
    assert_eq!(RuleKind::Bash.as_db_str(), "bash");
    assert_eq!(RuleKind::Domain.as_db_str(), "domain");
}

#[tokio::test]
async fn session_store_roundtrips_through_sqlite() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let mut store = make_store();
    store.set_active_model_target(
        "anthropic",
        Some("anthropic".parse().unwrap()),
        Some("anthropic/claude-sonnet-4-5".parse().unwrap()),
        "claude-sonnet-4-5",
    );

    storage
        .save_session_store_with_auth_policy(&store, SaveAuthPolicy::PreserveExisting)
        .await
        .unwrap();

    let loaded = storage.load_session_store_raw().await.unwrap().unwrap();
    assert_eq!(loaded.current_kind_id(), "anthropic");
    assert_eq!(
        loaded.active_connection_id().map(|id| id.as_str()),
        Some("anthropic")
    );
    assert_eq!(
        loaded.active_model_id().map(|id| id.as_str()),
        Some("anthropic/claude-sonnet-4-5")
    );
    assert!(loaded.session("anthropic").api_key.is_empty());
    assert_eq!(
        loaded.session("anthropic").credential_source,
        crate::session::CredentialSource::Keyring
    );
    let persisted_source: String =
        sqlx::query_scalar("SELECT source FROM provider_credentials WHERE provider_id = ?")
            .bind("anthropic")
            .fetch_one(&storage.pool)
            .await
            .unwrap();
    assert_eq!(persisted_source, "keyring");
    assert_eq!(loaded.session("anthropic").context_window, Some(200_000));
    assert_eq!(
        loaded.session("anthropic").reasoning,
        ReasoningSelection::from_effort(ReasoningEffort::High)
    );
}

#[tokio::test]
async fn authoritative_session_store_save_deletes_removed_provider_rows() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let mut store = make_store();
    storage
        .save_session_store_with_auth_policy(&store, SaveAuthPolicy::AllowClear)
        .await
        .unwrap();

    store.providers.remove("anthropic");
    storage
        .save_session_store_with_auth_policy(&store, SaveAuthPolicy::AllowClear)
        .await
        .unwrap();
    let settings: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM provider_settings WHERE provider_id = 'anthropic'",
    )
    .fetch_one(&storage.pool)
    .await
    .unwrap();
    let credentials: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM provider_credentials WHERE provider_id = 'anthropic'",
    )
    .fetch_one(&storage.pool)
    .await
    .unwrap();
    assert_eq!((settings, credentials), (0, 0));
    assert!(storage.load_session_store_raw().await.unwrap().is_none());

    // Reusing the same provider id creates fresh unauthorized state; the old
    // credential reference cannot be resurrected by a later catalog entry.
    store.ensure_provider("anthropic");
    store.set_current_kind_id("anthropic");
    storage
        .save_session_store_with_auth_policy(&store, SaveAuthPolicy::AllowClear)
        .await
        .unwrap();
    let restarted = storage.load_session_store_raw().await.unwrap().unwrap();
    assert!(restarted.session("anthropic").api_key.is_empty());
    assert_eq!(
        restarted.session("anthropic").credential_source,
        CredentialSource::None
    );
}

#[tokio::test]
async fn environment_and_codex_cache_tokens_persist_only_references() {
    const ENV_SECRET: &str = "env-credential-sentinel-2a6e";
    const CODEX_SECRET: &str = "codex-cache-sentinel-938b";
    let temp_dir = tempfile::TempDir::new().unwrap();
    let db_path = temp_dir.path().join("bonsai.db");
    let storage = Storage::open_at(&db_path).await.unwrap();
    let mut store = make_store();
    let anthropic = store.session_mut("anthropic");
    anthropic.api_key = ENV_SECRET.to_string();
    anthropic.credential_source = CredentialSource::Environment("ANTHROPIC_API_KEY".to_string());
    store.ensure_provider("codex");
    let codex = store.session_mut("codex");
    codex.api_key = CODEX_SECRET.to_string();
    codex.credential_source = CredentialSource::CodexCache;
    storage
        .save_session_store_with_auth_policy(&store, SaveAuthPolicy::AllowClear)
        .await
        .unwrap();

    let rows = sqlx::query_as::<_, (String, String, String)>(
        "SELECT provider_id, source, reference FROM provider_credentials \
         WHERE provider_id IN ('anthropic', 'codex') ORDER BY provider_id",
    )
    .fetch_all(&storage.pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            (
                "anthropic".to_string(),
                "environment".to_string(),
                "ANTHROPIC_API_KEY".to_string(),
            ),
            (
                "codex".to_string(),
                "codex_cache".to_string(),
                String::new(),
            ),
        ]
    );
    for path in [
        db_path.clone(),
        db_path.with_extension("db-wal"),
        db_path.with_extension("db-shm"),
    ] {
        if path.exists() {
            let bytes = std::fs::read(path).unwrap();
            for secret in [ENV_SECRET, CODEX_SECRET] {
                assert!(
                    !bytes
                        .windows(secret.len())
                        .any(|window| window == secret.as_bytes())
                );
            }
        }
    }
}

#[tokio::test]
async fn unauthorize_deletes_keyring_entry_and_session_only_does_not_resume() {
    let fixture = TestStorage::new().await;
    let credentials = CredentialStore::memory();
    credentials
        .set(&CredentialSource::Keyring, "anthropic", "keyring-sentinel")
        .await
        .unwrap();
    let mut stored = make_store();
    stored.session_mut("anthropic").credential_source = CredentialSource::Keyring;
    fixture
        .storage
        .save_session_store_with_auth_policy(&stored, SaveAuthPolicy::AllowClear)
        .await
        .unwrap();

    let mut loaded =
        SessionStore::load_with_storage_and_credential_store(&fixture.storage, credentials.clone())
            .await
            .unwrap();
    loaded.clear_provider_credential("anthropic").await.unwrap();
    assert!(
        credentials
            .get(&CredentialSource::Keyring, "anthropic")
            .await
            .unwrap()
            .is_none()
    );

    loaded
        .set_provider_credential(
            "anthropic",
            "session-sentinel".to_string(),
            CredentialSource::Session,
        )
        .await;
    loaded.save_allowing_auth_clear_async().await.unwrap();
    let restarted =
        SessionStore::load_with_storage_and_credential_store(&fixture.storage, credentials)
            .await
            .unwrap();
    assert!(restarted.session("anthropic").api_key.is_empty());
    assert_eq!(
        restarted.session("anthropic").credential_source,
        CredentialSource::None
    );
}

#[tokio::test]
async fn transcript_snapshot_populates_search_index() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let session_id = fixture.start_session().await;

    storage
        .replace_transcript_snapshot(
            session_id,
            &[
                TranscriptItem::UserMessage {
                    text: "find the regression".to_string(),
                },
                TranscriptItem::AssistantMessage {
                    text: "the regression is covered".to_string(),
                },
            ],
        )
        .await
        .unwrap();

    let hits = storage.search_messages("regression", 10).await.unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().any(|hit| hit.role == "user"));
    assert!(hits.iter().any(|hit| hit.role == "assistant"));
}

#[tokio::test]
async fn completion_report_roundtrips_as_one_typed_transcript_block() {
    let fixture = TestStorage::new().await;
    let session_id = fixture.start_session().await;
    let report = crate::completion_report::CompletionReport::from_evidence(
        crate::completion_report::CompletionStatus::Interrupted,
        crate::completion_report::CompletionEvidenceSnapshot::default(),
        crate::completion_report::CompletionSessionEvidence {
            verification: None,
            review: None,
            authorization_decisions: &[],
            usage: crate::agent::UsageTotals::default(),
            session_budget: crate::run_budget::SessionBudgetUsage::default(),
            budget_exhaustion: None,
        },
    );

    fixture
        .storage
        .replace_transcript_snapshot(
            session_id,
            &[TranscriptItem::CompletionReport(Box::new(report.clone()))],
        )
        .await
        .unwrap();

    assert_eq!(
        fixture
            .storage
            .count_transcript_blocks(session_id)
            .await
            .unwrap(),
        1
    );
    let snapshot = fixture
        .storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.transcript.len(), 1);
    assert!(matches!(
        &snapshot.transcript[0],
        TranscriptItem::CompletionReport(restored) if restored.as_ref() == &report
    ));

    // The /bug bundle accessor returns the same report without loading the
    // whole transcript, and the newest one wins when several runs finished.
    assert_eq!(
        fixture
            .storage
            .latest_completion_report(session_id)
            .await
            .unwrap()
            .as_ref(),
        Some(&report)
    );
    let newer = crate::completion_report::CompletionReport::from_evidence(
        crate::completion_report::CompletionStatus::Completed,
        crate::completion_report::CompletionEvidenceSnapshot::default(),
        crate::completion_report::CompletionSessionEvidence {
            verification: None,
            review: None,
            authorization_decisions: &[],
            usage: crate::agent::UsageTotals::default(),
            session_budget: crate::run_budget::SessionBudgetUsage::default(),
            budget_exhaustion: None,
        },
    );
    fixture
        .storage
        .replace_transcript_snapshot(
            session_id,
            &[
                TranscriptItem::CompletionReport(Box::new(report.clone())),
                TranscriptItem::CompletionReport(Box::new(newer.clone())),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        fixture
            .storage
            .latest_completion_report(session_id)
            .await
            .unwrap()
            .as_ref(),
        Some(&newer)
    );
    // A session with no finished run reads as None, not an error.
    let empty_session = fixture.start_session().await;
    assert_eq!(
        fixture
            .storage
            .latest_completion_report(empty_session)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn transcript_snapshot_skips_empty_model_text_and_context_fallback_blocks() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let session_id = fixture.start_session().await;

    storage
        .replace_transcript_snapshot(
            session_id,
            &[
                TranscriptItem::UserMessage {
                    text: "visible user".to_string(),
                },
                TranscriptItem::AssistantMessage {
                    text: String::new(),
                },
                TranscriptItem::ReasoningSummary {
                    text: " \n\t".to_string(),
                },
                TranscriptItem::CommandOutput {
                    kind: CommandOutputKind::Status,
                    text: NOISY_CONTEXT_STATUS.to_string(),
                },
                TranscriptItem::CommandOutput {
                    kind: CommandOutputKind::Status,
                    text: "normal status".to_string(),
                },
                TranscriptItem::AssistantMessage {
                    text: "visible assistant".to_string(),
                },
            ],
        )
        .await
        .unwrap();

    let block_count = storage.count_transcript_blocks(session_id).await.unwrap();
    assert_eq!(block_count, 3);

    let snapshot = storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.transcript.len(), 3);
    assert!(matches!(
        &snapshot.transcript[0],
        TranscriptItem::UserMessage { text } if text == "visible user"
    ));
    assert!(matches!(
        &snapshot.transcript[1],
        TranscriptItem::CommandOutput {
            kind: CommandOutputKind::Status,
            text,
        } if text == "normal status"
    ));
    assert!(matches!(
        &snapshot.transcript[2],
        TranscriptItem::AssistantMessage { text } if text == "visible assistant"
    ));
}

#[tokio::test]
async fn queued_user_blocks_are_dropped_on_restore() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let session_id = fixture.start_session().await;

    storage
        .replace_transcript_snapshot(
            session_id,
            &[
                TranscriptItem::UserMessage {
                    text: "sent message".to_string(),
                },
                TranscriptItem::QueuedUserMessage {
                    id: 7,
                    text: "still pending".to_string(),
                },
            ],
        )
        .await
        .unwrap();

    // The queued block is dropped on load (the loader has no `QueuedUserMessage`
    // arm), so pending input can't resurface as a misleading status line after
    // crash recovery — only the real sent message restores.
    let snapshot = storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.transcript.len(), 1);
    assert!(matches!(
        &snapshot.transcript[0],
        TranscriptItem::UserMessage { text } if text == "sent message"
    ));
}

#[tokio::test]
async fn duplicate_tool_call_id_does_not_abort_transcript_snapshot() {
    use crate::tui::transcript::{ToolActivity, ToolStatus};
    use std::time::Instant;

    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let session_id = fixture.start_session().await;

    let now = Instant::now();
    let activity = |result: &str| ToolActivity {
        id: "dup-call".to_string(),
        name: "read".to_string(),
        arguments: r#"{"path":"a.rs"}"#.to_string(),
        status: ToolStatus::Succeeded,
        result: Some(result.to_string()),
        diff: None,
        started_at: now,
        finished_at: Some(now),
    };

    // The same call_id can legitimately appear twice in one snapshot (standalone
    // plus inside an ExecutionGroup, or re-emitted). It must not violate the
    // UNIQUE(session_id, call_id) constraint and roll back the whole flush — the
    // later write wins instead (BUG-7).
    storage
        .replace_transcript_snapshot(
            session_id,
            &[
                TranscriptItem::ToolActivity(activity("first")),
                TranscriptItem::ToolActivity(activity("second")),
            ],
        )
        .await
        .expect("duplicate call_id should upsert, not abort the snapshot");

    let snapshot = storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    let tool_results: Vec<&str> = snapshot
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::ToolActivity(activity) => activity.result.as_deref(),
            _ => None,
        })
        .collect();
    assert!(
        !tool_results.is_empty(),
        "the tool call must survive the flush"
    );
    assert!(
        tool_results.iter().all(|result| *result == "second"),
        "last write wins on conflict, got {tool_results:?}"
    );
}

#[tokio::test]
async fn upsert_global_permission_rule_collapses_duplicates() {
    let ts = TestStorage::new().await;
    let pid = ts.storage.ensure_project(ts.project_path()).await.unwrap();

    let first = ts
        .storage
        .upsert_permission_rule(
            None,
            "curl *",
            Permission::Allow,
            PermissionScope::Global,
            RuleKind::Bash,
        )
        .await
        .unwrap();
    // Re-upserting the same global (NULL project_id) rule must collapse onto the
    // same row via the COALESCE(project_id, -1) unique index — the NULL-scope
    // case a plain UNIQUE index can't dedupe — rather than inserting a duplicate.
    let again = ts
        .storage
        .upsert_permission_rule(
            None,
            "curl *",
            Permission::Deny,
            PermissionScope::Global,
            RuleKind::Bash,
        )
        .await
        .unwrap();
    assert_eq!(first, again, "global upsert must reuse the same row");

    let rules = ts
        .storage
        .permission_rules_for_project(pid, RuleKind::Bash)
        .await
        .unwrap();
    let curl_rules: Vec<_> = rules
        .iter()
        .filter(|rule| rule.pattern == "curl *")
        .collect();
    assert_eq!(curl_rules.len(), 1, "no duplicate global rule");
    assert_eq!(curl_rules[0].decision, Permission::Deny);
}

#[tokio::test]
async fn session_snapshot_filters_legacy_empty_model_text_and_context_fallback_blocks() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let session_id = fixture.start_session().await;

    for (seq, kind, body) in [
        (0_i64, "assistant", ""),
        (1, "thinking", " \n\t"),
        (2, "status", NOISY_CONTEXT_STATUS),
        (3, "status", "normal status"),
        (4, "assistant", "visible assistant"),
    ] {
        storage
            .insert_legacy_transcript_block(session_id, seq, kind, body)
            .await
            .unwrap();
    }

    let snapshot = storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.transcript.len(), 2);
    assert!(matches!(
        &snapshot.transcript[0],
        TranscriptItem::CommandOutput {
            kind: CommandOutputKind::Status,
            text,
        } if text == "normal status"
    ));
    assert!(matches!(
        &snapshot.transcript[1],
        TranscriptItem::AssistantMessage { text } if text == "visible assistant"
    ));
}

#[tokio::test]
async fn search_treats_code_punctuation_as_literal_text() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let session_id = fixture.start_session().await;

    storage
        .replace_transcript_snapshot(
            session_id,
            &[TranscriptItem::UserMessage {
                text: "fix foo::bar in src/tui/run.rs".to_string(),
            }],
        )
        .await
        .unwrap();

    let hits = storage.search_messages("foo::bar", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
}

#[tokio::test]
async fn context_snapshot_roundtrips_raw_messages() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let session_id = fixture.start_session().await;
    let message = ChatCompletionRequestMessage::User(
        ChatCompletionRequestUserMessageArgs::default()
            .content("resume this exact context")
            .build()
            .unwrap(),
    );
    let context_snapshot = ContextMessageSnapshot {
        messages: vec![message.clone()],
        ids: vec!["msg-42".to_string()],
    };

    storage
        .replace_agent_context_snapshot(session_id, &context_snapshot, &[], &[], &[])
        .await
        .unwrap();

    let snapshot = storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.context_messages, vec![message]);
    assert_eq!(snapshot.context_message_ids, vec!["msg-42"]);
}

#[tokio::test]
async fn context_control_snapshot_roundtrips_controls_and_sources() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let session_id = storage
        .start_session(
            temp_dir.path(),
            "anthropic",
            "claude-sonnet-4-5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let source_message = ChatCompletionRequestMessage::User(
        ChatCompletionRequestUserMessageArgs::default()
            .content("source message")
            .build()
            .unwrap(),
    );
    let mut controls = HashMap::new();
    controls.insert(
        "msg-1".to_string(),
        ContextControlState {
            pinned: true,
            drop_next_turn: false,
            stubbed: false,
            stub_reason: None,
        },
    );
    let mut sources = HashMap::new();
    sources.insert("msg-2".to_string(), vec![source_message.clone()]);

    storage
        .replace_context_control_snapshot(session_id, &controls, &sources)
        .await
        .unwrap();

    let snapshot = storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.context_controls, controls);
    assert_eq!(snapshot.context_sources["msg-2"], vec![source_message]);
}

#[tokio::test]
async fn compaction_events_snapshot_roundtrips() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let session_id = storage
        .start_session(
            temp_dir.path(),
            "anthropic",
            "claude-sonnet-4-5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let events = vec![
        crate::agent::CompactionEvent {
            seq: 1,
            occurred_at_ms: 1_000,
            before_tokens: 162_200,
            after_tokens: 101_000,
            messages_omitted: 5,
            tool_outputs_stubbed: 12,
            summary_available: true,
            repack_id: Some("repack-1".to_string()),
            repack_reason: Some("manual-compaction".to_string()),
            prefix_hash_before: Some("abc123".to_string()),
            prefix_hash_after: Some("def456".to_string()),
            cacheable_prefix_tokens_before: Some(140_000),
            cacheable_prefix_tokens_after: Some(95_000),
        },
        crate::agent::CompactionEvent {
            seq: 2,
            occurred_at_ms: 2_000,
            before_tokens: 168_000,
            after_tokens: 101_500,
            messages_omitted: 0,
            tool_outputs_stubbed: 9,
            summary_available: false,
            ..Default::default()
        },
    ];

    storage
        .replace_compaction_events_snapshot(session_id, &events)
        .await
        .unwrap();

    let snapshot = storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.compaction_events, events);
}

fn usage_turn(
    seq: usize,
    status: crate::agent::UsageTurnStatus,
    rewrite_kind: crate::agent::ContextRewriteKind,
) -> crate::agent::UsageTurn {
    crate::agent::UsageTurn {
        seq,
        lane_kind: crate::agent::ExecutionLaneKind::Parent,
        lane_id: "parent-42".to_string(),
        lane_seq: seq,
        parent_tool_call_id: Some(format!("call-{seq}")),
        launch_group_id: Some("group-1".to_string()),
        status,
        finish_reason: Some(crate::provider::FinishReason::Stop),
        reasoning_chars: seq * 100,
        provider_attempts: vec![crate::agent::ProviderAttemptReport {
            attempt: 1,
            outcome: crate::agent::ProviderAttemptOutcome::Completed,
            latency_ms: 250,
            assistant_chars: 80,
            reasoning_chars: 20,
            finish_reason: Some(crate::provider::FinishReason::Stop),
            error_class: None,
            backoff_ms: None,
            prompt_tokens: Some(1_000),
            completion_tokens: Some(200),
            cache_read_input_tokens: Some(800),
            cache_creation_input_tokens: Some(0),
            cache_measured_input_tokens: Some(1_000),
        }],
        // Real values so the roundtrip proves per-turn attribution survives
        // the DELETE+INSERT snapshot rewrite.
        provider_id: Some("anthropic".to_string()),
        model: Some(format!("model-{seq}")),
        effective_reasoning: Some(crate::provider::ReasoningSelection::BudgetTokens(
            8_192 + seq as u32,
        )),
        prompt_tokens: (status != crate::agent::UsageTurnStatus::Missing).then_some(1_000),
        completion_tokens: (status != crate::agent::UsageTurnStatus::Missing).then_some(200),
        cache_read_input_tokens: Some(400),
        cache_creation_input_tokens: Some(50),
        cache_measured_input_tokens: Some(1_000),
        turn_cost_micros: Some(123),
        no_cache_cost_micros: Some(456),
        estimated_prompt_tokens: Some(1_100),
        estimate_source: Some(TokenCounterKind::Heuristic),
        estimate_confidence: Some(EstimateConfidence::Low),
        tool_schema_tokens: Some(75),
        tool_schema_hash: Some(format!("schema{seq:0>10}")),
        tool_schema_names: vec!["read".to_string(), "bash".to_string()],
        request_body_bytes: Some(4_096 + seq),
        request_body_hash: Some(format!("request{seq:0>9}")),
        cache_mechanism: Some("prompt_cache_key".to_string()),
        cache_route_fingerprint: Some("route1234567890".to_string()),
        expected_cacheable_percent: Some(81),
        actual_cache_read_percent: Some(40),
        local_reusable_prefix_tokens: Some(850),
        local_reusable_prefix_percent: Some(77),
        cacheable_prefix_tokens: Some(900),
        volatile_tail_tokens: Some(200),
        context_window_tokens: Some(128_000),
        rewrite_kind,
        rewrite_saved_tokens: Some(8_500),
        episode_seq: None,
        // Distinct per-turn timestamps so the roundtrip proves they are not
        // collapsed to a shared flush time.
        created_at_ms: 1_700_000_000_000 + seq as i64 * 1_000,
        latency_ms: Some(3_000 + seq as u64),
        ttft_ms: Some(500 + seq as u64),
        prefix_hash: Some(format!("prefix{seq:0>10}")),
        inspection_executed: seq,
        inspection_reused: seq + 1,
        inspection_rejected: seq + 2,
        inspection_returned_chars: 1_000 + seq,
        inspection_avoided_chars: 2_000 + seq,
        delegated_parent_overlap: seq % 2,
    }
}

#[tokio::test]
async fn usage_turns_snapshot_roundtrips() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let session_id = fixture.start_session().await;
    let turns = vec![
        usage_turn(
            1,
            crate::agent::UsageTurnStatus::Reported,
            crate::agent::ContextRewriteKind::None,
        ),
        usage_turn(
            2,
            crate::agent::UsageTurnStatus::Missing,
            crate::agent::ContextRewriteKind::Gc,
        ),
        usage_turn(
            3,
            crate::agent::UsageTurnStatus::Interrupted,
            crate::agent::ContextRewriteKind::Compaction,
        ),
    ];

    storage
        .sync_usage_turns_snapshot(session_id, &turns)
        .await
        .unwrap();

    let snapshot = storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.usage_turns, turns);

    // Re-flushing the same turns must not clobber their execution timestamps.
    storage
        .sync_usage_turns_snapshot(session_id, &turns)
        .await
        .unwrap();
    let reloaded = storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.usage_turns, turns);
    let report = crate::agent::UsageTurnReport::from(&turns[0]);
    assert_eq!(report.to_usage_turn(), turns[0]);
    assert_eq!(
        serde_json::to_value(&report).unwrap()["effective_reasoning"],
        serde_json::json!("budget:8193")
    );
    let created: Vec<i64> = reloaded
        .usage_turns
        .iter()
        .map(|turn| turn.created_at_ms)
        .collect();
    assert_eq!(
        created,
        vec![
            1_700_000_000_000 + 1_000,
            1_700_000_000_000 + 2_000,
            1_700_000_000_000 + 3_000,
        ]
    );

    sqlx::query(
        "UPDATE usage_turns SET effective_reasoning = NULL WHERE session_id = ? AND seq = 1",
    )
    .bind(session_id.as_i64())
    .execute(&storage.pool)
    .await
    .unwrap();
    let legacy = storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(legacy.usage_turns[0].effective_reasoning, None);
}

#[tokio::test]
async fn usage_turn_ledger_repairs_stale_session_no_cache_total() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let session_id = fixture.start_session().await;
    let turns = vec![
        usage_turn(
            1,
            crate::agent::UsageTurnStatus::Reported,
            crate::agent::ContextRewriteKind::None,
        ),
        usage_turn(
            2,
            crate::agent::UsageTurnStatus::Reported,
            crate::agent::ContextRewriteKind::None,
        ),
    ];

    storage
        .update_session_usage_totals(
            session_id,
            &crate::agent::UsageTotals {
                prompt_tokens: 2_000,
                completion_tokens: 400,
                cost_micros: Some(246),
                no_cache_cost_micros: Some(100),
                input_cache: None,
            },
        )
        .await
        .unwrap();
    storage
        .sync_usage_turns_snapshot(session_id, &turns)
        .await
        .unwrap();

    let summary = storage.session_summary(session_id).await.unwrap().unwrap();
    assert_eq!(summary.cost_micros, 246);
    assert_eq!(summary.no_cache_cost_micros, 912);

    // The reverse write order must also retain the ledger total.
    storage
        .update_session_usage_totals(
            session_id,
            &crate::agent::UsageTotals {
                prompt_tokens: 2_000,
                completion_tokens: 400,
                cost_micros: Some(246),
                no_cache_cost_micros: Some(100),
                input_cache: None,
            },
        )
        .await
        .unwrap();
    let summary = storage.session_summary(session_id).await.unwrap().unwrap();
    assert_eq!(summary.no_cache_cost_micros, 912);

    // IMPORTANT ACCOUNTING INVARIANT: an incomplete ledger must not win merely
    // because it has one priced row. Its partial sum can be below actual cost.
    sqlx::query(
        "UPDATE usage_turns SET no_cache_cost_micros = NULL WHERE session_id = ? AND seq = 2",
    )
    .bind(session_id.as_i64())
    .execute(&storage.pool)
    .await
    .unwrap();
    storage
        .update_session_usage_totals(
            session_id,
            &crate::agent::UsageTotals {
                prompt_tokens: 2_000,
                completion_tokens: 400,
                cost_micros: Some(246),
                no_cache_cost_micros: Some(700),
                input_cache: None,
            },
        )
        .await
        .unwrap();
    storage
        .sync_usage_turns_snapshot(session_id, &turns)
        .await
        .unwrap();
    let summary = storage.session_summary(session_id).await.unwrap().unwrap();
    assert_eq!(summary.no_cache_cost_micros, 700);
}

#[tokio::test]
async fn usage_turn_sync_appends_and_only_updates_mutable_attribution() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let session_id = fixture.start_session().await;
    let turns = vec![
        usage_turn(
            1,
            crate::agent::UsageTurnStatus::Reported,
            crate::agent::ContextRewriteKind::None,
        ),
        usage_turn(
            2,
            crate::agent::UsageTurnStatus::Reported,
            crate::agent::ContextRewriteKind::None,
        ),
    ];
    storage
        .sync_usage_turns_snapshot(session_id, &turns)
        .await
        .unwrap();

    sqlx::query("CREATE TABLE usage_turn_write_audit (operation TEXT NOT NULL)")
        .execute(&storage.pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER audit_usage_turn_insert AFTER INSERT ON usage_turns \
         BEGIN INSERT INTO usage_turn_write_audit VALUES ('insert'); END",
    )
    .execute(&storage.pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER audit_usage_turn_update AFTER UPDATE ON usage_turns \
         BEGIN INSERT INTO usage_turn_write_audit VALUES ('update'); END",
    )
    .execute(&storage.pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER audit_usage_turn_delete AFTER DELETE ON usage_turns \
         BEGIN INSERT INTO usage_turn_write_audit VALUES ('delete'); END",
    )
    .execute(&storage.pool)
    .await
    .unwrap();

    let mut appended = turns.clone();
    appended.push(usage_turn(
        3,
        crate::agent::UsageTurnStatus::Reported,
        crate::agent::ContextRewriteKind::None,
    ));
    storage
        .sync_usage_turns_snapshot(session_id, &appended)
        .await
        .unwrap();
    let operations: Vec<String> =
        sqlx::query_scalar("SELECT operation FROM usage_turn_write_audit ORDER BY rowid")
            .fetch_all(&storage.pool)
            .await
            .unwrap();
    assert_eq!(operations, ["insert"]);

    sqlx::query("DELETE FROM usage_turn_write_audit")
        .execute(&storage.pool)
        .await
        .unwrap();
    appended[0].parent_tool_call_id = Some("call-parent".to_string());
    appended[0].inspection_reused += 1;
    storage
        .sync_usage_turns_snapshot(session_id, &appended)
        .await
        .unwrap();
    let operations: Vec<String> =
        sqlx::query_scalar("SELECT operation FROM usage_turn_write_audit ORDER BY rowid")
            .fetch_all(&storage.pool)
            .await
            .unwrap();
    assert_eq!(operations, ["update"]);

    let snapshot = storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.usage_turns, appended);
}

#[tokio::test]
async fn fresh_database_uses_one_current_schema_baseline() {
    let fixture = TestStorage::new().await;
    let migration_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(&fixture.storage.pool)
        .await
        .unwrap();
    // 0001 is the frozen 1.0 baseline (sqlx checksums applied migrations —
    // editing it bricks existing databases); every schema change after it is
    // an additive migration. Bump alongside each new migrations/*.sql file.
    assert_eq!(migration_count, 3);

    let builtin_subagent_settings_table: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master \
         WHERE type = 'table' AND name = 'builtin_subagent_settings'",
    )
    .fetch_one(&fixture.storage.pool)
    .await
    .unwrap();
    assert_eq!(builtin_subagent_settings_table, 1);
    let invalid_setting = sqlx::query(
        "INSERT INTO builtin_subagent_settings \
         (subagent_id, enabled, primary_effort, updated_at_ms) VALUES ('explore', 1, 'high', 0)",
    )
    .execute(&fixture.storage.pool)
    .await;
    assert!(
        invalid_setting.is_err(),
        "effort without a model must be rejected"
    );

    let unique_wake_index: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_index_list('peer_wake_subscriptions') \
         WHERE name = 'idx_unique_pending_peer_wake' AND \"unique\" = 1 AND partial = 1",
    )
    .fetch_one(&fixture.storage.pool)
    .await
    .unwrap();
    assert_eq!(unique_wake_index, 1);

    let peer_operation_tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master
         WHERE type = 'table'
           AND name IN ('peer_send_operations', 'peer_wake_operations')
         ORDER BY name",
    )
    .fetch_all(&fixture.storage.pool)
    .await
    .unwrap();
    assert_eq!(
        peer_operation_tables,
        ["peer_send_operations", "peer_wake_operations"]
    );

    let atomic_peer_indexes: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master
         WHERE type = 'index'
           AND name IN (
             'idx_agent_messages_send_operation_recipient',
             'idx_unique_wake_request_message'
           )
         ORDER BY name",
    )
    .fetch_all(&fixture.storage.pool)
    .await
    .unwrap();
    assert_eq!(
        atomic_peer_indexes,
        [
            "idx_agent_messages_send_operation_recipient",
            "idx_unique_wake_request_message"
        ]
    );

    let escalation_column: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('verification_runs') \
         WHERE name = 'reasoning_escalations_json' AND \"notnull\" = 1",
    )
    .fetch_one(&fixture.storage.pool)
    .await
    .unwrap();
    assert_eq!(escalation_column, 1);

    let active_time_columns: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM pragma_table_info('sessions') \
         WHERE name IN ('active_run_ms', 'active_run_started_at_ms') ORDER BY cid",
    )
    .fetch_all(&fixture.storage.pool)
    .await
    .unwrap();
    assert_eq!(
        active_time_columns,
        ["active_run_ms", "active_run_started_at_ms"]
    );

    let no_cache_cost_column: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('sessions') \
         WHERE name = 'no_cache_cost_micros' AND \"notnull\" = 1",
    )
    .fetch_one(&fixture.storage.pool)
    .await
    .unwrap();
    assert_eq!(no_cache_cost_column, 1);

    let lane_columns: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('usage_turns') \
         WHERE name IN ('lane_kind', 'lane_id', 'lane_seq') AND \"notnull\" = 1",
    )
    .fetch_one(&fixture.storage.pool)
    .await
    .unwrap();
    assert_eq!(lane_columns, 3);

    let effective_reasoning_column: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('usage_turns') \
         WHERE name = 'effective_reasoning' AND \"notnull\" = 0",
    )
    .fetch_one(&fixture.storage.pool)
    .await
    .unwrap();
    assert_eq!(effective_reasoning_column, 1);

    let credential_columns: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM pragma_table_info('provider_credentials') ORDER BY cid",
    )
    .fetch_all(&fixture.storage.pool)
    .await
    .unwrap();
    assert_eq!(credential_columns, ["provider_id", "source", "reference"]);

    // Episode tables exist and usage_turns carries the rewrite CHECK
    // ('episode' accepted) plus the nullable episode_seq column.
    let episode_tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
         AND name IN ('episodes', 'episode_archive')",
    )
    .fetch_one(&fixture.storage.pool)
    .await
    .unwrap();
    assert_eq!(episode_tables, 2);
    let episode_seq_column: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('usage_turns') \
         WHERE name = 'episode_seq' AND \"notnull\" = 0",
    )
    .fetch_one(&fixture.storage.pool)
    .await
    .unwrap();
    assert_eq!(episode_seq_column, 1);
    let usage_turns_sql: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'usage_turns'",
    )
    .fetch_one(&fixture.storage.pool)
    .await
    .unwrap();
    assert!(
        usage_turns_sql.contains("'episode'"),
        "rewrite_kind CHECK must include 'episode': {usage_turns_sql}"
    );
}

fn upgrade_test_migrator(
    migrations: &[(i64, &'static str, &'static str)],
) -> sqlx::migrate::Migrator {
    use std::borrow::Cow;

    use sqlx::SqlSafeStr;
    use sqlx::migrate::{Migration, MigrationType};

    sqlx::migrate::Migrator::with_migrations(
        migrations
            .iter()
            .map(|(version, description, sql)| {
                Migration::new(
                    *version,
                    Cow::Borrowed(*description),
                    MigrationType::Simple,
                    (*sql).into_sql_str(),
                    false,
                )
            })
            .collect(),
    )
}

async fn seed_upgrade_test_database(paths: &BonsaiPaths) {
    let v1 = upgrade_test_migrator(&[(
        1,
        "public state",
        "CREATE TABLE public_state (value TEXT NOT NULL);",
    )]);
    let storage = Storage::open_paths_with_migrator(paths.clone(), &v1)
        .await
        .unwrap();
    sqlx::query("INSERT INTO public_state (value) VALUES ('keep-me')")
        .execute(&storage.pool)
        .await
        .unwrap();
    storage.close().await;
}

fn upgrade_backup_files(paths: &BonsaiPaths) -> Vec<PathBuf> {
    let backup_dir = paths.home_dir().join(UPGRADE_BACKUP_DIR);
    let mut files = std::fs::read_dir(backup_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    files.sort();
    files
}

#[tokio::test]
async fn schema_upgrade_preserves_state_and_creates_restorable_backup() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let paths = BonsaiPaths::from_home_dir(temp_dir.path().to_path_buf());
    seed_upgrade_test_database(&paths).await;
    let v2 = upgrade_test_migrator(&[
        (
            1,
            "public state",
            "CREATE TABLE public_state (value TEXT NOT NULL);",
        ),
        (
            2,
            "add state kind",
            "ALTER TABLE public_state ADD COLUMN kind TEXT NOT NULL DEFAULT 'saved';",
        ),
    ]);

    let storage = Storage::open_paths_with_migrator(paths.clone(), &v2)
        .await
        .unwrap();
    let row: (String, String) = sqlx::query_as("SELECT value, kind FROM public_state")
        .fetch_one(&storage.pool)
        .await
        .unwrap();
    assert_eq!(row, ("keep-me".to_string(), "saved".to_string()));
    storage.close().await;

    let backups = upgrade_backup_files(&paths);
    assert_eq!(backups.len(), 1);
    let backup_pool = SqlitePoolOptions::new()
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&backups[0])
                .create_if_missing(false),
        )
        .await
        .unwrap();
    let value: String = sqlx::query_scalar("SELECT value FROM public_state")
        .fetch_one(&backup_pool)
        .await
        .unwrap();
    assert_eq!(value, "keep-me");
    let kind_column_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('public_state') WHERE name = 'kind'",
    )
    .fetch_one(&backup_pool)
    .await
    .unwrap();
    assert_eq!(kind_column_count, 0);
    backup_pool.close().await;
}

#[tokio::test]
async fn failed_schema_upgrade_restores_pre_upgrade_database() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let paths = BonsaiPaths::from_home_dir(temp_dir.path().to_path_buf());
    seed_upgrade_test_database(&paths).await;
    let broken_v2 = upgrade_test_migrator(&[
        (
            1,
            "public state",
            "CREATE TABLE public_state (value TEXT NOT NULL);",
        ),
        (
            2,
            "broken upgrade",
            "ALTER TABLE public_state ADD COLUMN kind TEXT; THIS IS NOT SQL;",
        ),
    ]);

    let error = Storage::open_paths_with_migrator(paths.clone(), &broken_v2)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("restored the database"));

    let pool = SqlitePoolOptions::new()
        .connect_with(
            SqliteConnectOptions::new()
                .filename(paths.db_path())
                .create_if_missing(false),
        )
        .await
        .unwrap();
    let value: String = sqlx::query_scalar("SELECT value FROM public_state")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(value, "keep-me");
    let migration_version: i64 =
        sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(migration_version, 1);
    let kind_column_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('public_state') WHERE name = 'kind'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kind_column_count, 0);
    assert_eq!(upgrade_backup_files(&paths).len(), 1);
    pool.close().await;
}

/// Cross-session dashboard aggregates: day buckets, per-model rollups with
/// legacy fallback attribution, expensive sessions, projects, lifetime totals,
/// durations, and tool stats.
///
/// Timezone-safe by construction: turn timestamps sit at 12:00 UTC exactly
/// 24 h apart, which lands on distinct consecutive local days in every offset,
/// so assertions check bucket counts and julian-day deltas — never literal
/// local date strings.
#[tokio::test]
async fn usage_dashboard_aggregates_across_sessions() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    // 2024-01-10 12:00:00 UTC.
    const DAY0: i64 = 1_704_888_000_000;
    const DAY: i64 = 86_400_000;

    let turn = |seq: usize, at: i64, model: Option<&str>, cost: Option<u64>| {
        let mut turn = usage_turn(
            seq,
            crate::agent::UsageTurnStatus::Reported,
            crate::agent::ContextRewriteKind::None,
        );
        turn.created_at_ms = at;
        turn.provider_id = model.map(|_| "anthropic".to_string());
        turn.model = model.map(str::to_string);
        turn.turn_cost_micros = cost;
        turn.no_cache_cost_micros = cost.map(|cost| cost + 50);
        turn
    };

    // Session 1 (project A, opus-4): two turns on day 0, one on day 1, all
    // stamped with a per-turn model and known cost.
    let project_a = fixture.project_path().to_path_buf();
    let s1 = fixture.start_session_with("anthropic", "opus-4").await;
    storage
        .sync_usage_turns_snapshot(
            s1,
            &[
                turn(1, DAY0, Some("opus-4"), Some(100)),
                turn(2, DAY0 + 1_000, Some("opus-4"), Some(100)),
                turn(3, DAY0 + DAY, Some("opus-4"), Some(100)),
            ],
        )
        .await
        .unwrap();
    storage
        .update_session_usage(
            s1,
            3_000,
            600,
            Some(300),
            Some(InputCacheUsage::new(1_200, 100, 3_000)),
        )
        .await
        .unwrap();

    // Session 2 (project A, glm-4.7): one synthetic turn on day 1 with no
    // per-turn model identity and unknown cost; it falls back to the session's
    // model and count as approximated.
    let s2 = fixture.start_session_with("zai", "glm-4.7").await;
    let mut legacy_turn = turn(1, DAY0 + DAY + 2_000, None, None);
    legacy_turn.prompt_tokens = Some(500);
    legacy_turn.completion_tokens = Some(50);
    storage
        .sync_usage_turns_snapshot(s2, &[legacy_turn])
        .await
        .unwrap();
    storage
        .update_session_usage(s2, 500, 50, None, None)
        .await
        .unwrap();

    // Session 3 (project B, opus-4): no usage turns, only session rollups —
    // e.g. all spend came through nested subagents.
    let project_b = fixture.temp_dir.path().join("project-b");
    std::fs::create_dir_all(&project_b).unwrap();
    let s3 = storage
        .start_session(
            &project_b,
            "anthropic",
            "opus-4",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    storage
        .update_session_usage(s3, 10_000, 2_000, Some(5_000), None)
        .await
        .unwrap();

    // Deterministic durations: s3 is the longest-running session.
    for (id, start, end) in [
        (s1, DAY0, Some(DAY0 + 10_800_000)),
        (s2, DAY0 + DAY, None),
        (s3, DAY0 + 2 * DAY, Some(DAY0 + 2 * DAY + 44_820_000)),
    ] {
        sqlx::query("UPDATE sessions SET started_at_ms = ?, ended_at_ms = ?, updated_at_ms = ? WHERE id = ?")
            .bind(start)
            .bind(end)
            .bind(end.unwrap_or(start + 1_800_000))
            .bind(id.as_i64())
            .execute(&storage.pool)
            .await
            .unwrap();
    }

    // Tool calls on s1: two bash (one failed), one read.
    let started_at = Instant::now();
    let tool = |id: &str, name: &str, status: ToolStatus| ToolActivity {
        id: id.to_string(),
        name: name.to_string(),
        arguments: "{}".to_string(),
        status,
        result: Some("done".to_string()),
        diff: None,
        started_at,
        finished_at: Some(started_at + Duration::from_millis(10)),
    };
    storage
        .replace_transcript_snapshot(
            s1,
            &[TranscriptItem::ExecutionGroup(ExecutionGroup {
                id: 1,
                finished_at: Some(started_at + Duration::from_millis(10)),
                tools: vec![
                    tool("call-1", "bash", ToolStatus::Succeeded),
                    tool("call-2", "bash", ToolStatus::Failed),
                    tool("call-3", "read", ToolStatus::Succeeded),
                ],
            })],
        )
        .await
        .unwrap();

    let dashboard = storage.load_usage_dashboard().await.unwrap();

    // Pin SQLite's julian truncation (midnight-UTC .5 truncates to JDN − 1) —
    // the heatmap's month-label math (`usage_heatmap`) adds the 1 back and its
    // unit tests hardcode this same constant.
    let sqlite_y2k: i64 = sqlx::query_scalar("SELECT CAST(julianday('2000-01-01') AS INTEGER)")
        .fetch_one(&storage.pool)
        .await
        .unwrap();
    assert_eq!(sqlite_y2k, 2_451_544);

    // Day buckets: two distinct consecutive local days.
    assert_eq!(dashboard.days.len(), 2);
    let (day0, day1) = (&dashboard.days[0], &dashboard.days[1]);
    assert_eq!(day1.julian_day - day0.julian_day, 1);
    assert_eq!(
        (
            day0.turns,
            day0.sessions,
            day0.input_tokens,
            day0.output_tokens
        ),
        (2, 1, 2_000, 400)
    );
    assert_eq!((day0.cost_micros, day0.savings_micros), (200, 100));
    // Only seq > 1 turns count toward cache-break stats; the fixture reports
    // a 40% read for them, so none are cold.
    assert_eq!((day0.warm_eligible_turns, day0.cold_turns), (1, 0));
    assert_eq!(
        (
            day1.turns,
            day1.sessions,
            day1.input_tokens,
            day1.output_tokens
        ),
        (2, 2, 1_500, 250)
    );
    assert_eq!((day1.cost_micros, day1.savings_micros), (100, 50));

    // Models: opus-4 leads on cost; the legacy glm turn is fallback-attributed.
    assert_eq!(dashboard.models.len(), 2);
    let opus = &dashboard.models[0];
    assert_eq!(
        (opus.provider_id.as_str(), opus.model.as_str()),
        ("anthropic", "opus-4")
    );
    assert_eq!(
        (
            opus.turns,
            opus.sessions,
            opus.input_tokens,
            opus.output_tokens
        ),
        (3, 1, 3_000, 600)
    );
    assert_eq!(
        (
            opus.cost_micros,
            opus.unknown_cost_turns,
            opus.fallback_attributed_turns
        ),
        (300, 0, 0)
    );
    assert_eq!(opus.cache_hit_percent(), Some(400));
    assert_eq!(opus.last_used_ms, DAY0 + DAY);
    let glm = &dashboard.models[1];
    assert_eq!(
        (glm.provider_id.as_str(), glm.model.as_str()),
        ("zai", "glm-4.7")
    );
    assert_eq!(
        (
            glm.turns,
            glm.unknown_cost_turns,
            glm.fallback_attributed_turns
        ),
        (1, 1, 1)
    );
    assert_eq!(glm.cost_micros, 0);

    // Expensive sessions: unknown-cost s2 is excluded, s3 outranks s1.
    let top: Vec<_> = dashboard.top_sessions.iter().map(|s| s.id).collect();
    assert_eq!(top, vec![s3, s1]);

    // Projects ordered by tokens, shown by their directory display name.
    assert_eq!(dashboard.projects.len(), 2);
    assert_eq!(dashboard.projects[0].name, "project-b");
    assert_eq!(
        (dashboard.projects[0].sessions, dashboard.projects[0].tokens),
        (1, 12_000)
    );
    let project_a_name = project_a.file_name().unwrap().to_string_lossy();
    assert_eq!(dashboard.projects[1].name, project_a_name);
    assert_eq!(
        (dashboard.projects[1].sessions, dashboard.projects[1].tokens),
        (2, 4_150)
    );

    // Lifetime totals come from session rollups (subagent-inclusive), with the
    // unknown-cost session counted rather than silently zeroed.
    assert_eq!(dashboard.lifetime.input_tokens, 13_500);
    assert_eq!(dashboard.lifetime.output_tokens, 2_650);
    assert_eq!(dashboard.lifetime.cost_micros, 5_300);
    assert_eq!(dashboard.lifetime.unknown_cost_sessions, 1);
    assert_eq!(dashboard.lifetime.cache_read_tokens, 1_200);

    // Durations: s3 (12h 27m) is the longest; its session is named after its
    // project directory.
    assert_eq!(dashboard.session_stats.total_sessions, 3);
    assert_eq!(dashboard.session_stats.longest_duration_ms, 44_820_000);
    assert_eq!(dashboard.session_stats.longest_session_name, "project-b");
    assert_eq!(
        dashboard.session_stats.avg_duration_ms,
        (10_800_000 + 1_800_000 + 44_820_000) / 3
    );

    // Statuses and tools.
    assert_eq!(dashboard.status_counts, vec![(SessionStatus::Active, 3)]);
    assert_eq!(dashboard.tools.len(), 2);
    assert_eq!(
        (
            dashboard.tools[0].name.as_str(),
            dashboard.tools[0].calls,
            dashboard.tools[0].failed
        ),
        ("bash", 2, 1)
    );
    assert_eq!(
        (
            dashboard.tools[1].name.as_str(),
            dashboard.tools[1].calls,
            dashboard.tools[1].failed
        ),
        ("read", 1, 0)
    );
}

/// The TUI flush maps `UsageTurn` → `UsageTurnReport` → `to_usage_turn` before
/// persisting, so a field dropped by that projection is silently erased on the
/// next DELETE+INSERT snapshot. Lock the full round-trip down.
#[test]
fn usage_turn_report_projection_roundtrips() {
    let turn = usage_turn(
        1,
        crate::agent::UsageTurnStatus::Reported,
        crate::agent::ContextRewriteKind::Gc,
    );
    let report = crate::context_view::UsageTurnReport::from(&turn);
    assert_eq!(report.to_usage_turn(), turn);
}

#[tokio::test]
async fn batched_session_snapshot_rolls_back_on_error() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let session_id = fixture.start_session().await;
    let transcript = vec![TranscriptItem::UserMessage {
        text: "batched transcript".to_string(),
    }];
    let turns = vec![usage_turn(
        1,
        crate::agent::UsageTurnStatus::Reported,
        crate::agent::ContextRewriteKind::None,
    )];

    let result = storage
        .with_session_snapshot_tx("batched session snapshot test", async move |tx, now| {
            storage
                .replace_transcript_snapshot_in_tx(tx, session_id, &transcript, now)
                .await?;
            storage
                .sync_usage_turns_snapshot_in_tx(tx, session_id, &turns, now)
                .await?;
            anyhow::bail!("intentional snapshot failure");
        })
        .await;

    assert!(result.is_err());
    assert_eq!(
        storage.count_transcript_blocks(session_id).await.unwrap(),
        0
    );
    assert_eq!(storage.count_usage_turns(session_id).await.unwrap(), 0);

    let snapshot = storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert!(snapshot.transcript.is_empty());
    assert!(snapshot.usage_turns.is_empty());
}

#[tokio::test]
async fn usage_turn_estimate_metadata_uses_stable_db_values_with_legacy_decode() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let session_id = fixture.start_session().await;
    let mut turn = usage_turn(
        1,
        crate::agent::UsageTurnStatus::Reported,
        crate::agent::ContextRewriteKind::None,
    );
    turn.estimate_source = Some(TokenCounterKind::AnthropicCountTokens);
    turn.estimate_confidence = Some(EstimateConfidence::High);

    storage
        .sync_usage_turns_snapshot(session_id, &[turn.clone()])
        .await
        .unwrap();

    let row: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT estimate_source, estimate_confidence FROM usage_turns WHERE session_id = ?",
    )
    .bind(session_id.as_i64())
    .fetch_one(&storage.pool)
    .await
    .unwrap();
    assert_eq!(row.0.as_deref(), Some("anthropic-count-tokens"));
    assert_eq!(row.1.as_deref(), Some("high"));

    sqlx::query(
        "UPDATE usage_turns SET estimate_source = ?, estimate_confidence = ? WHERE session_id = ?",
    )
    .bind("anthropic count_tokens")
    .bind("high confidence")
    .bind(session_id.as_i64())
    .execute(&storage.pool)
    .await
    .unwrap();

    let snapshot = storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.usage_turns, vec![turn]);
}

#[tokio::test]
async fn deleting_session_cascades_usage_turns() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let session_id = fixture.start_session().await;
    storage
        .sync_usage_turns_snapshot(
            session_id,
            &[usage_turn(
                1,
                crate::agent::UsageTurnStatus::Reported,
                crate::agent::ContextRewriteKind::None,
            )],
        )
        .await
        .unwrap();
    assert_eq!(storage.count_usage_turns(session_id).await.unwrap(), 1);

    assert_eq!(
        storage
            .forget_session(fixture.project_path(), session_id)
            .await
            .unwrap(),
        ForgetSessionOutcome::Forgotten
    );
    assert_eq!(storage.count_usage_turns(session_id).await.unwrap(), 0);
}

#[tokio::test]
async fn forget_session_rejects_live_target() {
    let fixture = TestStorage::new().await;
    let session_id = fixture.start_session().await;
    fixture
        .storage
        .record_session_heartbeat(session_id, false)
        .await
        .unwrap();

    assert_eq!(
        fixture
            .storage
            .forget_session(fixture.project_path(), session_id)
            .await
            .unwrap(),
        ForgetSessionOutcome::Live
    );
    assert!(
        fixture
            .storage
            .session_summary(session_id)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn forget_session_rejects_target_from_another_project() {
    let fixture = TestStorage::new().await;
    let other_project = fixture.project_path().join("other-project");
    std::fs::create_dir(&other_project).unwrap();
    let session_id = fixture
        .storage
        .start_session(
            &other_project,
            "anthropic",
            "claude-sonnet-4-5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();

    assert_eq!(
        fixture
            .storage
            .forget_session(fixture.project_path(), session_id)
            .await
            .unwrap(),
        ForgetSessionOutcome::DifferentProject
    );
    assert!(
        fixture
            .storage
            .session_summary(session_id)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn forget_session_removes_from_recent_list_for_picker_refresh() {
    let fixture = TestStorage::new().await;
    let session_a = fixture.start_session().await;
    let session_b = fixture
        .storage
        .start_session(
            fixture.project_path(),
            "codex",
            "gpt-5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();

    // Both sessions in recent list
    let recent = fixture
        .storage
        .recent_sessions_for_project(fixture.project_path(), 20)
        .await
        .unwrap();
    assert_eq!(recent.len(), 2);

    // Delete session_a
    assert_eq!(
        fixture
            .storage
            .forget_session(fixture.project_path(), session_a)
            .await
            .unwrap(),
        ForgetSessionOutcome::Forgotten
    );

    // session_a gone from recent list, session_b remains
    let recent = fixture
        .storage
        .recent_sessions_for_project(fixture.project_path(), 20)
        .await
        .unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].id, session_b);
}

#[tokio::test]
async fn session_summary_persists_as_title() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let session_id = storage
        .start_session(
            temp_dir.path(),
            "anthropic",
            "claude-sonnet-4-5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();

    storage
        .set_session_summary(session_id, "Session titles")
        .await
        .unwrap();

    let snapshot = storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.summary.summary, "Session titles");
}

#[tokio::test]
async fn session_usage_totals_are_persisted() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let session_id = storage
        .start_session(
            temp_dir.path(),
            "anthropic",
            "claude-sonnet-4-5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();

    storage
        .update_session_usage_totals(
            session_id,
            &crate::agent::UsageTotals {
                prompt_tokens: 123,
                completion_tokens: 45,
                cost_micros: Some(678),
                no_cache_cost_micros: Some(900),
                input_cache: Some(InputCacheUsage::new(25, 10, 100)),
            },
        )
        .await
        .unwrap();

    let summary = storage.session_summary(session_id).await.unwrap().unwrap();
    assert_eq!(summary.prompt_token_count, 123);
    assert_eq!(summary.completion_token_count, 45);
    assert_eq!(summary.cache_read_input_token_count, 25);
    assert_eq!(summary.cache_creation_input_token_count, 10);
    assert_eq!(summary.cache_measured_input_token_count, 100);
    assert_eq!(
        decode_input_cache_usage(
            summary.cache_read_input_token_count,
            summary.cache_creation_input_token_count,
            summary.cache_measured_input_token_count
        ),
        Some(InputCacheUsage::new(25, 10, 100))
    );
    assert_eq!(summary.cost_micros, 678);
    assert_eq!(summary.no_cache_cost_micros, 900);
}

#[tokio::test]
async fn session_run_selection_is_persisted() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let session_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5.5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();

    storage
        .set_session_run_selection(
            session_id,
            "anthropic",
            "claude-sonnet-4-5",
            ReasoningSelection::from_effort(ReasoningEffort::High),
        )
        .await
        .unwrap();

    let summary = storage.session_summary(session_id).await.unwrap().unwrap();
    assert_eq!(summary.provider_id, "anthropic");
    assert_eq!(summary.model, "claude-sonnet-4-5");
    assert_eq!(
        summary.reasoning,
        ReasoningSelection::from_effort(ReasoningEffort::High)
    );
}

#[tokio::test]
async fn session_usage_unknown_cost_is_persisted_as_sentinel() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let session_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5.5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();

    storage
        .update_session_usage(session_id, 123, 45, None, None)
        .await
        .unwrap();

    let summary = storage.session_summary(session_id).await.unwrap().unwrap();
    assert_eq!(summary.prompt_token_count, 123);
    assert_eq!(summary.completion_token_count, 45);
    assert_eq!(summary.cache_read_input_token_count, 0);
    assert_eq!(summary.cache_creation_input_token_count, 0);
    assert_eq!(summary.cache_measured_input_token_count, 0);
    assert_eq!(
        decode_input_cache_usage(
            summary.cache_read_input_token_count,
            summary.cache_creation_input_token_count,
            summary.cache_measured_input_token_count
        ),
        None
    );
    assert_eq!(summary.cost_micros, -1);
    assert_eq!(summary.no_cache_cost_micros, -1);
    assert_eq!(decode_cost_micros(summary.cost_micros), None);
}

#[tokio::test]
async fn save_plan_to_library_roundtrips_and_upserts_same_saved_plan() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let source_session_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5.5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let mut first = sample_plan("Initial plan");
    first.edit().set_section("Approach", "Do the first thing.");
    first.edit().add_task("First task");

    let saved = storage
        .save_plan_to_library(source_session_id, None, &first, Some("main"))
        .await
        .unwrap();
    assert_eq!(saved.source_session_id, Some(source_session_id));
    assert_eq!(saved.branch.as_deref(), Some("main"));
    assert_eq!(saved.status, SavedPlanStatus::Draft);
    assert_eq!(saved.section_count, 1);
    assert_eq!(saved.task_count, 1);

    let clean_session_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5.5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let mut updated = sample_plan("Updated plan");
    updated
        .edit()
        .set_section("Approach", "Do the updated thing.");
    updated.edit().add_question("Which auth method?");
    updated.edit().add_task("Updated task");
    updated.edit().add_task("Another task");

    let updated_summary = storage
        .save_plan_to_library(clean_session_id, Some(saved.id), &updated, Some("feature"))
        .await
        .unwrap();
    assert_eq!(updated_summary.id, saved.id);
    assert_eq!(updated_summary.source_session_id, Some(source_session_id));
    assert_eq!(updated_summary.branch.as_deref(), Some("feature"));
    assert_eq!(updated_summary.task_count, 2);

    let snapshot = storage
        .load_saved_plan(saved.id)
        .await
        .unwrap()
        .expect("saved plan should load");
    assert_eq!(snapshot.plan.title, "Updated plan");
    assert_eq!(snapshot.plan.questions, ["Which auth method?"]);
    assert_eq!(snapshot.plan.tasks.len(), 2);
    assert_eq!(snapshot.summary.source_session_id, Some(source_session_id));
}

#[tokio::test]
async fn phased_plan_roundtrips_phases_and_bucketed_tasks() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let session_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5.5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();

    let mut plan = PlanDoc::default();
    plan.edit().set_title("Phased plan");
    plan.edit().set_section("Context", "why");
    plan.edit().add_phase("Phase 1: storage");
    plan.edit().add_phase("Phase 2: wiring");
    plan.edit()
        .add_task_to_phase("Phase 1: storage", "add table")
        .unwrap();
    plan.edit()
        .add_task_to_phase("Phase 1: storage", "migrate")
        .unwrap();
    plan.edit()
        .add_task_to_phase("Phase 2: wiring", "wire it")
        .unwrap();
    plan.edit().check_task("add table");

    let saved = storage
        .save_plan_to_library(session_id, None, &plan, Some("main"))
        .await
        .unwrap();
    // All tasks (across phases) are still counted.
    assert_eq!(saved.task_count, 3);

    let loaded = storage
        .load_saved_plan(saved.id)
        .await
        .unwrap()
        .expect("saved plan should load")
        .plan;

    assert!(loaded.is_phased());
    assert!(loaded.tasks.is_empty(), "phased plan has no flat tasks");
    assert_eq!(loaded.phases.len(), 2);
    assert_eq!(loaded.phases[0].name, "Phase 1: storage");
    assert_eq!(
        loaded.phases[0]
            .tasks
            .iter()
            .map(|t| (t.text.as_str(), t.done))
            .collect::<Vec<_>>(),
        [("add table", true), ("migrate", false)]
    );
    assert_eq!(loaded.phases[1].name, "Phase 2: wiring");
    assert_eq!(loaded.phases[1].tasks.len(), 1);
    assert!(!loaded.phases[1].tasks[0].done);
}

#[tokio::test]
async fn findings_roundtrip_through_saved_plan() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let session_id = storage
        .start_session(
            temp_dir.path(),
            "anthropic",
            "claude-sonnet-4-5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();

    let mut plan = PlanDoc::default();
    plan.edit().set_title("Findings plan");
    plan.edit().add_task("Fix it");
    plan.edit().add_finding(crate::plan::Finding {
        severity: crate::plan::Severity::Blocker,
        file: Some("src/foo.rs".to_string()),
        line: Some(42),
        issue: "data loss".to_string(),
        required_fix: "flush before close".to_string(),
        acceptance_tests: vec!["a passes".to_string(), "b passes".to_string()],
        source_ids: vec!["call-1".to_string()],
        task: Some("Fix it".to_string()),
        resolved: false,
    });
    plan.edit().add_finding(crate::plan::Finding {
        severity: crate::plan::Severity::Nit,
        file: None,
        line: None,
        issue: "naming".to_string(),
        required_fix: "rename".to_string(),
        acceptance_tests: vec![],
        source_ids: vec![],
        task: None,
        resolved: true,
    });

    let saved = storage
        .save_plan_to_library(session_id, None, &plan, Some("main"))
        .await
        .unwrap();
    let loaded = storage
        .load_saved_plan(saved.id)
        .await
        .unwrap()
        .expect("saved plan should load")
        .plan;

    // Findings round-trip exactly, including severity, optional file/line, the
    // JSON-encoded acceptance/source lists, the task link, and resolved state.
    assert_eq!(loaded.findings, plan.findings);
}

#[tokio::test]
async fn saved_plans_are_filtered_by_project() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let repo_a = temp_dir.path().join("repo-a");
    let repo_b = temp_dir.path().join("repo-b");
    std::fs::create_dir_all(&repo_a).unwrap();
    std::fs::create_dir_all(&repo_b).unwrap();
    let session_a = storage
        .start_session(&repo_a, "codex", "gpt-5.5", ReasoningSelection::default())
        .await
        .unwrap();
    let session_b = storage
        .start_session(&repo_b, "codex", "gpt-5.5", ReasoningSelection::default())
        .await
        .unwrap();

    let saved_a = storage
        .save_plan_to_library(session_a, None, &sample_plan("Plan A"), Some("main"))
        .await
        .unwrap();
    storage
        .save_plan_to_library(session_b, None, &sample_plan("Plan B"), Some("main"))
        .await
        .unwrap();

    let plans = storage.saved_plans_for_project(&repo_a, 10).await.unwrap();
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].title, "Plan A");
    assert_eq!(plans[0].id, saved_a.id);
}

#[tokio::test]
async fn saved_plan_snapshot_is_frozen_against_later_live_edits() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let session_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5.5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let mut plan = sample_plan("Frozen title");
    plan.edit().add_task("original task");

    let saved = storage
        .save_plan_to_library(session_id, None, &plan, Some("main"))
        .await
        .unwrap();

    // The session keeps working after the save: the periodic flush rewrites
    // its *live* plan rows. This was the M4 double-duty bug — the library
    // entry shared those rows and silently mutated along.
    let mut live = plan.clone();
    live.edit().set_title("Mutated after save");
    live.edit().add_task("sneaky new task");
    storage
        .with_session_snapshot_tx("live plan flush", async |tx, now| {
            storage
                .replace_plan_snapshot_in_tx(tx, session_id, &live, now)
                .await
        })
        .await
        .unwrap();

    let snapshot = storage
        .load_saved_plan(saved.id)
        .await
        .unwrap()
        .expect("saved plan should load");
    assert_eq!(snapshot.plan.title, "Frozen title");
    assert_eq!(snapshot.plan, plan, "the snapshot must be save-time exact");
    let expected_tasks = plan.tasks.len() as i64;
    assert_eq!(snapshot.summary.task_count, expected_tasks);
}

#[tokio::test]
async fn delete_saved_plan_keeps_source_session() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let session_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5.5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    storage
        .replace_transcript_snapshot(
            session_id,
            &[TranscriptItem::UserMessage {
                text: "keep this transcript".to_string(),
            }],
        )
        .await
        .unwrap();
    // Persist the session's live plan too: deleting the library entry must
    // only remove the frozen snapshot, never the session's own plan rows.
    let live_plan = sample_plan("Delete me");
    storage
        .with_session_snapshot_tx("persist live plan", async |tx, now| {
            storage
                .replace_plan_snapshot_in_tx(tx, session_id, &live_plan, now)
                .await
        })
        .await
        .unwrap();
    let saved = storage
        .save_plan_to_library(session_id, None, &sample_plan("Delete me"), None)
        .await
        .unwrap();

    assert!(storage.delete_saved_plan(saved.id).await.unwrap());
    assert!(storage.load_saved_plan(saved.id).await.unwrap().is_none());

    let snapshot = storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .expect("session transcript should remain");
    assert_eq!(snapshot.transcript.len(), 1);
    assert!(
        !snapshot.plan.is_empty(),
        "the source session's plan must survive deleting the saved-plan library entry"
    );
}

#[tokio::test]
async fn mark_plan_started_links_execution_session() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let plan_session_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5.5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let execution_session_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5.5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let saved = storage
        .save_plan_to_library(
            plan_session_id,
            None,
            &sample_plan("Start me"),
            Some("main"),
        )
        .await
        .unwrap();

    assert!(
        storage
            .mark_plan_started(saved.id, execution_session_id)
            .await
            .unwrap()
    );

    let summary = storage
        .load_saved_plan(saved.id)
        .await
        .unwrap()
        .expect("saved plan should remain")
        .summary;
    assert_eq!(summary.status, SavedPlanStatus::Started);
    assert_eq!(summary.execution_session_id, Some(execution_session_id));

    let source_plan_id = storage.source_plan_id(execution_session_id).await.unwrap();
    assert_eq!(source_plan_id, Some(saved.id.as_i64()));
}

#[tokio::test]
async fn session_summary_roundtrips_source_plan_link() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let plan_session_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5.5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let execution_session_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5.5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let saved = storage
        .save_plan_to_library(
            plan_session_id,
            None,
            &sample_plan("Persist link"),
            Some("main"),
        )
        .await
        .unwrap();

    storage
        .set_session_source_plan(execution_session_id, Some(saved.id))
        .await
        .unwrap();

    let snapshot = storage
        .load_session_snapshot(execution_session_id)
        .await
        .unwrap()
        .expect("execution session should load");
    assert_eq!(snapshot.summary.source_plan_id, Some(saved.id));
}

#[tokio::test]
async fn terminal_session_statuses_record_end_time() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();

    for status in [
        SessionStatus::Completed,
        SessionStatus::Forgotten,
        SessionStatus::Interrupted,
        SessionStatus::Failed,
    ] {
        let session_id = storage
            .start_session(
                temp_dir.path(),
                "codex",
                "gpt-5.5",
                ReasoningSelection::default(),
            )
            .await
            .unwrap();

        storage
            .mark_session_status(session_id, status.clone())
            .await
            .unwrap();

        let ended_at = storage.ended_at_ms(session_id).await.unwrap();
        assert!(
            ended_at.is_some(),
            "{} should set ended_at_ms",
            status.as_db_str()
        );
    }
}

#[tokio::test]
async fn promote_active_sessions_to_interrupted_only_promotes_leftovers() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;

    // A previous run that crashed: its row is still `Active`.
    let crashed = fixture.start_session().await;
    // An earlier run that exited cleanly.
    let completed = fixture.start_session().await;
    storage
        .mark_session_status(completed, SessionStatus::Completed)
        .await
        .unwrap();
    // The freshly started session for this run.
    let current = fixture.start_session().await;

    let promoted = storage
        .promote_active_sessions_to_interrupted(fixture.project_path(), current, 5)
        .await
        .unwrap();

    // Only the crashed leftover is promoted and surfaced, now marked interrupted.
    assert_eq!(promoted.len(), 1);
    assert_eq!(promoted[0].id, crashed);
    assert_eq!(promoted[0].status, SessionStatus::Interrupted);

    // The cleanly-closed session is untouched and never surfaced.
    assert!(promoted.iter().all(|session| session.id != completed));
    let completed_summary = storage.session_summary(completed).await.unwrap().unwrap();
    assert_eq!(completed_summary.status, SessionStatus::Completed);

    // The current session stays active and is never promoted.
    assert!(promoted.iter().all(|session| session.id != current));
    let current_summary = storage.session_summary(current).await.unwrap().unwrap();
    assert_eq!(current_summary.status, SessionStatus::Active);

    // Idempotent: a second pass finds nothing left to promote.
    let second = storage
        .promote_active_sessions_to_interrupted(fixture.project_path(), current, 5)
        .await
        .unwrap();
    assert!(second.is_empty());
    let current_summary = storage.session_summary(current).await.unwrap().unwrap();
    assert_eq!(current_summary.status, SessionStatus::Active);
}

#[tokio::test]
async fn abrupt_loss_promotes_and_resumes_the_last_committed_snapshot() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let crashed = fixture.start_session().await;
    storage
        .replace_transcript_snapshot(
            crashed,
            &[TranscriptItem::UserMessage {
                text: "resume this committed work".to_string(),
            }],
        )
        .await
        .unwrap();
    storage
        .set_session_heartbeat_for_tests(
            crashed,
            now_ms() - peers::PEER_LIVENESS_THRESHOLD_MS - 1_000,
        )
        .await
        .unwrap();
    let current = fixture.start_session().await;

    let promoted = storage
        .promote_active_sessions_to_interrupted(fixture.project_path(), current, 5)
        .await
        .unwrap();
    assert!(
        promoted.iter().any(|session| {
            session.id == crashed && session.status == SessionStatus::Interrupted
        })
    );
    assert_eq!(
        storage
            .switch_active_session(
                current,
                crashed,
                fixture.project_path(),
                SessionStatus::Completed,
            )
            .await
            .unwrap(),
        ResumeSessionOutcome::Resumed
    );

    let resumed = storage
        .load_session_snapshot(crashed)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.summary.status, SessionStatus::Active);
    assert!(matches!(
        resumed.transcript.as_slice(),
        [TranscriptItem::UserMessage { text }] if text == "resume this committed work"
    ));
    assert_eq!(
        storage
            .session_summary(current)
            .await
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Completed
    );
}

#[tokio::test]
async fn transcript_write_failure_preserves_the_last_committed_snapshot() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let session = fixture.start_session().await;
    storage
        .replace_transcript_snapshot(
            session,
            &[TranscriptItem::UserMessage {
                text: "last committed snapshot".to_string(),
            }],
        )
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER simulate_full_disk BEFORE INSERT ON transcript_blocks \
         BEGIN SELECT RAISE(FAIL, 'database or disk is full'); END",
    )
    .execute(&storage.pool)
    .await
    .unwrap();
    // Force WAL checkpoint so the schema change is visible to every pool
    // connection — the CREATE TRIGGER is DDL, and other connections may
    // cache a stale schema under WAL journal mode.
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&storage.pool)
        .await
        .unwrap();

    let error = storage
        .replace_transcript_snapshot(
            session,
            &[TranscriptItem::UserMessage {
                text: "partial replacement".to_string(),
            }],
        )
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("database or disk is full"),
        "unexpected error: {error:#}"
    );

    let snapshot = storage
        .load_session_snapshot(session)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        snapshot.transcript.as_slice(),
        [TranscriptItem::UserMessage { text }] if text == "last committed snapshot"
    ));
}

#[tokio::test]
async fn damaged_session_row_fails_with_recovery_guidance_and_can_be_repaired() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let session = fixture.start_session().await;
    let original_reasoning: String =
        sqlx::query_scalar("SELECT reasoning_json FROM sessions WHERE id = ?")
            .bind(session.as_i64())
            .fetch_one(&storage.pool)
            .await
            .unwrap();
    sqlx::query("UPDATE sessions SET reasoning_json = 'not-json' WHERE id = ?")
        .bind(session.as_i64())
        .execute(&storage.pool)
        .await
        .unwrap();

    let error = storage.session_summary(session).await.unwrap_err();
    let detail = format!("{error:#}");
    assert!(detail.contains(&format!("Persisted session {session} is damaged")));
    assert!(detail.contains("bonsai doctor"));
    assert!(detail.contains("Failed to parse persisted session reasoning"));

    sqlx::query("UPDATE sessions SET reasoning_json = ? WHERE id = ?")
        .bind(original_reasoning)
        .bind(session.as_i64())
        .execute(&storage.pool)
        .await
        .unwrap();
    assert!(
        storage
            .load_session_snapshot(session)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn heartbeat_updates_liveness_without_touching_content_recency() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let session = fixture.start_session().await;

    let before = storage.session_summary(session).await.unwrap().unwrap();
    assert_eq!(
        storage.session_heartbeat_for_tests(session).await.unwrap(),
        None,
        "a fresh session has no heartbeat until the writer ticks"
    );

    storage
        .record_session_heartbeat(session, false)
        .await
        .unwrap();

    let heartbeat = storage.session_heartbeat_for_tests(session).await.unwrap();
    assert!(heartbeat.is_some(), "heartbeat should be recorded");
    let after = storage.session_summary(session).await.unwrap().unwrap();
    assert_eq!(
        after.updated_at_ms, before.updated_at_ms,
        "a liveness heartbeat must not advance content recency (/resume ordering)"
    );
}

#[tokio::test]
async fn active_time_recovery_counts_only_through_the_last_heartbeat() {
    let fixture = TestStorage::new().await;
    let session = fixture.start_session().await;
    sqlx::query(
        "UPDATE sessions SET active_run_ms = 1000, active_run_started_at_ms = 10000, \
         last_heartbeat_ms = 13000 WHERE id = ?",
    )
    .bind(session.as_i64())
    .execute(&fixture.storage.pool)
    .await
    .unwrap();

    let recovered = fixture.storage.begin_session_run(session).await.unwrap();

    assert_eq!(recovered, 4_000, "the idle gap before resume is excluded");
    let final_ms = fixture.storage.finish_session_run(session).await.unwrap();
    assert!(final_ms >= recovered);
}

#[tokio::test]
async fn idle_heartbeats_do_not_consume_active_time() {
    let fixture = TestStorage::new().await;
    let session = fixture.start_session().await;
    sqlx::query(
        "UPDATE sessions SET active_run_ms = 5000, active_run_started_at_ms = NULL WHERE id = ?",
    )
    .bind(session.as_i64())
    .execute(&fixture.storage.pool)
    .await
    .unwrap();

    fixture
        .storage
        .record_session_heartbeat(session, false)
        .await
        .unwrap();

    assert_eq!(
        fixture
            .storage
            .session_active_run_ms(session)
            .await
            .unwrap(),
        5_000
    );
}

#[tokio::test]
async fn terminal_timing_finalization_is_idempotent() {
    let fixture = TestStorage::new().await;
    let session = fixture.start_session().await;
    let started_at_ms = crate::util::time::now_ms() - 2_000;
    sqlx::query(
        "UPDATE sessions SET active_run_ms = 1000, active_run_started_at_ms = ?, busy = 1 WHERE id = ?",
    )
    .bind(started_at_ms)
    .bind(session.as_i64())
    .execute(&fixture.storage.pool)
    .await
    .unwrap();

    fixture
        .storage
        .mark_session_termination(session, SessionStatus::Completed, None)
        .await
        .unwrap();
    let first = fixture
        .storage
        .session_active_run_ms(session)
        .await
        .unwrap();
    fixture
        .storage
        .mark_session_termination(session, SessionStatus::Completed, None)
        .await
        .unwrap();
    let second = fixture
        .storage
        .session_active_run_ms(session)
        .await
        .unwrap();

    assert!(first >= 3_000);
    assert_eq!(second, first);
    let busy: bool = sqlx::query_scalar("SELECT busy FROM sessions WHERE id = ?")
        .bind(session.as_i64())
        .fetch_one(&fixture.storage.pool)
        .await
        .unwrap();
    assert!(!busy, "terminal sessions must never retain a busy marker");
}

#[tokio::test]
async fn finishing_turn_keeps_process_session_live() {
    let fixture = TestStorage::new().await;
    let session = fixture.start_session().await;
    fixture
        .storage
        .record_session_heartbeat(session, true)
        .await
        .unwrap();
    fixture.storage.begin_session_run(session).await.unwrap();

    fixture.storage.finish_session_run(session).await.unwrap();

    assert_eq!(
        fixture
            .storage
            .session_summary(session)
            .await
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Active
    );
    assert!(fixture.storage.is_session_live(session).await.unwrap());
}

#[tokio::test]
async fn activating_session_stamps_heartbeat_immediately() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let session = fixture.start_session().await;
    let active_session_id = std::sync::Arc::new(tokio::sync::Mutex::new(None));

    activate_session_heartbeat(storage, &active_session_id, session)
        .await
        .unwrap();

    assert_eq!(*active_session_id.lock().await, Some(session));
    assert!(
        storage
            .session_heartbeat_for_tests(session)
            .await
            .unwrap()
            .is_some(),
        "activation must stamp the heartbeat before the periodic writer ticks"
    );
}

#[tokio::test]
async fn switch_active_session_rejects_live_target_without_completing_current() {
    let fixture = TestStorage::new().await;
    let current = fixture.start_session().await;
    let target = fixture.start_session().await;
    fixture
        .storage
        .record_session_heartbeat(current, false)
        .await
        .unwrap();
    fixture
        .storage
        .record_session_heartbeat(target, false)
        .await
        .unwrap();

    assert_eq!(
        fixture
            .storage
            .switch_active_session(
                current,
                target,
                fixture.project_path(),
                SessionStatus::Completed,
            )
            .await
            .unwrap(),
        ResumeSessionOutcome::Live
    );
    assert_eq!(
        fixture
            .storage
            .session_summary(current)
            .await
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Active
    );
}

#[tokio::test]
async fn switch_active_session_claims_stale_target_and_stamps_heartbeat() {
    let fixture = TestStorage::new().await;
    let current = fixture.start_session().await;
    let target = fixture.start_session().await;
    let stale = crate::util::time::now_ms() - peers::PEER_LIVENESS_THRESHOLD_MS - 1_000;
    fixture
        .storage
        .set_session_heartbeat_for_tests(target, stale)
        .await
        .unwrap();

    assert_eq!(
        fixture
            .storage
            .switch_active_session(
                current,
                target,
                fixture.project_path(),
                SessionStatus::Completed,
            )
            .await
            .unwrap(),
        ResumeSessionOutcome::Resumed
    );
    assert_eq!(
        fixture
            .storage
            .session_summary(current)
            .await
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Completed
    );
    assert!(fixture.storage.is_session_live(target).await.unwrap());
}

#[tokio::test]
async fn switch_active_session_attaches_recovery_in_the_same_transaction() {
    let fixture = TestStorage::new().await;
    let current = fixture.start_session().await;
    let target = fixture.start_session().await;
    fixture
        .storage
        .mark_session_status(target, SessionStatus::Completed)
        .await
        .unwrap();
    let recovery_id = insert_active_recovery(&fixture, "atomic-resume-success").await;

    assert_eq!(
        fixture
            .storage
            .switch_active_session(
                current,
                target,
                fixture.project_path(),
                SessionStatus::Completed,
            )
            .await
            .unwrap(),
        ResumeSessionOutcome::Resumed
    );
    assert_eq!(
        fixture
            .storage
            .recovery_point(&recovery_id)
            .await
            .unwrap()
            .session_id,
        Some(target)
    );
}

#[tokio::test]
async fn recovery_attachment_failure_rolls_back_the_entire_session_switch() {
    let fixture = TestStorage::new().await;
    let current = fixture.start_session().await;
    let target = fixture.start_session().await;
    fixture
        .storage
        .mark_session_status(target, SessionStatus::Completed)
        .await
        .unwrap();
    insert_active_recovery(&fixture, "atomic-resume-failure").await;
    sqlx::query(
        "CREATE TRIGGER fail_recovery_attachment BEFORE UPDATE OF session_id ON recovery_points \
         BEGIN SELECT RAISE(FAIL, 'injected recovery attachment failure'); END",
    )
    .execute(&fixture.storage.pool)
    .await
    .unwrap();

    let error = fixture
        .storage
        .switch_active_session(
            current,
            target,
            fixture.project_path(),
            SessionStatus::Completed,
        )
        .await
        .unwrap_err();

    assert!(
        format!("{error:#}").contains("injected recovery attachment failure"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fixture
            .storage
            .session_summary(current)
            .await
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Active
    );
    let target_summary = fixture
        .storage
        .session_summary(target)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(target_summary.status, SessionStatus::Completed);
    assert!(!fixture.storage.is_session_live(target).await.unwrap());
}

#[tokio::test]
async fn claim_session_for_resume_has_one_live_owner() {
    let fixture = TestStorage::new().await;
    let target = fixture.start_session().await;
    fixture
        .storage
        .mark_session_status(target, SessionStatus::Completed)
        .await
        .unwrap();

    assert_eq!(
        fixture
            .storage
            .claim_session_for_resume(fixture.project_path(), target)
            .await
            .unwrap(),
        ResumeSessionOutcome::Resumed
    );
    assert_eq!(
        fixture
            .storage
            .claim_session_for_resume(fixture.project_path(), target)
            .await
            .unwrap(),
        ResumeSessionOutcome::Live
    );
}

/// The cross-project guard on explicit `-c <id>` resume: claiming a session
/// that belongs to another project must be rejected without mutating the
/// foreign row — resuming it would run that conversation against the wrong
/// working tree, permissions, and files. Mirrors
/// `forget_session_rejects_target_from_another_project`.
#[tokio::test]
async fn claim_session_for_resume_rejects_target_from_another_project() {
    let fixture = TestStorage::new().await;
    let other_project = fixture.project_path().join("other-project");
    std::fs::create_dir(&other_project).unwrap();
    let session_id = fixture
        .storage
        .start_session(
            &other_project,
            "anthropic",
            "claude-sonnet-4-5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    fixture
        .storage
        .mark_session_status(session_id, SessionStatus::Completed)
        .await
        .unwrap();

    assert_eq!(
        fixture
            .storage
            .claim_session_for_resume(fixture.project_path(), session_id)
            .await
            .unwrap(),
        ResumeSessionOutcome::DifferentProject
    );
    // The rejected claim must leave the foreign session untouched: still
    // Completed (not flipped Active) and still resumable from its own project.
    let summary = fixture
        .storage
        .session_summary(session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(summary.status, SessionStatus::Completed);
    assert_eq!(
        fixture
            .storage
            .claim_session_for_resume(&other_project, session_id)
            .await
            .unwrap(),
        ResumeSessionOutcome::Resumed
    );
}

#[tokio::test]
async fn claim_session_for_resume_reports_missing_target() {
    let fixture = TestStorage::new().await;
    assert_eq!(
        fixture
            .storage
            .claim_session_for_resume(fixture.project_path(), SessionId::from_raw(999_999))
            .await
            .unwrap(),
        ResumeSessionOutcome::NotFound
    );
}

#[tokio::test]
async fn promotion_spares_live_peers_but_promotes_stale_and_legacy_rows() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let now = crate::util::time::now_ms();

    // A live concurrent process: active with a fresh heartbeat.
    let live_peer = fixture.start_session().await;
    storage
        .record_session_heartbeat(live_peer, true)
        .await
        .unwrap();
    // A crashed process: active but its heartbeat went stale.
    let stale = fixture.start_session().await;
    storage
        .set_session_heartbeat_for_tests(stale, now - peers::PEER_LIVENESS_THRESHOLD_MS - 1_000)
        .await
        .unwrap();
    // A legacy row from before heartbeats existed: active, NULL heartbeat.
    let legacy = fixture.start_session().await;
    let current = fixture.start_session().await;

    let promoted = storage
        .promote_active_sessions_to_interrupted(fixture.project_path(), current, 5)
        .await
        .unwrap();

    let promoted_ids: Vec<SessionId> = promoted.iter().map(|session| session.id).collect();
    assert!(
        !promoted_ids.contains(&live_peer),
        "a live concurrent session must never be flipped to interrupted"
    );
    assert!(promoted_ids.contains(&stale), "stale heartbeat = crashed");
    assert!(promoted_ids.contains(&legacy), "legacy NULL = old behavior");

    let live_summary = storage.session_summary(live_peer).await.unwrap().unwrap();
    assert_eq!(live_summary.status, SessionStatus::Active);
}

#[tokio::test]
async fn live_peer_listing_excludes_self_stale_terminal_and_other_projects() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let now = crate::util::time::now_ms();

    let me = fixture.start_session().await;
    storage.record_session_heartbeat(me, false).await.unwrap();

    let live = fixture.start_session().await;
    storage.record_session_heartbeat(live, false).await.unwrap();

    let stale = fixture.start_session().await;
    storage
        .set_session_heartbeat_for_tests(stale, now - peers::PEER_LIVENESS_THRESHOLD_MS - 1_000)
        .await
        .unwrap();

    let completed = fixture.start_session().await;
    storage
        .record_session_heartbeat(completed, false)
        .await
        .unwrap();
    storage
        .mark_session_status(completed, SessionStatus::Completed)
        .await
        .unwrap();

    // A live session in a *different* project root must not appear.
    let other_dir = tempfile::TempDir::new().unwrap();
    let other = storage
        .start_session(
            other_dir.path(),
            "anthropic",
            "claude-sonnet-4-5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    storage
        .record_session_heartbeat(other, false)
        .await
        .unwrap();

    let peers = storage
        .list_live_peer_sessions(fixture.project_path(), me)
        .await
        .unwrap();

    let ids: Vec<SessionId> = peers.iter().map(|peer| peer.id).collect();
    assert_eq!(ids, vec![live], "only the live same-project peer qualifies");
}

#[tokio::test]
async fn heartbeat_carries_working_state_to_peer_listing() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;

    let me = fixture.start_session().await;
    storage.record_session_heartbeat(me, false).await.unwrap();
    let peer = fixture.start_session().await;

    // Mid-turn heartbeat → peers see it as working.
    storage.record_session_heartbeat(peer, true).await.unwrap();
    let peers = storage
        .list_live_peer_sessions(fixture.project_path(), me)
        .await
        .unwrap();
    assert!(peers[0].working, "busy heartbeat must read back as working");

    // The turn finished; the next heartbeat flips it back to idle.
    storage.record_session_heartbeat(peer, false).await.unwrap();
    let peers = storage
        .list_live_peer_sessions(fixture.project_path(), me)
        .await
        .unwrap();
    assert!(!peers[0].working, "idle heartbeat must read back as idle");
}

#[tokio::test]
async fn memory_refresh_fanout_scopes_project_and_user_tiers() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let sender = fixture.start_session().await;
    let same_project = fixture.start_session().await;
    let other_dir = tempfile::TempDir::new().unwrap();
    let other_project = storage
        .start_session(
            other_dir.path(),
            "anthropic",
            "claude-sonnet-4-5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    for session in [sender, same_project, other_project] {
        storage
            .record_session_heartbeat(session, false)
            .await
            .unwrap();
    }
    let project_id = storage
        .ensure_project(fixture.project_path())
        .await
        .unwrap();

    assert_eq!(
        storage
            .publish_memory_refresh(
                project_id,
                sender,
                crate::memory::entry::MemoryTier::Project
            )
            .await
            .unwrap(),
        1
    );
    let project_delivery = storage
        .claim_ui_undelivered_messages(same_project)
        .await
        .unwrap();
    assert_eq!(project_delivery.len(), 1);
    assert_eq!(
        project_delivery[0].kind,
        peers::PeerMessageKind::MemoryRefresh
    );
    assert!(project_delivery[0].body.is_empty());
    assert!(
        storage
            .claim_ui_undelivered_messages(other_project)
            .await
            .unwrap()
            .is_empty()
    );

    assert_eq!(
        storage
            .publish_memory_refresh(project_id, sender, crate::memory::entry::MemoryTier::User)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        storage
            .claim_agent_undelivered_messages(other_project)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        storage
            .claim_ui_undelivered_messages(sender)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        storage
            .pending_agent_message_count(same_project)
            .await
            .unwrap(),
        0,
        "internal refresh notifications are not actionable peer messages"
    );
}

#[tokio::test]
async fn peer_message_consumers_lease_and_ack_independently() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let sender = fixture.start_session().await;
    let recipient = fixture.start_session().await;

    storage
        .send_peer_message(
            fixture.project_path(),
            sender,
            recipient,
            PeerMessageKind::Text,
            "wake me when you are done",
            0,
        )
        .await
        .unwrap();

    // A live UI lease excludes a competing poller.
    let ui = storage
        .claim_ui_undelivered_messages(recipient)
        .await
        .unwrap();
    assert_eq!(ui.len(), 1);
    assert_eq!(ui[0].from_session_id, sender);
    assert_eq!(ui[0].kind, PeerMessageKind::Text);
    assert_eq!(ui[0].body, "wake me when you are done");
    assert!(
        storage
            .claim_ui_undelivered_messages(recipient)
            .await
            .unwrap()
            .is_empty(),
        "a live UI lease must exclude another poller"
    );
    storage
        .acknowledge_ui_peer_deliveries(recipient, &[ui[0].receipt.clone()])
        .await
        .unwrap();

    // The agent claim is independent of the UI claim.
    let agent = storage
        .claim_agent_undelivered_messages(recipient)
        .await
        .unwrap();
    assert_eq!(agent.len(), 1);
    assert!(
        storage
            .claim_agent_undelivered_messages(recipient)
            .await
            .unwrap()
            .is_empty(),
        "a live agent lease must exclude another poller"
    );
    storage
        .acknowledge_agent_peer_deliveries(recipient, &[agent[0].receipt.clone()])
        .await
        .unwrap();

    // Nothing addressed to the sender.
    assert!(
        storage
            .claim_ui_undelivered_messages(sender)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn expired_peer_delivery_lease_replays_until_acknowledged() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let sender = fixture.start_session().await;
    let recipient = fixture.start_session().await;
    storage
        .send_peer_message(
            fixture.project_path(),
            sender,
            recipient,
            PeerMessageKind::Text,
            "durable handoff",
            0,
        )
        .await
        .unwrap();

    let first = storage
        .claim_ui_undelivered_messages(recipient)
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    sqlx::query("UPDATE agent_messages SET ui_lease_expires_at_ms = 0 WHERE id = ?")
        .bind(first[0].id)
        .execute(&storage.pool)
        .await
        .unwrap();

    let replay = storage
        .claim_ui_undelivered_messages(recipient)
        .await
        .unwrap();
    assert_eq!(replay.len(), 1);
    assert_eq!(replay[0].id, first[0].id);
    assert_ne!(replay[0].receipt, first[0].receipt);
    assert!(
        storage
            .acknowledge_ui_peer_deliveries(recipient, &[first[0].receipt.clone()])
            .await
            .is_err(),
        "an expired, superseded lease cannot acknowledge the replay"
    );
    storage
        .acknowledge_ui_peer_deliveries(recipient, &[replay[0].receipt.clone()])
        .await
        .unwrap();
    assert!(
        storage
            .claim_ui_undelivered_messages(recipient)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn agent_peer_ack_rolls_back_with_failed_context_snapshot() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let sender = fixture.start_session().await;
    let recipient = fixture.start_session().await;
    storage
        .send_peer_message(
            fixture.project_path(),
            sender,
            recipient,
            PeerMessageKind::Text,
            "persist me with context",
            0,
        )
        .await
        .unwrap();
    let first = storage
        .claim_agent_undelivered_messages(recipient)
        .await
        .unwrap();
    let context = ContextMessageSnapshot {
        messages: vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessageArgs::default()
                .content("persist me with context")
                .build()
                .unwrap(),
        )],
        ids: vec!["peer-msg".to_string()],
    };

    let failed_context = context.clone();
    let failed_receipt = first[0].receipt.clone();
    let failed: Result<()> = storage
        .with_session_snapshot_tx("failed peer context boundary", async move |tx, now| {
            storage
                .replace_context_snapshot_in_tx(tx, recipient, &failed_context, now)
                .await?;
            storage
                .acknowledge_peer_deliveries_in_tx(
                    tx,
                    recipient,
                    PeerDeliveryConsumer::Agent,
                    &[failed_receipt],
                    now,
                )
                .await?;
            anyhow::bail!("simulated crash before commit")
        })
        .await;
    assert!(failed.is_err());
    let persisted_context: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM context_messages WHERE session_id = ?")
            .bind(recipient.as_i64())
            .fetch_one(&storage.pool)
            .await
            .unwrap();
    assert_eq!(persisted_context, 0, "the context write must roll back too");

    sqlx::query("UPDATE agent_messages SET agent_lease_expires_at_ms = 0 WHERE id = ?")
        .bind(first[0].id)
        .execute(&storage.pool)
        .await
        .unwrap();
    let replay = storage
        .claim_agent_undelivered_messages(recipient)
        .await
        .unwrap();
    assert_eq!(replay[0].id, first[0].id);
    storage
        .replace_agent_context_snapshot(recipient, &context, &[], &[], &[replay[0].receipt.clone()])
        .await
        .unwrap();
    assert!(
        storage
            .claim_agent_undelivered_messages(recipient)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn peer_secrets_are_redacted_before_any_database_write() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let sender = fixture.start_session().await;
    let recipient = fixture.start_session().await;
    let github_token = format!("ghp_{}", "a1B2c3D4e5".repeat(4));
    let openai_key = format!("sk-{}", "ABCdef0123".repeat(4));
    let bearer = "abcDEF123456ghiJKL789";
    let message = format!(
        "review src/peer.rs with feature = true; token={github_token}; \
         Authorization: Bearer {bearer}"
    );

    storage
        .send_peer_message(
            fixture.project_path(),
            sender,
            recipient,
            PeerMessageKind::Text,
            &message,
            0,
        )
        .await
        .unwrap();
    let stored_message: String =
        sqlx::query_scalar("SELECT body FROM agent_messages WHERE kind = 'text'")
            .fetch_one(&storage.pool)
            .await
            .unwrap();
    assert!(stored_message.contains("review src/peer.rs with feature = true"));
    assert!(stored_message.contains("[REDACTED:GitHub token]"));
    assert!(stored_message.contains("Authorization: Bearer [REDACTED]"));
    assert!(!stored_message.contains(&github_token));
    assert!(!stored_message.contains(bearer));

    let note = format!("run cargo test after the handoff; key={openai_key}");
    storage
        .add_wake_subscription(fixture.project_path(), sender, recipient, &note, 0)
        .await
        .unwrap();
    let stored_note: String =
        sqlx::query_scalar("SELECT note FROM peer_wake_subscriptions WHERE fired_at_ms IS NULL")
            .fetch_one(&storage.pool)
            .await
            .unwrap();
    assert!(stored_note.contains("run cargo test after the handoff"));
    assert!(stored_note.contains("[REDACTED:OpenAI API key]"));
    assert!(!stored_note.contains(&openai_key));

    storage.fire_wake_subscriptions(recipient).await.unwrap();
    let done_notice: String =
        sqlx::query_scalar("SELECT body FROM agent_messages WHERE kind = 'done_notice'")
            .fetch_one(&storage.pool)
            .await
            .unwrap();
    assert!(done_notice.contains("run cargo test after the handoff"));
    assert!(done_notice.contains("[REDACTED:OpenAI API key]"));
    assert!(!done_notice.contains(&openai_key));
}

#[tokio::test]
async fn session_file_changes_upsert_by_path_and_list_newest_first() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let session = fixture.start_session().await;

    storage
        .record_session_file_changes(session, &["src/a.rs".to_string(), "src/b.rs".to_string()])
        .await
        .unwrap();
    // Re-recording the same path upserts instead of duplicating.
    storage
        .record_session_file_changes(session, &["src/a.rs".to_string()])
        .await
        .unwrap();

    let changed = storage
        .recent_session_file_changes(session, 10)
        .await
        .unwrap();
    assert_eq!(changed.len(), 2, "same path must not duplicate");
    assert!(changed.contains(&"src/a.rs".to_string()));
    assert!(changed.contains(&"src/b.rs".to_string()));
}

#[tokio::test]
async fn other_session_file_changes_since_filters_project_session_and_timestamp() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let own_session = fixture.start_session().await;
    let peer_session = fixture.start_session().await;
    let other_project = fixture.temp_dir.path().join("other-project");
    std::fs::create_dir(&other_project).unwrap();
    let other_project_session = storage
        .start_session(
            &other_project,
            "anthropic",
            "claude-sonnet-4-5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();

    storage
        .record_session_file_changes(
            own_session,
            &["src/own.rs".to_string(), "src/shared.rs".to_string()],
        )
        .await
        .unwrap();
    storage
        .record_session_file_changes(
            peer_session,
            &["src/peer.rs".to_string(), "src/stale.rs".to_string()],
        )
        .await
        .unwrap();
    storage
        .record_session_file_changes(other_project_session, &["src/other.rs".to_string()])
        .await
        .unwrap();
    sqlx::query(
        "UPDATE session_file_changes SET last_changed_at_ms = CASE path
         WHEN 'src/peer.rs' THEN 2000
         WHEN 'src/stale.rs' THEN 999
         ELSE last_changed_at_ms END",
    )
    .execute(&storage.pool)
    .await
    .unwrap();

    let paths = storage
        .other_session_file_changes_since(fixture.project_path(), own_session, 1_000)
        .await
        .unwrap();

    assert_eq!(paths, vec!["src/peer.rs".to_string()]);
}

#[tokio::test]
async fn wake_subscriptions_fire_exactly_once_and_deliver_done_notices() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let requester = fixture.start_session().await;
    let target = fixture.start_session().await;

    let created = storage
        .add_wake_subscription(
            fixture.project_path(),
            requester,
            target,
            "I'll validate our work",
            2,
        )
        .await
        .unwrap();
    assert_eq!(created.outcome, WakeSubscriptionOutcome::Created);
    let subscription_id = created.subscription_id.unwrap();

    let duplicate = storage
        .add_wake_subscription(fixture.project_path(), requester, target, "duplicate", 0)
        .await
        .unwrap();
    assert_eq!(duplicate.outcome, WakeSubscriptionOutcome::AlreadyPending);
    assert_eq!(duplicate.subscription_id, Some(subscription_id));

    let relationships = storage.wake_relationships(target).await.unwrap();
    assert_eq!(relationships.waiters, vec![requester]);

    let notified = storage.fire_wake_subscriptions(target).await.unwrap();
    assert_eq!(notified, vec![requester]);

    // The done notice landed in the requester's inbox with the note.
    let inbox = storage
        .claim_ui_undelivered_messages(requester)
        .await
        .unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].kind, PeerMessageKind::DoneNotice);
    assert_eq!(inbox[0].from_session_id, target);
    assert!(
        inbox[0].body.contains("finished its run"),
        "{}",
        inbox[0].body
    );
    assert!(
        inbox[0].body.contains("I'll validate our work"),
        "{}",
        inbox[0].body
    );
    // The done notice inherits the subscription's originating hop so a wake
    // chain cannot reset the anti-loop counter by parking.
    assert_eq!(inbox[0].hop_count, 2);
    assert_eq!(inbox[0].wake_subscription_id, Some(subscription_id));

    // Single-shot: a second fire finds nothing to claim and sends nothing.
    let second = storage.fire_wake_subscriptions(target).await.unwrap();
    assert!(second.is_empty(), "a subscription must never fire twice");
    assert!(
        storage
            .claim_ui_undelivered_messages(requester)
            .await
            .unwrap()
            .is_empty()
    );

    let next_run = storage
        .add_wake_subscription(fixture.project_path(), requester, target, "next run", 0)
        .await
        .unwrap();
    assert_eq!(next_run.outcome, WakeSubscriptionOutcome::Created);
}

#[tokio::test]
async fn add_wake_subscription_refuses_reverse_pending_pair() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let a = fixture.start_session().await;
    let b = fixture.start_session().await;

    let forward = storage
        .add_wake_subscription(fixture.project_path(), a, b, "", 0)
        .await
        .unwrap();
    assert_eq!(forward.outcome, WakeSubscriptionOutcome::Created);

    // B parking on A while A is parked on B would deadlock both sessions.
    let reverse = storage
        .add_wake_subscription(fixture.project_path(), b, a, "", 0)
        .await
        .unwrap();
    assert_eq!(reverse.outcome, WakeSubscriptionOutcome::ReversePending);
    assert!(
        storage
            .wake_relationships(a)
            .await
            .unwrap()
            .waiters
            .is_empty(),
        "the refused reverse subscription must not be stored"
    );

    // Once the forward edge fires, the reverse direction is allowed again.
    storage.fire_wake_subscriptions(b).await.unwrap();
    let reverse_after_fire = storage
        .add_wake_subscription(fixture.project_path(), b, a, "", 0)
        .await
        .unwrap();
    assert_eq!(reverse_after_fire.outcome, WakeSubscriptionOutcome::Created);
}

#[tokio::test]
async fn expire_wake_subscription_claims_once_and_synthesizes_done_notice() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let requester = fixture.start_session().await;
    let target = fixture.start_session().await;

    storage
        .add_wake_subscription(fixture.project_path(), requester, target, "rebase after", 1)
        .await
        .unwrap();

    let expired = storage
        .expire_wake_subscription(requester, target)
        .await
        .unwrap();
    assert!(expired, "first expiry must claim the pending row");

    let inbox = storage
        .claim_ui_undelivered_messages(requester)
        .await
        .unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].kind, PeerMessageKind::DoneNotice);
    assert_eq!(inbox[0].from_session_id, target);
    assert_eq!(inbox[0].hop_count, 1);
    assert!(
        inbox[0].body.contains("no longer running"),
        "{}",
        inbox[0].body
    );
    assert!(inbox[0].body.contains("rebase after"), "{}", inbox[0].body);

    // Exactly-once versus both a second expiry and the real fire.
    assert!(
        !storage
            .expire_wake_subscription(requester, target)
            .await
            .unwrap(),
        "a second expiry must find nothing to claim"
    );
    assert!(
        storage
            .fire_wake_subscriptions(target)
            .await
            .unwrap()
            .is_empty(),
        "the real fire must not double-notify an expired subscription"
    );
    assert!(
        storage
            .claim_ui_undelivered_messages(requester)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn recheck_wake_subscription_claims_only_the_exact_wait_once() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let requester = fixture.start_session().await;
    let target = fixture.start_session().await;

    let registration = storage
        .add_wake_subscription(fixture.project_path(), requester, target, "check files", 2)
        .await
        .unwrap();
    let subscription_id = registration.subscription_id.unwrap();
    let other_target = fixture.start_session().await;

    assert!(
        !storage
            .recheck_wake_subscription(requester, other_target, subscription_id)
            .await
            .unwrap(),
        "the subscription id must not resume a different target"
    );
    assert!(
        storage
            .recheck_wake_subscription(requester, target, subscription_id)
            .await
            .unwrap(),
        "the exact wait must be claimed"
    );

    let inbox = storage
        .claim_ui_undelivered_messages(requester)
        .await
        .unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].kind, PeerMessageKind::DoneNotice);
    assert_eq!(inbox[0].from_session_id, target);
    assert_eq!(inbox[0].hop_count, 2);
    assert!(inbox[0].body.contains("time limit"), "{}", inbox[0].body);
    assert!(inbox[0].body.contains("check files"), "{}", inbox[0].body);
    assert!(
        !storage
            .recheck_wake_subscription(requester, target, subscription_id)
            .await
            .unwrap(),
        "the periodic recheck must be one-shot"
    );
    assert!(
        storage
            .fire_wake_subscriptions(target)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn purge_reaps_fired_and_orphaned_wake_subscriptions() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let requester = fixture.start_session().await;
    let target = fixture.start_session().await;

    storage
        .add_wake_subscription(fixture.project_path(), requester, target, "", 0)
        .await
        .unwrap();
    storage.fire_wake_subscriptions(target).await.unwrap();
    storage
        .add_wake_subscription(fixture.project_path(), requester, target, "", 0)
        .await
        .unwrap();

    // Age both rows (one fired, one pending) past the retention window.
    let ancient = now_ms() - 8 * 24 * 60 * 60 * 1000;
    sqlx::query(
        "UPDATE peer_wake_subscriptions
         SET created_at_ms = ?, fired_at_ms = CASE WHEN fired_at_ms IS NULL THEN NULL ELSE ? END",
    )
    .bind(ancient)
    .bind(ancient)
    .execute(&storage.pool)
    .await
    .unwrap();

    storage.purge_stale_peer_state().await.unwrap();

    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM peer_wake_subscriptions")
        .fetch_one(&storage.pool)
        .await
        .unwrap();
    assert_eq!(
        remaining, 0,
        "both the fired and the orphaned pending row must be reaped"
    );
}

#[tokio::test]
async fn promotion_fires_wake_subscriptions_for_promoted_sessions() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let requester = fixture.start_session().await;
    let crashed = fixture.start_session().await;
    let booting = fixture.start_session().await;

    storage
        .add_wake_subscription(fixture.project_path(), requester, crashed, "", 0)
        .await
        .unwrap();
    // The waited-on session crashes: stale heartbeat, still Active.
    storage
        .set_session_heartbeat_for_tests(crashed, now_ms() - 60_000)
        .await
        .unwrap();
    // The requester stays live so it is not itself promoted.
    storage
        .record_session_heartbeat(requester, false)
        .await
        .unwrap();

    let promoted = storage
        .promote_active_sessions_to_interrupted(fixture.project_path(), booting, 5)
        .await
        .unwrap();
    assert!(
        promoted.iter().any(|s| s.id == crashed),
        "the crashed session must be promoted"
    );

    // Promotion fired the crashed session's subscriptions: the waiter got a
    // done notice instead of staying parked forever.
    let inbox = storage
        .claim_ui_undelivered_messages(requester)
        .await
        .unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].kind, PeerMessageKind::DoneNotice);
    assert_eq!(inbox[0].from_session_id, crashed);
}

#[tokio::test]
async fn peer_claims_lifecycle_upsert_release_and_terminal_cleanup() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let session = fixture.start_session().await;

    storage
        .add_peer_claim(fixture.project_path(), session, "running full test suite")
        .await
        .unwrap();
    storage
        .add_peer_claim(fixture.project_path(), session, "owns src/tui/")
        .await
        .unwrap();
    // Re-claiming is an upsert, not a duplicate.
    storage
        .add_peer_claim(fixture.project_path(), session, "owns src/tui/")
        .await
        .unwrap();
    assert_eq!(
        storage.peer_claims_for_session(session).await.unwrap(),
        vec![
            "running full test suite".to_string(),
            "owns src/tui/".to_string()
        ]
    );

    // Explicit release drops one claim; releasing again reports nothing live.
    assert!(
        storage
            .release_peer_claim(session, "owns src/tui/")
            .await
            .unwrap()
    );
    assert!(
        !storage
            .release_peer_claim(session, "owns src/tui/")
            .await
            .unwrap()
    );
    assert_eq!(
        storage.peer_claims_for_session(session).await.unwrap(),
        vec!["running full test suite".to_string()]
    );

    // A terminal status releases everything the session still held.
    storage
        .mark_session_status(session, SessionStatus::Completed)
        .await
        .unwrap();
    assert!(
        storage
            .peer_claims_for_session(session)
            .await
            .unwrap()
            .is_empty(),
        "terminal sessions must hold no claims"
    );
}

#[tokio::test]
async fn promotion_releases_the_promoted_sessions_claims() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;

    let crashed = fixture.start_session().await;
    storage
        .add_peer_claim(fixture.project_path(), crashed, "running full test suite")
        .await
        .unwrap();
    let current = fixture.start_session().await;

    let promoted = storage
        .promote_active_sessions_to_interrupted(fixture.project_path(), current, 5)
        .await
        .unwrap();
    assert!(promoted.iter().any(|session| session.id == crashed));
    assert!(
        storage
            .peer_claims_for_session(crashed)
            .await
            .unwrap()
            .is_empty(),
        "crash-recovery promotion must release the leftover's claims"
    );
}

#[tokio::test]
async fn peer_message_block_round_trips_without_message_row() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let session_id = fixture.start_session().await;

    storage
        .replace_transcript_snapshot(
            session_id,
            &[
                TranscriptItem::PeerMessage {
                    source_message_id: Some(123),
                    session_id: 45,
                    outgoing: false,
                    text: "what did you change?".to_string(),
                },
                TranscriptItem::PeerMessage {
                    source_message_id: None,
                    session_id: 45,
                    outgoing: true,
                    text: "touched src/tui only".to_string(),
                },
            ],
        )
        .await
        .unwrap();

    let snapshot = storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            snapshot.transcript.as_slice(),
            [
                TranscriptItem::PeerMessage {
                    session_id: 45,
                    outgoing: false,
                    ..
                },
                TranscriptItem::PeerMessage {
                    session_id: 45,
                    outgoing: true,
                    ..
                },
            ]
        ),
        "peer messages should reload with direction intact: {:?}",
        snapshot.transcript
    );

    // Peer chat must not write `messages` rows — the exchange reaches model
    // context via the injection path, so a history row would double it.
    let history = storage.load_message_history(session_id).await.unwrap();
    assert!(
        history.is_empty(),
        "peer messages must not become message-history rows: {history:?}"
    );
}

fn sample_plan(title: &str) -> PlanDoc {
    let mut plan = PlanDoc::default();
    plan.edit().set_title(title);
    plan
}

#[tokio::test]
async fn latest_prior_session_is_project_scoped_and_excludes_current() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let other_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let old_id = storage
        .start_session(
            temp_dir.path(),
            "anthropic",
            "old",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(2)).await;
    let prior_id = storage
        .start_session(
            temp_dir.path(),
            "anthropic",
            "prior",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(2)).await;
    let other_id = storage
        .start_session(
            other_dir.path(),
            "anthropic",
            "other",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(2)).await;
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "anthropic",
            "current",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();

    let latest = storage
        .latest_prior_session_for_project(temp_dir.path(), current_id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(latest.id, prior_id);
    assert_ne!(latest.id, current_id);
    assert_ne!(latest.id, old_id);
    assert_ne!(latest.id, other_id);
}

#[tokio::test]
async fn transcript_snapshot_rehydrates_execution_group_tools() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let session_id = storage
        .start_session(
            temp_dir.path(),
            "anthropic",
            "claude-sonnet-4-5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let started_at = Instant::now();
    let transcript = vec![TranscriptItem::ExecutionGroup(ExecutionGroup {
        id: 7,
        finished_at: Some(started_at + Duration::from_millis(5)),
        tools: vec![ToolActivity {
            id: "call-1".to_string(),
            name: "bash".to_string(),
            arguments: r#"{"cmd":"cargo test"}"#.to_string(),
            status: ToolStatus::Succeeded,
            result: Some("ok".to_string()),
            diff: None,
            started_at,
            finished_at: Some(started_at + Duration::from_millis(5)),
        }],
    })];

    storage
        .replace_transcript_snapshot(session_id, &transcript)
        .await
        .unwrap();

    let snapshot = storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    match snapshot.transcript.as_slice() {
        [TranscriptItem::ExecutionGroup(group)] => {
            assert_eq!(group.id, 7);
            assert_eq!(group.tools.len(), 1);
            assert_eq!(group.tools[0].id, "call-1");
            assert_eq!(group.tools[0].arguments, r#"{"cmd":"cargo test"}"#);
            assert_eq!(group.tools[0].result.as_deref(), Some("ok"));
            assert_eq!(group.tools[0].status, ToolStatus::Succeeded);
        }
        other => panic!("expected execution group, got {other:?}"),
    }
}

#[tokio::test]
async fn self_review_tool_block_metadata_contains_structured_findings() {
    let fixture = TestStorage::new().await;
    let session_id = fixture.start_session().await;
    let started_at = Instant::now();
    let transcript = vec![TranscriptItem::ToolActivity(ToolActivity {
        id: "self-review-1".to_string(),
        name: "agent".to_string(),
        arguments: r#"{"agent":"self-review","prompt":"review"}"#.to_string(),
        status: ToolStatus::Succeeded,
        result: Some("Major: src/lib.rs:1 is wrong.\nNit: rename it.".to_string()),
        diff: None,
        started_at,
        finished_at: Some(started_at + Duration::from_millis(5)),
    })];

    fixture
        .storage
        .replace_transcript_snapshot(session_id, &transcript)
        .await
        .unwrap();
    let metadata: String = sqlx::query_scalar(
        "SELECT metadata_json FROM transcript_blocks WHERE session_id = ? AND kind = 'tool'",
    )
    .bind(session_id.as_i64())
    .fetch_one(&fixture.storage.pool)
    .await
    .unwrap();
    let metadata: serde_json::Value = serde_json::from_str(&metadata).unwrap();
    assert_eq!(metadata["self_review"]["finding_count"], 2);
    assert_eq!(metadata["self_review"]["findings"]["major"], 1);
    assert_eq!(metadata["self_review"]["findings"]["nit"], 1);
}

#[tokio::test]
async fn tool_call_snapshot_records_real_start_and_finish_timestamps() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let session_id = storage
        .start_session(
            temp_dir.path(),
            "anthropic",
            "claude-sonnet-4-5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let started_at = Instant::now();
    let transcript = vec![TranscriptItem::ExecutionGroup(ExecutionGroup {
        id: 1,
        finished_at: Some(started_at + Duration::from_millis(250)),
        tools: vec![ToolActivity {
            id: "call-1".to_string(),
            name: "bash".to_string(),
            arguments: r#"{"cmd":"cargo test"}"#.to_string(),
            status: ToolStatus::Succeeded,
            result: Some("ok".to_string()),
            diff: None,
            started_at,
            finished_at: Some(started_at + Duration::from_millis(250)),
        }],
    })];

    storage
        .replace_transcript_snapshot(session_id, &transcript)
        .await
        .unwrap();

    let row: (Option<i64>, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT started_at_ms, finished_at_ms, duration_ms FROM tool_calls \
         WHERE session_id = ? AND call_id = 'call-1'",
    )
    .bind(session_id.as_i64())
    .fetch_one(&storage.pool)
    .await
    .unwrap();
    let (started, finished, duration) = row;
    let started = started.expect("started_at_ms must be populated, not NULL");
    let finished = finished.expect("finished_at_ms must be populated for a finished tool");
    let duration = duration.expect("duration_ms must be recorded");
    assert_eq!(
        finished - started,
        duration,
        "the persisted start/finish must span exactly the real duration"
    );
    assert!(duration >= 250, "the real elapsed time must be preserved");
}

#[tokio::test]
async fn worklog_block_round_trips_and_writes_no_assistant_message() {
    let fixture = TestStorage::new().await;
    let storage = &fixture.storage;
    let session_id = fixture.start_session().await;

    storage
        .replace_transcript_snapshot(
            session_id,
            &[
                TranscriptItem::UserMessage {
                    text: "go".to_string(),
                },
                TranscriptItem::WorkLog {
                    text: "Need edit.".to_string(),
                },
            ],
        )
        .await
        .unwrap();

    // A downgraded scratch note reloads as a WorkLog, not an assistant reply.
    let snapshot = storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            snapshot.transcript.as_slice(),
            [
                TranscriptItem::UserMessage { .. },
                TranscriptItem::WorkLog { text },
            ] if text == "Need edit."
        ),
        "WorkLog should round-trip, got: {:?}",
        snapshot.transcript.as_slice()
    );

    // It must not be persisted as an assistant message in the `messages`
    // table, which feeds the resume-context fallback path.
    let history = storage.load_message_history(session_id).await.unwrap();
    assert!(
        history.iter().all(|(role, _)| role != "assistant"),
        "a WorkLog must not write an assistant messages row, got: {history:?}"
    );
}

#[tokio::test]
async fn serenity_preference_round_trips() {
    let fixture = TestStorage::new().await;
    // Missing key means off: existing installs keep the normal presentation.
    assert!(!fixture.storage.serenity_mode().await.unwrap());

    fixture.storage.set_serenity_mode(true).await.unwrap();
    assert!(fixture.storage.serenity_mode().await.unwrap());

    fixture.storage.set_serenity_mode(false).await.unwrap();
    assert!(!fixture.storage.serenity_mode().await.unwrap());
}

#[tokio::test]
async fn support_log_preference_round_trips_and_defaults_off() {
    let fixture = TestStorage::new().await;
    // Missing key means off: the lifecycle log is strictly opt-in.
    assert!(!fixture.storage.support_log_enabled().await.unwrap());

    fixture.storage.set_support_log_enabled(true).await.unwrap();
    assert!(fixture.storage.support_log_enabled().await.unwrap());

    fixture
        .storage
        .set_support_log_enabled(false)
        .await
        .unwrap();
    assert!(!fixture.storage.support_log_enabled().await.unwrap());
}

#[tokio::test]
async fn run_budget_is_unset_by_default_and_round_trips() {
    let fixture = TestStorage::new().await;
    assert_eq!(
        fixture.storage.run_budget().await.unwrap(),
        crate::run_budget::RunBudget::default()
    );

    let budget = crate::run_budget::RunBudget {
        max_turns: Some(50),
        max_run_seconds: Some(900),
        max_generation_seconds: Some(180),
        max_output_chars: Some(64_000),
        max_tool_seconds: Some(120),
        max_session_turns: Some(500),
        max_session_output_chars: Some(1_000_000),
        max_session_active_seconds: Some(7_200),
        max_session_cost_micros: Some(10_000_000),
    };
    fixture.storage.set_run_budget(budget).await.unwrap();

    assert_eq!(fixture.storage.run_budget().await.unwrap(), budget);
}

#[tokio::test]
async fn session_budget_exhaustion_reason_round_trips_and_clears() {
    let fixture = TestStorage::new().await;
    let session_id = fixture.start_session().await;
    let reason = crate::run_budget::RunBudgetExhaustion::MaxTurns { limit: 50 };

    fixture
        .storage
        .mark_session_termination(session_id, SessionStatus::Interrupted, Some(reason))
        .await
        .unwrap();
    let summary = fixture
        .storage
        .session_summary(session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(summary.status, SessionStatus::Interrupted);
    assert_eq!(summary.terminal_reason, Some(reason));

    fixture
        .storage
        .mark_session_status(session_id, SessionStatus::Completed)
        .await
        .unwrap();
    let summary = fixture
        .storage
        .session_summary(session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(summary.status, SessionStatus::Completed);
    assert_eq!(summary.terminal_reason, None);
}

#[tokio::test]
async fn credential_persistence_preference_round_trips() {
    let fixture = TestStorage::new().await;
    assert_eq!(
        fixture.storage.credential_persistence().await.unwrap(),
        None
    );

    fixture
        .storage
        .set_credential_persistence(crate::session::CredentialPersistence::Keyring)
        .await
        .unwrap();
    assert_eq!(
        fixture.storage.credential_persistence().await.unwrap(),
        Some(crate::session::CredentialPersistence::Keyring)
    );

    fixture
        .storage
        .set_credential_persistence(crate::session::CredentialPersistence::Session)
        .await
        .unwrap();
    assert_eq!(
        fixture.storage.credential_persistence().await.unwrap(),
        Some(crate::session::CredentialPersistence::Session)
    );
}

#[tokio::test]
async fn first_run_progress_starts_with_credential_choice_and_tracks_checkpoints() {
    let fixture = TestStorage::new().await;
    assert_eq!(
        fixture.storage.first_run_progress().await.unwrap(),
        crate::onboarding::FirstRunProgress::default()
    );

    fixture
        .storage
        .begin_first_run(crate::session::CredentialPersistence::File)
        .await
        .unwrap();
    let started = fixture.storage.first_run_progress().await.unwrap();
    assert!(started.started);
    assert!(!started.model_confirmed);
    assert_eq!(
        fixture.storage.credential_persistence().await.unwrap(),
        Some(crate::session::CredentialPersistence::File)
    );

    for checkpoint in [
        crate::onboarding::FirstRunCheckpoint::ModelConfirmed,
        crate::onboarding::FirstRunCheckpoint::SandboxReviewed,
        crate::onboarding::FirstRunCheckpoint::AutonomyReviewed,
        crate::onboarding::FirstRunCheckpoint::Completed,
    ] {
        fixture
            .storage
            .mark_first_run_checkpoint(checkpoint)
            .await
            .unwrap();
    }
    assert_eq!(
        fixture.storage.first_run_progress().await.unwrap(),
        crate::onboarding::FirstRunProgress {
            started: true,
            model_confirmed: true,
            sandbox_reviewed: true,
            autonomy_reviewed: true,
            completed: true,
        }
    );
}

#[tokio::test]
async fn smol_preference_defaults_off_migrates_auto_and_round_trips() {
    let fixture = TestStorage::new().await;

    assert_eq!(
        fixture.storage.smol_preference().await.unwrap(),
        crate::smol::SmolPreference::Off
    );

    sqlx::query("INSERT INTO user_preferences (key, value) VALUES ('smol', 'auto')")
        .execute(&fixture.storage.pool)
        .await
        .unwrap();
    assert_eq!(
        fixture.storage.smol_preference().await.unwrap(),
        crate::smol::SmolPreference::Off
    );

    fixture
        .storage
        .set_smol_preference(crate::smol::SmolPreference::On)
        .await
        .unwrap();
    assert_eq!(
        fixture.storage.smol_preference().await.unwrap(),
        crate::smol::SmolPreference::On
    );

    fixture
        .storage
        .set_smol_preference(crate::smol::SmolPreference::Off)
        .await
        .unwrap();
    assert_eq!(
        fixture.storage.smol_preference().await.unwrap(),
        crate::smol::SmolPreference::Off
    );
}

#[tokio::test]
async fn autonomy_and_sandbox_preferences_round_trip() {
    let fixture = TestStorage::new().await;
    assert_eq!(fixture.storage.approval_level().await.unwrap(), None);
    assert_eq!(fixture.storage.sandbox_enabled().await.unwrap(), None);
    assert_eq!(fixture.storage.sandbox_deny_network().await.unwrap(), None);

    fixture
        .storage
        .set_approval_level(crate::tool::ApprovalLevel::AutoAccept)
        .await
        .unwrap();
    fixture.storage.set_sandbox_enabled(false).await.unwrap();
    fixture
        .storage
        .set_sandbox_deny_network(true)
        .await
        .unwrap();

    assert_eq!(
        fixture.storage.approval_level().await.unwrap(),
        Some(crate::tool::ApprovalLevel::AutoAccept)
    );
    assert_eq!(
        fixture.storage.sandbox_enabled().await.unwrap(),
        Some(false)
    );
    assert_eq!(
        fixture.storage.sandbox_deny_network().await.unwrap(),
        Some(true)
    );
}
