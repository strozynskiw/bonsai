//! Building the compaction draft: deciding which message groups to omit, which
//! tool outputs to stub, and rendering the stub text. Pure planning — no
//! mutation of agent state.

use super::*;

impl Agent {
    /// Find the retained, fresh read behind an earlier compact reuse when it
    /// already covers this incoming structured read. The run loop uses this to
    /// return real bytes when a model asks again after receiving a pointer.
    pub(in crate::agent) fn current_covering_compact_reuse_target(
        &self,
        tool_call: &ToolCall,
    ) -> Option<String> {
        let current = self.tool_context_details.get(&tool_call.id)?;
        self.inspection_events
            .iter()
            .find_map(|(reuse_call_id, admission)| {
                if admission.outcome != InspectionOutcome::Reused
                    || !self.messages.iter().enumerate().any(|(index, message)| {
                        tool_message_call_id(message).as_deref() == Some(reuse_call_id)
                            && !self.message_has_control(index, message, |state| {
                                state.stubbed || state.drop_next_turn
                            })
                    })
                {
                    return None;
                }
                let target_call_id = admission.reuse_target_tool_call_id.as_ref()?;
                let target = self.tool_context_details.get(target_call_id)?;
                let evidence = target.read_evidence.as_ref()?;
                if !evidence.observation_is_current() {
                    return None;
                }
                read_detail_is_covered(target, current).then(|| target_call_id.clone())
            })
    }

