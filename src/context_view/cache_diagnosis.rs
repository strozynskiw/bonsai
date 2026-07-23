//! Per-turn prompt-cache diagnosis for the `/ctx` Turns view.
//!
//! Answers "what was cached, and what broke caching last round" strictly from
//! recorded facts: provider-reported cache read/creation tokens, the recorded
//! rewrite kind (gc/compaction/manual), compaction-event prefix hashes, and
//! the byte-stable system-prefix fingerprint each turn carried. When the
//! evidence is missing the diagnosis says so instead of guessing.

use super::telemetry::{compact_tokens, turn_cache_percent};
use super::{CompactionEvent, ContextReport, UsageTurnReport};
use crate::agent::{ContextRewriteKind, UsageTurnStatus};

/// A turn is "warm" when at least this share of measured input was read from
/// cache; the last-message breakpoint moves every turn, so 100% is impossible
/// and healthy sessions sit in the 85-95% band.
const WARM_READ_PERCENT: u64 = 800;

const ANTHROPIC_CACHE_TTL_MS: i64 = 5 * 60 * 1_000;
const CODEX_GPT_5_6_CACHE_TTL_MS: i64 = 30 * 60 * 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheBreakCause {
    /// The agent recorded a context rewrite (gc/compaction/manual) before this
    /// turn — the definitive explanation when present.
    Rewrite(ContextRewriteKind),
    /// A compaction event's before/after prefix hashes match this turn's
    /// transition exactly.
    Compaction {
        event_seq: usize,
    },
    /// The system prefix fingerprint changed with no recorded rewrite.
    PrefixChurn,
    /// Prefix stable but the session idled past the cache TTL.
    IdleExpiry {
        idle_ms: u64,
    },
    /// The provider cache-routing key changed between adjacent turns in the
    /// same execution lane, so the second request could not reach the first
    /// request's cache shard.
    RouteChanged,
    /// The serialized request stopped matching before the healthy reuse band.
    /// This is byte evidence, not an inferred token count.
    RequestPrefixChanged {
        reusable_percent: u64,
    },
    /// Route and system prefix stayed stable and the serialized request kept a
    /// healthy reusable prefix, yet the backend reported an unexpectedly low
    /// cache read. This names the observed failure without guessing whether the
    /// backend evicted, rejected, or failed to route the cache entry.
    BackendMiss {
        reusable_percent: Option<u64>,
    },
    Unknown,
}

