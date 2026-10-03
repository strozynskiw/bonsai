use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::Deserialize;

use super::*;
use crate::tool::read_evidence::DelegatedReadEvidence;
use crate::tool::{EditTool, ReadRegionTool, ReadSymbolTool, ReadTool};

#[derive(Debug, Deserialize)]
struct ReplayFixture {
    session: SessionBaseline,
    baseline: ReplayBaseline,
    reads: Vec<ReadEvent>,
    mutations: Vec<MutationEvent>,
    git_inspections: Vec<GitInspection>,
    subagents: Vec<SubagentEvent>,
    usage_turns: Vec<UsageTurn>,
    context_role_stats: Vec<ContextRoleStat>,
    background_context_events: Vec<BackgroundContextEvent>,
    #[serde(default)]
    delegated_scopes: Vec<DelegatedScope>,
    #[serde(default)]
    guard_replays: Vec<GuardReplay>,
}

#[derive(Debug, Deserialize)]
struct GuardReplay {
    scenario: String,
    tool_finish_reason: String,
    empty_stop_after_rejection: bool,
    guard_reason: String,
    recovery_action: String,
    progress_followed: bool,
}

#[derive(Debug, Deserialize)]
struct SessionBaseline {
    provider_id: String,
    model: String,
    prompt_tokens: u64,
    completion_tokens: u64,
    cache_read_tokens: u64,
    cache_measured_tokens: u64,
    cost_micros: i64,
}

#[derive(Debug, Deserialize)]
struct ReplayBaseline {
    reads: ReadBaseline,
    usage: UsageBaseline,
}

#[derive(Debug, Deserialize)]
struct ReadBaseline {
    attempts: usize,
    succeeded: usize,
    distinct_paths: usize,
    result_chars: usize,
    reuse_marked: usize,
    reuse_marked_result_chars: usize,
}

#[derive(Debug, Deserialize)]
struct UsageBaseline {
    turns: usize,
    zero_cache_turns: usize,
    gc_rewrite_turns: usize,
}

#[derive(Debug, Deserialize)]
struct ReadEvent {
    seq: u64,
    path: String,
    offset: Option<usize>,
    requested_limit: Option<usize>,
    status: String,
    result_chars: usize,
    reuse_marked: u8,
}

#[derive(Debug, Deserialize)]
struct MutationEvent {
    seq: u64,
    path: String,
    status: String,
}

#[derive(Debug, Deserialize)]
struct GitInspection {
    seq: u64,
    op: String,
    target: Option<String>,
    path: Option<String>,
    stat_only: u8,
    status: String,
    result_chars: usize,
    result_digest: String,
}

#[derive(Debug, Deserialize)]
struct SubagentEvent {
    seq: u64,
    agent: String,
    detached: u8,
    status: String,
}