    /// Plan a compaction that brings the prompt down to `target_tokens` with the
    /// least information loss and least cost, in graduated tiers:
    ///
    /// - **Tier 0** — if the prompt is already within target, change nothing.
    /// - **Tier 1** — evict (stub) large tool outputs oldest-first. Cheap, needs
    ///   no provider call, and the originals stay restorable via `/ctx`. Stops as
    ///   soon as the estimate is under target, so recent tool outputs stay
    ///   verbatim when possible.
    /// - **Tier 2** — only if stubbing every eligible tool output is still not
    ///   enough, omit the oldest message groups (collected into a summary row)
    ///   until under target. This is the only tier that triggers the hidden
    ///   provider summary call.
    pub(in crate::agent) fn build_compaction_draft(&self, target_tokens: usize) -> CompactionDraft {
        let groups = message_groups(&self.messages);
        let tool_schema = self.active_tool_schema();
        let (message_tokens, estimate) = self.payload_message_token_estimate(
            &self.messages,
            &self.message_ids_for_messages(self.messages.len()),
            &self.context_controls,
            &tool_schema,
        );
        let latest_user =
            latest_user_message_indices(&self.messages, COMPACTION_LATEST_USER_MESSAGES_TO_KEEP);
        let mandatory_tail = last_group_indices(&groups, COMPACTION_PROTECTED_TAIL_GROUPS);
        let read_reuse_targets = self.read_reuse_target_indices();

        let mut estimated = estimate.input_tokens;

        // Tier 1: stub bulky tool outputs until within target. Superseded reads
        // (a later read/edit/write touched the same path) are evicted first since
        // they're stale or redundant; everything else follows oldest-first.
        // `stub_tokens` tracks each stub's post-eviction size so Tier 2 can price
        // a group accurately if it ends up omitting one of these messages anyway.
        let mut stubs: HashMap<usize, CompactionStub> = HashMap::new();
        let mut stub_tokens: HashMap<usize, usize> = HashMap::new();
        if estimated > target_tokens {
            for index in self.tier1_eviction_order(&read_reuse_targets) {
                if estimated <= target_tokens {
                    break;
                }
                if read_reuse_targets.contains(&index) {
                    continue;
                }
                let message = &self.messages[index];
                let Some(stub) = self.compaction_stub_for_message(index, message) else {
                    continue;
                };
                let original = message_tokens.get(index).copied().unwrap_or(0);
                let stubbed = estimated_stub_tokens(&stub.replacement);
                estimated = estimated.saturating_sub(original.saturating_sub(stubbed));
                stub_tokens.insert(index, stubbed);
                stubs.insert(index, stub);
            }
        }

        // Tier 2: summarize the oldest non-mandatory groups, oldest-first, until
        // within target. Prices each group at its current (post-stub) size, and
        // budgets for the summary row that replaces the omitted groups.
        let mut omitted_groups = HashSet::new();
        if estimated > target_tokens {
            for (group_index, group) in groups.iter().enumerate() {
                let summary_cost = if omitted_groups.is_empty() {
                    0
                } else {
                    estimated_compaction_summary_tokens(omitted_groups.len())
                };
                if estimated.saturating_add(summary_cost) <= target_tokens {
                    break;
                }
                if group_is_mandatory(self, group, &latest_user)
                    || mandatory_tail.contains(&group_index)
                    || group
                        .indices
                        .iter()
                        .any(|index| read_reuse_targets.contains(index))
                {
                    continue;
                }
                let current_tokens: usize = group
                    .indices
                    .iter()
                    .map(|index| {
                        stub_tokens
                            .get(index)
                            .copied()
                            .or_else(|| message_tokens.get(*index).copied())
                            .unwrap_or(0)
                    })
                    .sum();
                omitted_groups.insert(group_index);
                estimated = estimated.saturating_sub(current_tokens);
            }
        }

        let omitted_indices = groups
            .iter()
            .enumerate()
            .filter(|(index, _group)| omitted_groups.contains(index))
            .flat_map(|(_index, group)| group.indices.iter().copied())
            .collect::<HashSet<_>>();

        // A group that is summarized away no longer needs its tool outputs
        // stubbed — drop those stubs so the work isn't double-counted.
        stubs.retain(|index, _stub| !omitted_indices.contains(index));

        let mut omitted = Vec::new();
        let mut kept = Vec::new();
        // Only kept messages need to be owned; omitted ones are read by
        // reference to gather their restore sources, so don't clone them.
        for (index, message) in self.messages.iter().enumerate() {
            if index != 0 && omitted_indices.contains(&index) {
                omitted.push(CompactionOmittedMessage {
                    originals: self.original_sources_for_message(index, message),
                });
            } else {
                kept.push(CompactionKeptMessage {
                    old_index: index,
                    message: message.clone(),
                });
            }
        }

        let mut tool_outputs_to_stub = stubs.into_values().collect::<Vec<_>>();
        tool_outputs_to_stub.sort_by_key(|stub| stub.old_index);

        CompactionDraft {
            messages_omitted: omitted.len(),
            omitted,
            kept,
            tool_outputs_to_stub,
            target_tokens,
        }
    }

    /// Order in which Tier 1 considers messages for stubbing: superseded reads
    /// first (lowest-value), then everything else oldest-first. Takes the
    /// caller's already-computed reuse targets so a single
    /// `build_compaction_draft` pass never recomputes them.
    fn tier1_eviction_order(&self, read_reuse_targets: &HashSet<usize>) -> Vec<usize> {
        let superseded = self.superseded_read_indices_with_targets(read_reuse_targets);
        let len = self.messages.len();
        let mut order: Vec<usize> = (0..len)
            .filter(|index| superseded.contains(index))
            .collect();
        order.extend((0..len).filter(|index| !superseded.contains(index)));
        order
    }

    /// Message indices of structured read outputs that a later tool call has
    /// made redundant or stale — a later covering read (newer copy exists) or
    /// a file edit on the same path (content changed). These are the
    /// lowest-value tool outputs to evict during compaction, so Tier 1 stubs
    /// them first. Detection uses typed read evidence and edit results rather
    /// than a tool-name allowlist; the originals stay restorable via `/ctx`
    /// like any other stub.
    pub(in crate::agent) fn superseded_read_indices(&self) -> HashSet<usize> {
        self.superseded_read_indices_with_targets(&self.read_reuse_target_indices())
    }

