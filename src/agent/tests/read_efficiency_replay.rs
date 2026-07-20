use std::collections::{HashMap, HashSet};

use serde::Deserialize;

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