#[derive(Debug, Deserialize)]
struct UsageTurn {
    seq: u64,
    prompt_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_measured_tokens: Option<u64>,
    rewrite_kind: String,
    prefix_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ContextRoleStat {
    role: String,
    messages: usize,
    chars: usize,
}

#[derive(Debug, Deserialize)]
struct BackgroundContextEvent {
    seq: u64,
    chars: usize,
    event_kind: String,
}

#[derive(Debug, Deserialize)]
struct DelegatedScope {
    seq: u64,
    paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RequestedRange {
    start: usize,
    end: usize,
}

impl RequestedRange {
    fn from_read(read: &ReadEvent) -> Self {
        let start = read.offset.unwrap_or(1);
        let end = read
            .requested_limit
            .map(|limit| start.saturating_add(limit.saturating_sub(1)))
            .unwrap_or(usize::MAX);
        Self { start, end }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ReadReplayStats {
    covered_requests: usize,
    exact_shape_repeat_groups: usize,
    redundant_result_chars: usize,
    projected_avoided_chars: usize,
}

fn fixture_opencode() -> ReplayFixture {
    serde_json::from_str(include_str!(
        "fixtures/read_efficiency_replay_opencode.json"
    ))
    .expect("opencode replay fixture should be valid")
}

fn fixture_codex() -> ReplayFixture {
    serde_json::from_str(include_str!("fixtures/read_efficiency_replay_codex.json"))
        .expect("codex replay fixture should be valid")
}

fn verify_baseline(fixture: &ReplayFixture) {
    let reads = &fixture.baseline.reads;
    assert_eq!(fixture.reads.len(), reads.attempts);
    assert_eq!(
        fixture
            .reads
            .iter()
            .filter(|read| read.status == "succeeded")
            .count(),
        reads.succeeded
    );
    assert_eq!(
        fixture
            .reads
            .iter()
            .map(|read| read.path.as_str())
            .collect::<HashSet<_>>()
            .len(),
        reads.distinct_paths
    );
    assert_eq!(
        fixture
            .reads
            .iter()
            .map(|read| read.result_chars)
            .sum::<usize>(),
        reads.result_chars
    );
    assert_eq!(
        fixture
            .reads
            .iter()
            .filter(|read| read.reuse_marked == 1)
            .count(),
        reads.reuse_marked
    );
    assert_eq!(
        fixture
            .reads
            .iter()
            .filter(|read| read.reuse_marked == 1)
            .map(|read| read.result_chars)
            .sum::<usize>(),
        reads.reuse_marked_result_chars
    );

    let usage = &fixture.baseline.usage;
    assert_eq!(fixture.usage_turns.len(), usage.turns);
    assert_eq!(
        fixture
            .usage_turns
            .iter()
            .filter(|turn| turn.cache_read_tokens.unwrap_or_default() == 0)
            .count(),
        usage.zero_cache_turns
    );
    assert_eq!(
        fixture
            .usage_turns
            .iter()
            .filter(|turn| turn.rewrite_kind == "gc")
            .count(),
        usage.gc_rewrite_turns
    );
    assert_eq!(
        fixture.usage_turns.last().map(|turn| turn.seq as usize),
        Some(usage.turns)
    );
}

fn analyze_reads(fixture: &ReplayFixture) -> ReadReplayStats {
    let successful_mutations = fixture
        .mutations
        .iter()
        .filter(|mutation| mutation.status == "succeeded")
        .fold(HashMap::<&str, Vec<u64>>::new(), |mut by_path, mutation| {
            by_path
                .entry(mutation.path.as_str())
                .or_default()
                .push(mutation.seq);
            by_path
        });
    let mut coverage = HashMap::<(&str, Option<u64>), Vec<RequestedRange>>::new();
    let mut shapes = HashMap::<(&str, Option<usize>, Option<usize>), usize>::new();
    let mut covered_requests = 0;
    let mut redundant_result_chars = 0usize;
    let mut projected_avoided_chars = 0usize;

    for read in fixture
        .reads
        .iter()
        .filter(|read| read.status == "succeeded")
    {
        *shapes
            .entry((read.path.as_str(), read.offset, read.requested_limit))
            .or_default() += 1;

        let generation = successful_mutations
            .get(read.path.as_str())
            .and_then(|mutations| {
                mutations
                    .iter()
                    .copied()
                    .filter(|seq| *seq < read.seq)
                    .max()
            });
        let ranges = coverage
            .entry((read.path.as_str(), generation))
            .or_default();
        let requested = RequestedRange::from_read(read);
        if range_is_covered(ranges, requested) {
            covered_requests += 1;
            redundant_result_chars = redundant_result_chars.saturating_add(read.result_chars);
            // Production compact reuse/rejection responses are bounded well
            // below this allowance. Keeping 256 chars per redundant call makes
            // this replay gate conservative without coupling it to exact prose.
            projected_avoided_chars =
                projected_avoided_chars.saturating_add(read.result_chars.saturating_sub(256));
        }
        insert_range(ranges, requested);
    }

    ReadReplayStats {
        covered_requests,
        exact_shape_repeat_groups: shapes.values().filter(|count| **count > 1).count(),
        redundant_result_chars,
        projected_avoided_chars,
    }
}

fn range_is_covered(ranges: &[RequestedRange], requested: RequestedRange) -> bool {
    ranges
        .iter()
        .any(|range| range.start <= requested.start && range.end >= requested.end)
}

fn insert_range(ranges: &mut Vec<RequestedRange>, requested: RequestedRange) {
    ranges.push(requested);
    ranges.sort_unstable_by_key(|range| range.start);
    let mut merged = Vec::<RequestedRange>::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        if let Some(previous) = merged.last_mut()
            && range.start <= previous.end.saturating_add(1)
        {
            previous.end = previous.end.max(range.end);
            continue;
        }
        merged.push(range);
    }
    *ranges = merged;
}

fn role_stat<'a>(fixture: &'a ReplayFixture, role: &str) -> &'a ContextRoleStat {
    fixture
        .context_role_stats
        .iter()
        .find(|stat| stat.role == role)
        .expect("fixture should contain the requested role")
}

#[test]
fn opencode_replay_preserves_small_file_and_covered_range_baseline() {
    let fixture = fixture_opencode();
    verify_baseline(&fixture);

    assert_eq!(fixture.session.provider_id, "opencode");
    assert_eq!(fixture.session.model, "deepseek-v4-pro");
    assert_eq!(fixture.session.prompt_tokens, 1_112_621);
    assert_eq!(fixture.session.completion_tokens, 40_334);
    assert_eq!(fixture.session.cache_read_tokens, 953_984);
    assert_eq!(fixture.session.cache_measured_tokens, 1_112_621);
    assert_eq!(fixture.session.cost_micros, -1);

    let stats = analyze_reads(&fixture);
    assert_eq!(stats.exact_shape_repeat_groups, 8);
    assert_eq!(stats.covered_requests, 9);
    assert!(
        stats.projected_avoided_chars.saturating_mul(100)
            >= stats.redundant_result_chars.saturating_mul(80),
        "projected compact reuse must avoid at least 80% of redundant bytes: {stats:?}"
    );
    assert_eq!(
        fixture
            .reads
            .iter()
            .filter(|read| read.offset == Some(1) && read.requested_limit == Some(80))
            .count(),
        3
    );
}

#[test]
fn codex_replay_preserves_diff_delegation_and_cache_baseline() {
    let fixture = fixture_codex();
    verify_baseline(&fixture);

    assert_eq!(fixture.session.provider_id, "codex");
    assert_eq!(fixture.session.model, "openai/gpt-5.5");
    assert_eq!(fixture.session.prompt_tokens, 3_400_549);
    assert_eq!(fixture.session.completion_tokens, 46_881);
    assert_eq!(fixture.session.cache_read_tokens, 1_288_192);
    assert_eq!(fixture.session.cache_measured_tokens, 3_400_549);
    assert_eq!(fixture.session.cost_micros, 12_612_311);

    let stats = analyze_reads(&fixture);
    assert!(stats.covered_requests >= 15, "stats: {stats:?}");
    assert!(
        stats.projected_avoided_chars.saturating_mul(100)
            >= stats.redundant_result_chars.saturating_mul(80),
        "projected compact reuse must avoid at least 80% of redundant bytes: {stats:?}"
    );

    let run_loop_diffs = fixture
        .git_inspections
        .iter()
        .filter(|inspection| {
            inspection.op == "diff"
                && inspection.target.as_deref() == Some("HEAD")
                && inspection.path.as_deref() == Some("src/agent/run_loop.rs")
                && inspection.stat_only == 0
                && inspection.status == "succeeded"
        })
        .collect::<Vec<_>>();
    assert_eq!(run_loop_diffs.len(), 6);
    assert_eq!(
        run_loop_diffs
            .iter()
            .map(|inspection| inspection.result_digest.as_str())
            .collect::<HashSet<_>>()
            .len(),
        1
    );
    assert_eq!(
        run_loop_diffs
            .iter()
            .map(|inspection| inspection.result_chars)
            .sum::<usize>(),
        21_276
    );
    assert_eq!(
        run_loop_diffs.first().map(|inspection| inspection.seq),
        Some(75)
    );
    assert_eq!(
        run_loop_diffs.last().map(|inspection| inspection.seq),
        Some(114)
    );

    assert_eq!(fixture.subagents.len(), 4);
    assert!(fixture.subagents.iter().all(|event| {
        event.seq <= 3
            && event.agent == "explore"
            && event.detached == 1
            && event.status == "running"
    }));
    let delegated_paths = fixture
        .delegated_scopes
        .iter()
        .flat_map(|scope| scope.paths.iter().map(String::as_str))
        .collect::<HashSet<_>>();
    assert_eq!(
        fixture
            .delegated_scopes
            .iter()
            .map(|scope| scope.seq)
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
    let parent_read_paths = fixture
        .reads
        .iter()
        .map(|read| read.path.as_str())
        .collect::<HashSet<_>>();
    assert_eq!(delegated_paths.len(), 21);
    assert_eq!(delegated_paths.intersection(&parent_read_paths).count(), 21);

    let background_status = fixture
        .background_context_events
        .iter()
        .filter(|event| event.event_kind == "subagent_status")
        .collect::<Vec<_>>();
    assert_eq!(background_status.len(), 6);
    assert_eq!(
        background_status
            .iter()
            .map(|event| event.chars)
            .sum::<usize>(),
        36_215
    );
    assert_eq!(background_status.first().map(|event| event.seq), Some(7));
    let background_results = fixture
        .background_context_events
        .iter()
        .filter(|event| event.event_kind == "subagent_result")
        .collect::<Vec<_>>();
    assert_eq!(background_results.len(), 2);
    assert_eq!(
        background_results
            .iter()
            .map(|event| event.chars)
            .sum::<usize>(),
        22_517
    );

    let user = role_stat(&fixture, "user");
    assert_eq!((user.messages, user.chars), (10, 111_417));
    let tool = role_stat(&fixture, "tool");
    assert_eq!((tool.messages, tool.chars), (121, 735_530));

    let mut prefix_groups = HashMap::<&str, (usize, u64, u64, u64)>::new();
    for turn in &fixture.usage_turns {
        let Some(prefix_hash) = turn.prefix_hash.as_deref() else {
            continue;
        };
        let entry = prefix_groups.entry(prefix_hash).or_default();
        entry.0 += 1;
        entry.1 += turn.prompt_tokens.unwrap_or_default();
        entry.2 += turn.cache_read_tokens.unwrap_or_default();
        entry.3 += turn.cache_measured_tokens.unwrap_or_default();
    }
    assert_eq!(
        prefix_groups.get("8be141a1abb37405"),
        Some(&(28, 2_556_647, 1_039_872, 2_556_647))
    );
    assert_eq!(
        prefix_groups.get("2028388618d706ac"),
        Some(&(32, 843_902, 248_320, 843_902))
    );
}

fn guard_replay<'a>(fixture: &'a ReplayFixture, scenario: &str) -> &'a GuardReplay {
    fixture
        .guard_replays
        .iter()
        .find(|replay| replay.scenario == scenario)
        .unwrap_or_else(|| panic!("fixture should contain {scenario} guard replay"))
}

