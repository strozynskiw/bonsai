use super::*;

impl Storage {
    #[cfg(test)]
    pub async fn replace_transcript_snapshot(
        &self,
        session_id: SessionId,
        transcript: &[TranscriptItem],
    ) -> Result<()> {
        self.with_session_snapshot_tx("transcript snapshot", async move |tx, now| {
            self.replace_transcript_snapshot_in_tx(tx, session_id, transcript, now)
                .await
        })
        .await
    }

    pub(crate) async fn replace_transcript_snapshot_in_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        session_id: SessionId,
        transcript: &[TranscriptItem],
        now: i64,
    ) -> Result<()> {
        storage_op!(
            tx,
            "delete transcript messages",
            sqlx::query("DELETE FROM messages WHERE session_id = ?").bind(session_id.as_i64()),
        )?;
        storage_op!(
            tx,
            "delete transcript blocks",
            sqlx::query("DELETE FROM transcript_blocks WHERE session_id = ?")
                .bind(session_id.as_i64()),
        )?;
        storage_op!(
            tx,
            "delete transcript tool calls",
            sqlx::query("DELETE FROM tool_calls WHERE session_id = ?").bind(session_id.as_i64()),
        )?;

        let mut message_seq = 0_i64;
        let mut tool_seq = 0_i64;
        for (seq, item) in transcript.iter().enumerate() {
            if is_suppressed_transcript_item(item) {
                continue;
            }
            let block = transcript_block_record(item)?;
            sqlx::query(
                r#"
                INSERT INTO transcript_blocks (
                  session_id, seq, kind, title, body, metadata_json
                )
                VALUES (?, ?, ?, ?, ?, ?)
                "#,
            )
            .bind(session_id.as_i64())
            .bind(seq as i64)
            .bind(block.kind)
            .bind(block.title)
            .bind(&block.body)
            .bind(block.metadata_json)
            .execute(&mut **tx)
            .await?;

            if let Some(role) = block.message_role {
                sqlx::query(
                    r#"
                    INSERT INTO messages (
                      session_id, seq, role, content
                    )
                    VALUES (?, ?, ?, ?)
                    "#,
                )
                .bind(session_id.as_i64())
                .bind(message_seq)
                .bind(role)
                .bind(&block.body)
                .execute(&mut **tx)
                .await?;
                message_seq += 1;
            }

            for activity in tool_activities(item) {
                insert_tool_call(tx, session_id.as_i64(), tool_seq, activity, now).await?;
                tool_seq += 1;
            }
        }

        // Touch the session so `updated_at_ms` reflects this snapshot. Real
        // Token accounting lives in the split prompt/completion columns written
        // by `update_session_usage`; transcript flushes only advance recency.
        storage_op!(
            tx,
            "touch session updated_at",
            sqlx::query("UPDATE sessions SET updated_at_ms = ? WHERE id = ?")
                .bind(now)
                .bind(session_id.as_i64()),
        )?;
        Ok(())
    }
    /// The newest completion report persisted for `session_id`, if any run has
    /// finished in it. Queries the single block directly instead of loading
    /// the whole transcript — `/bug` bundles want just this summary. A row
    /// whose metadata no longer parses (schema drift in an old session) reads
    /// as `None` rather than an error.
    pub(crate) async fn latest_completion_report(
        &self,
        session_id: SessionId,
    ) -> Result<Option<crate::completion_report::CompletionReport>> {
        let row = sqlx::query(
            r#"
            SELECT metadata_json
            FROM transcript_blocks
            WHERE session_id = ? AND kind = 'completion_report'
            ORDER BY seq DESC
            LIMIT 1
            "#,
        )
        .bind(session_id.as_i64())
        .fetch_optional(&self.pool)
        .await
        .with_context(|| format!("Failed to load completion report for session {session_id}"))?;
        Ok(row
            .and_then(|row| row.try_get::<String, _>("metadata_json").ok())
            .and_then(|metadata_json| serde_json::from_str(&metadata_json).ok()))
    }

    pub(super) async fn load_transcript_items(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<TranscriptItem>> {
        let tools = self.load_tool_call_records(session_id).await?;
        let rows = sqlx::query(
            r#"
            SELECT kind, title, body, metadata_json
            FROM transcript_blocks
            WHERE session_id = ?
            ORDER BY seq
            "#,
        )
        .bind(session_id.as_i64())
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("Failed to load transcript for session {session_id}"))?;

        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            let kind: String = row.try_get("kind")?;
            let title: String = row.try_get("title")?;
            let body: String = row.try_get("body")?;
            let metadata_json: String = row.try_get("metadata_json")?;
            if is_suppressed_transcript_block(&kind, &body) {
                continue;
            }
            items.push(match kind.as_str() {
                "user" => TranscriptItem::UserMessage { text: body },
                "assistant" => TranscriptItem::AssistantMessage { text: body },
                "thinking" => TranscriptItem::ReasoningSummary { text: body },
                "status" => TranscriptItem::CommandOutput {
                    kind: CommandOutputKind::Status,
                    text: body,
                },
                "compaction_status" => TranscriptItem::CommandOutput {
                    kind: CommandOutputKind::CompactionStatus,
                    text: body,
                },
                "command_error" => TranscriptItem::CommandOutput {
                    kind: CommandOutputKind::Error,
                    text: body,
                },
                "completion_report" => serde_json::from_str(&metadata_json)
                    .map(|report| TranscriptItem::CompletionReport(Box::new(report)))
                    .unwrap_or_else(|_| TranscriptItem::CommandOutput {
                        kind: CommandOutputKind::Status,
                        text: body,
                    }),
                "error" => TranscriptItem::Error {
                    error: crate::tui::event::UiError {
                        title,
                        detail: body,
                    },
                },
                "edit" => TranscriptItem::Edit { text: body },
                "todo" => TranscriptItem::TodoUpdate { text: body },
                "worklog" => TranscriptItem::WorkLog { text: body },
                "peer_message" => {
                    let metadata: Value =
                        serde_json::from_str(&metadata_json).unwrap_or(Value::Null);
                    TranscriptItem::PeerMessage {
                        source_message_id: metadata
                            .get("source_message_id")
                            .and_then(Value::as_i64),
                        session_id: metadata
                            .get("session_id")
                            .and_then(Value::as_i64)
                            .unwrap_or_default(),
                        outgoing: metadata
                            .get("outgoing")
                            .and_then(Value::as_bool)
                            .unwrap_or_default(),
                        text: body,
                    }
                }
                "tool" => load_tool_block(&metadata_json, &tools).unwrap_or_else(|| {
                    TranscriptItem::CommandOutput {
                        kind: CommandOutputKind::Status,
                        text: titled_body(title, body),
                    }
                }),
                "execution_group" => load_execution_group_block(&metadata_json, &tools)
                    .unwrap_or_else(|| TranscriptItem::CommandOutput {
                        kind: CommandOutputKind::Status,
                        text: titled_body(title, body),
                    }),
                _ => TranscriptItem::CommandOutput {
                    kind: CommandOutputKind::Status,
                    text: body,
                },
            });
        }
        Ok(items)
    }

    pub(super) async fn load_message_history(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query(
            r#"
            SELECT role, content
            FROM messages
            WHERE session_id = ? AND role IN ('user', 'assistant')
            ORDER BY seq
            "#,
        )
        .bind(session_id.as_i64())
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("Failed to load message history for session {session_id}"))?;

        rows.into_iter()
            .map(|row| Ok((row.try_get("role")?, row.try_get("content")?)))
            .collect()
    }

    async fn load_tool_call_records(
        &self,
        session_id: SessionId,
    ) -> Result<HashMap<String, ToolCallRecord>> {
        let rows = sqlx::query(
            r#"
            SELECT call_id, name, args_json, result_json, diff_json, delegated_model,
                   duration_ms, status
            FROM tool_calls
            WHERE session_id = ?
            ORDER BY seq
            "#,
        )
        .bind(session_id.as_i64())
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("Failed to load tool calls for session {session_id}"))?;

        let mut records = HashMap::new();
        for row in rows {
            let call_id: String = row.try_get("call_id")?;
            records.insert(
                call_id.clone(),
                ToolCallRecord {
                    call_id,
                    name: row.try_get("name")?,
                    arguments: row.try_get("args_json")?,
                    delegated_model: row.try_get("delegated_model")?,
                    result: parse_tool_result(row.try_get("result_json")?)?,
                    diff: parse_tool_diff(row.try_get("diff_json")?)?,
                    duration_ms: row.try_get("duration_ms")?,
                    status: ToolStatus::from_db_str(&row.try_get::<String, _>("status")?),
                },
            );
        }
        Ok(records)
    }
}