    /// Core of [`Self::superseded_read_indices`], taking the reuse
    /// targets as a parameter so a caller that already computed them (like
    /// [`Self::build_compaction_draft`]) doesn't pay for a second scan.
    fn superseded_read_indices_with_targets(
        &self,
        read_reuse_targets: &HashSet<usize>,
    ) -> HashSet<usize> {
        let mut latest_write_by_path: HashMap<String, usize> = HashMap::new();
        let mut reads: Vec<(usize, ReadWindowKey)> = Vec::new();
        for (index, message) in self.messages.iter().enumerate() {
            let Some(call_id) = tool_message_call_id(message) else {
                continue;
            };
            let Some(detail) = self.tool_context_details.get(&call_id) else {
                continue;
            };
            if let Some(key) = structured_read_window_for_supersession(detail) {
                reads.push((index, key));
                continue;
            }
            if matches!(detail.result, ToolContextResult::Edit { .. })
                && let Some(path) = tool_argument_string(&detail.arguments, "path")
            {
                latest_write_by_path.insert(normalize_path_for_supersession(&path), index);
            }
        }
        reads
            .iter()
            .filter(|(index, key)| {
                !read_reuse_targets.contains(index)
                    && (reads.iter().any(|(later_index, later_key)| {
                        index < later_index && later_key.covers(key)
                    }) || latest_write_by_path
                        .get(&key.path)
                        .is_some_and(|last| index < last))
            })
            .map(|(index, _key)| *index)
            .collect()
    }

    pub(in crate::agent) fn original_sources_for_message(
        &self,
        index: usize,
        message: &ChatCompletionRequestMessage,
    ) -> Vec<ChatCompletionRequestMessage> {
        let message_id = self.message_id_or_synthetic(index);
        if let Some(source) = self.summary_sources.get(&message_id) {
            return source.clone();
        }
        if let Some(call_id) = tool_message_call_id(message)
            && let Some(source) = self
                .summary_sources
                .get(&ContextNodeId::tool(&call_id).into_string())
        {
            return source.clone();
        }
        vec![message.clone()]
    }

    pub(super) fn compaction_stub_for_message(
        &self,
        old_index: usize,
        message: &ChatCompletionRequestMessage,
    ) -> Option<CompactionStub> {
        self.tool_output_stub_for_message(
            old_index,
            message,
            COMPACTION_TOOL_OUTPUT_STUB_MIN_CHARS,
            None,
        )
    }

    pub(super) fn tool_output_stub_for_message(
        &self,
        old_index: usize,
        message: &ChatCompletionRequestMessage,
        min_chars: usize,
        reason: Option<ContextStubReason>,
    ) -> Option<CompactionStub> {
        if old_index == 0 || self.message_has_control(old_index, message, |state| state.pinned) {
            return None;
        }
        let call_id = tool_message_call_id(message)?;
        let value = serde_json::to_value(message).ok()?;
        let original = message_content_text(&value);
        if original.trim_start().starts_with("[Compacted tool output]")
            || original.chars().count() < min_chars
        {
            return None;
        }
        let replacement = compacted_tool_output_message(
            &call_id,
            &self.compacted_tool_output_text_with_reason(&call_id, &original, reason),
        )?;
        Some(CompactionStub {
            old_index,
            tool_id: ContextNodeId::tool(&call_id).into_string(),
            original: message.clone(),
            replacement,
        })
    }

    pub(super) fn compacted_tool_output_text(&self, call_id: &str, original: &str) -> String {
        self.compacted_tool_output_text_with_reason(call_id, original, None)
    }