impl CacheBreakCause {
    /// Whether we have *recorded proof* that the agent broke the cache — a
    /// rewrite or a compaction event. Only these justify the loud "cache BROKE"
    /// verdict. A bare `PrefixChurn` is an inference ("the fingerprint changed
    /// and we don't know why"), which on lane-scoped providers is usually just a
    /// benign lane switch (subagent/plan persona), so it must not be asserted as
    /// a break — it reads as "cold — prefix changed" instead. Backend misses,
    /// route changes, and idle expiry are not proof that Bonsai rewrote input.
    fn is_self_inflicted(&self) -> bool {
        matches!(self, Self::Rewrite(_) | Self::Compaction { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnCacheAssessment {
    /// First turn of the session — cold by definition, not a break.
    FirstTurn,
    Warm {
        read_percent: u64,
    },
    Partial {
        read_percent: u64,
        cause: CacheBreakCause,
    },
    Cold {
        cause: CacheBreakCause,
    },
    /// Provider did not report cache telemetry for this turn; distinct from a
    /// genuine 0% (which arrives as `Some(0)`).
    NoCacheData,
    /// Usage missing or the turn was interrupted.
    NoUsage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnCacheDiagnosis {
    pub seq: usize,
    pub assessment: TurnCacheAssessment,
    /// One-line human summary, e.g. "cold — compaction #3 rewrote prefix".
    pub summary: String,
}

/// Diagnose every recorded turn, oldest first (parallel to
/// `report.usage_turns`).
pub(crate) fn diagnose_turns(report: &ContextReport) -> Vec<TurnCacheDiagnosis> {
    let mut previous = std::collections::HashMap::new();
    report
        .usage_turns
        .iter()
        .map(|turn| {
            let key = (turn.lane_kind, turn.lane_id.as_str());
            let diagnosis =
                diagnose_turn(turn, previous.get(&key).copied(), &report.compaction_events);
            previous.insert(key, turn);
            diagnosis
        })
        .collect()
}

/// Diagnosis of the most recent turn that produced usage (interrupted and
/// usage-less turns are skipped).
pub(crate) fn last_turn_diagnosis(report: &ContextReport) -> Option<TurnCacheDiagnosis> {
    diagnose_turns(report)
        .into_iter()
        .rev()
        .find(|diagnosis| diagnosis.assessment != TurnCacheAssessment::NoUsage)
}

/// Header verdict for the most recent turn that produced usage, shown in every
/// `/ctx` view mode.
pub(crate) fn last_turn_verdict(report: &ContextReport) -> Option<String> {
    let diagnoses = diagnose_turns(report);
    let (diagnosis, turn) = diagnoses
        .iter()
        .zip(&report.usage_turns)
        .rev()
        .find(|(diagnosis, _)| diagnosis.assessment != TurnCacheAssessment::NoUsage)?;
    let verdict = match &diagnosis.assessment {
        TurnCacheAssessment::FirstTurn => "cache cold — first turn".to_string(),
        TurnCacheAssessment::Warm { read_percent } => {
            let hash = turn
                .prefix_hash
                .as_deref()
                .map(|hash| format!(" · prefix stable {}", short_hash(hash)))
                .unwrap_or_default();
            format!(
                "cache warm ({}.{}){hash}",
                read_percent / 10,
                read_percent % 10
            )
        }
        TurnCacheAssessment::Partial { read_percent, .. } => {
            format!(
                "cache partial ({}.{}) — {}",
                read_percent / 10,
                read_percent % 10,
                diagnosis.summary
            )
        }
        TurnCacheAssessment::Cold { cause } => {
            // Only a prefix *we* provably rewrote is a "break". A cold response
            // with only a stable system hash is unverified, not proof of our bug.
            let headline = if cause.is_self_inflicted() {
                "cache BROKE last turn"
            } else {
                "cache cold last turn"
            };
            format!("{headline} — {}", diagnosis.summary)
        }
        TurnCacheAssessment::NoCacheData => {
            "cache n/a — provider reported no cache data".to_string()
        }
        TurnCacheAssessment::NoUsage => unreachable!("filtered above"),
    };
    Some(verdict)
}

fn diagnose_turn(
    turn: &UsageTurnReport,
    prev: Option<&UsageTurnReport>,
    events: &[CompactionEvent],
) -> TurnCacheDiagnosis {
    let assessment = assess(turn, prev, events);
    let summary = summarize(turn, &assessment);
    TurnCacheDiagnosis {
        seq: turn.seq,
        assessment,
        summary,
    }
}

fn assess(
    turn: &UsageTurnReport,
    prev: Option<&UsageTurnReport>,
    events: &[CompactionEvent],
) -> TurnCacheAssessment {
    match turn.status {
        UsageTurnStatus::Missing | UsageTurnStatus::Interrupted => {
            return TurnCacheAssessment::NoUsage;
        }
        UsageTurnStatus::Reported => {}
    }
    let Some(read_percent) = turn_cache_percent(turn) else {
        return TurnCacheAssessment::NoCacheData;
    };
    if read_percent >= WARM_READ_PERCENT {
        return TurnCacheAssessment::Warm { read_percent };
    }
    if prev.is_none() {
        return TurnCacheAssessment::FirstTurn;
    }
    let cause = break_cause(turn, prev, events);
    if read_percent == 0 {
        TurnCacheAssessment::Cold { cause }
    } else {
        TurnCacheAssessment::Partial {
            read_percent,
            cause,
        }
    }
}

fn break_cause(
    turn: &UsageTurnReport,
    prev: Option<&UsageTurnReport>,
    events: &[CompactionEvent],
) -> CacheBreakCause {
    let hashes = prev
        .and_then(|prev| prev.prefix_hash.as_deref())
        .zip(turn.prefix_hash.as_deref());
    if let Some((prev_hash, hash)) = hashes
        && prev_hash != hash
        && let Some(event) = matching_compaction(events, prev_hash, hash)
    {
        return CacheBreakCause::Compaction {
            event_seq: event.seq,
        };
    }
    if turn.rewrite_kind != ContextRewriteKind::None {
        return CacheBreakCause::Rewrite(turn.rewrite_kind);
    }
    match hashes {
        Some((prev_hash, hash)) if prev_hash != hash => CacheBreakCause::PrefixChurn,
        Some(_) => stable_prefix_cause(turn, prev),
        None => CacheBreakCause::Unknown,
    }
}

/// Cause for a cold turn whose system-prefix fingerprint was byte-stable. Only
/// claim idle expiry when the active provider/model has a known retention
/// window; a stable system hash alone is not proof of provider eviction.
fn stable_prefix_cause(turn: &UsageTurnReport, prev: Option<&UsageTurnReport>) -> CacheBreakCause {
    let Some(prev) = prev else {
        return CacheBreakCause::Unknown;
    };

    if prev
        .cache_route_fingerprint
        .as_deref()
        .zip(turn.cache_route_fingerprint.as_deref())
        .is_some_and(|(previous, current)| previous != current)
    {
        return CacheBreakCause::RouteChanged;
    }

    if let Some(reusable_percent) = turn.local_reusable_prefix_percent
        && reusable_percent.saturating_mul(10) < WARM_READ_PERCENT
    {
        return CacheBreakCause::RequestPrefixChanged { reusable_percent };
    }

    // Legacy persisted rows carry zeroed timestamps. Keep using the stronger
    // route/request evidence above, but never invent an idle interval.
    if prev.created_at_ms > 0 && turn.created_at_ms > 0 {
        let idle = turn.created_at_ms.saturating_sub(prev.created_at_ms);
        if cache_ttl_ms(turn).is_some_and(|ttl| idle > ttl) {
            return CacheBreakCause::IdleExpiry {
                idle_ms: idle as u64,
            };
        }
    }

    // IMPORTANT DIAGNOSTIC CONTRACT: do not collapse this back to "miss
    // unverified". We know the backend under-read a stable eligible request;
    // only the backend's private eviction/rejection reason is unknowable.
    CacheBreakCause::BackendMiss {
        reusable_percent: turn.local_reusable_prefix_percent,
    }
}

fn cache_ttl_ms(turn: &UsageTurnReport) -> Option<i64> {
    let provider = turn.provider_id.as_deref()?.to_ascii_lowercase();
    if provider == "anthropic" {
        return Some(ANTHROPIC_CACHE_TTL_MS);
    }
    let model = turn.model.as_deref()?.to_ascii_lowercase();
    if provider == "codex" && model.starts_with("gpt-5.6") {
        return Some(CODEX_GPT_5_6_CACHE_TTL_MS);
    }
    None
}

fn matching_compaction<'a>(
    events: &'a [CompactionEvent],
    prev_hash: &str,
    hash: &str,
) -> Option<&'a CompactionEvent> {
    events.iter().rev().find(|event| {
        event.prefix_hash_before.as_deref() == Some(prev_hash)
            && event.prefix_hash_after.as_deref() == Some(hash)
    })
}

fn summarize(turn: &UsageTurnReport, assessment: &TurnCacheAssessment) -> String {
    match assessment {
        TurnCacheAssessment::FirstTurn => "first turn — cache cold by definition".to_string(),
        TurnCacheAssessment::Warm { read_percent } => {
            format!(
                "warm — {}.{} read from cache",
                read_percent / 10,
                read_percent % 10
            )
        }
        TurnCacheAssessment::Partial { cause, .. } => cause_text(turn, cause),
        TurnCacheAssessment::Cold { cause } => {
            let mut text = cause_text(turn, cause);
            if let Some(written) = turn
                .cache_creation_input_tokens
                .filter(|written| *written > 0)
            {
                text.push_str(&format!(
                    " · wrote {} to cache",
                    compact_tokens(written as usize)
                ));
            }
            text
        }
        TurnCacheAssessment::NoCacheData => "no cache telemetry from provider".to_string(),
        TurnCacheAssessment::NoUsage => match turn.status {
            UsageTurnStatus::Interrupted => "interrupted — no usage".to_string(),
            _ => "no usage reported".to_string(),
        },
    }
}

fn cause_text(turn: &UsageTurnReport, cause: &CacheBreakCause) -> String {
    match cause {
        CacheBreakCause::Rewrite(kind) => {
            let saved = turn
                .rewrite_saved_tokens
                .filter(|saved| *saved > 0)
                .map(|saved| format!(" -{}", compact_tokens(saved)))
                .unwrap_or_default();
            format!("{} rewrite{saved}", kind.as_db_str())
        }
        CacheBreakCause::Compaction { event_seq } => {
            format!("compaction #{event_seq} rewrote prefix")
        }
        CacheBreakCause::PrefixChurn => {
            let transition = hash_transition(turn);
            format!("prefix changed{transition} — no recorded rewrite")
        }
        CacheBreakCause::IdleExpiry { idle_ms } => {
            format!("idle {} — cache likely expired", format_idle(*idle_ms))
        }
        CacheBreakCause::RouteChanged => {
            let route = turn
                .cache_route_fingerprint
                .as_deref()
                .map(short_hash)
                .unwrap_or("unknown");
            format!("cache route changed →{route}")
        }
        CacheBreakCause::RequestPrefixChanged { reusable_percent } => {
            format!("serialized request diverged at {reusable_percent}% of current body")
        }
        CacheBreakCause::BackendMiss { reusable_percent } => {
            let expected = turn
                .expected_cacheable_percent
                .map(|percent| {
                    let tenths = percent.saturating_mul(10);
                    format!(" · estimated {}.{}% cacheable", tenths / 10, tenths % 10)
                })
                .unwrap_or_default();
            let reusable = reusable_percent
                .map(|percent| {
                    let tenths = percent.saturating_mul(10);
                    format!(" · wire prefix {}.{}%", tenths / 10, tenths % 10)
                })
                .unwrap_or_else(|| " · wire prefix unavailable".to_string());
            format!("backend cache miss despite stable route/system prefix{reusable}{expected}")
        }
        CacheBreakCause::Unknown => {
            if turn.prefix_hash.is_some() {
                // The fingerprint covers only the system prefix, so a stable
                // hash pins the miss to tools or message history.
                let expected = turn
                    .expected_cacheable_percent
                    .map(|percent| {
                        let tenths = percent.saturating_mul(10);
                        format!("expected {}.{}% cacheable", tenths / 10, tenths % 10)
                    })
                    .unwrap_or_else(|| "expected cacheable ratio unknown".to_string());
                let actual = turn
                    .actual_cache_read_percent
                    .map(|percent| {
                        let tenths = percent.saturating_mul(10);
                        format!("provider read {}.{}%", tenths / 10, tenths % 10)
                    })
                    .unwrap_or_else(|| "provider read n/a".to_string());
                let mechanism = turn
                    .cache_mechanism
                    .as_deref()
                    .unwrap_or("no cache hint recorded");
                format!("system prefix stable — {actual}; {expected}; hint {mechanism}")
            } else {
                "cause unknown — no prefix fingerprint".to_string()
            }
        }
    }
}

fn hash_transition(turn: &UsageTurnReport) -> String {
    turn.prefix_hash
        .as_deref()
        .map(|hash| format!(" →{}", short_hash(hash)))
        .unwrap_or_default()
}

pub(crate) fn short_hash(hash: &str) -> &str {
    &hash[..hash.len().min(8)]
}

fn format_idle(idle_ms: u64) -> String {
    let minutes = idle_ms / 60_000;
    if minutes >= 60 {
        format!("{}h{}m", minutes / 60, minutes % 60)
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{}s", idle_ms / 1_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(seq: usize, read: Option<u64>, measured: Option<u64>) -> UsageTurnReport {
        UsageTurnReport {
            seq,
            lane_kind: crate::agent::ExecutionLaneKind::Parent,
            lane_id: "parent-1".to_string(),
            lane_seq: seq,
            parent_tool_call_id: None,
            launch_group_id: None,
            status: UsageTurnStatus::Reported,
            finish_reason: None,
            reasoning_chars: 0,
            provider_attempts: Vec::new(),
            provider_id: None,
            model: None,
            effective_reasoning: None,
            prompt_tokens: Some(1_000),
            completion_tokens: Some(200),
            cache_read_input_tokens: read,
            cache_creation_input_tokens: Some(0),
            cache_measured_input_tokens: measured,
            turn_cost_micros: Some(100),
            no_cache_cost_micros: Some(200),
            estimated_prompt_tokens: Some(1_000),
            estimate_source: None,
            estimate_confidence: None,
            tool_schema_tokens: None,
            tool_schema_hash: None,
            tool_schema_names: Vec::new(),
            request_body_bytes: None,
            request_body_hash: None,
            cache_mechanism: None,
            cache_route_fingerprint: None,
            expected_cacheable_percent: None,
            actual_cache_read_percent: None,
            local_reusable_prefix_tokens: None,
            local_reusable_prefix_percent: None,
            cacheable_prefix_tokens: None,
            volatile_tail_tokens: None,
            context_window_tokens: None,
            rewrite_kind: ContextRewriteKind::None,
            rewrite_saved_tokens: None,
            episode_seq: None,
            created_at_ms: 1_700_000_000_000 + seq as i64 * 30_000,
            latency_ms: Some(3_000),
            ttft_ms: Some(500),
            prefix_hash: Some("aaaa1111bbbb".to_string()),
            inspection_executed: 0,
            inspection_reused: 0,
            inspection_rejected: 0,
            inspection_returned_chars: 0,
            inspection_avoided_chars: 0,
            delegated_parent_overlap: 0,
        }
    }

    fn report_with(turns: Vec<UsageTurnReport>, events: Vec<CompactionEvent>) -> ContextReport {
        ContextReport {
            usage_turns: turns,
            compaction_events: events,
            ..Default::default()
        }
    }

    #[test]
    fn warm_chain_stays_warm_with_stable_prefix() {
        let report = report_with(
            vec![
                turn(1, Some(920), Some(1_000)),
                turn(2, Some(950), Some(1_000)),
            ],
            Vec::new(),
        );
        let diagnoses = diagnose_turns(&report);
        assert_eq!(
            diagnoses[1].assessment,
            TurnCacheAssessment::Warm { read_percent: 950 }
        );
    }

    #[test]
    fn first_turn_is_not_a_break() {
        let report = report_with(vec![turn(1, Some(0), Some(1_000))], Vec::new());
        let diagnoses = diagnose_turns(&report);
        assert_eq!(diagnoses[0].assessment, TurnCacheAssessment::FirstTurn);
        assert_eq!(
            last_turn_verdict(&report).unwrap(),
            "cache cold — first turn"
        );
    }

    #[test]
    fn interleaved_child_cold_start_does_not_break_parent_lane() {
        let mut parent_first = turn(1, Some(900), Some(1_000));
        parent_first.lane_seq = 1;
        let mut child = turn(2, Some(0), Some(1_000));
        child.lane_kind = crate::agent::ExecutionLaneKind::Subagent;
        child.lane_id = "sub-1".to_string();
        child.lane_seq = 1;
        child.prefix_hash = Some("child-prefix".to_string());
        let mut parent_second = turn(3, Some(950), Some(1_000));
        parent_second.lane_seq = 2;

        let report = report_with(vec![parent_first, child, parent_second], Vec::new());
        let diagnoses = diagnose_turns(&report);
        assert_eq!(diagnoses[1].assessment, TurnCacheAssessment::FirstTurn);
        assert_eq!(
            diagnoses[2].assessment,
            TurnCacheAssessment::Warm { read_percent: 950 }
        );
    }

    #[test]
    fn gc_rewrite_explains_cold_turn() {
        let mut cold = turn(2, Some(0), Some(1_000));
        cold.rewrite_kind = ContextRewriteKind::Gc;
        cold.rewrite_saved_tokens = Some(8_500);
        cold.cache_creation_input_tokens = Some(43_000);
        let report = report_with(vec![turn(1, Some(900), Some(1_000)), cold], Vec::new());
        let diagnoses = diagnose_turns(&report);
        assert_eq!(
            diagnoses[1].assessment,
            TurnCacheAssessment::Cold {
                cause: CacheBreakCause::Rewrite(ContextRewriteKind::Gc)
            }
        );
        assert_eq!(
            diagnoses[1].summary,
            "gc rewrite -8.5k · wrote 43k to cache"
        );
        assert!(
            last_turn_verdict(&report)
                .unwrap()
                .starts_with("cache BROKE last turn — gc rewrite")
        );
    }

    #[test]
    fn compaction_event_hash_match_wins_over_rewrite_kind() {
        let mut cold = turn(2, Some(0), Some(1_000));
        cold.rewrite_kind = ContextRewriteKind::Compaction;
        cold.prefix_hash = Some("cccc2222dddd".to_string());
        let event = CompactionEvent {
            seq: 3,
            prefix_hash_before: Some("aaaa1111bbbb".to_string()),
            prefix_hash_after: Some("cccc2222dddd".to_string()),
            ..Default::default()
        };
        let report = report_with(vec![turn(1, Some(900), Some(1_000)), cold], vec![event]);
        let diagnoses = diagnose_turns(&report);
        assert_eq!(
            diagnoses[1].assessment,
            TurnCacheAssessment::Cold {
                cause: CacheBreakCause::Compaction { event_seq: 3 }
            }
        );
        assert!(
            diagnoses[1]
                .summary
                .contains("compaction #3 rewrote prefix")
        );
    }

    #[test]
    fn prefix_churn_without_recorded_rewrite() {
        let mut churned = turn(2, Some(100), Some(1_000));
        churned.prefix_hash = Some("eeee3333ffff".to_string());
        let report = report_with(vec![turn(1, Some(900), Some(1_000)), churned], Vec::new());
        let diagnoses = diagnose_turns(&report);
        assert_eq!(
            diagnoses[1].assessment,
            TurnCacheAssessment::Partial {
                read_percent: 100,
                cause: CacheBreakCause::PrefixChurn
            }
        );
        assert!(diagnoses[1].summary.contains("prefix changed"));
        assert!(diagnoses[1].summary.contains("→eeee3333"));
    }

    #[test]
    fn unexplained_prefix_change_is_cold_not_a_break() {
        // A bare fingerprint change with no recorded rewrite is an inference —
        // on lane-scoped providers usually a benign lane switch — so it must not
        // be asserted as a break the agent caused.
        let mut cold = turn(2, Some(0), Some(1_000));
        cold.prefix_hash = Some("eeee3333ffff".to_string());
        let report = report_with(vec![turn(1, Some(900), Some(1_000)), cold], Vec::new());
        let diagnoses = diagnose_turns(&report);
        assert_eq!(
            diagnoses[1].assessment,
            TurnCacheAssessment::Cold {
                cause: CacheBreakCause::PrefixChurn
            }
        );
        let verdict = last_turn_verdict(&report).unwrap();
        assert!(verdict.starts_with("cache cold last turn"), "{verdict}");
        assert!(!verdict.contains("BROKE"), "{verdict}");
    }

    #[test]
    fn idle_gap_past_ttl_reads_as_expiry() {
        let mut first = turn(1, Some(900), Some(1_000));
        first.provider_id = Some("anthropic".to_string());
        first.model = Some("claude-sonnet-4-5".to_string());
        let mut late = turn(2, Some(0), Some(1_000));
        late.provider_id = first.provider_id.clone();
        late.model = first.model.clone();
        late.created_at_ms = first.created_at_ms + 12 * 60_000;
        let report = report_with(vec![first, late], Vec::new());
        let diagnoses = diagnose_turns(&report);
        assert_eq!(
            diagnoses[1].assessment,
            TurnCacheAssessment::Cold {
                cause: CacheBreakCause::IdleExpiry { idle_ms: 720_000 }
            }
        );
        assert!(diagnoses[1].summary.contains("idle 12m"));
    }

    #[test]
    fn codex_gpt_5_6_uses_its_longer_retention_window() {
        let mut first = turn(1, Some(900), Some(1_000));
        first.provider_id = Some("codex".to_string());
        first.model = Some("gpt-5.6-sol".to_string());
        let mut cold = turn(2, Some(0), Some(1_000));
        cold.provider_id = first.provider_id.clone();
        cold.model = first.model.clone();
        cold.created_at_ms = first.created_at_ms + 12 * 60_000;
        let report = report_with(vec![first, cold], Vec::new());

        assert_eq!(
            diagnose_turns(&report)[1].assessment,
            TurnCacheAssessment::Cold {
                cause: CacheBreakCause::BackendMiss {
                    reusable_percent: None
                }
            }
        );
    }

    #[test]
    fn zeroed_legacy_timestamps_never_claim_idle_expiry() {
        let mut prev = turn(1, Some(900), Some(1_000));
        prev.created_at_ms = 0;
        let mut cold = turn(2, Some(0), Some(1_000));
        cold.created_at_ms = 0;
        let report = report_with(vec![prev, cold], Vec::new());
        let diagnoses = diagnose_turns(&report);
        assert_eq!(
            diagnoses[1].assessment,
            TurnCacheAssessment::Cold {
                cause: CacheBreakCause::BackendMiss {
                    reusable_percent: None
                }
            }
        );
        assert!(diagnoses[1].summary.contains("backend cache miss"));
        assert!(!diagnoses[1].summary.contains("expired"));
    }

    #[test]
    fn stable_route_and_request_prefix_identify_backend_miss() {
        // IMPORTANT DIAGNOSTIC INVARIANT: a byte-stable system prompt does not
        // by itself prove the full cache breakpoint was stable. The request-body
        // and route evidence below let us identify a backend miss while still
        // refusing to invent a private eviction reason.
        let mut first = turn(1, Some(900), Some(1_000));
        first.cache_route_fingerprint = Some("route-aaaa".to_string());
        let mut cold = turn(2, Some(0), Some(1_000));
        cold.expected_cacheable_percent = Some(90);
        cold.actual_cache_read_percent = Some(0);
        cold.cache_route_fingerprint = first.cache_route_fingerprint.clone();
        cold.local_reusable_prefix_percent = Some(94);
        let report = report_with(vec![first, cold], Vec::new());

        let diagnoses = diagnose_turns(&report);

        assert_eq!(
            diagnoses[1].assessment,
            TurnCacheAssessment::Cold {
                cause: CacheBreakCause::BackendMiss {
                    reusable_percent: Some(94)
                }
            }
        );
        assert!(diagnoses[1].summary.contains("backend cache miss"));
        assert!(diagnoses[1].summary.contains("wire prefix 94.0"));
        assert!(diagnoses[1].summary.contains("estimated 90.0% cacheable"));

        // The user-facing verdict must NOT scream "BROKE" for a stable prefix —
        // that reads as our bug when it is a transient provider miss.
        let verdict = last_turn_verdict(&report).unwrap();
        assert!(verdict.starts_with("cache cold last turn"), "{verdict}");
        assert!(!verdict.contains("BROKE"), "{verdict}");
    }

    #[test]
    fn changed_cache_route_is_reported_as_the_miss_reason() {
        let mut first = turn(1, Some(900), Some(1_000));
        first.cache_route_fingerprint = Some("route-one".to_string());
        let mut cold = turn(2, Some(0), Some(1_000));
        cold.cache_route_fingerprint = Some("route-two".to_string());
        cold.local_reusable_prefix_percent = Some(97);
        let report = report_with(vec![first, cold], Vec::new());

        let diagnosis = &diagnose_turns(&report)[1];

        assert_eq!(
            diagnosis.assessment,
            TurnCacheAssessment::Cold {
                cause: CacheBreakCause::RouteChanged
            }
        );
        assert!(diagnosis.summary.contains("cache route changed"));
    }

    #[test]
    fn early_serialized_request_divergence_is_reported_as_the_miss_reason() {
        let mut first = turn(1, Some(900), Some(1_000));
        first.cache_route_fingerprint = Some("route-one".to_string());
        let mut cold = turn(2, Some(0), Some(1_000));
        cold.cache_route_fingerprint = first.cache_route_fingerprint.clone();
        cold.local_reusable_prefix_percent = Some(31);
        let report = report_with(vec![first, cold], Vec::new());

        let diagnosis = &diagnose_turns(&report)[1];

        assert_eq!(
            diagnosis.assessment,
            TurnCacheAssessment::Cold {
                cause: CacheBreakCause::RequestPrefixChanged {
                    reusable_percent: 31
                }
            }
        );
        assert!(diagnosis.summary.contains("diverged at 31%"));
    }

    #[test]
    fn provider_without_cache_telemetry_reports_no_data_not_zero() {
        let report = report_with(vec![turn(1, None, None), turn(2, None, None)], Vec::new());
        let diagnoses = diagnose_turns(&report);
        assert!(
            diagnoses
                .iter()
                .all(|d| d.assessment == TurnCacheAssessment::NoCacheData)
        );
        assert_eq!(
            last_turn_verdict(&report).unwrap(),
            "cache n/a — provider reported no cache data"
        );
    }

    #[test]
    fn missing_prefix_hashes_degrade_to_unknown() {
        let mut prev = turn(1, Some(900), Some(1_000));
        prev.prefix_hash = None;
        let mut cold = turn(2, Some(0), Some(1_000));
        cold.prefix_hash = None;
        let report = report_with(vec![prev, cold], Vec::new());
        let diagnoses = diagnose_turns(&report);
        assert_eq!(
            diagnoses[1].assessment,
            TurnCacheAssessment::Cold {
                cause: CacheBreakCause::Unknown
            }
        );
        assert!(diagnoses[1].summary.contains("no prefix fingerprint"));
    }

    #[test]
    fn interrupted_turns_are_skipped_by_the_verdict() {
        let mut interrupted = turn(3, None, None);
        interrupted.status = UsageTurnStatus::Interrupted;
        let report = report_with(
            vec![
                turn(1, Some(900), Some(1_000)),
                turn(2, Some(950), Some(1_000)),
                interrupted,
            ],
            Vec::new(),
        );
        let diagnoses = diagnose_turns(&report);
        assert_eq!(diagnoses[2].assessment, TurnCacheAssessment::NoUsage);
        assert!(
            last_turn_verdict(&report)
                .unwrap()
                .contains("cache warm (95.0)")
        );
    }
}