#[derive(Debug)]
struct TranscriptBlockRecord {
    kind: &'static str,
    title: String,
    body: String,
    metadata_json: String,
    message_role: Option<&'static str>,
}

#[derive(Debug, Clone)]
struct ToolCallRecord {
    call_id: String,
    name: String,
    arguments: String,
    delegated_model: Option<String>,
    result: Option<String>,
    diff: Option<crate::diff::FileDiff>,
    duration_ms: Option<i64>,
    status: ToolStatus,
}

fn titled_body(title: String, body: String) -> String {
    if title.is_empty() {
        body
    } else if body.is_empty() {
        title
    } else {
        format!("{title}\n{body}")
    }
}

fn is_suppressed_transcript_block(kind: &str, body: &str) -> bool {
    (matches!(kind, "assistant" | "thinking" | "worklog") && body.trim().is_empty())
        || (kind == "status" && is_noisy_context_fallback_status(body))
        // Queued user input is ephemeral: the loader has no `QueuedUserMessage`
        // arm, so a restored `queued_user` block would otherwise resurface as a
        // misleading, non-actionable status line after crash recovery. Drop it
        // on load — pending input doesn't survive a restart. (Still shown live.)
        || kind == "queued_user"
}

fn is_suppressed_transcript_item(item: &TranscriptItem) -> bool {
    item.is_suppressed()
}