    pub(in crate::agent) fn compacted_tool_output_text_with_reason(
        &self,
        call_id: &str,
        original: &str,
        reason: Option<ContextStubReason>,
    ) -> String {
        let detail = self.tool_context_details.get(call_id);
        let preview = evidence_preview(original, COMPACTION_TOOL_OUTPUT_STUB_PREVIEW_CHARS);
        let mut lines = vec![
            "[Compacted tool output]".to_string(),
            format!("call_id: {call_id}"),
            format!("original_chars: {}", original.chars().count()),
            format!("original_bytes: {}", original.len()),
        ];
        if let Some(reason) = reason {
            lines.push(format!("retention_reason: {}", reason.label()));
        }
        if let Some(detail) = detail {
            lines.push(format!("tool: {}", detail.name));
            if let Some(command) = tool_argument_string(&detail.arguments, "command") {
                lines.push(format!("command: {command}"));
            }
            if let Some(path) = tool_argument_string(&detail.arguments, "path") {
                lines.push(format!("path: {path}"));
            }
            match &detail.result {
                ToolContextResult::Command {
                    exit_code,
                    timed_out,
                    truncation,
                    ..
                } => {
                    lines.push(command_status_text(*exit_code, *timed_out));
                    if let Some(truncation) = truncation {
                        lines.push(format!("truncation_file: {}", truncation.path));
                        lines.push(format!(
                            "truncation_total_chars: {}",
                            truncation.total_chars
                        ));
                        lines.push(format!(
                            "truncation_preview_chars: {}",
                            truncation.preview_chars
                        ));
                    }
                }
                ToolContextResult::Edit {
                    summary,
                    diff_preview,
                } => {
                    lines.push(format!("edit_summary: {summary}"));
                    lines.push(format!(
                        "diff_preview: {}",
                        one_line_preview(diff_preview, 240)
                    ));
                }
                ToolContextResult::Image { description, image } => {
                    lines.push(format!("image_description: {description}"));
                    lines.push(format!("image_mime_type: {}", image.mime_type));
                    lines.push(format!("image_base64_bytes: {}", image.base64_bytes));
                }
                ToolContextResult::BackgroundTaskStarted { task_id, message } => {
                    lines.push(format!("background_task_id: {task_id}"));
                    lines.push(format!(
                        "background_message: {}",
                        one_line_preview(message, 240)
                    ));
                }
                ToolContextResult::SubagentStarted {
                    subtask_id,
                    message,
                } => {
                    lines.push(format!("background_subagent_id: {subtask_id}"));
                    lines.push(format!(
                        "background_subagent_message: {}",
                        one_line_preview(message, 240)
                    ));
                }
                ToolContextResult::Text { .. } => {}
            }
        }
        lines.push("restore: full original is available from /ctx".to_string());
        match reason {
            Some(ContextStubReason::SupersededRead) => {
                lines.push(
                    "evidence_preview: omitted because a later read/edit for this path is retained"
                        .to_string(),
                );
            }
            Some(ContextStubReason::User) => {
                lines.push("evidence_preview: omitted by user stub control".to_string());
            }
            Some(ContextStubReason::OldSuccessfulToolOutput) | None => {
                lines.push("evidence_preview:".to_string());
                lines.push(preview);
            }
        }
        lines.join("\n")
    }

    pub(in crate::agent) fn read_reuse_target_indices(&self) -> HashSet<usize> {
        let target_call_ids = self
            .tool_context_details
            .values()
            .filter_map(read_reuse_target_call_id)
            .map(str::to_string)
            .collect::<HashSet<_>>();
        if target_call_ids.is_empty() {
            return HashSet::new();
        }

        self.messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| {
                tool_message_call_id(message)
                    .filter(|call_id| target_call_ids.contains(call_id))
                    .map(|_call_id| index)
            })
            .collect()
    }
}

/// Typed displayed window used by compaction supersession. The narrow legacy
/// `read` fallback keeps old persisted sessions eligible; current read tools
/// all attach [`ReadEvidence`](crate::tool::ReadEvidence), so new tools join
/// this policy without another name-based branch.
fn structured_read_window_for_supersession(detail: &ToolContextDetail) -> Option<ReadWindowKey> {
    match read_signature_for_detail(detail) {
        Some(ReadSignature::StructuredRead(snapshot)) => Some(snapshot.window),
        Some(ReadSignature::BashRead { .. }) => None,
        None if detail.name == "read" && !read_detail_is_reuse_pointer(detail) => {
            let path = tool_argument_string(&detail.arguments, "path")?;
            Some(ReadWindowKey::from_arguments(
                normalize_path_for_supersession(&path),
                &detail.arguments,
            ))
        }
        None => None,
    }
}

