use super::*;
use crate::eval::stream_fixture::*;
use crate::provider::*;
use serde_json::json;

pub(super) struct DeterministicProvider {
    preparation: crate::provider::opencode::PortableChatPreparation,
    script: MockScript,
    calls: AtomicUsize,
}
impl DeterministicProvider {
    pub fn new(
        preparation: crate::provider::opencode::PortableChatPreparation,
        script: MockScript,
        _seed: u64,
    ) -> Self {
        Self {
            preparation,
            script,
            calls: AtomicUsize::new(0),
        }
    }
}
#[async_trait]
impl Provider for DeterministicProvider {
    fn prepare_request_for_estimation(
        &self,
        m: &[ChatCompletionRequestMessage],
        t: &[ChatCompletionTool],
        o: ProviderRequestOptions,
        c: &ProviderRequestContext,
    ) -> ProviderResult<PreparedProviderRequest> {
        self.preparation.prepare(m, t, o, c)
    }
    fn set_tool_call_identity_store(&mut self, store: ToolCallIdentityStore) {
        self.preparation.tool_call_identities = store;
    }
    fn set_conversation_cache_key(&mut self, key: &str) {
        self.preparation.prompt_cache_key = key.into();
    }
    fn project_state_cache_strategy(&self) -> ProjectStateCacheStrategy {
        self.preparation.cache_strategy()
    }
    fn reasoning(&self) -> ReasoningSelection {
        ReasoningSelection::Off
    }
    async fn chat_stream(
        &self,
        _m: &[ChatCompletionRequestMessage],
        _t: &[ChatCompletionTool],
        token: CancellationToken,
        sink: SharedSink,
    ) -> ProviderResult<StreamedResponse> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.script.wait_for_cancellation {
            sink.assistant_delta("prefix");
            token.cancelled().await;
            return Ok(StreamedResponse {
                content: "prefix".into(),
                terminal: StreamTerminal::Interrupted,
                ..Default::default()
            });
        }
        if index < self.script.transient_errors {
            sink.assistant_delta("discard me");
            return Err(ProviderFailure::http(503, "controlled outage", Some(0)));
        }
        let index = index - self.script.transient_errors;
        let mut calls = match index {
            0 => vec![ToolCall {
                id: "wire-read".into(),
                name: "read".into(),
                arguments: json!({"path":"README.md"}).to_string(),
            }],
            1 => vec![ToolCall {
                id: "wire-write".into(),
                name: "write".into(),
                arguments: json!({"path":"README.md","content":self.script.write.first().map(|write| write.content.as_str()).unwrap_or("new\n")}).to_string(),
            }],
            _ => Vec::new(),
        };
        self.preparation
            .tool_call_identities
            .assign_calls(&self.preparation.tool_call_scope, &mut calls);
        let content = if calls.is_empty() { "Done." } else { "" };
        sink.assistant_delta(content);
        Ok(StreamedResponse {
            content: content.into(),
            terminal: StreamTerminal::Completed(if calls.is_empty() {
                FinishReason::Stop
            } else {
                FinishReason::ToolCalls
            }),
            tool_calls: calls,
            usage: Some(TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 10,
                input_cache: None,
            }),
            reasoning_chars: 0,
        })
    }
    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

fn header() -> StreamHeader {
    StreamHeader {version:1,suite:"roundtrip".into(),task:"case".into(),seed:7,provider:"fixture-provider".into(),model:"fixture-model".into(),context_tokens:128000,platform:"fixture-os (fixture-arch)".into(),
        preparation:serde_json::from_value(json!({"model":"fixture-model","output_limit":1024,"reasoning_codec":"openai-compatible","prompt_cache_policy":"transport-default","supports_prompt_cache":false,"supports_vision":false,"echoes_reasoning_content":false,"prompt_cache_header":null,"tool_call_scope":"fixture-scope","native_replay_scope":"fixture-native"})).unwrap()}
}

async fn round_trip(errors: usize, cancel: bool) {
    round_trip_content(errors, cancel, "new\n").await;
}