fn load_tool_block(
    metadata_json: &str,
    tools: &HashMap<String, ToolCallRecord>,
) -> Option<TranscriptItem> {
    let metadata = serde_json::from_str::<Value>(metadata_json).ok()?;
    let id = metadata.get("id")?.as_str()?;
    let record = tools.get(id)?;
    Some(TranscriptItem::ToolActivity(tool_activity_from_record(
        record,
    )))
}

fn load_execution_group_block(
    metadata_json: &str,
    tools: &HashMap<String, ToolCallRecord>,
) -> Option<TranscriptItem> {
    let metadata = serde_json::from_str::<Value>(metadata_json).ok()?;
    let group_id = metadata.get("id")?.as_u64()?;
    let finished = metadata
        .get("finished")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let tool_ids = metadata.get("tool_ids")?.as_array()?;
    let group_tools = tool_ids
        .iter()
        .filter_map(Value::as_str)
        .filter_map(|id| tools.get(id))
        .map(tool_activity_from_record)
        .collect::<Vec<_>>();
    if group_tools.is_empty() {
        return None;
    }
    Some(TranscriptItem::ExecutionGroup(ExecutionGroup {
        id: group_id,
        finished_at: finished.then(Instant::now),
        tools: group_tools,
    }))
}

fn tool_activity_from_record(record: &ToolCallRecord) -> ToolActivity {
    let now = Instant::now();
    let duration = record
        .duration_ms
        .and_then(|ms| (ms >= 0).then_some(Duration::from_millis(ms as u64)))
        .unwrap_or_default();
    let finished_at = (!matches!(record.status, ToolStatus::Running)).then_some(now);
    ToolActivity {
        id: record.call_id.clone(),
        name: record.name.clone(),
        arguments: record.arguments.clone(),
        delegated_model: record.delegated_model.clone(),
        status: record.status,
        result: record.result.clone(),
        diff: record.diff.clone().map(Box::new),
        started_at: now.checked_sub(duration).unwrap_or(now),
        finished_at,
    }
}

fn parse_tool_result(value: Option<String>) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let parsed: Value = serde_json::from_str(&value).context("Failed to parse tool result JSON")?;
    Ok(parsed
        .get("text")
        .and_then(Value::as_str)
        .map(ToString::to_string))
}

fn parse_tool_diff(value: Option<String>) -> Result<Option<crate::diff::FileDiff>> {
    value
        .map(|value| serde_json::from_str(&value).context("Failed to parse tool diff JSON"))
        .transpose()
}