/// Cheap token estimate for a compaction stub message, used only to decide how
/// many tool outputs to evict to reach the target. The stub is bounded by
/// `COMPACTION_TOOL_OUTPUT_STUB_PREVIEW_CHARS` plus a little metadata, so a
/// chars/4 heuristic is accurate enough; the report's real `after_tokens` comes
/// from the prompt estimator over the final candidate.
/// Conservative path normalization for supersession matching: a mismatch only
/// causes a missed supersession (safe), never a wrong eviction, so light
/// canonicalization (trim, strip a leading `./`) is enough.
fn normalize_path_for_supersession(path: &str) -> String {
    let trimmed = path.trim();
    trimmed.strip_prefix("./").unwrap_or(trimmed).to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReadWindowKey {
    path: String,
    start_line: usize,
    end_line: Option<usize>,
    total_lines: Option<usize>,
    depth: Option<usize>,
}

impl ReadWindowKey {
    fn from_detail(
        path: String,
        arguments: &str,
        evidence: Option<&crate::tool::ReadEvidence>,
    ) -> Self {
        if let Some(evidence) = evidence {
            let window = evidence.observation().window();
            return Self {
                path,
                start_line: window.start_line,
                end_line: window.end_line,
                total_lines: window.total_lines,
                depth: tool_argument_usize(arguments, "depth"),
            };
        }
        Self::from_arguments(path, arguments)
    }

    fn from_arguments(path: String, arguments: &str) -> Self {
        let start_line = tool_argument_usize(arguments, "offset").unwrap_or(1);
        let requested_limit = tool_argument_usize(arguments, "limit").unwrap_or(2000);
        Self {
            path,
            start_line,
            end_line: Some(start_line.saturating_add(requested_limit).saturating_sub(1)),
            total_lines: None,
            depth: tool_argument_usize(arguments, "depth"),
        }
    }

    fn covers(&self, other: &Self) -> bool {
        if self.path != other.path || self.depth != other.depth {
            return false;
        }
        if self.start_line > other.start_line {
            return false;
        }
        match (self.end_line, other.end_line) {
            (Some(self_end), Some(other_end)) => self_end >= other_end,
            // A read whose end is unknown is not a stable coverage target.
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadObservationSnapshot {
    window: ReadWindowKey,
    file_digest_at_read: Option<blake3::Hash>,
}

/// Identifies a tool output that displays a single file's content, so a repeated
/// identical read can be collapsed to a pointer. The two tools are kept distinct
/// so a `bash` read never points at a `read`-tool copy (their rendered text
/// differs anyway, but the separation makes that explicit).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReadSignature {
    /// A typed read output, keyed by its displayed window and immutable file digest.
    StructuredRead(ReadObservationSnapshot),
    /// A read-only `bash` command (`cat`/`head`/`tail`) of one in-tree file,
    /// keyed by that path.
    BashRead { path: String },
}

fn read_detail_is_covered(covering: &ToolContextDetail, current: &ToolContextDetail) -> bool {
    let (
        Some(ReadSignature::StructuredRead(covering)),
        Some(ReadSignature::StructuredRead(current)),
    ) = (
        read_signature_for_detail(covering),
        read_signature_for_detail(current),
    )
    else {
        return false;
    };

    covering.file_digest_at_read == current.file_digest_at_read
        && covering.window.covers(&current.window)
}

impl ReadSignature {
    fn path(&self) -> &str {
        match self {
            Self::StructuredRead(snapshot) => &snapshot.window.path,
            Self::BashRead { path } => path,
        }
    }
}

/// The reuse signature of a tool result, or `None` when it is not a single-file
/// read (or is itself already a reuse pointer, which must never be re-matched).
fn read_signature_for_detail(detail: &ToolContextDetail) -> Option<ReadSignature> {
    if read_detail_is_reuse_pointer(detail) {
        return None;
    }
    if let Some(evidence) = detail.read_evidence.as_ref() {
        let observation = evidence.observation();
        return Some(ReadSignature::StructuredRead(ReadObservationSnapshot {
            window: ReadWindowKey::from_detail(
                normalize_path_for_supersession(observation.display_path()),
                &detail.arguments,
                Some(evidence),
            ),
            file_digest_at_read: observation.file_digest_at_read(),
        }));
    }
    match detail.name.as_str() {
        "bash" => {
            // Only a clean, successful single-file read qualifies; a non-zero
            // exit or timeout may have produced partial or error output.
            let ToolContextResult::Command {
                stdout,
                exit_code: Some(0),
                timed_out: false,
                ..
            } = &detail.result
            else {
                return None;
            };
            let command = tool_argument_string(&detail.arguments, "command")?;
            let path = crate::tool::single_read_path(&command, stdout)?;
            Some(ReadSignature::BashRead {
                path: normalize_path_for_supersession(&path),
            })
        }
        _ => None,
    }
}

fn tool_argument_usize(arguments: &str, key: &str) -> Option<usize> {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get(key)
                .and_then(serde_json::Value::as_u64)
                .and_then(|number| usize::try_from(number).ok())
        })
}

pub(in crate::agent) enum ReadAdmission {
    Execute(InspectionReason),
    Reuse(ReadReuse),
    Reject(ReadReuse),
}

pub(in crate::agent) struct ReadReuse {
    pub(in crate::agent) target_call_id: String,
    pub(in crate::agent) pointer: String,
}

impl Agent {
    /// Decide whether a completed read must append its real bytes or can point
    /// at fresh, still-live parent-visible coverage. The tool always executes
    /// first, so this policy compares current output/evidence and never reuses
    /// across a stale, changed, stubbed, or dropped target.
    pub(in crate::agent) fn read_admission(
        &self,
        new_call_id: &str,
        new_rendered: &str,
    ) -> ReadAdmission {
        let Some(detail) = self.tool_context_details.get(new_call_id) else {
            return ReadAdmission::Execute(InspectionReason::NotReusable);
        };
        if let Some(key) = git_inspection_key(detail) {
            return self.git_inspection_admission(&key, new_rendered);
        }
        let Some(signature) = read_signature_for_detail(detail) else {
            return ReadAdmission::Execute(InspectionReason::NotReusable);
        };

        // Scan newest-first for the most recent live prior read that proves this
        // read's visible bytes are already available in context. Exact windows
        // still match, and read-tool windows can now reuse a larger covering
        // window of the same file instead of re-injecting shifted ranges.
        for (index, message) in self.messages.iter().enumerate().rev() {
            let Some(prior_id) = tool_message_call_id(message) else {
                continue;
            };
            let Some(prior_detail) = self.tool_context_details.get(&prior_id) else {
                continue;
            };
            if prior_detail
                .read_evidence
                .as_ref()
                .is_some_and(|evidence| !evidence.observation_is_current())
            {
                continue;
            }
            // The earlier copy must still be sent verbatim next turn, or the
            // pointer would reference content the model no longer has.
            if self.message_has_control(index, message, |state| {
                state.stubbed || state.drop_next_turn
            }) {
                continue;
            }
            let Ok(value) = serde_json::to_value(message) else {
                continue;
            };
            if message_content_text(&value) != new_rendered {
                let prior_rendered = message_content_text(&value);
                if !read_signatures_overlap_with_same_content(
                    &signature,
                    prior_detail,
                    &prior_rendered,
                    new_rendered,
                ) {
                    continue;
                }
            } else if read_signature_for_detail(prior_detail).as_ref() != Some(&signature)
                && !read_signatures_overlap_with_same_content(
                    &signature,
                    prior_detail,
                    &message_content_text(&value),
                    new_rendered,
                )
            {
                continue;
            }
            return ReadAdmission::Reuse(ReadReuse {
                target_call_id: prior_id.clone(),
                pointer: format_reused_read_pointer(&prior_id, &signature, new_rendered),
            });
        }
        ReadAdmission::Execute(InspectionReason::NoFreshVisibleCoverage)
    }

    fn git_inspection_admission(&self, key: &str, new_rendered: &str) -> ReadAdmission {
        for (index, message) in self.messages.iter().enumerate().rev() {
            let Some(prior_id) = tool_message_call_id(message) else {
                continue;
            };
            let Some(prior_detail) = self.tool_context_details.get(&prior_id) else {
                continue;
            };
            if read_detail_is_reuse_pointer(prior_detail)
                || git_inspection_key(prior_detail).as_deref() != Some(key)
                || self.message_has_control(index, message, |state| {
                    state.stubbed || state.drop_next_turn
                })
            {
                continue;
            }
            let Ok(value) = serde_json::to_value(message) else {
                continue;
            };
            if message_content_text(&value) != new_rendered {
                continue;
            }
            if self.git_inspection_was_reused(key, &prior_id) {
                return ReadAdmission::Reject(ReadReuse {
                    target_call_id: prior_id,
                    pointer: repeated_git_inspection_message(key),
                });
            }
            return ReadAdmission::Reuse(ReadReuse {
                target_call_id: prior_id.clone(),
                pointer: format_reused_git_inspection(&prior_id, key),
            });
        }
        ReadAdmission::Execute(InspectionReason::NoFreshVisibleCoverage)
    }

    fn git_inspection_was_reused(&self, key: &str, target_call_id: &str) -> bool {
        self.inspection_events.iter().any(|(call_id, admission)| {
            admission.outcome == InspectionOutcome::Reused
                && admission.reuse_target_tool_call_id.as_deref() == Some(target_call_id)
                && self
                    .tool_context_details
                    .get(call_id)
                    .and_then(git_inspection_key)
                    .as_deref()
                    == Some(key)
                && self.messages.iter().enumerate().any(|(index, message)| {
                    tool_message_call_id(message).as_deref() == Some(call_id)
                        && !self.message_has_control(index, message, |state| {
                            state.stubbed || state.drop_next_turn
                        })
                })
        })
    }
}

fn git_inspection_key(detail: &ToolContextDetail) -> Option<String> {
    if detail.name != "git" {
        return None;
    }
    let value = serde_json::from_str::<serde_json::Value>(&detail.arguments).ok()?;
    let op = value.get("op").and_then(serde_json::Value::as_str)?;
    if !matches!(op, "diff" | "show") {
        return None;
    }
    Some(format!(
        "git {op} {}",
        crate::util::tool_args::normalize_tool_call_arguments_json(&detail.arguments)
    ))
}

fn format_reused_git_inspection(prior_id: &str, key: &str) -> String {
    format!(
        "{REUSED_INSPECTION_MARKER} {key} — exact output fingerprint matches tool {prior_id}; \
         that retained result remains in context."
    )
}

fn repeated_git_inspection_message(key: &str) -> String {
    format!(
        "Error: repeated unchanged inspection blocked for {key}. The exact result was already \
         retained and then reused once; use it, inspect a different target, make progress, or finish."
    )
}

/// Whether a tool detail is a compact read-reuse pointer. Pointers are excluded
/// from supersession so they never stub the full read they reference.
fn read_detail_is_reuse_pointer(detail: &ToolContextDetail) -> bool {
    detail.reuse_target_call_id.is_some()
}

fn read_reuse_target_call_id(detail: &ToolContextDetail) -> Option<&str> {
    detail.reuse_target_call_id.as_deref()
}

fn read_signatures_overlap_with_same_content(
    new_signature: &ReadSignature,
    prior_detail: &ToolContextDetail,
    prior_rendered: &str,
    new_rendered: &str,
) -> bool {
    let Some(prior_signature) = read_signature_for_detail(prior_detail) else {
        return false;
    };
    let (ReadSignature::StructuredRead(new), ReadSignature::StructuredRead(prior)) =
        (new_signature, &prior_signature)
    else {
        return false;
    };
    if !prior.window.covers(&new.window) {
        return false;
    }
    if let (Some(prior_digest), Some(new_digest)) =
        (prior.file_digest_at_read, new.file_digest_at_read)
    {
        return prior_digest == new_digest;
    }
    line_numbered_output_covers(prior_rendered, new_rendered)
}

fn line_numbered_output_covers(prior_rendered: &str, new_rendered: &str) -> bool {
    let prior_lines = prior_rendered
        .lines()
        .filter(|line| line_numbered_read_line(line))
        .collect::<HashSet<_>>();
    let mut new_line_count = 0usize;
    for line in new_rendered
        .lines()
        .filter(|line| line_numbered_read_line(line))
    {
        new_line_count = new_line_count.saturating_add(1);
        if !prior_lines.contains(line) {
            return false;
        }
    }
    new_line_count > 0
}

fn line_numbered_read_line(line: &str) -> bool {
    let Some((number, _rest)) = line.split_once(": ") else {
        return false;
    };
    number.trim().parse::<usize>().is_ok()
}

/// First and last line numbers shown in line-numbered read output (`"N: ..."`),
/// ignoring any trailing truncation footer. `None` for output without line
/// numbers (e.g. directory listings), so the pointer falls back to a path-only
/// sentence.
fn displayed_line_range(rendered: &str) -> Option<(usize, usize)> {
    let mut first = None;
    let mut last = None;
    for line in rendered.lines() {
        let Some((number, _rest)) = line.split_once(": ") else {
            continue;
        };
        let Ok(parsed) = number.trim().parse::<usize>() else {
            continue;
        };
        first.get_or_insert(parsed);
        last = Some(parsed);
    }
    Some((first?, last?))
}

/// Compact result for a read whose current bytes are already covered by a
/// fresh, still-live earlier result. Stale or missing targets never reach this
/// formatter; they execute with full content instead.
fn format_reused_read_pointer(prior_id: &str, signature: &ReadSignature, rendered: &str) -> String {
    let path = signature.path();
    match (
        displayed_line_range(rendered),
        truncated_read_continuation(signature, rendered),
    ) {
        (Some((start, end)), Some((next_line, total_lines))) => {
            format!(
                "{REUSED_READ_MARKER} {path} lines {start}-{end} — unchanged since tool {prior_id}; \
                 that retained output remains in context. To inspect unseen lines \
                 {next_line}-{total_lines}, continue with offset={next_line} and a narrower limit; \
                 do not restart at offset=1 unless the stale-files notice requires it."
            )
        }
        (Some((start, end)), None) => format!(
            "{REUSED_READ_MARKER} {path} lines {start}-{end} — unchanged since tool {prior_id}; \
             that retained output remains in context. Do not re-read unless the stale-files notice \
             requires it."
        ),
        (None, _) => format!(
            "{REUSED_READ_MARKER} {path} — unchanged since tool {prior_id}; that retained output \
             remains in context. Do not re-read unless the stale-files notice requires it."
        ),
    }
}

fn truncated_read_continuation(
    signature: &ReadSignature,
    rendered: &str,
) -> Option<(usize, usize)> {
    if !rendered.contains("[Truncated:") {
        return None;
    }
    let ReadSignature::StructuredRead(snapshot) = signature else {
        return None;
    };
    let end_line = snapshot.window.end_line?;
    let total_lines = snapshot.window.total_lines?;
    (end_line < total_lines).then(|| (end_line.saturating_add(1), total_lines))
}

fn estimated_stub_tokens(message: &ChatCompletionRequestMessage) -> usize {
    serde_json::to_value(message)
        .ok()
        .map(|value| message_content_text(&value).chars().count())
        .unwrap_or(0)
        .div_ceil(4)
}
