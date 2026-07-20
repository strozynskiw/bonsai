use super::*;

#[tokio::test]
async fn mention_expansion_injects_file_and_marks_read() {
    let fixture = TestFixture::new();
    let file = fixture.create_file("src/main.rs", "fn main() {}\nprintln!(\"hi\");\n");

    let expanded = super::expand_mentions_for_context(
        &fixture.project_root,
        &fixture.read_tracker,
        "read @src/main.rs",
    )
    .await
    .expect("mention expansion should succeed");

    assert!(expanded.contains("# @-mention context"));
    assert!(expanded.contains("File: src/main.rs"));
    assert!(expanded.contains("1: fn main() {}"));
    assert!(expanded.contains("2: println!(\"hi\");"));
    assert!(
        fixture
            .read_tracker
            .is_read(&file.canonicalize().expect("canonical file"))
            .await
    );
}

#[tokio::test]
async fn mention_expansion_returns_read_evidence() {
    let fixture = TestFixture::new();
    let file = fixture.create_file("src/main.rs", "fn main() {}\n");
    let canonical = file.canonicalize().expect("canonical file");

    let expansion = super::expand_mentions_for_context_with_evidence(
        &fixture.project_root,
        &fixture.read_tracker,
        "read @src/main.rs",
    )
    .await
    .expect("mention expansion should succeed");

    assert!(expansion.text.contains("File: src/main.rs"));
    assert_eq!(expansion.read_evidence.len(), 1);
    let evidence = &expansion.read_evidence[0];
    assert_eq!(evidence.display_path(), "src/main.rs");
    assert_eq!(evidence.canonical_path(), canonical);
    assert_eq!(evidence.coverage(), ReadCoverage::Full);
    assert_eq!(evidence.window().start_line, 1);
    assert_eq!(evidence.window().end_line, Some(1));
    assert!(evidence.file_digest_at_read().is_some());
    assert!(fixture.read_tracker.was_fully_read(&canonical).await);
}

#[tokio::test]
async fn mention_expansion_truncates_at_utf8_boundary() {
    let fixture = TestFixture::new();
    let content = format!("{}é tail", "a".repeat(super::MENTION_FILE_CAP_BYTES - 1));
    fixture.create_file("src/unicode.txt", &content);

    let expanded = super::expand_mentions_for_context(
        &fixture.project_root,
        &fixture.read_tracker,
        "read @src/unicode.txt",
    )
    .await
    .expect("mention expansion should succeed");

    assert!(expanded.contains("File: src/unicode.txt"));
    assert!(expanded.contains("truncated"));
    assert!(!expanded.contains("skipped non-UTF-8"));
}

#[tokio::test]
async fn mention_expansion_warns_for_binary_image_hidden_duplicate_and_truncation() {
    let fixture = TestFixture::new();
    fixture.create_binary_file("bin.dat");
    fixture.create_image_file("image.png");
    fixture.create_file(".env", "SECRET=1\n");
    fixture.create_file("large.txt", &"a".repeat(super::MENTION_FILE_CAP_BYTES + 10));

    let expanded = super::expand_mentions_for_context(
        &fixture.project_root,
        &fixture.read_tracker,
        "read @bin.dat @image.png @.env @large.txt @large.txt",
    )
    .await
    .expect("mention expansion should succeed");

    assert!(expanded.contains("skipped binary file"));
    assert!(expanded.contains("skipped image file"));
    assert!(expanded.contains("hidden paths cannot be injected"));
    assert!(expanded.contains("duplicate mention skipped"));
    assert!(expanded.contains("large.txt truncated"));
}

#[tokio::test]
async fn mention_expansion_lists_directory_with_cap() {
    let fixture = TestFixture::new();
    for index in 0..(super::MENTION_DIRECTORY_ENTRY_CAP + 5) {
        fixture.create_file(&format!("dir/file-{index}.txt"), "x\n");
    }

    let expanded = super::expand_mentions_for_context(
        &fixture.project_root,
        &fixture.read_tracker,
        "list @dir",
    )
    .await
    .expect("mention expansion should succeed");

    assert!(expanded.contains("Directory: dir/"));
    assert!(expanded.contains("directory listing capped"));
}

