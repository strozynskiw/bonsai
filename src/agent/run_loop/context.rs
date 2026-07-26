//! Context admission and tool-result projection for the agent run loop.

use super::*;

impl Agent {
    /// Resolve a read/inspection tool result against its [`ReadAdmission`]
    /// verdict: `Reuse`/`Reject` collapse the result to a compact pointer
    /// (rewriting the stored context detail and finishing the sink), while
    /// `Execute` returns the full rendered summary. Returns the model-visible
    /// content plus any inspection metadata to record. Extracted from
    /// [`apply_tool_result`](Self::apply_tool_result) so the three-arm decision
    /// is a self-contained, independently testable unit.
    fn apply_read_admission(
        &mut self,
        admission: ReadAdmission,
        tool_call: &ToolCall,
        model_rendered_summary: String,
        requested_chars: usize,
        record_admission: bool,
        sink: &SharedSink,
    ) -> (String, Option<ReadAdmissionMetadata>) {
        match admission {
            ReadAdmission::Reuse(reuse) => {
                let returned_chars = reuse.pointer.chars().count();
                let metadata = ReadAdmissionMetadata {
                    outcome: InspectionOutcome::Reused,
                    reason: InspectionReason::FreshVisibleCoverage,
                    reuse_target_tool_call_id: Some(reuse.target_call_id.clone()),
                    requested_chars,
                    returned_chars,
                    avoided_chars: requested_chars.saturating_sub(returned_chars),
                };
                if let Some(detail) = self.tool_context_details.get_mut(&tool_call.id) {
                    detail.read_evidence = None;
                    detail.result = ToolContextResult::Text {
                        rendered: reuse.pointer.clone(),
                    };
                    detail.reuse_target_call_id = Some(reuse.target_call_id);
                }
                sink.tool_finished(
                    &tool_call.id,
                    &reuse.pointer,
                    crate::output::ToolExecutionStatus::Succeeded,
                );
                (reuse.pointer, Some(metadata))
            }
            ReadAdmission::Execute(reason) => {
                let metadata = record_admission.then_some(ReadAdmissionMetadata {
                    outcome: InspectionOutcome::Executed,
                    reason,
                    reuse_target_tool_call_id: None,
                    requested_chars,
                    returned_chars: requested_chars,
                    avoided_chars: 0,
                });
                (model_rendered_summary, metadata)
            }
            ReadAdmission::Reject(rejection) => {
                let returned_chars = rejection.pointer.chars().count();
                let metadata = ReadAdmissionMetadata {
                    outcome: InspectionOutcome::Rejected,
                    reason: InspectionReason::RepeatedFreshReuse,
                    reuse_target_tool_call_id: Some(rejection.target_call_id.clone()),
                    requested_chars,
                    returned_chars,
                    avoided_chars: requested_chars.saturating_sub(returned_chars),
                };
                if let Some(detail) = self.tool_context_details.get_mut(&tool_call.id) {
                    detail.read_evidence = None;
                    detail.result = ToolContextResult::Text {
                        rendered: rejection.pointer.clone(),
                    };
                    detail.reuse_target_call_id = Some(rejection.target_call_id);
                }
                sink.tool_finished(
                    &tool_call.id,
                    &rejection.pointer,
                    crate::output::ToolExecutionStatus::Failed,
                );
                (rejection.pointer, Some(metadata))
            }
        }
    }