async fn round_trip_content(errors: usize, cancel: bool, content: &str) {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("fixture")).unwrap();
    fs::write(temp.path().join("fixture/README.md"), "old\n").unwrap();
    let suite_path = temp.path().join("suite.toml");
    fs::write(
        &suite_path,
        format!(
            r#"id = "roundtrip"
seed = 7
[[tasks]]
id = "case"
fixture = "fixture"
prompt = "Update README.md to new and finish."
expected_status = "{}"
{}
[tasks.budgets]
max_duration_ms = 10000
[tasks.mock]
transient_errors = {}
wait_for_cancellation = {}
[[tasks.graders]]
type = "assertion"
contains = ["{}"]
"#,
            if cancel { "interrupted" } else { "completed" },
            if cancel {
                "cancel_after_provider_attempts = 1"
            } else {
                ""
            },
            errors,
            cancel,
            if cancel { "prefix" } else { "Done." }
        ),
    )
    .unwrap();
    let mut suite = EvalSuite::load(&suite_path).unwrap();
    suite.tasks[0].mock.as_mut().unwrap().write = vec![MockWrite {
        path: "README.md".into(),
        content: content.into(),
    }];
    let directory = temp.path().join("streams");
    let capture_dir = temp.path().join("capture");
    let storage = Storage::open_at(capture_dir.join("bonsai.db"))
        .await
        .unwrap();
    let capture = run_task(
        &suite,
        &suite.tasks[0],
        "case",
        7,
        &capture_dir,
        &storage,
        (
            &ProviderSetup::CaptureTest {
                header: Box::new(header()),
            },
            Some(directory.as_path()),
        ),
    )
    .await
    .unwrap();
    assert!(capture.passed, "{capture:?}");
    if std::env::var_os("BONSAI_GENERATE_TEST_STREAMS").is_some() && content == "new\n" {
        let name = if cancel {
            "cancellation"
        } else if errors > 0 {
            "retry"
        } else {
            "tools"
        };
        let destination =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("eval/fixtures/provider_streams");
        let destination = destination.join(name);
        fs::create_dir_all(&destination).unwrap();
        fs::copy(
            stream_path(&directory, &suite.id, "case"),
            stream_path(&destination, &suite.id, "case"),
        )
        .unwrap();
    }
    let (header, attempts) =
        crate::eval::stream_replay::load(&stream_path(&directory, &suite.id, "case"))
            .await
            .unwrap();
    assert_eq!(attempts.len(), if cancel { 1 } else { 3 + errors });
    let replay_dir = temp.path().join("different-root");
    let storage = Storage::open_at(replay_dir.join("bonsai.db"))
        .await
        .unwrap();
    let replay = run_task(
        &suite,
        &suite.tasks[0],
        "case",
        7,
        &replay_dir,
        &storage,
        (
            &ProviderSetup::Replay {
                directory: directory.clone(),
                header: Box::new(header),
            },
            None,
        ),
    )
    .await
    .unwrap();
    assert!(replay.passed, "{replay:?}");
    assert_eq!(capture.output, replay.output);
    assert_eq!(capture.status, replay.status);
    assert_eq!(capture.usage, replay.usage);
    assert_eq!(
        capture.budget.metrics.provider_attempts,
        replay.budget.metrics.provider_attempts
    );
    if !cancel {
        assert_eq!(
            fs::read_to_string(replay_dir.join("worktrees/case/README.md")).unwrap(),
            content
        );
        assert_eq!(
            fs::read(capture_dir.join("worktrees/case/README.md")).unwrap(),
            content.as_bytes()
        );
    }
    let (header, mut attempts) =
        crate::eval::stream_replay::load(&stream_path(&directory, &suite.id, "case"))
            .await
            .unwrap();
    let mut unused = attempts.last().unwrap().clone();
    unused.index = attempts.len();
    attempts.push(unused);
    let mut records = vec![StreamRecord::Header(header.clone())];
    let count = attempts.len();
    records.extend(attempts.into_iter().map(StreamRecord::Attempt));
    records.push(StreamRecord::Complete { attempts: count });
    write_records(&stream_path(&directory, &suite.id, "case"), &records).await;
    let mut expected_error = suite.tasks[0].clone();
    expected_error.expected_status = TaskStatus::Error;
    let root = temp.path().join("unused-root");
    let storage = Storage::open_at(root.join("bonsai.db")).await.unwrap();
    let unused = run_task(
        &suite,
        &expected_error,
        "case",
        7,
        &root,
        &storage,
        (
            &ProviderSetup::Replay {
                directory,
                header: Box::new(header),
            },
            None,
        ),
    )
    .await
    .unwrap();
    assert!(!unused.passed);
    assert!(unused.run_error.unwrap().contains("unused"));
}