async fn insert_tool_call(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session_id: i64,
    seq: i64,
    activity: &ToolActivity,
    now: i64,
) -> Result<()> {
    let result_json = activity
        .result
        .as_ref()
        .map(|result| json!({ "text": result }).to_string());
    let diff_json = activity
        .diff
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .context("Failed to serialize tool diff")?;
    let duration_ms = activity
        .finished_at
        .map(|_| duration_to_i64_ms(activity.duration()));
    // The activity clock is a monotonic `Instant`, so absolute start/finish are
    // anchored to the flush time (`now`): a finished tool finished ~now, and its
    // start is `now - duration` (duration is the real elapsed time). This gives
    // a correct duration placement; the absolute anchor is within a flush
    // interval of the truth. `duration_ms` remains the authoritative per-tool
    // timing column.
    let finished_at_ms = activity.finished_at.map(|_| now);
    let started_at_ms = finished_at_ms
        .map(|finished| finished - duration_ms.unwrap_or(0))
        .unwrap_or(now);

    sqlx::query(
        r#"
        INSERT INTO tool_calls (
          session_id, call_id, seq, name, args_json, result_json, diff_json,
          delegated_model, duration_ms, status, started_at_ms, finished_at_ms
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        -- A call_id can legitimately appear twice in one snapshot (standalone
        -- plus inside an ExecutionGroup, or re-emitted); last write wins rather
        -- than violating UNIQUE(session_id, call_id) and rolling back the whole
        -- transcript flush (BUG-7).
        ON CONFLICT(session_id, call_id) DO UPDATE SET
          seq = excluded.seq,
          name = excluded.name,
          args_json = excluded.args_json,
          result_json = excluded.result_json,
          diff_json = excluded.diff_json,
          delegated_model = excluded.delegated_model,
          duration_ms = excluded.duration_ms,
          status = excluded.status,
          started_at_ms = excluded.started_at_ms,
          finished_at_ms = excluded.finished_at_ms
        "#,
    )
    .bind(session_id)
    .bind(&activity.id)
    .bind(seq)
    .bind(&activity.name)
    .bind(json_text_or_wrapped(&activity.arguments))
    .bind(result_json)
    .bind(diff_json)
    .bind(&activity.delegated_model)
    .bind(duration_ms)
    .bind(activity.status.as_db_str())
    .bind(started_at_ms)
    .bind(finished_at_ms)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn transcript_block_record(item: &TranscriptItem) -> Result<TranscriptBlockRecord> {
    match item {
        TranscriptItem::UserMessage { text } => Ok(block("user", "", text, Some("user"))),
        TranscriptItem::QueuedUserMessage { id, text, delivery } => Ok(TranscriptBlockRecord {
            kind: "queued_user",
            title: String::new(),
            body: text.clone(),
            metadata_json: json!({
                "id": id,
                "delivery": delivery.pending_label(),
            })
            .to_string(),
            message_role: None,
        }),
        TranscriptItem::AssistantMessage { text } => {
            Ok(block("assistant", "", text, Some("assistant")))
        }
        TranscriptItem::ReasoningSummary { text } => {
            Ok(block("thinking", "", text, Some("thinking")))
        }
        TranscriptItem::ToolActivity(activity) => {
            let mut metadata = json!({
                "id": activity.id,
                "status": activity.status.as_db_str(),
            });
            if let Some(review) = self_review_tool_metadata(activity) {
                metadata["self_review"] = review;
            }
            Ok(TranscriptBlockRecord {
                kind: "tool",
                title: activity.name.clone(),
                body: activity.result.clone().unwrap_or_default(),
                metadata_json: metadata.to_string(),
                message_role: None,
            })
        }
        TranscriptItem::CommandOutput { kind, text } => Ok(TranscriptBlockRecord {
            kind: kind.as_db_str(),
            title: String::new(),
            body: text.clone(),
            metadata_json: "{}".to_string(),
            message_role: Some(kind.message_role()),
        }),
        TranscriptItem::CompletionReport(report) => Ok(TranscriptBlockRecord {
            kind: "completion_report",
            title: "Completion Report".to_string(),
            body: report.render_compact(),
            metadata_json: serde_json::to_string(report.as_ref())
                .context("Failed to serialize completion report")?,
            message_role: None,
        }),
        TranscriptItem::ExecutionGroup(group) => Ok(execution_group_block(group)),
        TranscriptItem::Error { error } => Ok(TranscriptBlockRecord {
            kind: "error",
            title: error.title.clone(),
            body: error.detail.clone(),
            metadata_json: "{}".to_string(),
            message_role: Some("error"),
        }),
        TranscriptItem::Edit { text } => Ok(block("edit", "", text, Some("system"))),
        TranscriptItem::TodoUpdate { text } => Ok(block("todo", "", text, Some("system"))),
        // `message_role: None` so a downgraded scratch note writes no
        // `messages` row and can't resurface as an assistant response in
        // the message-history fallback on resume.
        TranscriptItem::WorkLog { text } => Ok(block("worklog", "", text, None)),
        // Peer chat also writes no `messages` row: the exchange already reaches
        // model context through the injection path, so a history row would
        // double it on resume.
        TranscriptItem::PeerMessage {
            source_message_id,
            session_id,
            outgoing,
            text,
        } => Ok(TranscriptBlockRecord {
            kind: "peer_message",
            title: String::new(),
            body: text.clone(),
            metadata_json: json!({
                "source_message_id": source_message_id,
                "session_id": session_id,
                "outgoing": outgoing
            })
            .to_string(),
            message_role: None,
        }),
    }
}

fn block(
    kind: &'static str,
    title: &str,
    body: &str,
    message_role: Option<&'static str>,
) -> TranscriptBlockRecord {
    TranscriptBlockRecord {
        kind,
        title: title.to_string(),
        body: body.to_string(),
        metadata_json: "{}".to_string(),
        message_role,
    }
}

fn execution_group_block(group: &ExecutionGroup) -> TranscriptBlockRecord {
    let body = group
        .tools
        .iter()
        .map(|activity| {
            format!(
                "{}: {}",
                activity.name,
                activity
                    .result
                    .clone()
                    .unwrap_or_else(|| "running".to_string())
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let self_reviews = group
        .tools
        .iter()
        .filter_map(|activity| {
            self_review_tool_metadata(activity).map(|metadata| (activity.id.clone(), metadata))
        })
        .collect::<serde_json::Map<_, _>>();
    let mut metadata = json!({
        "id": group.id,
        "finished": group.finished_at.is_some(),
        "tool_ids": group.tools.iter().map(|activity| activity.id.as_str()).collect::<Vec<_>>(),
    });
    if !self_reviews.is_empty() {
        metadata["self_reviews"] = Value::Object(self_reviews);
    }
    TranscriptBlockRecord {
        kind: "execution_group",
        title: format!("{} tool call(s)", group.tools.len()),
        body,
        metadata_json: metadata.to_string(),
        message_role: None,
    }
}

fn self_review_tool_metadata(activity: &ToolActivity) -> Option<Value> {
    if activity.name != "agent" {
        return None;
    }
    let arguments = serde_json::from_str::<Value>(&activity.arguments).ok()?;
    if arguments.get("agent").and_then(Value::as_str) != Some("self-review") {
        return None;
    }
    let findings = crate::self_review::SelfReviewFindingCounts::parse(
        activity.result.as_deref().unwrap_or_default(),
    );
    Some(json!({
        "findings": findings,
        "finding_count": findings.total(),
    }))
}

fn tool_activities(item: &TranscriptItem) -> Vec<&ToolActivity> {
    match item {
        TranscriptItem::ToolActivity(activity) => vec![activity],
        TranscriptItem::ExecutionGroup(group) => group.tools.iter().collect(),
        _ => Vec::new(),
    }
}
fn json_text_or_wrapped(text: &str) -> String {
    if text.trim().is_empty() {
        return "{}".to_string();
    }
    match serde_json::from_str::<Value>(text) {
        Ok(_) => text.to_string(),
        Err(_) => json!({ "raw": text }).to_string(),
    }
}
fn duration_to_i64_ms(duration: Duration) -> i64 {
    duration.as_millis().min(i64::MAX as u128) as i64
}