    /// Apply one tool result to the conversation: usage bookkeeping, the
    /// stored context detail, trusted-context and successful-Rust-edit
    /// collection for the batch, read-admission rewriting, and the tool
    /// message (plus a trailing image message) pushed into history.
    pub(super) async fn apply_tool_result(
        &mut self,
        tool_call: ToolCall,
        result: ToolOutput,
        status: crate::output::ToolExecutionStatus,
        sink: &SharedSink,
        batch: ToolResultBatchContext<'_, '_>,
    ) -> Result<()> {
        let success = status.is_success();
        // Episode title signal: successful title-bearing tools are observed
        // here, before their result message is appended; resolution waits for
        // the next preflight, when the complete group exists. The observer
        // ignores every unrelated tool.
        if success {
            self.observe_episode_title_result(&tool_call);
        }
        if success && tool_call.name == "todowrite" {
            self.observe_todowrite_result().await;
        }
        self.record_verification_tool_result(&tool_call, &result, status)
            .await;
        if let Some(usage_totals) = result.usage_totals() {
            self.absorb_usage_totals(usage_totals);
        }
        if let Some(usage_turns) = result.usage_turns() {
            let mut attributed = usage_turns.to_vec();
            for turn in &mut attributed {
                turn.attach_parent_context(&tool_call.id, batch.launch_group_id);
            }
            self.absorb_usage_turns(&attributed);
        }
        if success && let Some(evidence) = result.delegated_read_evidence() {
            self.import_delegated_read_evidence(evidence);
            self.refresh_stale_read_advisory();
        }
        let detail = tool_context_detail(&tool_call, &result);
        self.tool_context_details
            .insert(tool_call.id.clone(), detail);
        // Only *trusted* context is promoted into a system message. Untrusted
        // content (ToolOutput::UntrustedContext, e.g. WebFetch output) must never
        // reach here — its instructions stay data inside the tool message, per
        // the M5 prompt-injection gate.
        if success
            && matches!(
                result.context_provenance(),
                Some(MessageProvenance::Harness)
            )
            && let ToolOutput::TrustedContext { content, .. } = &result
        {
            batch.trusted_contexts.push(content.clone());
            // Record a model-invoked skill load so the skills manager can mark it.
            if tool_call.name == "skill"
                && let Ok(args) = serde_json::from_str::<serde_json::Value>(&tool_call.arguments)
                && let Some(name) = args.get("name").and_then(|value| value.as_str())
            {
                self.mark_skill_loaded(name);
            }
        }
        let rendered_summary = result.rendered_summary().to_string();
        if let ToolOutput::BackgroundTaskStarted { task_id, .. } = &result {
            let _ = self
                .background_tasks
                .attach_tool_call(task_id, tool_call.id.clone())
                .await;
        }
        if success && is_mutation_tool(&tool_call.name) {
            batch
                .successful_rust_edits
                .extend(self.resolve_rust_mutation_paths(&tool_call).await);
            self.rebaseline_read_evidence_for_mutation(&tool_call).await;
        }
        // Feed one typed post-execution effect into verification and review.
        // A resolved read-only delegation and a write-capable delegation with
        // no observed delta are both `NoMutation`; a lone delegated snapshot
        // delta is reviewable as a low-confidence window after peer subtraction,
        // while ambiguous multi-call deltas remain unscoped.
        // Foreground Bash is handled separately from observed before/after
        // workspace snapshots at the batch boundary, so no-op bookkeeping
        // commands do not arm review while a formatter or generator that
        // actually changes files does. Known mutation paths scope the eventual
        // review diff to the agent's own work.
        if success || result.workspace_effect().is_some() {
            let typed_paths = if is_mutation_tool(&tool_call.name) {
                self.project_relative_mutation_paths(&tool_call)
            } else {
                Vec::new()
            };
            match completed_tool_workspace_effect(&tool_call, &result, typed_paths) {
                crate::tool::ToolWorkspaceEffect::NoMutation => {}
                crate::tool::ToolWorkspaceEffect::ScopedMutation(paths) => {
                    self.note_typed_verification_worthy_mutation(paths);
                }
                crate::tool::ToolWorkspaceEffect::WindowMutation(paths) => {
                    self.note_bash_window_verification_worthy_mutation(paths);
                }
                crate::tool::ToolWorkspaceEffect::Unscoped => {
                    self.note_typed_verification_worthy_mutation(Vec::new());
                }
            }
        }
        if let Some(advisory) = repair_advisory_for_tool_result(&tool_call, &result, success) {
            self.set_repair_advisory(Some(advisory));
        } else if success && tool_clears_repair_advisory(&tool_call.name) {
            self.set_repair_advisory(None);
        }

        // Inspection admission is per completed call. Fresh visible file
        // coverage and byte-identical git diff/show output can become compact
        // pointers; stale, changed, missing, or stubbed cases keep real output.
        // Tools still execute first, so write safety and repository freshness
        // never rely on an unvalidated cache guess.
        let model_rendered_summary = compact_successful_command_model_content(
            &tool_call,
            &result,
            &rendered_summary,
            success,
        )
        .unwrap_or_else(|| rendered_summary.clone());
        let typed_read = matches!(
            &result,
            ToolOutput::Read { .. } | ToolOutput::ReadDelta { .. }
        );
        let partial_read_avoided = match &result {
            ToolOutput::ReadDelta { avoided_chars, .. } => Some(*avoided_chars),
            _ => None,
        };
        let precomputed_read = match &result {
            ToolOutput::ReadReuse {
                text,
                target_call_ids,
                requested_chars,
            } => target_call_ids.first().map(|target_call_id| {
                (
                    ReadReuse {
                        target_call_id: target_call_id.clone(),
                        pointer: text.clone(),
                    },
                    *requested_chars,
                )
            }),
            _ => None,
        };
        let structured_read_name = structured_file_read_tool(&tool_call.name);
        let repeated_fresh_reuse = success
            && (structured_read_name || tool_call.name == "bash")
            && self.read_follows_compact_reuse(&tool_call);
        let git_inspection = tool_call.name == "git"
            && serde_json::from_str::<serde_json::Value>(&tool_call.arguments)
                .ok()
                .and_then(|value| {
                    value
                        .get("op")
                        .and_then(|op| op.as_str())
                        .map(str::to_string)
                })
                .is_some_and(|op| matches!(op.as_str(), "diff" | "show"));
        let admission = if let Some((reuse, _requested_chars)) = &precomputed_read {
            ReadAdmission::Reuse(ReadReuse {
                target_call_id: reuse.target_call_id.clone(),
                pointer: reuse.pointer.clone(),
            })
        } else if repeated_fresh_reuse {
            // A compact pointer is useful once. If the model explicitly asks
            // again, honor that request with real bytes instead of trapping it
            // in a denied-read loop. The repeated-inspection and
            // per-path storm guards still stop a model that ignores the bytes.
            ReadAdmission::Execute(InspectionReason::RepeatedFreshReuse)
        } else if success && (typed_read || tool_call.name == "bash" || git_inspection) {
            self.read_admission(&tool_call.id, &model_rendered_summary)
        } else if !success && (structured_read_name || git_inspection) {
            ReadAdmission::Execute(InspectionReason::ToolFailed)
        } else {
            ReadAdmission::Execute(InspectionReason::NotReusable)
        };
        let requested_chars = precomputed_read
            .as_ref()
            .map(|(_reuse, requested_chars)| *requested_chars)
            .unwrap_or_else(|| model_rendered_summary.chars().count());
        let record_admission = typed_read
            || structured_read_name
            || git_inspection
            || repeated_fresh_reuse
            || matches!(
                admission,
                ReadAdmission::Reuse(_) | ReadAdmission::Reject(_)
            );
        let (model_content, mut admission_metadata) = self.apply_read_admission(
            admission,
            &tool_call,
            model_rendered_summary,
            requested_chars,
            record_admission,
            sink,
        );
        if let (Some(avoided_chars), Some(metadata)) =
            (partial_read_avoided, admission_metadata.as_mut())
        {
            metadata.outcome = InspectionOutcome::Reused;
            metadata.reason = InspectionReason::FreshVisibleCoverage;
            metadata.requested_chars = metadata.returned_chars.saturating_add(avoided_chars);
            metadata.avoided_chars = avoided_chars;
            metadata.reuse_target_tool_call_id = match &result {
                ToolOutput::ReadDelta {
                    target_call_ids, ..
                } => target_call_ids.first().cloned(),
                _ => None,
            };
        }
        if let Some(metadata) = admission_metadata {
            self.usage
                .record_inspection(&self.execution_lane, &metadata);
            self.read_evidence
                .inspection_events
                .insert(tool_call.id.clone(), metadata);
        }

        // Repeat-recall dedup (anti-cascade): an identical recall page whose
        // prior copy is still live collapses to a pointer instead of doubling
        // the archived bytes in context. A first recall is never blocked.
        let model_content = if success && tool_call.name == "recall" {
            match self.recall_reuse_pointer(&tool_call, &model_content) {
                Some(pointer) => {
                    if let Some(detail) = self.tool_context_details.get_mut(&tool_call.id) {
                        detail.result = ToolContextResult::Text {
                            rendered: pointer.clone(),
                        };
                    }
                    pointer
                }
                None => model_content,
            }
        } else {
            model_content
        };
        let tool_message = ChatCompletionRequestMessage::Tool(
            ChatCompletionRequestToolMessageArgs::default()
                .content(model_content.as_str())
                .tool_call_id(&tool_call.id)
                .build()?,
        );
        self.push_message(tool_message);

        if let ToolOutput::Image {
            mime_type,
            base64_data,
            ..
        } = result
        {
            self.push_message(image_user_message(&mime_type, &base64_data));
        }
        Ok(())
    }
}