#[tokio::test]
async fn stream_round_trip_tools_fresh_ids_and_roots() {
    round_trip(0, false).await;
}
#[tokio::test]
async fn stream_round_trip_json_source_bytes() {
    round_trip_content(0, false, "{\"b\": 2, \"a\": 1}\n").await;
}

#[test]
fn stream_json_shaped_source_changes_remain_meaningful() {
    let make_request = |source: &str| {
        let messages = vec![ChatCompletionRequestMessage::User(
            async_openai::types::chat::ChatCompletionRequestUserMessageArgs::default()
                .content(source)
                .build()
                .unwrap(),
        )];
        Normalizer::default()
            .request(
                &messages,
                &[],
                ProviderRequestOptions::default(),
                &ProviderRequestContext::default(),
                &Default::default(),
                json!({}),
            )
            .unwrap()
    };
    let source = "{\"b\": 2, \"a\": 1}\n";
    let original = make_request(source);
    assert_eq!(original.messages[0]["content"], source);
    let changed = make_request("{\"a\":1,\"b\":2}");
    assert!(
        first_difference(
            &serde_json::to_value(original).unwrap(),
            &serde_json::to_value(changed).unwrap(),
            "$"
        )
        .is_some()
    );
}

#[tokio::test]
async fn stream_round_trip_partial_failure_retry() {
    round_trip(1, false).await;
}
#[tokio::test]
async fn stream_round_trip_attempt_cancellation() {
    round_trip(0, true).await;
}

