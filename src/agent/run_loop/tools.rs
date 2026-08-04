//! Tool-batch orchestration and individual tool-call execution for the agent
//! run loop.

use super::*;

impl Agent {
    /// Run one batch concurrently, selected against `cancellation_token` so a
    /// mid-batch cancel returns synthetic interrupted results instead of
    /// waiting out the slowest call. `*interrupted_mid_tools` is set on that
    /// path so the caller stops issuing further batches this turn.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_tool_batch(
        background_wakes: Option<Arc<crate::background_wake::BackgroundWakeCoordinator>>,
        batch: Vec<ToolCall>,
        tool_registry: &Arc<ToolRegistry>,
        tool_rejections: &ToolRejections,
        sink: &SharedSink,
        cancellation_token: &CancellationToken,
        interrupted_mid_tools: &mut bool,
        hooks: &Arc<crate::hooks::HookEngine>,
        tool_origin: Option<String>,
        max_tool_duration: Option<Duration>,
    ) -> Vec<(ToolCall, ToolOutput, crate::output::ToolExecutionStatus)> {
        let started_calls = batch
            .iter()
            .map(|tool_call| {
                ToolCallStart::new(
                    tool_call.id.clone(),
                    tool_call.name.clone(),
                    tool_call.arguments.clone(),
                )
            })
            .collect::<Vec<_>>();
        sink.tool_calls_started(&started_calls);

        // `cloned()` is required, not redundant: each `tool_call` is moved into
        // its async closure while `batch` itself is still moved into
        // `interrupted_tool_results(batch)` on the cancel arm below.
        let pending = join_all(batch.iter().cloned().map(|tool_call| {
            let authorization_context =
                crate::tool::AuthorizationCallContext::new(tool_call.id.clone(), sink.clone());
            crate::tool::with_authorization_call_context(
                authorization_context,
                Self::execute_single_tool_call(
                    background_wakes.clone(),
                    tool_call,
                    tool_registry.clone(),
                    tool_rejections.clone(),
                    sink.clone(),
                    cancellation_token.clone(),
                    hooks.clone(),
                    tool_origin.clone(),
                    max_tool_duration,
                ),
            )
        }));
        tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => {
                *interrupted_mid_tools = true;
                interrupted_tool_results(batch)
            }
            results = pending => results,
        }
    }

    /// Run one tool call to completion: reject it outright (planning-research
    /// or repeated-inspection budget), report it unknown, fail to parse its
    /// arguments, or execute it — in that order, each a guard clause rather
    /// than a further nesting level.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_single_tool_call(
        background_wakes: Option<Arc<crate::background_wake::BackgroundWakeCoordinator>>,
        tool_call: ToolCall,
        tool_registry: Arc<ToolRegistry>,
        tool_rejections: ToolRejections,
        sink: SharedSink,
        cancellation_token: CancellationToken,
        hooks: Arc<crate::hooks::HookEngine>,
        tool_origin: Option<String>,
        max_tool_duration: Option<Duration>,
    ) -> (ToolCall, ToolOutput, crate::output::ToolExecutionStatus) {
        sink.thinking(&format!("Running {}", tool_call.name));

        // Set only once execution is actually attempted, so `PostToolUse`
        // below fires solely for a call that reached the tool — never for a
        // rejection, an unknown tool, an arg-parse failure, or a
        // `PreToolUse` block, none of which the tool ever saw. Carries the
        // hook-match name (an MCP tool's dotted display id, not its wire
        // name — see `Tool::hook_match_name`) for both Pre/PostToolUse.
        let mut executed_hook_name: Option<String> = None;

        let (mut result, status) = 'call: {
            // Loop/policy guards reject the model turn, not filesystem I/O.
            // Resolve them before compact read reuse; otherwise an already
            // covered read silently becomes another successful pointer and the
            // model can keep spending turns past the storm boundary forever.
            if let Some(message) = tool_rejections.message_for(&tool_call) {
                break 'call (
                    ToolOutput::Text(message),
                    // Policy rejected the call before the tool ran. Keep that
                    // distinct from a real execution failure: failed Bash
                    // results arm repair mode and repeated-failure tracking,
                    // while a guard redirect should only steer the next turn.
                    crate::output::ToolExecutionStatus::Skipped,
                );
            }
            if let Some(reuse) = tool_rejections.precomputed_read.get(&tool_call.id) {
                break 'call (
                    ToolOutput::ReadReuse {
                        text: reuse.pointer.clone(),
                        target_call_ids: reuse.target_call_ids.clone(),
                        requested_chars: reuse.requested_chars,
                    },
                    crate::output::ToolExecutionStatus::Succeeded,
                );
            }
            let Some(tool) = tool_registry.get(&tool_call.name) else {
                break 'call (
                    ToolOutput::Text(format!(
                        "Error: {}",
                        tool_registry.unknown_tool_message(&tool_call.name)
                    )),
                    crate::output::ToolExecutionStatus::Failed,
                );
            };
            let effect_policy = tool.effect_policy();
            tracing::trace!(
                tool = %tool_call.name,
                effect_policy = ?effect_policy,
                "executing tool under declared effect contract"
            );
            let mut args =
                match parse_tool_arguments(tool.as_ref(), &tool_call.name, &tool_call.arguments) {
                    Ok(args) => args,
                    Err(message) => {
                        break 'call (
                            ToolOutput::Text(message),
                            crate::output::ToolExecutionStatus::Failed,
                        );
                    }
                };
            if let Some(delta) = tool_rejections.precomputed_read_delta.get(&tool_call.id) {
                args = delta.arguments.clone();
            }
            if tool_rejections.auto_background.contains(&tool_call.id)
                && let Some(object) = args.as_object_mut()
            {
                object.insert(
                    "run_in_background".to_string(),
                    serde_json::Value::Bool(true),
                );
            }
            match effect_policy {
                crate::tool::ToolEffectPolicy::LocalState => {
                    tool_registry
                        .authorization_ledger()
                        .record_internal(&tool_call.name)
                        .await;
                }
                crate::tool::ToolEffectPolicy::Delegated => {
                    tool_registry
                        .authorization_ledger()
                        .record_delegation(&tool_call.name)
                        .await;
                }
                crate::tool::ToolEffectPolicy::ReadOnly
                | crate::tool::ToolEffectPolicy::SelfAuthorized => {}
            }
            let hook_name = tool.hook_match_name().to_string();

            let pre_outcome = hooks
                .fire(
                    crate::hooks::HookEvent::PreToolUse,
                    crate::hooks::HookContext {
                        tool_name: Some(&hook_name),
                        tool_args: Some(&args),
                        ..Default::default()
                    },
                )
                .await;
            emit_hook_warnings(&sink, &pre_outcome.warnings);
            match pre_outcome.decision {
                crate::hooks::HookDecision::Block { reason } => {
                    break 'call (
                        ToolOutput::Text(reason),
                        crate::output::ToolExecutionStatus::Failed,
                    );
                }
                crate::hooks::HookDecision::ModifyArgs { args: rewritten } => {
                    sink.status(&format!(
                        "[hooks] rewrote arguments for '{}'",
                        tool_call.name
                    ));
                    args = rewritten;
                }
                crate::hooks::HookDecision::Continue
                | crate::hooks::HookDecision::AddContext { .. } => {}
            }

            executed_hook_name = Some(hook_name);
            let tool_cancellation = cancellation_token.child_token();
            let mut context =
                crate::tool::ToolExecutionContext::new(tool_call.id.clone(), sink.clone())
                    .with_cancellation_token(tool_cancellation.clone());
            if let Some(background_wakes) = background_wakes {
                context = context.with_background_wakes(background_wakes);
            }
            if let Some(origin) = tool_origin {
                context = context.with_origin(origin);
            }
            let execution = tool.execute_with_context(args, context);
            let execution_result = match max_tool_duration {
                Some(limit) => {
                    tokio::select! {
                        biased;
                        _ = async {
                            if !limit.is_zero() {
                                tokio::time::sleep(limit).await;
                            }
                        } => {
                            tool_cancellation.cancel();
                            break 'call (
                                ToolOutput::Text(format!(
                                    "Error: tool '{}' exceeded its {}s runtime budget and was cancelled.",
                                    tool_call.name,
                                    limit.as_secs()
                                )),
                                crate::output::ToolExecutionStatus::Interrupted,
                            );
                        }
                        result = execution => result,
                    }
                }
                None => execution.await,
            };
            match execution_result {
                Ok(mut result) => {
                    if let Some(delta) = tool_rejections.precomputed_read_delta.get(&tool_call.id) {
                        result = match result {
                            ToolOutput::Read { text, evidence } => ToolOutput::ReadDelta {
                                text: format!("{}\n{}", delta.prefix, text),
                                evidence,
                                target_call_ids: delta.target_call_ids.clone(),
                                avoided_chars: delta.avoided_chars,
                            },
                            other => other,
                        };
                    }
                    if tool_rejections.auto_background.contains(&tool_call.id)
                        && let ToolOutput::BackgroundTaskStarted { message, .. } = &mut result
                    {
                        *message = format!(
                            "Auto-promoted known slow verification to background execution. {message}"
                        );
                    }
                    let status = result.execution_status();
                    (result, status)
                }
                Err(e) => (
                    ToolOutput::Text(format!("Error: {e}")),
                    crate::output::ToolExecutionStatus::Failed,
                ),
            }
        };

        if let Some(hook_name) = &executed_hook_name {
            // Hooks receive only a redacted excerpt. Keep the full result intact
            // until the final redaction boundary below so a post hook's context
            // is masked in the same pass as the tool output.
            let redacted_summary = crate::redact::redact(result.rendered_summary());
            let output_excerpt = crate::hooks::truncate_excerpt(redacted_summary.as_ref());
            let post_outcome = hooks
                .fire(
                    crate::hooks::HookEvent::PostToolUse,
                    crate::hooks::HookContext {
                        tool_name: Some(hook_name),
                        output_excerpt: Some(&output_excerpt),
                        ..Default::default()
                    },
                )
                .await;
            emit_hook_warnings(&sink, &post_outcome.warnings);
            // Non-veto: folded into the result as framed untrusted data,
            // never a system message. Preserve delegated usage/effect metadata
            // on `TextWithUsage`; every other variant collapses to `Text` (and
            // e.g. loses `Command`'s separate stdout/stderr) so a formerly
            // `Trusted` result cannot carry a post-hook note through trusted-
            // context promotion in `apply_tool_result`.
            if let crate::hooks::HookDecision::AddContext { text } = post_outcome.decision
                && !matches!(result, ToolOutput::Image { .. })
            {
                let framed = crate::tool::wrap_untrusted_content("hook:PostToolUse", &text);
                if let ToolOutput::TextWithUsage { text, .. } = &mut result {
                    text.push_str("\n\n");
                    text.push_str(&framed);
                } else {
                    result = ToolOutput::Text(format!("{}\n\n{framed}", result.rendered_summary()));
                }
            }
        }

        // PostToolUse can append arbitrary hook context. This final boundary
        // masks both that context and the original tool result before any sink,
        // model context, or persisted snapshot can observe either.
        result.redact_secrets();
        report_tool_completion(&sink, &tool_call, &result, status);

        (tool_call, result, status)
    }
}