#[tokio::test]
async fn mention_expansion_reports_invalid_brace_escape() {
    let fixture = TestFixture::new();

    let expanded = super::expand_mentions_for_context(
        &fixture.project_root,
        &fixture.read_tracker,
        "read @{src\\qmain.rs}",
    )
    .await
    .expect("mention expansion should succeed");

    assert!(expanded.contains("invalid escape '\\q'"));
}

#[tokio::test]
async fn mention_expansion_ignores_at_signs_inside_words() {
    let fixture = TestFixture::new();

    let expanded = super::expand_mentions_for_context(
        &fixture.project_root,
        &fixture.read_tracker,
        "mail person@example.com for details",
    )
    .await
    .expect("mention expansion should succeed");

    assert_eq!(expanded, "mail person@example.com for details");
}

#[tokio::test]
async fn mention_expansion_handles_unbraced_mentions_next_to_punctuation() {
    let fixture = TestFixture::new();
    fixture.create_file("src/main.rs", "fn main() {}\n");

    let expanded = super::expand_mentions_for_context(
        &fixture.project_root,
        &fixture.read_tracker,
        "check (@src/main.rs), please",
    )
    .await
    .expect("mention expansion should succeed");

    assert!(expanded.contains("File: src/main.rs"));
    assert!(!expanded.contains("path not found"));
}