fn replay_tool_response(
    replay: &GuardReplay,
    id: &str,
    name: &str,
    arguments: serde_json::Value,
) -> crate::provider::ProviderResult<StreamedResponse> {
    let finish_reason = match replay.tool_finish_reason.as_str() {
        "stop" => crate::provider::FinishReason::Stop,
        "tool_calls" => crate::provider::FinishReason::ToolCalls,
        other => panic!("unsupported replay finish reason: {other}"),
    };
    Ok(StreamedResponse {
        tool_calls: vec![test_tool_call(id, name, &arguments.to_string())],
        terminal: crate::provider::StreamTerminal::Completed(finish_reason),
        ..StreamedResponse::default()
    })
}

fn replay_empty_stop() -> crate::provider::ProviderResult<StreamedResponse> {
    Ok(StreamedResponse {
        terminal: crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::Stop),
        ..StreamedResponse::default()
    })
}

fn replay_finish(content: &str) -> crate::provider::ProviderResult<StreamedResponse> {
    Ok(StreamedResponse {
        content: content.to_string(),
        terminal: crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::Stop),
        ..StreamedResponse::default()
    })
}

fn replay_registry(fixture: &TestFixture) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ReadTool::new(
        fixture.project_root.clone(),
        fixture.read_tracker.clone(),
    )));
    registry.register(Arc::new(ReadRegionTool::new(
        fixture.project_root.clone(),
        fixture.read_tracker.clone(),
    )));
    registry.register(Arc::new(ReadSymbolTool::new(
        fixture.project_root.clone(),
        fixture.read_tracker.clone(),
    )));
    registry.register(Arc::new(EditTool::new(
        fixture.project_root.clone(),
        fixture.read_tracker.clone(),
    )));
    Arc::new(registry)
}

