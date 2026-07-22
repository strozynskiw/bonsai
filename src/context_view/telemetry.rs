use super::{ContextNode, ContextNodeKind, ContextReport, ContextRole, UsageTurnReport};
use crate::agent::{ContextRewriteKind, UsageTurnStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextDriverKind {
    System,
    Chat,
    ToolInputs,
    ToolOutputs,
    ToolSchemas,
    Pending,
    Framing,
}

impl ContextDriverKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Chat => "chat",
            Self::ToolInputs => "tool inputs",
            Self::ToolOutputs => "tool outputs",
            Self::ToolSchemas => "tool schemas",
            Self::Pending => "pending",
            Self::Framing => "framing",
        }
    }

    const fn ordered() -> [Self; 7] {
        [
            Self::System,
            Self::Chat,
            Self::ToolOutputs,
            Self::ToolInputs,
            Self::ToolSchemas,
            Self::Pending,
            Self::Framing,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContextDriver {
    pub(crate) kind: ContextDriverKind,
    pub(crate) tokens: usize,
    pub(crate) percent: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CostTelemetry {
    pub(crate) context_percent: usize,
    pub(crate) context_warning: Option<&'static str>,
    pub(crate) session_cache_percent: Option<u64>,
    pub(crate) recent_cache_percent: Option<u64>,
    pub(crate) last_cache_percent: Option<u64>,
    pub(crate) drivers: Vec<ContextDriver>,
}

impl CostTelemetry {
    pub(crate) fn from_report(report: &ContextReport) -> Self {
        let context_percent = context_percent(report);
        let total = prompt_total(report);
        let totals = ContextDriverTotals::from_report(report);
        let drivers = ContextDriverKind::ordered()
            .into_iter()
            .filter_map(|kind| {
                let tokens = totals.tokens(kind);
                (tokens > 0).then_some(ContextDriver {
                    kind,
                    tokens,
                    percent: percent_of(tokens, total),
                })
            })
            .collect();

        Self {
            context_percent,
            context_warning: context_warning(context_percent),
            session_cache_percent: report
                .session_input_cache
                .and_then(|cache| cache.hit_rate_percent()),
            recent_cache_percent: recent_cache_percent(&report.usage_turns),
            last_cache_percent: report
                .last_input_cache
                .and_then(|cache| cache.hit_rate_percent()),
            drivers,
        }
    }

    pub(crate) fn header_label(&self, report: &ContextReport, width: usize) -> String {
        let mut parts = vec![format!("ctx {}%", self.context_percent)];
        if width >= 72 {
            parts.push(session_cost_label(report));
        }
        if width >= 88 {
            parts.push(cache_label(
                session_cache_label(report),
                self.recent_cache_percent,
            ));
        }
        if width >= 112 {
            parts.push(session_io_label(report));
        }
        format!(" {} ", parts.join(" · "))
    }

    pub(crate) fn session_summary_line(&self, report: &ContextReport) -> String {
        let mut parts = vec![
            session_io_label(report),
            session_cost_label(report),
            cache_label(session_cache_label(report), self.session_cache_percent),
        ];
        if let Some(savings) = report.session_savings_micros.filter(|savings| *savings > 0) {
            parts.push(format!("saved {}", format_cost_micros(savings)));
        }
        parts.push(format!(
            "ctx {}/{} ({})",
            compact_tokens(report.used_tokens()),
            compact_tokens(report.budget_tokens),
            percent_label(self.context_percent)
        ));
        parts.join(" · ")
    }

    pub(crate) fn last_turn_summary_line(&self, report: &ContextReport) -> Option<String> {
        let input = report.last_input_tokens()?;
        let output = report.last_completion_tokens?;
        let mut parts = vec![
            format!(
                "↑{} ↓{}",
                compact_tokens(input as usize),
                compact_tokens(output as usize)
            ),
            cache_label("cache", self.last_cache_percent),
            format!(
                "cost {}",
                optional_cost_micros(report.last_turn_cost_micros)
            ),
        ];
        if let Some(savings) = report
            .last_turn_savings_micros
            .filter(|savings| *savings > 0)
        {
            parts.push(format!("saved {}", format_cost_micros(savings)));
        }
        parts.push(format!(
            "ctx {}/{} ({})",
            compact_tokens(report.used_tokens()),
            compact_tokens(report.budget_tokens),
            percent_label(self.context_percent)
        ));
        Some(parts.join(" · "))
    }

    pub(crate) fn usage_lines(&self, report: &ContextReport) -> Vec<String> {
        let mut lines = vec![format!("session {}", self.session_summary_line(report))];
        if let Some(last_turn) = self.last_turn_summary_line(report) {
            lines.push(format!("last turn {last_turn}"));
        } else {
            lines.push("last turn n/a".to_string());
        }
        if let Some(verdict) = super::cache_diagnosis::last_turn_verdict(report) {
            lines.push(verdict);
        }
        lines.extend(self.recent_turn_lines(report));
        lines.extend(self.driver_lines(report));
        lines
    }

    fn recent_turn_lines(&self, report: &ContextReport) -> Vec<String> {
        if report.usage_turns.is_empty() {
            return Vec::new();
        }
        let recent = report.usage_turns.iter().rev().take(12).collect::<Vec<_>>();
        let mut lines = Vec::with_capacity(recent.len() + 1);
        lines.push("Usage: recent turns".to_string());
        for turn in recent.into_iter().rev() {
            lines.push(format_usage_turn_line(turn));
        }
        lines
    }

    pub(crate) fn driver_lines(&self, report: &ContextReport) -> Vec<String> {
        let mut lines = Vec::new();
        let top = self.top_drivers(4);
        if !top.is_empty() {
            let contributors = top
                .into_iter()
                .map(driver_label)
                .collect::<Vec<_>>()
                .join(" · ");
            lines.push(format!("drivers {contributors}"));
        }
        if let Some(tool_outputs) = self.driver(ContextDriverKind::ToolOutputs) {
            lines.push(driver_label(tool_outputs));
        }
        if report.tool_schema_tokens > 0 && self.driver(ContextDriverKind::ToolSchemas).is_none() {
            lines.push(format!(
                "tool schemas {}",
                compact_tokens(report.tool_schema_tokens)
            ));
        }
        if let Some(warning) = self.context_warning {
            lines.push(format!("warning {warning}"));
        }
        lines
    }

    fn driver(&self, kind: ContextDriverKind) -> Option<&ContextDriver> {
        self.drivers.iter().find(|driver| driver.kind == kind)
    }

    fn top_drivers(&self, limit: usize) -> Vec<&ContextDriver> {
        let mut drivers = self.drivers.iter().collect::<Vec<_>>();
        drivers.sort_by(|left, right| {
            right
                .tokens
                .cmp(&left.tokens)
                .then_with(|| left.kind.label().cmp(right.kind.label()))
        });
        drivers.truncate(limit);
        drivers
    }
}

pub(crate) fn context_percent(report: &ContextReport) -> usize {
    prompt_total(report)
        .saturating_mul(100)
        .checked_div(report.budget_tokens.max(1))
        .unwrap_or(0)
}

pub(crate) fn compact_tokens(tokens: usize) -> String {
    if tokens >= 1_000_000_000 {
        let billions = tokens as f64 / 1_000_000_000.0;
        format!("{billions:.2}b")
    } else if tokens >= 1_000_000 {
        format!("{}m", tokens / 1_000_000)
    } else if tokens >= 1_000 {
        let whole = tokens / 1_000;
        let tenth = (tokens % 1_000) / 100;
        if tenth == 0 {
            format!("{whole}k")
        } else {
            format!("{whole}.{tenth}k")
        }
    } else {
        tokens.to_string()
    }
}

pub(crate) fn session_io_label(report: &ContextReport) -> String {
    format!(
        "↑{} ↓{}",
        compact_tokens(report.session_input_tokens() as usize),
        compact_tokens(report.session_completion_tokens as usize)
    )
}

pub(crate) fn session_cost_label(report: &ContextReport) -> String {
    match report.session_cost_micros {
        // Known cost, but an unpriced turn means the real total is higher — mark
        // it as a lower bound so a frozen figure never reads as a stopped counter.
        Some(_) if report.session_cost_is_partial() => {
            format!("cost {}+", optional_cost_micros(report.session_cost_micros))
        }
        Some(_) => format!("cost {}", optional_cost_micros(report.session_cost_micros)),
        // Nothing priced at all: say why rather than a bare "n/a".
        None if !report.unpriced_models().is_empty() => "cost n/a (unpriced model)".to_string(),
        None => format!("cost {}", optional_cost_micros(report.session_cost_micros)),
    }
}

pub(crate) fn optional_cost_micros(cost_micros: Option<u64>) -> String {
    cost_micros
        .map(format_cost_micros)
        .unwrap_or_else(|| "n/a".to_string())
}

pub(crate) fn format_cost_micros(cost_micros: u64) -> String {
    if cost_micros == 0 {
        return "$0".to_string();
    }
    let dollars = cost_micros as f64 / 1_000_000.0;
    if cost_micros >= 1_000_000 {
        format!("${dollars:.2}")
    } else if cost_micros >= 1_000 {
        format!("${dollars:.4}")
    } else {
        format!("${dollars:.6}")
    }
}

fn prompt_total(report: &ContextReport) -> usize {
    report.used_tokens()
}

fn context_warning(percent: usize) -> Option<&'static str> {
    if percent >= 95 {
        Some("context window is nearly full")
    } else if percent >= 85 {
        Some("context window is high")
    } else if percent >= 70 {
        Some("context window is warming up")
    } else {
        None
    }
}

fn cache_label(prefix: &str, percent: Option<u64>) -> String {
    percent
        .map(|percent| format!("{prefix} {percent}%"))
        .unwrap_or_else(|| format!("{prefix} n/a"))
}

fn percent_label(percent: usize) -> String {
    format!("{percent}%")
}

fn driver_label(driver: &ContextDriver) -> String {
    format!(
        "{} {} ({})",
        driver.kind.label(),
        compact_tokens(driver.tokens),
        percent_label(driver.percent)
    )
}

fn format_usage_turn_line(turn: &UsageTurnReport) -> String {
    let input = turn
        .cache_measured_input_tokens
        .or(turn.prompt_tokens)
        .map(format_u64_tokens)
        .unwrap_or_else(|| "n/a".to_string());
    let output = turn
        .completion_tokens
        .map(format_u64_tokens)
        .unwrap_or_else(|| "n/a".to_string());
    let cache = turn_cache_percent(turn)
        .map(|percent| format!("{percent}%"))
        .unwrap_or_else(|| "n/a".to_string());
    let context = turn
        .estimated_prompt_tokens
        .map(compact_tokens)
        .unwrap_or_else(|| "n/a".to_string());
    let mut parts = vec![
        format!("↑{input} ↓{output}"),
        format!("cache {cache}"),
        format!("cost {}", optional_cost_micros(turn.turn_cost_micros)),
        format!("ctx {context}"),
    ];
    let flags = turn_flags(turn);
    if !flags.is_empty() {
        parts.push(format!("flags {}", flags.join(", ")));
    }
    format!(
        "turn {:>3} [{}:{}#{}] {}",
        turn.seq,
        turn.lane_kind.as_db_str(),
        turn.lane_id,
        turn.lane_seq,
        parts.join(" · ")
    )
}

fn session_cache_label(report: &ContextReport) -> &'static str {
    let lanes = report
        .usage_turns
        .iter()
        .map(|turn| (turn.lane_kind, turn.lane_id.as_str()))
        .collect::<std::collections::HashSet<_>>();
    if lanes.len() > 1 {
        "cache (mixed lanes)"
    } else {
        "cache"
    }
}

fn format_u64_tokens(tokens: u64) -> String {
    compact_tokens(usize::try_from(tokens).unwrap_or(usize::MAX))
}

pub(crate) fn turn_cache_percent(turn: &UsageTurnReport) -> Option<u64> {
    turn.cache_read_input_tokens?
        .saturating_mul(100)
        .checked_div(turn.cache_measured_input_tokens?)
}

fn recent_cache_percent(turns: &[UsageTurnReport]) -> Option<u64> {
    let (total, count) = turns
        .iter()
        .rev()
        .take(5)
        .filter_map(turn_cache_percent)
        .fold((0_u64, 0_u64), |(total, count), percent| {
            (total.saturating_add(percent), count + 1)
        });
    total.checked_div(count)
}

fn turn_flags(turn: &UsageTurnReport) -> Vec<String> {
    let mut flags = Vec::new();
    if let Some(reason) = &turn.finish_reason {
        flags.push(format!("finish {}", reason.as_db_str()));
    }
    if turn.reasoning_chars > 0 {
        flags.push(format!(
            "reasoning {} chars",
            compact_tokens(turn.reasoning_chars)
        ));
    }
    if turn_cache_percent(turn).is_some_and(|percent| percent < 50) {
        flags.push("cold".to_string());
    }
    if let Some(expected) = turn.expected_cacheable_percent {
        let actual = turn
            .actual_cache_read_percent
            .map(|percent| percent.to_string())
            .unwrap_or_else(|| "n/a".to_string());
        flags.push(format!("cache exp {expected}%/read {actual}%"));
    }
    match turn.status {
        UsageTurnStatus::Missing => flags.push("missing usage".to_string()),
        UsageTurnStatus::Interrupted => flags.push("interrupted".to_string()),
        UsageTurnStatus::Reported => {}
    }
    match turn.rewrite_kind {
        ContextRewriteKind::None => {}
        kind => {
            let mut flag = format!("rewrite {}", kind.as_db_str());
            if let Some(saved) = turn.rewrite_saved_tokens.filter(|saved| *saved > 0) {
                flag.push_str(&format!(" -{}", compact_tokens(saved)));
            }
            flags.push(flag);
        }
    }
    flags
}

fn percent_of(part: usize, total: usize) -> usize {
    part.saturating_mul(100)
        .checked_div(total.max(1))
        .unwrap_or(0)
}

#[derive(Debug, Default)]
struct ContextDriverTotals {
    /// Tokens per driver, indexed by `ContextDriverKind as usize`.
    by_kind: [usize; 7],
}

impl ContextDriverTotals {
    fn from_report(report: &ContextReport) -> Self {
        let mut totals = Self::default();
        for node in &report.ledger {
            totals.add_totals(Self::for_node(node, None));
        }
        if totals.is_empty() {
            totals.set(
                ContextDriverKind::System,
                report.tokens_for(ContextRole::System),
            );
            totals.set(
                ContextDriverKind::Chat,
                report
                    .tokens_for(ContextRole::User)
                    .saturating_add(report.tokens_for(ContextRole::Assistant)),
            );
            totals.set(
                ContextDriverKind::ToolOutputs,
                report.tokens_for(ContextRole::Tool),
            );
            totals.set(
                ContextDriverKind::ToolSchemas,
                report.tokens_for(ContextRole::ToolSchema),
            );
        }
        totals
    }

    fn for_node(node: &ContextNode, inherited: Option<ContextDriverKind>) -> Self {
        if !node.inclusion.counts_toward_prompt() {
            return Self::default();
        }

        let kind = Self::node_kind(node, inherited);
        let node_tokens = node.counted_tokens();
        if node.children.is_empty() {
            return Self::single(kind, node_tokens);
        }

        let mut child_totals = Self::default();
        for child in &node.children {
            child_totals.add_totals(Self::for_node(child, Some(kind)));
        }

        let child_tokens = child_totals.total();
        if child_tokens == 0 {
            return Self::single(kind, node_tokens);
        }
        if child_tokens <= node_tokens {
            child_totals.add(kind, node_tokens - child_tokens);
            return child_totals;
        }

        child_totals.capped_to_parent(kind, node_tokens)
    }

    fn node_kind(node: &ContextNode, inherited: Option<ContextDriverKind>) -> ContextDriverKind {
        match node.kind {
            ContextNodeKind::SystemRoot
            | ContextNodeKind::Persona
            | ContextNodeKind::ProjectEnvironment
            | ContextNodeKind::VolatileState
            | ContextNodeKind::ProjectInstructions
            | ContextNodeKind::SteeringFile
            | ContextNodeKind::MemoryIndex
            | ContextNodeKind::Summary => ContextDriverKind::System,
            ContextNodeKind::ChatRoot
            | ContextNodeKind::ChatMessage
            | ContextNodeKind::Attachment
            | ContextNodeKind::Mention
            | ContextNodeKind::Background => ContextDriverKind::Chat,
            ContextNodeKind::ToolsRoot
            | ContextNodeKind::ToolCall
            | ContextNodeKind::ToolResult
            | ContextNodeKind::Stdout
            | ContextNodeKind::Stderr
            | ContextNodeKind::OutputText
            | ContextNodeKind::ReadFreshness
            | ContextNodeKind::Diff
            | ContextNodeKind::Image
            | ContextNodeKind::TruncationFile
            | ContextNodeKind::TaskStatus => ContextDriverKind::ToolOutputs,
            ContextNodeKind::ToolInput => ContextDriverKind::ToolInputs,
            ContextNodeKind::ToolSchemasRoot | ContextNodeKind::ToolSchema => {
                ContextDriverKind::ToolSchemas
            }
            ContextNodeKind::PendingRoot
            | ContextNodeKind::QueuedInput
            | ContextNodeKind::ComposerDraft => ContextDriverKind::Pending,
            ContextNodeKind::FramingAdjustment => inherited.unwrap_or(ContextDriverKind::Framing),
            ContextNodeKind::NotSentRoot
            | ContextNodeKind::PlanState
            | ContextNodeKind::TodoState => ContextDriverKind::Pending,
        }
    }

    fn single(kind: ContextDriverKind, tokens: usize) -> Self {
        let mut totals = Self::default();
        totals.add(kind, tokens);
        totals
    }

    fn capped_to_parent(mut self, parent_kind: ContextDriverKind, cap: usize) -> Self {
        if self.total() <= cap {
            return self;
        }

        let parent_tokens = self.tokens(parent_kind);
        let non_parent_tokens = self.total().saturating_sub(parent_tokens);
        if non_parent_tokens < cap {
            self.set(parent_kind, cap - non_parent_tokens);
            return self;
        }

        let mut capped = Self::default();
        let mut remaining = cap;
        for kind in ContextDriverKind::ordered()
            .into_iter()
            .filter(|kind| *kind != parent_kind)
        {
            if remaining == 0 {
                break;
            }
            let tokens = self.tokens(kind).min(remaining);
            capped.add(kind, tokens);
            remaining -= tokens;
        }
        capped
    }

    fn add_totals(&mut self, other: Self) {
        for (slot, added) in self.by_kind.iter_mut().zip(other.by_kind) {
            *slot = slot.saturating_add(added);
        }
    }

    fn add(&mut self, kind: ContextDriverKind, tokens: usize) {
        let slot = &mut self.by_kind[kind as usize];
        *slot = slot.saturating_add(tokens);
    }

    fn set(&mut self, kind: ContextDriverKind, tokens: usize) {
        self.by_kind[kind as usize] = tokens;
    }

    fn tokens(&self, kind: ContextDriverKind) -> usize {
        self.by_kind[kind as usize]
    }

    fn total(&self) -> usize {
        self.by_kind.iter().sum()
    }

    fn is_empty(&self) -> bool {
        self.by_kind.iter().all(|&tokens| tokens == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_view::{
        ContextEntry, ContextInclusion, ContextNodeKind, ContextTokenMetadata,
    };
    use crate::provider::{EstimateConfidence, InputCacheUsage, TokenCounterKind};

    /// A parent-lane turn with reported usage and an optional price. `None` cost
    /// models an unpriced turn (the model has no catalog pricing).
    fn priced_turn(seq: usize, model: &str, cost_micros: Option<u64>) -> UsageTurnReport {
        UsageTurnReport {
            seq,
            lane_kind: crate::agent::ExecutionLaneKind::Parent,
            lane_id: "parent-1".to_string(),
            lane_seq: seq,
            parent_tool_call_id: None,
            launch_group_id: None,
            status: crate::agent::UsageTurnStatus::Reported,
            finish_reason: None,
            reasoning_chars: 0,
            provider_attempts: Vec::new(),
            provider_id: None,
            model: Some(model.to_string()),
            effective_reasoning: None,
            prompt_tokens: Some(1_000),
            completion_tokens: Some(200),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            cache_measured_input_tokens: None,
            turn_cost_micros: cost_micros,
            no_cache_cost_micros: None,
            estimated_prompt_tokens: None,
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
            rewrite_kind: crate::agent::ContextRewriteKind::None,
            rewrite_saved_tokens: None,
            episode_seq: None,
            created_at_ms: 1_700_000_000_000 + seq as i64 * 30_000,
            latency_ms: None,
            ttft_ms: None,
            prefix_hash: None,
            inspection_executed: 0,
            inspection_reused: 0,
            inspection_rejected: 0,
            inspection_returned_chars: 0,
            inspection_avoided_chars: 0,
            delegated_parent_overlap: 0,
        }
    }

    #[test]
    fn session_cost_label_marks_partial_when_a_turn_ran_unpriced() {
        // Priced planning turn, then an unpriced coding turn (session 23).
        let report = ContextReport {
            session_cost_micros: Some(73_710),
            usage_turns: vec![
                priced_turn(1, "deepseek/deepseek-v4-pro", Some(73_710)),
                priced_turn(2, "moonshotai/kimi-k3", None),
            ],
            ..ContextReport::default()
        };
        assert!(report.session_cost_is_partial());
        assert_eq!(
            report.unpriced_models(),
            vec!["moonshotai/kimi-k3".to_string()]
        );
        assert!(
            session_cost_label(&report).ends_with('+'),
            "partial cost must carry a `+` lower-bound marker: {}",
            session_cost_label(&report)
        );
    }

    #[test]
    fn session_cost_label_is_exact_when_every_turn_priced() {
        let report = ContextReport {
            session_cost_micros: Some(73_710),
            usage_turns: vec![priced_turn(1, "deepseek/deepseek-v4-pro", Some(73_710))],
            ..ContextReport::default()
        };
        assert!(!report.session_cost_is_partial());
        assert!(!session_cost_label(&report).contains('+'));
    }

    #[test]
    fn session_cost_label_names_fully_unpriced_session() {
        let report = ContextReport {
            session_cost_micros: None,
            usage_turns: vec![priced_turn(1, "moonshotai/kimi-k3", None)],
            ..ContextReport::default()
        };
        assert_eq!(session_cost_label(&report), "cost n/a (unpriced model)");
    }

    fn token_metadata() -> ContextTokenMetadata {
        ContextTokenMetadata {
            source: TokenCounterKind::Heuristic,
            confidence: EstimateConfidence::Low,
        }
    }

    fn report() -> ContextReport {
        ContextReport {
            budget_tokens: 10_000,
            entries: vec![
                ContextEntry {
                    role: ContextRole::System,
                    tokens: 1_000,
                    text: "system".to_string(),
                },
                ContextEntry {
                    role: ContextRole::Tool,
                    tokens: 4_000,
                    text: "tool".to_string(),
                },
                ContextEntry {
                    role: ContextRole::User,
                    tokens: 500,
                    text: "user".to_string(),
                },
            ],
            ledger: vec![
                ContextNode::parent(
                    "root-tools",
                    ContextNodeKind::ToolsRoot,
                    ContextInclusion::Included,
                    Some(ContextRole::Tool),
                    "Tools",
                    4_000,
                    "tool",
                    token_metadata(),
                    vec![ContextNode::parent(
                        "tool-call",
                        ContextNodeKind::ToolCall,
                        ContextInclusion::Included,
                        Some(ContextRole::Tool),
                        "read",
                        4_000,
                        "tool",
                        token_metadata(),
                        vec![
                            ContextNode::leaf(
                                "tool-call-input",
                                ContextNodeKind::ToolInput,
                                ContextInclusion::Included,
                                Some(ContextRole::Tool),
                                "Input JSON",
                                250,
                                "{}",
                                token_metadata(),
                            ),
                            ContextNode::leaf(
                                "tool-call-output",
                                ContextNodeKind::OutputText,
                                ContextInclusion::Included,
                                Some(ContextRole::Tool),
                                "Model-visible output",
                                3_750,
                                "file",
                                token_metadata(),
                            ),
                        ],
                    )],
                ),
                ContextNode::leaf(
                    "system",
                    ContextNodeKind::SystemRoot,
                    ContextInclusion::Included,
                    Some(ContextRole::System),
                    "System",
                    1_000,
                    "system",
                    token_metadata(),
                ),
            ],
            prompt_estimate_tokens: 5_500,
            tool_schema_tokens: 250,
            last_prompt_tokens: Some(5_500),
            last_completion_tokens: Some(700),
            last_input_cache: Some(InputCacheUsage::new(2_750, 0, 5_500)),
            last_turn_cost_micros: Some(12_345),
            last_turn_savings_micros: Some(2_000),
            session_prompt_tokens: 12_000,
            session_completion_tokens: 1_200,
            session_input_cache: Some(InputCacheUsage::new(6_000, 0, 12_000)),
            session_cost_micros: Some(42_000),
            session_savings_micros: Some(8_000),
            ..Default::default()
        }
    }

    fn overlapping_tool_report() -> ContextReport {
        ContextReport {
            budget_tokens: 1_000,
            entries: vec![ContextEntry {
                role: ContextRole::Tool,
                tokens: 100,
                text: "tool".to_string(),
            }],
            ledger: vec![ContextNode::parent(
                "root-tools",
                ContextNodeKind::ToolsRoot,
                ContextInclusion::Included,
                Some(ContextRole::Tool),
                "Tools",
                100,
                "tool",
                token_metadata(),
                vec![ContextNode::parent(
                    "tool-call",
                    ContextNodeKind::ToolCall,
                    ContextInclusion::Included,
                    Some(ContextRole::Tool),
                    "bash",
                    100,
                    "tool",
                    token_metadata(),
                    vec![
                        ContextNode::leaf(
                            "tool-call-input",
                            ContextNodeKind::ToolInput,
                            ContextInclusion::Included,
                            Some(ContextRole::Tool),
                            "Input JSON",
                            10,
                            "{}",
                            token_metadata(),
                        ),
                        ContextNode::leaf(
                            "tool-call-output",
                            ContextNodeKind::OutputText,
                            ContextInclusion::Included,
                            Some(ContextRole::Tool),
                            "Model-visible output",
                            100,
                            "rendered",
                            token_metadata(),
                        ),
                        ContextNode::leaf(
                            "tool-call-stdout",
                            ContextNodeKind::Stdout,
                            ContextInclusion::Included,
                            Some(ContextRole::Tool),
                            "stdout",
                            80,
                            "stdout",
                            token_metadata(),
                        ),
                    ],
                )],
            )],
            prompt_estimate_tokens: 100,
            ..Default::default()
        }
    }

    fn usage_turn(
        seq: usize,
        status: UsageTurnStatus,
        cache_read: Option<u64>,
        rewrite_kind: ContextRewriteKind,
    ) -> UsageTurnReport {
        UsageTurnReport {
            seq,
            lane_kind: crate::agent::ExecutionLaneKind::Parent,
            lane_id: "parent-1".to_string(),
            lane_seq: seq,
            parent_tool_call_id: None,
            launch_group_id: None,
            status,
            finish_reason: None,
            reasoning_chars: 0,
            provider_attempts: Vec::new(),
            provider_id: None,
            model: None,
            effective_reasoning: None,
            prompt_tokens: Some(1_000),
            completion_tokens: Some(200),
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: cache_read.map(|_| 0),
            cache_measured_input_tokens: cache_read.map(|_| 1_000),
            turn_cost_micros: Some(123),
            no_cache_cost_micros: Some(456),
            estimated_prompt_tokens: Some(9_000),
            estimate_source: Some(crate::provider::TokenCounterKind::Heuristic),
            estimate_confidence: Some(crate::provider::EstimateConfidence::Low),
            tool_schema_tokens: Some(100),
            tool_schema_hash: None,
            tool_schema_names: Vec::new(),
            request_body_bytes: None,
            request_body_hash: None,
            cache_mechanism: None,
            cache_route_fingerprint: None,
            expected_cacheable_percent: None,
            actual_cache_read_percent: cache_read.map(|read| read.saturating_mul(100) / 1_000),
            local_reusable_prefix_tokens: Some(8_000),
            local_reusable_prefix_percent: Some(80),
            cacheable_prefix_tokens: Some(8_000),
            volatile_tail_tokens: Some(1_000),
            context_window_tokens: Some(128_000),
            rewrite_kind,
            rewrite_saved_tokens: Some(8_500),
            episode_seq: None,
            created_at_ms: 1_700_000_000_000 + seq as i64 * 60_000,
            latency_ms: Some(3_400),
            ttft_ms: Some(600),
            prefix_hash: Some("a1b2c3d4".to_string()),
            inspection_executed: 0,
            inspection_reused: 0,
            inspection_rejected: 0,
            inspection_returned_chars: 0,
            inspection_avoided_chars: 0,
            delegated_parent_overlap: 0,
        }
    }

    #[test]
    fn status_cache_percentage_averages_last_five_rounds() {
        let mut report = report();
        report.usage_turns = (0..11)
            .map(|seq| {
                usage_turn(
                    seq,
                    UsageTurnStatus::Reported,
                    Some(if seq == 0 { 1_000 } else { seq as u64 * 100 }),
                    ContextRewriteKind::None,
                )
            })
            .collect();

        let telemetry = CostTelemetry::from_report(&report);

        assert_eq!(telemetry.recent_cache_percent, Some(80));
        assert!(telemetry.header_label(&report, 88).contains("cache 80%"));
    }

    #[test]
    fn telemetry_summarizes_cost_cache_and_context_drivers() {
        let report = report();
        let telemetry = CostTelemetry::from_report(&report);

        assert_eq!(telemetry.context_percent, 55);
        assert_eq!(telemetry.session_cache_percent, Some(50));
        assert_eq!(telemetry.recent_cache_percent, None);
        assert_eq!(telemetry.last_cache_percent, Some(50));
        assert_eq!(
            telemetry.driver(ContextDriverKind::ToolOutputs),
            Some(&ContextDriver {
                kind: ContextDriverKind::ToolOutputs,
                tokens: 3_750,
                percent: 68,
            })
        );
        assert_eq!(
            telemetry.driver(ContextDriverKind::ToolInputs),
            Some(&ContextDriver {
                kind: ContextDriverKind::ToolInputs,
                tokens: 250,
                percent: 4,
            })
        );
        assert!(
            telemetry
                .session_summary_line(&report)
                .contains("cost $0.0420")
        );
        assert!(
            telemetry
                .last_turn_summary_line(&report)
                .unwrap()
                .contains("cache 50%")
        );
    }

    #[test]
    fn telemetry_formats_compact_tokens_and_session_io() {
        assert_eq!(compact_tokens(999), "999");
        assert_eq!(compact_tokens(1_000), "1k");
        assert_eq!(compact_tokens(12_500), "12.5k");
        assert_eq!(compact_tokens(2_000_000), "2m");

        let report = ContextReport {
            session_prompt_tokens: 0,
            session_completion_tokens: 12_000,
            session_input_cache: Some(InputCacheUsage::new(42_100, 0, 42_100)),
            ..Default::default()
        };

        assert_eq!(session_io_label(&report), "↑42.1k ↓12k");
    }

    #[test]
    fn driver_lines_attribute_tool_output_separately() {
        let text = CostTelemetry::from_report(&report())
            .driver_lines(&report())
            .join("\n");

        assert!(text.contains("drivers tool outputs 3.7k (68%)"));
        assert!(text.contains("tool outputs 3.7k (68%)"));
    }

    #[test]
    fn usage_lines_include_recent_turn_flags() {
        let mut report = report();
        report.usage_turns = vec![
            usage_turn(
                1,
                UsageTurnStatus::Reported,
                Some(900),
                ContextRewriteKind::None,
            ),
            usage_turn(2, UsageTurnStatus::Missing, None, ContextRewriteKind::Gc),
            usage_turn(
                3,
                UsageTurnStatus::Interrupted,
                Some(100),
                ContextRewriteKind::Compaction,
            ),
        ];

        let text = CostTelemetry::from_report(&report)
            .usage_lines(&report)
            .join("\n");

        assert!(text.contains("Usage: recent turns"), "{text}");
        assert!(text.contains("turn   2"), "{text}");
        assert!(text.contains("missing usage"), "{text}");
        assert!(text.contains("rewrite gc -8.5k"), "{text}");
        assert!(text.contains("interrupted"), "{text}");
        assert!(text.contains("cold"), "{text}");
        assert!(text.contains("rewrite compaction -8.5k"), "{text}");
    }

    #[test]
    fn telemetry_caps_overlapping_child_rows_to_parent_tokens() {
        let telemetry = CostTelemetry::from_report(&overlapping_tool_report());
        let total = telemetry
            .drivers
            .iter()
            .map(|driver| driver.tokens)
            .sum::<usize>();

        assert_eq!(total, 100);
        assert_eq!(
            telemetry.driver(ContextDriverKind::ToolInputs),
            Some(&ContextDriver {
                kind: ContextDriverKind::ToolInputs,
                tokens: 10,
                percent: 10,
            })
        );
        assert_eq!(
            telemetry.driver(ContextDriverKind::ToolOutputs),
            Some(&ContextDriver {
                kind: ContextDriverKind::ToolOutputs,
                tokens: 90,
                percent: 90,
            })
        );
    }
}
