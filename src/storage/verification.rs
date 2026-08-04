use super::*;
use crate::verification::{
    VerificationCheckRecord, VerificationCheckStatus, VerificationKind, VerificationRunRecord,
    VerificationRunStatus, VerificationTerminalReason,
};

impl Storage {
    #[cfg(test)]
    pub(crate) async fn replace_verification_runs_snapshot(
        &self,
        session_id: SessionId,
        runs: &[VerificationRunRecord],
    ) -> Result<()> {
        self.with_session_snapshot_tx("verification runs snapshot", async move |tx, now| {
            self.replace_verification_runs_snapshot_in_tx(tx, session_id, runs, now)
                .await
        })
        .await
    }

    pub(crate) async fn replace_verification_runs_snapshot_in_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        session_id: SessionId,
        runs: &[VerificationRunRecord],
        now: i64,
    ) -> Result<()> {
        storage_op!(
            tx,
            "delete verification runs",
            sqlx::query("DELETE FROM verification_runs WHERE session_id = ?")
                .bind(session_id.as_i64()),
        )?;

        for (run_seq, run) in runs.iter().enumerate() {
            let changes_json = serde_json::to_string(&run.workspace_changes_after_last_check)
                .context("Failed to serialize verification workspace changes")?;
            let reasoning_escalations_json = serde_json::to_string(&run.reasoning_escalations)
                .context("Failed to serialize verification reasoning escalations")?;
            let delivered_binding_json = run
                .delivered_workspace_binding
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .context("Failed to serialize delivered verification binding")?;
            storage_op!(
                tx,
                "insert verification run",
                sqlx::query(
                    r#"
                    INSERT INTO verification_runs (
                      session_id, seq, kind, status, started_at_ms, finished_at_ms,
                      observed_final_workspace, workspace_changes_json, repair_attempts,
                      reasoning_escalations_json, terminal_reason, terminal_reason_kind,
                      delivered_binding_json
                    )
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                    "#,
                )
                .bind(session_id.as_i64())
                .bind(run_seq as i64)
                .bind(run.kind.label())
                .bind(run.status.label())
                .bind(run.started_at_ms)
                .bind(run.finished_at_ms)
                .bind(run.observed_final_workspace.map(i64::from))
                .bind(changes_json)
                .bind(i64::from(run.repair_attempts))
                .bind(reasoning_escalations_json)
                .bind(&run.terminal_reason)
                .bind(
                    run.terminal_reason_kind
                        .as_ref()
                        .map(VerificationTerminalReason::label)
                )
                .bind(delivered_binding_json),
            )?;

            for (check_seq, check) in run.checks.iter().enumerate() {
                let binding_json = check
                    .binding
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()
                    .context("Failed to serialize verification check binding")?;
                let delivered_binding_json = check
                    .delivered_binding
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()
                    .context("Failed to serialize delivered check binding")?;
                let attempt_timestamps_json =
                    serde_json::to_string(&check.attempt_timestamps_ms)
                        .context("Failed to serialize verification attempt timestamps")?;
                let failure_signatures_json = serde_json::to_string(&check.failure_signatures)
                    .context("Failed to serialize verification failure signatures")?;
                storage_op!(
                    tx,
                    "insert verification check",
                    sqlx::query(
                        r#"
                        INSERT INTO verification_checks (
                          session_id, run_seq, seq, name, command, status,
                          tool_call_id, exit_code, completed_at_ms, attempt_count,
                          last_failure_signature, binding_json, delivered_binding_json,
                          attempt_timestamps_json,
                          failure_signatures_json, terminal_reason_kind
                        )
                        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                        "#,
                    )
                    .bind(session_id.as_i64())
                    .bind(run_seq as i64)
                    .bind(check_seq as i64)
                    .bind(&check.name)
                    .bind(&check.command)
                    .bind(check.status.label())
                    .bind(&check.tool_call_id)
                    .bind(check.exit_code)
                    .bind(check.completed_at_ms)
                    .bind(i64::from(check.attempt_count))
                    .bind(&check.last_failure_signature)
                    .bind(binding_json)
                    .bind(delivered_binding_json)
                    .bind(attempt_timestamps_json)
                    .bind(failure_signatures_json)
                    .bind(
                        check
                            .terminal_reason_kind
                            .as_ref()
                            .map(VerificationTerminalReason::label)
                    ),
                )?;
            }
        }
        touch_session(tx, session_id, now).await
    }

    pub(super) async fn load_verification_runs(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<VerificationRunRecord>> {
        let run_rows = sqlx::query(
            "SELECT seq, kind, status, started_at_ms, finished_at_ms, \
             observed_final_workspace, workspace_changes_json, repair_attempts, \
             reasoning_escalations_json, terminal_reason, terminal_reason_kind, \
             delivered_binding_json \
             FROM verification_runs WHERE session_id = ? ORDER BY seq",
        )
        .bind(session_id.as_i64())
        .fetch_all(&self.pool)
        .await
        .context("Failed to load verification runs")?;

        let mut runs = Vec::with_capacity(run_rows.len());
        for row in run_rows {
            let run_seq: i64 = row.try_get("seq")?;
            let kind_label: String = row.try_get("kind")?;
            let status_label: String = row.try_get("status")?;
            let check_rows = sqlx::query(
                "SELECT name, command, status, tool_call_id, exit_code, completed_at_ms, \
                 attempt_count, last_failure_signature, binding_json, delivered_binding_json, \
                 attempt_timestamps_json, \
                 failure_signatures_json, terminal_reason_kind \
                 FROM verification_checks \
                 WHERE session_id = ? AND run_seq = ? ORDER BY seq",
            )
            .bind(session_id.as_i64())
            .bind(run_seq)
            .fetch_all(&self.pool)
            .await
            .context("Failed to load verification checks")?;
            let checks = check_rows
                .into_iter()
                .map(|check_row| {
                    let check_status: String = check_row.try_get("status")?;
                    let binding_json: Option<String> = check_row.try_get("binding_json")?;
                    let delivered_binding_json: Option<String> =
                        check_row.try_get("delivered_binding_json")?;
                    let terminal_reason_kind: Option<String> =
                        check_row.try_get("terminal_reason_kind")?;
                    Ok(VerificationCheckRecord {
                        name: check_row.try_get("name")?,
                        command: check_row.try_get("command")?,
                        status: VerificationCheckStatus::from_label(&check_status).with_context(
                            || format!("Unknown verification check status {check_status:?}"),
                        )?,
                        tool_call_id: check_row.try_get("tool_call_id")?,
                        exit_code: check_row.try_get("exit_code")?,
                        completed_at_ms: check_row.try_get("completed_at_ms")?,
                        attempt_count: u32::try_from(check_row.try_get::<i64, _>("attempt_count")?)
                            .context("Verification attempt count is out of range")?,
                        last_failure_signature: check_row.try_get("last_failure_signature")?,
                        binding: binding_json
                            .as_deref()
                            .map(serde_json::from_str)
                            .transpose()
                            .context("Failed to parse verification check binding")?,
                        delivered_binding: delivered_binding_json
                            .as_deref()
                            .map(serde_json::from_str)
                            .transpose()
                            .context("Failed to parse delivered check binding")?,
                        attempt_timestamps_ms: serde_json::from_str(
                            &check_row.try_get::<String, _>("attempt_timestamps_json")?,
                        )
                        .context("Failed to parse verification attempt timestamps")?,
                        failure_signatures: serde_json::from_str(
                            &check_row.try_get::<String, _>("failure_signatures_json")?,
                        )
                        .context("Failed to parse verification failure signatures")?,
                        terminal_reason_kind: terminal_reason_kind
                            .as_deref()
                            .map(|label| {
                                VerificationTerminalReason::from_label(label).with_context(|| {
                                    format!("Unknown verification terminal reason {label:?}")
                                })
                            })
                            .transpose()?,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let changes_json: String = row.try_get("workspace_changes_json")?;
            let reasoning_escalations_json: String = row.try_get("reasoning_escalations_json")?;
            let terminal_reason_kind: Option<String> = row.try_get("terminal_reason_kind")?;
            let delivered_binding_json: Option<String> = row.try_get("delivered_binding_json")?;
            runs.push(VerificationRunRecord {
                kind: VerificationKind::from_label(&kind_label)
                    .with_context(|| format!("Unknown verification kind {kind_label:?}"))?,
                status: VerificationRunStatus::from_label(&status_label)
                    .with_context(|| format!("Unknown verification run status {status_label:?}"))?,
                checks,
                started_at_ms: row.try_get("started_at_ms")?,
                finished_at_ms: row.try_get("finished_at_ms")?,
                observed_final_workspace: row
                    .try_get::<Option<i64>, _>("observed_final_workspace")?
                    .map(|value| value != 0),
                workspace_changes_after_last_check: serde_json::from_str(&changes_json)
                    .context("Failed to parse verification workspace changes")?,
                repair_attempts: u32::try_from(row.try_get::<i64, _>("repair_attempts")?)
                    .context("Verification repair count is out of range")?,
                reasoning_escalations: serde_json::from_str(&reasoning_escalations_json)
                    .context("Failed to parse verification reasoning escalations")?,
                terminal_reason: row.try_get("terminal_reason")?,
                terminal_reason_kind: terminal_reason_kind
                    .as_deref()
                    .map(|label| {
                        VerificationTerminalReason::from_label(label).with_context(|| {
                            format!("Unknown verification terminal reason {label:?}")
                        })
                    })
                    .transpose()?,
                delivered_workspace_binding: delivered_binding_json
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()
                    .context("Failed to parse delivered verification binding")?,
            });
        }
        Ok(runs)
    }
}