#[tokio::test]
async fn restore_text_history_does_not_expand_mentions() {
    let fixture = TestFixture::new();
    let file = fixture.create_file("src/main.rs", "fn main() {}\n");
    let provider = Box::new(MockProvider::new(vec![]));
    let mut agent = Agent::new(
        provider,
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();

    agent
        .restore_text_history(&[("user".to_string(), "read @src/main.rs".to_string())])
        .await
        .expect("restore text history");

    assert_eq!(
        user_message_content(&agent.messages[1]),
        "read @src/main.rs"
    );
    assert!(
        !fixture
            .read_tracker
            .is_read(&file.canonicalize().expect("canonical file"))
            .await
    );
}

#[tokio::test]
async fn queued_messages_expand_mentions_when_sent() {
    let fixture = TestFixture::new();
    fixture.create_file("queued.txt", "queued context\n");
    let provider = MockProvider::new(vec![Ok(StreamedResponse {
        content: "done".to_string(),
        tool_calls: vec![],
        terminal: crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::Stop),
        usage: None,
        ..StreamedResponse::default()
    })]);
    let requests = provider.requests();
    let (sender, receiver) = mpsc::unbounded_channel();
    sender
        .send(QueuedUserMessageCommand::Send(QueuedUserMessage {
            id: 1,
            display_text: "also @queued.txt".to_string(),
            input: crate::agent::UserInput::from_text("also @queued.txt"),
        }))
        .expect("queued message should send before run starts");
    let mut agent = Agent::new(
        Box::new(provider),
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();

    agent
        .run_with_queue(
            crate::agent::UserInput::from_text("first"),
            CancellationToken::new(),
            Arc::new(StdoutSink),
            receiver,
        )
        .await
        .expect("run should complete");

    let requests = requests.lock().await;
    let user_messages = user_messages_in(&requests[0]);
    assert!(user_messages[1].contains("File: queued.txt"));
    assert!(user_messages[1].contains("1: queued context"));
}

#[tokio::test]
async fn live_run_expands_mentions_for_provider_context() {
    let fixture = TestFixture::new();
    fixture.create_file("live.txt", "live context\n");
    let provider = MockProvider::new(vec![Ok(StreamedResponse {
        content: "done".to_string(),
        tool_calls: vec![],
        terminal: crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::Stop),
        usage: None,
        ..StreamedResponse::default()
    })]);
    let requests = provider.requests();
    let mut agent = Agent::new(
        Box::new(provider),
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();

    agent
        .run(
            "read @live.txt",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .expect("run should complete");

    let requests = requests.lock().await;
    let user_messages = user_messages_in(&requests[0]);
    assert!(user_messages[0].contains("File: live.txt"));
    assert!(user_messages[0].contains("1: live context"));
}

#[tokio::test]
async fn live_mention_read_evidence_surfaces_stale_advisory_without_rewriting_history() {
    let fixture = TestFixture::new();
    let file = fixture.create_file("live.txt", "live context\n");
    let provider = MockProvider::new(vec![Ok(StreamedResponse {
        content: "done".to_string(),
        tool_calls: vec![],
        terminal: crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::Stop),
        usage: None,
        ..StreamedResponse::default()
    })])
    .with_append_only_project_state();
    let context = crate::context::ProjectContextSnapshot {
        environment: "## Environment\n- cwd: /x".to_string(),
        volatile_state: String::new(),
        steering_files: Vec::new(),
        repo_map: String::new(),
        skills_index: String::new(),
        smol_skills_index: String::new(),
        agents_index: String::new(),
        memory_index: String::new(),
        stale_read_advisory: String::new(),
        peer_status: String::new(),
    };
    let mut agent = Agent::builder(
        Box::new(provider),
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .project_context_snapshot(context)
    .build()
    .unwrap();

    agent
        .run(
            "read @live.txt",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .expect("run should complete");
    let stored_user_message = user_message_content(&agent.messages[1]);

    tokio::fs::write(&file, "changed context\n").await.unwrap();
    agent.refresh_read_evidence_freshness().await;
    agent.refresh_stale_read_advisory();

    let report = agent.context_report();
    assert!(
        find_context_node(&report.ledger, ContextNodeKind::ReadFreshness, "stale full").is_some(),
        "mention freshness child should be present"
    );
    assert!(agent.append_volatile_context_if_changed());
    let outgoing = agent.outgoing_messages_for(&agent.messages);
    let states = project_state_messages_in(&outgoing);
    assert!(
        states
            .last()
            .is_some_and(|state| state.contains("Files changed since you read them")),
        "stale mention reads should ride the append-only project-state advisory"
    );
    assert!(
        !message_content(&outgoing[0]).contains("Files changed since you read them"),
        "IMPORTANT cache invariant: stale-read state must not rewrite message zero"
    );
    assert_eq!(
        user_message_content(&agent.messages[1]),
        stored_user_message
    );
}

#[tokio::test]
async fn live_mention_read_evidence_rebaselines_after_agent_mutation() {
    let fixture = TestFixture::new();
    let file = fixture.create_file("live.txt", "live context\n");
    let provider = MockProvider::new(vec![Ok(StreamedResponse {
        content: "done".to_string(),
        tool_calls: vec![],
        terminal: crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::Stop),
        usage: None,
        ..StreamedResponse::default()
    })]);
    let context = crate::context::ProjectContextSnapshot {
        environment: "## Environment\n- cwd: /x".to_string(),
        volatile_state: String::new(),
        steering_files: Vec::new(),
        repo_map: String::new(),
        skills_index: String::new(),
        smol_skills_index: String::new(),
        agents_index: String::new(),
        memory_index: String::new(),
        stale_read_advisory: String::new(),
        peer_status: String::new(),
    };
    let mut agent = Agent::builder(
        Box::new(provider),
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .project_context_snapshot(context)
    .build()
    .unwrap();

    agent
        .run(
            "read @live.txt",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .expect("run should complete");

    tokio::fs::write(&file, "agent changed this\n")
        .await
        .unwrap();
    agent
        .rebaseline_read_evidence_for_mutation(&test_tool_call(
            "write-1",
            "write",
            r#"{"path":"live.txt","content":"agent changed this\n"}"#,
        ))
        .await;
    agent.refresh_read_evidence_freshness().await;
    agent.refresh_stale_read_advisory();

    let report = agent.context_report();
    assert!(
        find_context_node(&report.ledger, ContextNodeKind::ReadFreshness, "fresh full").is_some(),
        "mention freshness should be rebaselined to the agent-authored content"
    );
    let outgoing = agent.outgoing_messages_for(&agent.messages);
    let system = serde_json::to_value(&outgoing[0]).unwrap();
    assert!(
        !system["content"]
            .as_str()
            .unwrap()
            .contains("Files changed since you read them"),
        "agent-authored writes should not leave a stale mention advisory"
    );
}
