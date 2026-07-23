use super::*;
use crate::tool::digest_content;
use crate::tool::read_evidence::{
    InspectionEventRecord, InspectionOutcome, InspectionReason, ReadAdmissionMetadata,
    ReadEvidenceRecord, ReadFreshness, ReadProvenance,
};

#[tokio::test]
async fn child_manifest_exports_live_reads_without_file_bodies() {
    let fixture = TestFixture::new();
    let path = fixture.project_root.join("sample.rs");
    tokio::fs::write(&path, "alpha\n").await.unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(crate::tool::ReadTool::new(
        fixture.project_root.clone(),
        fixture.read_tracker.clone(),
    )));
    let provider = MockProvider::new(vec![
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![test_tool_call(
                "call-read",
                "read",
                r#"{"path":"sample.rs"}"#,
            )],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: "Finding at sample.rs:1".to_string(),
            tool_calls: Vec::new(),
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
    ]);
    let mut child = Agent::new(
        Box::new(provider),
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker,
        String::new(),
        fixture.project_root,
    )
    .unwrap();

    child
        .run(
            "inspect sample.rs",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .unwrap();
    let manifest = child.delegated_read_evidence_manifest("sub-3", "Finding at sample.rs:1");

    assert_eq!(manifest.len(), 1);
    assert_eq!(manifest[0].subtask_id, "sub-3");
    assert_eq!(manifest[0].source_id, "tool:call-read");
    assert!(manifest[0].cited_in_result);
    assert_eq!(
        manifest[0].evidence.observation().coverage(),
        ReadCoverage::Full
    );
    assert_eq!(manifest[0].evidence.observation().visible_chars(), 9);
}