#[tokio::test]
async fn stream_checked_in_replay_suites_are_keyless() {
    let temp = tempfile::tempdir().unwrap();
    for name in ["tools", "retry", "cancellation"] {
        let outcome = run(EvalCliConfig {
            suite: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("eval/suites/provider_stream_{name}.toml")),
            mode: EvalMode::Replay,
            replay_dir: Some(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join(format!("eval/fixtures/provider_streams/{name}")),
            ),
            out_dir: temp.path().join(name),
            fail_on_task_failure: true,
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(
            !outcome.should_fail_process(),
            "{name}: {}",
            fs::read_to_string(outcome.report_path).unwrap()
        );
    }
}

#[tokio::test]
async fn stream_prepared_counts_and_request_contract_are_portable() {
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("short");
    let b = temp.path().join("longer-root-for-replay");
    let output_a = temp.path().join("out-a");
    let output_b = temp.path().join("longer-output-b");
    let header = header();
    let handle = crate::eval::stream_record::create_capture(
        &temp.path().join("capture.jsonl"),
        header.clone(),
        Normalizer::new(&a, &output_a).unwrap(),
    )
    .await
    .unwrap();
    let provider = DeterministicProvider::new(
        header.preparation.clone(),
        toml::from_str("final_response='Done.'").unwrap(),
        7,
    );
    let capture = crate::eval::stream_record::RecordingProvider::new(Box::new(provider), handle);
    let replay = crate::eval::stream_replay::ReplayProvider::with_state(
        &header,
        Normalizer::new(&b, &output_b).unwrap(),
        crate::eval::stream_replay::ReplayState::new(Vec::new()),
    );
    let message = |path: &Path| {
        vec![ChatCompletionRequestMessage::User(
            async_openai::types::chat::ChatCompletionRequestUserMessageArgs::default()
                .content(path.display().to_string())
                .build()
                .unwrap(),
        )]
    };
    let options = ProviderRequestOptions::default();
    let context = ProviderRequestContext::default();
    let prepared_a = capture
        .prepare_request_for_estimation(&message(&a), &[], options, &context)
        .unwrap();
    let prepared_b = replay
        .prepare_request_for_estimation(&message(&b), &[], options, &context)
        .unwrap();
    assert_eq!(prepared_a.local_count_body(), prepared_b.local_count_body());
    let mut normalizer = Normalizer::new(&a, &output_a).unwrap();
    let request = normalizer
        .request(
            &message(&a),
            &[],
            options,
            &context,
            &Default::default(),
            prepared_a.local_count_body(),
        )
        .unwrap();
    for field in ["messages", "tools", "options", "context", "local_count"] {
        let mut changed = serde_json::to_value(&request).unwrap();
        changed[field] = json!(["semantic change"]);
        assert!(
            first_difference(&serde_json::to_value(&request).unwrap(), &changed, "$").is_some()
        );
    }
}

fn logical_request() -> LogicalRequest {
    LogicalRequest {
        messages: json!([]),
        tools: json!([]),
        options: json!({"reasoning":null}),
        context: json!({"assistant_response_ids":null,"response_id":null}),
        identities: json!([]),
        local_count: json!({"messages":[]}),
    }
}
fn attempt() -> StreamAttempt {
    StreamAttempt {
        index: 0,
        request: logical_request(),
        events: vec![StreamEvent::Assistant("prefix".into())],
        outcome: RecordedOutcome::Response {
            content: "prefix".into(),
            tool_calls: Vec::new(),
            terminal: StreamTerminal::Interrupted,
            usage: None,
            reasoning_chars: 0,
        },
    }
}
async fn write_records(path: &Path, records: &[StreamRecord]) {
    let bytes = records
        .iter()
        .map(|record| serde_json::to_string(record).unwrap() + "\n")
        .collect::<String>();
    tokio::fs::write(path, bytes).await.unwrap();
}

#[tokio::test]
async fn stream_rejects_versions_incomplete_malformed_order_and_limits() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fixture.jsonl");
    for records in [
        vec![StreamRecord::Header(header())],
        vec![
            StreamRecord::Header(StreamHeader {
                version: 99,
                ..header()
            }),
            StreamRecord::Complete { attempts: 0 },
        ],
        vec![
            StreamRecord::Header(header()),
            StreamRecord::Complete { attempts: 1 },
        ],
        vec![
            StreamRecord::Header(header()),
            StreamRecord::Complete { attempts: 0 },
            StreamRecord::Attempt(attempt()),
        ],
        vec![
            StreamRecord::Header(header()),
            StreamRecord::Attempt(StreamAttempt {
                index: 2,
                ..attempt()
            }),
            StreamRecord::Complete { attempts: 1 },
        ],
    ] {
        write_records(&path, &records).await;
        assert!(crate::eval::stream_replay::load(&path).await.is_err());
    }
    tokio::fs::write(&path, "not json\n").await.unwrap();
    assert!(crate::eval::stream_replay::load(&path).await.is_err());
    let file = tokio::fs::File::create(&path).await.unwrap();
    file.set_len(MAX_FIXTURE_BYTES as u64 + 1).await.unwrap();
    assert!(crate::eval::stream_replay::load(&path).await.is_err());
}

#[derive(Default)]
struct EventSink {
    events: std::sync::Mutex<Vec<String>>,
}
impl OutputSink for EventSink {
    fn assistant_delta(&self, text: &str) {
        self.events.lock().unwrap().push(text.into());
    }
}