fn replay_tool_messages(messages: &[ChatCompletionRequestMessage]) -> Vec<(String, String)> {
    messages
        .iter()
        .filter_map(|message| {
            let value = serde_json::to_value(message).ok()?;
            (value.get("role").and_then(serde_json::Value::as_str) == Some("tool")).then_some((
                value.get("tool_call_id")?.as_str()?.to_string(),
                value.get("content")?.as_str()?.to_string(),
            ))
        })
        .collect()
}

struct StaleReplayProvider {
    responses: tokio::sync::Mutex<Vec<crate::provider::ProviderResult<StreamedResponse>>>,
    calls: std::sync::atomic::AtomicUsize,
    path: std::path::PathBuf,
}

struct CancellationReplayProvider {
    responses: tokio::sync::Mutex<Vec<crate::provider::ProviderResult<StreamedResponse>>>,
    calls: std::sync::atomic::AtomicUsize,
    cancel_on_call: usize,
    cancellation: CancellationToken,
}

#[async_trait]
impl Provider for StaleReplayProvider {
    async fn chat_stream(
        &self,
        _messages: &[ChatCompletionRequestMessage],
        _tools: &[ChatCompletionTool],
        _cancellation_token: CancellationToken,
        _sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if call == 1 {
            tokio::fs::write(&self.path, "fn target() -> i32 {\n    9\n}\n")
                .await
                .expect("replay should apply the concurrent edit");
        }
        Ok(self
            .responses
            .lock()
            .await
            .remove(0)
            .expect("replay response should be successful"))
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

#[async_trait]
impl Provider for CancellationReplayProvider {
    async fn chat_stream(
        &self,
        _messages: &[ChatCompletionRequestMessage],
        _tools: &[ChatCompletionTool],
        _cancellation_token: CancellationToken,
        _sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if call == self.cancel_on_call {
            self.cancellation.cancel();
        }
        self.responses.lock().await.remove(0)
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

async fn run_read_storm_replay(fixture: &ReplayFixture) {
    let replay = guard_replay(fixture, "read_storm");
    let project = TestFixture::new();
    project.create_file(
        "replay.rs",
        "fn target() -> i32 {\n    1\n}\n\nfn sibling() -> i32 {\n    9\n}\n",
    );
    let mut responses = vec![
        replay_tool_response(
            replay,
            "read-1",
            "read",
            serde_json::json!({"path": "replay.rs", "offset": 1, "limit": 1}),
        ),
        replay_tool_response(
            replay,
            "read-2",
            "read_region",
            serde_json::json!({"path": "replay.rs", "start_line": 1, "end_line": 2}),
        ),
        replay_tool_response(
            replay,
            "read-3",
            "read_region",
            serde_json::json!({"path": "replay.rs", "start_line": 2, "end_line": 3}),
        ),
        replay_tool_response(
            replay,
            "read-rejected",
            "read_region",
            serde_json::json!({"path": "replay.rs", "start_line": 1, "end_line": 3}),
        ),
    ];
    if replay.empty_stop_after_rejection {
        responses.push(replay_empty_stop());
    }
    responses.extend([
        replay_tool_response(
            replay,
            "read-recovery",
            "read_symbol",
            serde_json::json!({"path": "replay.rs", "query": "target", "kind": "function"}),
        ),
        replay_tool_response(
            replay,
            "edit-progress",
            "edit",
            serde_json::json!({
                "path": "replay.rs",
                "old_string": "    1",
                "new_string": "    2"
            }),
        ),
        replay_finish("recovered"),
    ]);
    let provider = MockProvider::new(responses);
    let requests = provider.requests();
    let mut agent = Agent::new(
        Box::new(provider),
        replay_registry(&project),
        empty_registry(),
        project.read_tracker.clone(),
        String::new(),
        project.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);
    agent.budget.max_iterations = 12;

    let result = agent
        .run(
            "replay the read recovery and apply the recovered edit",
            CancellationToken::new(),
            Arc::new(CaptureSink::default()),
        )
        .await
        .unwrap();

    assert_eq!(result, AgentRunResult::Completed("recovered".to_string()));
    let tools = replay_tool_messages(&agent.messages);
    let rejected = tools
        .iter()
        .find(|(id, _)| id == "read-rejected")
        .expect("stormed read should produce a synthetic result");
    let recovered = tools
        .iter()
        .find(|(id, _)| id == "read-recovery")
        .expect("narrow recovery should execute");
    let content = std::fs::read_to_string(project.project_root.join("replay.rs")).unwrap();
    let observed_guard_reason = rejected
        .1
        .contains("repeated read storm")
        .then_some("read_storm");
    let observed_recovery_action =
        (!recovered.1.contains("repeated read storm")).then_some("read_symbol");
    assert_eq!(
        observed_guard_reason,
        Some(replay.guard_reason.as_str()),
        "unexpected guard result: {}",
        rejected.1
    );
    assert_eq!(
        observed_recovery_action,
        Some(replay.recovery_action.as_str())
    );
    assert_eq!(content.contains("    2"), replay.progress_followed);
    let requests = requests.lock().await;
    let saw_empty_turn_nudge = requests
        .iter()
        .any(|request| format!("{request:?}").contains("no tool calls and no answer text"));
    assert_eq!(saw_empty_turn_nudge, replay.empty_stop_after_rejection);
}

fn delegated_evidence(project: &TestFixture) -> DelegatedReadEvidence {
    let path = project.project_root.join("delegated.rs");
    let canonical = std::fs::canonicalize(&path).unwrap();
    let body = std::fs::read(&canonical).unwrap();
    let metadata = std::fs::metadata(&canonical).unwrap();
    DelegatedReadEvidence {
        subtask_id: "child-1".to_string(),
        launch_group_id: Some("group-1".to_string()),
        source_id: "tool:child-read".to_string(),
        cited_in_result: true,
        evidence: ReadEvidence::new(
            "delegated.rs",
            canonical,
            ReadWindow {
                requested_offset: 1,
                requested_limit: 2000,
                start_line: 1,
                end_line: Some(3),
                total_lines: Some(3),
            },
            ReadCoverage::Full,
            "1: fn target() -> i32 {\n2:     1\n3: }\n",
            metadata.modified().ok(),
            metadata.len(),
            Some(crate::tool::digest_content(&body)),
        ),
    }
}

async fn run_delegated_reread_replay(fixture: &ReplayFixture) {
    let replay = guard_replay(fixture, "delegated_parent_reread");
    let project = TestFixture::new();
    project.create_file("delegated.rs", "fn target() -> i32 {\n    1\n}\n");
    let mut responses = vec![replay_tool_response(
        replay,
        "delegated-rejected",
        "read",
        serde_json::json!({"path": "delegated.rs"}),
    )];
    if replay.empty_stop_after_rejection {
        responses.push(replay_empty_stop());
    }
    responses.extend([
        replay_tool_response(
            replay,
            "edit-before-read",
            "edit",
            serde_json::json!({
                "path": "delegated.rs",
                "old_string": "    1",
                "new_string": "    2"
            }),
        ),
        replay_tool_response(
            replay,
            "delegated-recovery",
            "read",
            serde_json::json!({
                "path": "delegated.rs",
                "reason": "need complete source to authorize the parent edit"
            }),
        ),
        replay_tool_response(
            replay,
            "edit-after-read",
            "edit",
            serde_json::json!({
                "path": "delegated.rs",
                "old_string": "    1",
                "new_string": "    2"
            }),
        ),
        replay_finish("delegated recovery complete"),
    ]);
    let mut agent = Agent::new(
        Box::new(MockProvider::new(responses)),
        replay_registry(&project),
        empty_registry(),
        project.read_tracker.clone(),
        String::new(),
        project.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);
    agent.import_delegated_read_evidence(&[delegated_evidence(&project)]);
    agent.budget.max_iterations = 12;

    let result = agent
        .run(
            "replay delegated recovery and apply the recovered edit",
            CancellationToken::new(),
            Arc::new(CaptureSink::default()),
        )
        .await
        .unwrap();

    assert_eq!(
        result,
        AgentRunResult::Completed("delegated recovery complete".to_string())
    );
    let tools = replay_tool_messages(&agent.messages);
    let rejected = tools
        .iter()
        .find(|(id, _)| id == "delegated-rejected")
        .expect("unjustified parent read should be rejected");
    let unauthorized_edit = tools
        .iter()
        .find(|(id, _)| id == "edit-before-read")
        .expect("pre-read edit should return a guard error");
    let recovery = tools
        .iter()
        .find(|(id, _)| id == "delegated-recovery")
        .expect("reasoned parent read should execute");
    let content = std::fs::read_to_string(project.project_root.join("delegated.rs")).unwrap();
    assert_eq!(
        rejected
            .1
            .contains("broad parent reread deferred")
            .then_some("delegated_read"),
        Some(replay.guard_reason.as_str())
    );
    assert!(
        unauthorized_edit.1.to_ascii_lowercase().contains("read"),
        "delegated/untrusted evidence must not authorize edits: {}",
        unauthorized_edit.1
    );
    assert_eq!(
        recovery
            .1
            .contains("fn target")
            .then_some("explicit_reason"),
        Some(replay.recovery_action.as_str())
    );
    assert_eq!(content.contains("    2"), replay.progress_followed);
    assert_eq!(agent.usage_turns()[0].delegated_parent_overlap, 1);
}

async fn run_stale_edit_replay(fixture: &ReplayFixture) {
    let replay = guard_replay(fixture, "stale_edit_evidence");
    let project = TestFixture::new();
    project.create_file("stale.rs", "fn target() -> i32 {\n    1\n}\n");
    let path = project.project_root.join("stale.rs");
    let responses = vec![
        replay_tool_response(
            replay,
            "stale-read-1",
            "read",
            serde_json::json!({"path": "stale.rs"}),
        ),
        replay_tool_response(
            replay,
            "stale-edit-rejected",
            "edit",
            serde_json::json!({
                "path": "stale.rs",
                "old_string": "    1",
                "new_string": "    2"
            }),
        ),
        replay_tool_response(
            replay,
            "stale-reread",
            "read",
            serde_json::json!({"path": "stale.rs"}),
        ),
        replay_tool_response(
            replay,
            "stale-edit-progress",
            "edit",
            serde_json::json!({
                "path": "stale.rs",
                "old_string": "    9",
                "new_string": "    2"
            }),
        ),
        replay_finish("stale recovery complete"),
    ];
    let provider = StaleReplayProvider {
        responses: tokio::sync::Mutex::new(responses),
        calls: std::sync::atomic::AtomicUsize::new(0),
        path: path.clone(),
    };
    let mut agent = Agent::new(
        Box::new(provider),
        replay_registry(&project),
        empty_registry(),
        project.read_tracker.clone(),
        String::new(),
        project.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);

    let result = agent
        .run(
            "edit the file while replaying stale edit recovery",
            CancellationToken::new(),
            Arc::new(CaptureSink::default()),
        )
        .await
        .unwrap();

    assert_eq!(
        result,
        AgentRunResult::Completed("stale recovery complete".to_string())
    );
    let tools = replay_tool_messages(&agent.messages);
    let rejected = tools
        .iter()
        .find(|(id, _)| id == "stale-edit-rejected")
        .expect("stale edit should be rejected");
    let reread = tools
        .iter()
        .find(|(id, _)| id == "stale-reread")
        .expect("stale file should be reread");
    let content = std::fs::read_to_string(path).unwrap();
    let rejection = rejected.1.to_ascii_lowercase();
    assert_eq!(
        (rejection.contains("changed") || rejection.contains("stale")).then_some("mtime_stale"),
        Some(replay.guard_reason.as_str()),
        "unexpected stale-edit result: {}",
        rejected.1
    );
    assert_eq!(
        reread.1.contains("    9").then_some("reread"),
        Some(replay.recovery_action.as_str())
    );
    assert_eq!(content.contains("    2"), replay.progress_followed);
}

#[tokio::test]
async fn read_guard_replays_recover_under_codex_and_opencode_stop_shapes() {
    for fixture in [fixture_codex(), fixture_opencode()] {
        run_read_storm_replay(&fixture).await;
        run_delegated_reread_replay(&fixture).await;
        run_stale_edit_replay(&fixture).await;
    }
}

#[tokio::test]
async fn repeated_unjustified_delegated_reread_returns_typed_blocker() {
    let fixture = fixture_codex();
    let replay = guard_replay(&fixture, "delegated_parent_reread");
    let project = TestFixture::new();
    project.create_file("delegated.rs", "fn target() -> i32 {\n    1\n}\n");
    let mut agent = Agent::new(
        Box::new(MockProvider::new(vec![
            replay_tool_response(
                replay,
                "delegated-1",
                "read",
                serde_json::json!({"path": "delegated.rs"}),
            ),
            replay_tool_response(
                replay,
                "delegated-2",
                "read",
                serde_json::json!({"path": "delegated.rs"}),
            ),
        ])),
        replay_registry(&project),
        empty_registry(),
        project.read_tracker.clone(),
        String::new(),
        project.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);
    agent.import_delegated_read_evidence(&[delegated_evidence(&project)]);

    let error = agent
        .run(
            "repeat an unjustified reread",
            CancellationToken::new(),
            Arc::new(CaptureSink::default()),
        )
        .await
        .unwrap_err();
    let blocker = error
        .downcast_ref::<crate::agent::run_loop::ReadGuardBlocker>()
        .expect("delegated loop should return a typed read blocker");
    assert_eq!(
        blocker.reason(),
        crate::agent::run_loop::ReadGuardBlockerReason::DelegatedReread
    );
}

#[tokio::test]
async fn persistent_read_storm_returns_typed_blocker_after_one_recovery_budget() {
    let fixture = fixture_codex();
    let replay = guard_replay(&fixture, "read_storm");
    let project = TestFixture::new();
    project.create_file("loop.rs", "fn target() {}\n");
    let responses = (1..=8)
        .map(|turn| {
            replay_tool_response(
                replay,
                &format!("loop-{turn}"),
                "read_region",
                serde_json::json!({"path": "loop.rs", "start_line": 1, "end_line": 1}),
            )
        })
        .collect();
    let mut agent = Agent::new(
        Box::new(MockProvider::new(responses)),
        replay_registry(&project),
        empty_registry(),
        project.read_tracker.clone(),
        String::new(),
        project.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);
    agent.budget.max_iterations = 10;

    let error = agent
        .run(
            "keep reading without progress",
            CancellationToken::new(),
            Arc::new(CaptureSink::default()),
        )
        .await
        .unwrap_err();
    let blocker = error
        .downcast_ref::<crate::agent::run_loop::ReadGuardBlocker>()
        .expect("persistent storm should return a typed read blocker");
    assert_eq!(
        blocker.reason(),
        crate::agent::run_loop::ReadGuardBlockerReason::ReadStorm
    );
}

#[tokio::test]
async fn cancellation_after_read_storm_rejection_never_forms_an_empty_turn_loop() {
    let fixture = fixture_codex();
    let replay = guard_replay(&fixture, "read_storm");
    let project = TestFixture::new();
    project.create_file("cancel.rs", "fn target() {}\n");
    let cancellation = CancellationToken::new();
    let mut responses = (1..=4)
        .map(|turn| {
            replay_tool_response(
                replay,
                &format!("cancel-read-{turn}"),
                "read_region",
                serde_json::json!({"path": "cancel.rs", "start_line": 1, "end_line": 1}),
            )
        })
        .collect::<Vec<_>>();
    responses.push(replay_empty_stop());
    let provider = CancellationReplayProvider {
        responses: tokio::sync::Mutex::new(responses),
        calls: std::sync::atomic::AtomicUsize::new(0),
        cancel_on_call: 4,
        cancellation: cancellation.clone(),
    };
    let mut agent = Agent::new(
        Box::new(provider),
        replay_registry(&project),
        empty_registry(),
        project.read_tracker.clone(),
        String::new(),
        project.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);

    let result = agent
        .run(
            "cancel after the storm redirect",
            cancellation,
            Arc::new(CaptureSink::default()),
        )
        .await
        .unwrap();

    assert_eq!(result, AgentRunResult::Interrupted(String::new()));
    let tools = replay_tool_messages(&agent.messages);
    assert!(
        tools.iter().any(|(id, content)| {
            id == "cancel-read-4" && content.contains("repeated read storm")
        }),
        "the actionable rejection must land before cancellation"
    );
    assert!(
        !agent
            .messages
            .iter()
            .any(|message| { format!("{message:?}").contains("no tool calls and no answer text") }),
        "the cancellation race must not append an empty-response nudge"
    );
}

#[tokio::test]
async fn broad_reread_reason_never_bypasses_project_scope() {
    let fixture = fixture_codex();
    let replay = guard_replay(&fixture, "delegated_parent_reread");
    let project = TestFixture::new();
    let outside = tempfile::tempdir().unwrap();
    let outside_path = outside.path().join("outside.rs");
    std::fs::write(&outside_path, "fn secret() {}\n").unwrap();
    let mut agent = Agent::new(
        Box::new(MockProvider::new(vec![
            replay_tool_response(
                replay,
                "outside-read",
                "read",
                serde_json::json!({
                    "path": outside_path,
                    "reason": "need complete source to authorize the parent edit"
                }),
            ),
            replay_finish("scope preserved"),
        ])),
        replay_registry(&project),
        empty_registry(),
        project.read_tracker.clone(),
        String::new(),
        project.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);

    let result = agent
        .run(
            "try an out-of-scope reasoned read",
            CancellationToken::new(),
            Arc::new(CaptureSink::default()),
        )
        .await
        .unwrap();

    assert_eq!(
        result,
        AgentRunResult::Completed("scope preserved".to_string())
    );
    let tools = replay_tool_messages(&agent.messages);
    let rejected = tools
        .iter()
        .find(|(id, _)| id == "outside-read")
        .expect("out-of-scope read should return an error");
    let message = rejected.1.to_ascii_lowercase();
    assert!(
        message.contains("outside") && message.contains("project"),
        "reason must not widen read scope: {}",
        rejected.1
    );
}