#[tokio::test]
async fn resume_restores_only_unchanged_full_parent_reads_to_write_guard() {
    let fixture = TestFixture::new();
    let path = fixture.project_root.join("sample.rs");
    let original = b"alpha\n";
    tokio::fs::write(&path, original).await.unwrap();
    let canonical_path = tokio::fs::canonicalize(&path).await.unwrap();
    let metadata = tokio::fs::metadata(&canonical_path).await.unwrap();
    let rendered = "1: alpha\n";
    let evidence = ReadEvidence::new(
        "sample.rs",
        canonical_path.clone(),
        ReadWindow {
            requested_offset: 1,
            requested_limit: 100,
            start_line: 1,
            end_line: Some(1),
            total_lines: Some(1),
        },
        ReadCoverage::Full,
        rendered,
        metadata.modified().ok(),
        metadata.len(),
        Some(digest_content(original)),
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
    let messages = vec![
        system_message(AgentMode::Coding, ""),
        assistant_tool_call_message("call-read", "read", r#"{"path":"sample.rs"}"#),
        tool_result_message("call-read", rendered),
    ];
    let ids = vec![
        "msg-0".to_string(),
        "msg-1".to_string(),
        "msg-2".to_string(),
    ];
    let mut agent = Agent::builder(
        MockProvider::empty(),
        mock_registry(&["read"]),
        mock_registry(&["plan_read"]),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .system_context("")
    .build()
    .unwrap();

    agent
        .restore_context_messages_with_ids(messages.clone(), ids.clone())
        .await
        .unwrap();
    agent.restore_read_evidence(vec![record.clone()]).await;

    assert!(fixture.read_tracker.was_fully_read(&canonical_path).await);
    assert!(
        fixture
            .read_tracker
            .is_unchanged_since_read(&canonical_path)
            .await
    );
    assert_eq!(
        agent
            .tool_context_details
            .get("call-read")
            .and_then(|detail| detail.read_evidence.as_ref())
            .map(ReadEvidence::freshness),
        Some(ReadFreshness::Fresh)
    );

    tokio::fs::write(&path, b"bravo\n").await.unwrap();
    agent
        .restore_context_messages_with_ids(messages, ids)
        .await
        .unwrap();
    agent.restore_read_evidence(vec![record.clone()]).await;

    assert!(!fixture.read_tracker.is_read(&canonical_path).await);
    assert_eq!(
        agent
            .tool_context_details
            .get("call-read")
            .and_then(|detail| detail.read_evidence.as_ref())
            .map(ReadEvidence::freshness),
        Some(ReadFreshness::Stale)
    );

    tokio::fs::write(&path, original).await.unwrap();
    let mismatched_messages = vec![
        system_message(AgentMode::Coding, ""),
        assistant_tool_call_message("call-read", "read", r#"{"path":"sample.rs"}"#),
        tool_result_message("call-read", "1: substituted\n"),
    ];
    agent
        .restore_context_messages_with_ids(
            mismatched_messages,
            vec![
                "msg-0".to_string(),
                "msg-1".to_string(),
                "msg-2".to_string(),
            ],
        )
        .await
        .unwrap();
    agent.restore_read_evidence(vec![record]).await;

    assert!(!fixture.read_tracker.is_read(&canonical_path).await);
    assert!(!agent.tool_context_details.contains_key("call-read"));
}

#[tokio::test]
async fn resume_restores_typed_reuse_target_without_parsing_pointer_text() {
    let fixture = TestFixture::new();
    let path = fixture.create_file("sample.rs", "alpha\n");
    let canonical_path = path.canonicalize().unwrap();
    let metadata = std::fs::metadata(&canonical_path).unwrap();
    let rendered = "1: alpha\n";
    let pointer = format!(
        "{} sample.rs lines 1-1 — unchanged since tool call-read-1; use that retained output.",
        crate::agent::REUSED_READ_MARKER
    );
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
        metadata.modified().ok(),
        metadata.len(),
        Some(digest_content(b"alpha\n")),
    );
    let evidence_record = ReadEvidenceRecord {
        source_id: "tool:call-read-1".to_string(),
        provenance: ReadProvenance::ParentVisible,
        target_message_id: "msg-2".to_string(),
        target_content_digest: digest_content(rendered.as_bytes()),
        target_tool_call_id: Some("call-read-1".to_string()),
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
    let inspection_record = InspectionEventRecord {
        call_id: "call-read-2".to_string(),
        target_message_id: "msg-4".to_string(),
        target_content_digest: digest_content(pointer.as_bytes()),
        tool_name: "read".to_string(),
        tool_arguments: r#"{"path":"sample.rs"}"#.to_string(),
        target_live: true,
        target_stubbed: false,
        admission: ReadAdmissionMetadata {
            outcome: InspectionOutcome::Reused,
            reason: InspectionReason::FreshVisibleCoverage,
            reuse_target_tool_call_id: Some("call-read-1".to_string()),
            requested_chars: rendered.chars().count(),
            returned_chars: pointer.chars().count(),
            avoided_chars: rendered
                .chars()
                .count()
                .saturating_sub(pointer.chars().count()),
        },
    };
    let messages = vec![
        system_message(AgentMode::Coding, ""),
        assistant_tool_call_message("call-read-1", "read", r#"{"path":"sample.rs"}"#),
        tool_result_message("call-read-1", rendered),
        assistant_tool_call_message("call-read-2", "read", r#"{"path":"sample.rs"}"#),
        tool_result_message("call-read-2", &pointer),
    ];
    let mut agent = Agent::builder(
        MockProvider::empty(),
        mock_registry(&["read"]),
        mock_registry(&["plan_read"]),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .system_context("")
    .build()
    .unwrap();

    agent
        .restore_context_messages_with_ids(
            messages,
            (0..=4).map(|index| format!("msg-{index}")).collect(),
        )
        .await
        .unwrap();
    agent.restore_read_evidence(vec![evidence_record]).await;
    agent.restore_inspection_events(vec![inspection_record]);

    let restored = agent.tool_context_details.get("call-read-2").unwrap();
    assert_eq!(
        restored.reuse_target_call_id.as_deref(),
        Some("call-read-1")
    );
    assert!(restored.read_evidence.is_none());
    assert_eq!(
        agent.read_evidence.inspection_events["call-read-2"].outcome,
        InspectionOutcome::Reused
    );
}