#[tokio::test]
async fn stream_mismatches_and_extra_calls_emit_nothing_and_poison_teardown() {
    use crate::eval::stream_replay::{ReplayProvider, ReplayState};
    for change in ["prompt", "schema", "options", "context", "extra"] {
        let mut expected = attempt();
        expected.request = logical_request();
        match change {
            "prompt" => expected.request.messages = json!([{"role":"user","content":"different"}]),
            "schema" => expected.request.tools = json!([{"type":"function"}]),
            "options" => expected.request.options = json!({"reasoning":"off"}),
            "context" => {
                expected.request.context = json!({"assistant_response_ids":[],"response_id":null})
            }
            _ => {}
        }
        let state = ReplayState::new(if change == "extra" {
            Vec::new()
        } else {
            vec![expected]
        });
        let provider = ReplayProvider::with_state(&header(), Normalizer::default(), state.clone());
        let sink = Arc::new(EventSink::default());
        assert!(
            provider
                .chat_stream(&[], &[], CancellationToken::new(), sink.clone())
                .await
                .is_err(),
            "{change}"
        );
        assert!(sink.events.lock().unwrap().is_empty());
        assert!(state.lock().unwrap().finish().is_err());
    }
    assert!(
        ReplayState::new(vec![attempt()])
            .lock()
            .unwrap()
            .finish()
            .unwrap_err()
            .to_string()
            .contains("unused")
    );
}

#[tokio::test]
async fn stream_capture_refuses_overwrite_and_write_failure() {
    use crate::eval::stream_record::{create_capture, finish_capture};
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fixture.jsonl");
    let handle = create_capture(&path, header(), Normalizer::default())
        .await
        .unwrap();
    assert!(
        create_capture(&path, header(), Normalizer::default())
            .await
            .is_err()
    );
    assert!(crate::eval::stream_replay::load(&path).await.is_err());
    fs::remove_file(&path).unwrap();
    assert!(finish_capture(&handle).await.is_err());
}

#[test]
fn stream_json_rebasing_preserves_unusual_roots() {
    let workspace = Path::new("/tmp/root-with-\"quote\\slash\n");
    let normalizer = Normalizer::new(workspace, Path::new("/tmp/output")).unwrap();
    let arguments = normalizer.rebase_arguments(
        r#"{"path":"@EVAL_WORKSPACE@/file","nested":[{"path":"@EVAL_WORKSPACE@/other"}]}"#,
    );
    let value: serde_json::Value = serde_json::from_str(&arguments).unwrap();
    assert_eq!(value["path"], format!("{}/file", workspace.display()));
    assert_eq!(
        value["nested"][0]["path"],
        format!("{}/other", workspace.display())
    );
}

#[tokio::test]
async fn stream_input_is_bounded_and_regular_including_header() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fixture.jsonl");
    fs::write(&path, vec![b' '; MAX_ATTEMPT_BYTES + 1]).unwrap();
    assert!(
        crate::eval::stream_replay::load(&path)
            .await
            .unwrap_err()
            .to_string()
            .contains("header exceeds")
    );
    #[cfg(unix)]
    assert!(
        crate::eval::stream_replay::load(Path::new("/dev/zero"))
            .await
            .unwrap_err()
            .to_string()
            .contains("regular file")
    );
}

#[tokio::test]
async fn stream_identity_failure_remains_invalid_after_consumption() {
    use crate::eval::stream_replay::{ReplayProvider, ReplayState};
    let header = header();
    let mut normalizer = Normalizer::default();
    let options = ProviderRequestOptions::default();
    let context = ProviderRequestContext::default();
    let prepared = header
        .preparation
        .prepare(&[], &[], options, &context)
        .unwrap();
    let request = normalizer
        .request(
            &[],
            &[],
            options,
            &context,
            &Default::default(),
            prepared.local_count_body(),
        )
        .unwrap();
    let state = ReplayState::new(vec![StreamAttempt {
        index: 0,
        request,
        events: Vec::new(),
        outcome: RecordedOutcome::Response {
            content: String::new(),
            tool_calls: vec![RecordedToolCall {
                id: "@CALL_99@".into(),
                name: "read".into(),
                arguments: "{}".into(),
                wire_id: "wire".into(),
            }],
            terminal: StreamTerminal::Completed(FinishReason::ToolCalls),
            usage: None,
            reasoning_chars: 0,
        },
    }]);
    let provider = ReplayProvider::with_state(&header, Normalizer::default(), state.clone());
    assert!(
        provider
            .chat_stream(
                &[],
                &[],
                CancellationToken::new(),
                Arc::new(EventSink::default())
            )
            .await
            .is_err()
    );
    assert!(state.lock().unwrap().finish().is_err());
}
