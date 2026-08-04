use super::*;
use crate::self_review::{
    SelfReviewDisposition, SelfReviewFindingCounts, SelfReviewMode, SelfReviewRunRecord,
    SelfReviewRunStatus, SelfReviewScope, SelfReviewStats,
};

impl Storage {
    #[cfg(test)]
    pub(crate) async fn replace_self_review_runs_snapshot(
        &self,
        session_id: SessionId,
        runs: &[SelfReviewRunRecord],
    ) -> Result<()> {
        self.with_session_snapshot_tx("self-review runs snapshot", async move |tx, now| {
            self.replace_self_review_runs_snapshot_in_tx(tx, session_id, runs, now)
                .await
        })
        .await
    }

    pub(crate) async fn replace_self_review_runs_snapshot_in_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        session_id: SessionId,
        runs: &[SelfReviewRunRecord],
        now: i64,
    ) -> Result<()> {
        storage_op!(
            tx,
            "delete self-review runs",
            sqlx::query("DELETE FROM self_review_runs WHERE session_id = ?")
                .bind(session_id.as_i64()),
        )?;

        for (seq, run) in runs.iter().enumerate() {
            storage_op!(
                tx,
                "insert self-review run",
                sqlx::query(
                    r#"
                    INSERT INTO self_review_runs (
                      session_id, seq, tool_call_id, started_at_ms, mode, scope, diff_line_count,
                      reviewer_duration_ms, reviewer_prompt_tokens,
                      reviewer_completion_tokens, reviewer_cost_micros,
                      status, result, blocker_count, major_count, minor_count, nit_count,
                      disposition
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                    "#,
                )
                .bind(session_id.as_i64())
                .bind(i64::try_from(seq).context("Self-review sequence is out of range")?)
                .bind(run.tool_call_id.as_deref())
                .bind(run.started_at_ms)
                .bind(run.mode.label())
                .bind(run.scope.label())
                .bind(i64::from(run.diff_line_count))
                .bind(i64::try_from(run.reviewer_duration_ms).unwrap_or(i64::MAX))
                .bind(i64::try_from(run.reviewer_prompt_tokens).unwrap_or(i64::MAX))
                .bind(i64::try_from(run.reviewer_completion_tokens).unwrap_or(i64::MAX))
                .bind(
                    run.reviewer_cost_micros
                        .map(|value| i64::try_from(value).unwrap_or(i64::MAX)),
                )
                .bind(run.status.label())
                .bind(run.result.as_deref())
                .bind(i64::from(run.findings.blocker))
                .bind(i64::from(run.findings.major))
                .bind(i64::from(run.findings.minor))
                .bind(i64::from(run.findings.nit))
                .bind(run.disposition.map(SelfReviewDisposition::label)),
            )?;
        }
        touch_session(tx, session_id, now).await
    }

    pub(super) async fn load_self_review_runs(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SelfReviewRunRecord>> {
        let rows = sqlx::query(
            r#"
            SELECT tool_call_id, started_at_ms, mode, scope, diff_line_count,
                   reviewer_duration_ms, reviewer_prompt_tokens,
                   reviewer_completion_tokens, reviewer_cost_micros,
                   status, result, blocker_count, major_count, minor_count, nit_count,
                   disposition
            FROM self_review_runs WHERE session_id = ? ORDER BY seq
            "#,
        )
        .bind(session_id.as_i64())
        .fetch_all(&self.pool)
        .await
        .context("Failed to load self-review runs")?;

        let mut runs = rows
            .iter()
            .map(self_review_run_from_row)
            .collect::<Result<Vec<_>>>()?;
        crate::self_review::reconcile_abandoned_runs(&mut runs);
        Ok(runs)
    }

    #[cfg(test)]
    pub(super) async fn load_self_review_stats(&self) -> Result<SelfReviewStats> {
        self.load_self_review_stats_with_quarantine()
            .await
            .map(|(stats, _)| stats)
    }

    pub(super) async fn load_self_review_stats_with_quarantine(
        &self,
    ) -> Result<(SelfReviewStats, i64)> {
        let runs = self.load_all_self_review_runs().await?;
        let duplicates =
            super::quality_evidence::duplicate_self_review_fingerprints(runs.as_slice())?;
        let mut stats = SelfReviewStats::default();
        let mut quarantined = 0_i64;

        for (_, run) in runs {
            if !run.status.is_terminal() {
                continue;
            }
            let fingerprint = super::quality_evidence::self_review_fingerprint(&run)?;
            if duplicates.contains(&fingerprint) {
                quarantined = quarantined.saturating_add(1);
                continue;
            }

            stats.runs = stats.runs.saturating_add(1);
            if run.findings.total() > 0 {
                stats.runs_with_findings = stats.runs_with_findings.saturating_add(1);
            }
            match run.disposition {
                Some(SelfReviewDisposition::Fixed) => {
                    stats.fixed = stats.fixed.saturating_add(1);
                }
                Some(SelfReviewDisposition::NoneNeeded) => {
                    stats.none_needed = stats.none_needed.saturating_add(1);
                }
                Some(SelfReviewDisposition::Rebutted) => {
                    stats.rebutted = stats.rebutted.saturating_add(1);
                }
                None => {}
            }
            stats.findings = stats
                .findings
                .saturating_add(i64::from(run.findings.total()));
            stats.reviewer_duration_ms = stats.reviewer_duration_ms.saturating_add(
                i64::try_from(run.reviewer_duration_ms)
                    .context("Reviewer duration is out of range")?,
            );
            if let Some(cost) = run.reviewer_cost_micros {
                stats.reviewer_cost_micros = stats
                    .reviewer_cost_micros
                    .saturating_add(i64::try_from(cost).context("Reviewer cost is out of range")?);
            }
        }

        Ok((stats, quarantined))
    }

    async fn load_all_self_review_runs(&self) -> Result<Vec<(SessionId, SelfReviewRunRecord)>> {
        let rows = sqlx::query(
            r#"
            SELECT session_id, seq, tool_call_id, started_at_ms, mode, scope, diff_line_count,
                   reviewer_duration_ms, reviewer_prompt_tokens,
                   reviewer_completion_tokens, reviewer_cost_micros,
                   status, result, blocker_count, major_count, minor_count, nit_count,
                   disposition
            FROM self_review_runs
            ORDER BY session_id, seq
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to load all self-review runs")?;

        rows.iter()
            .map(|row| {
                Ok((
                    SessionId::from_raw(row.try_get("session_id")?),
                    self_review_run_from_row(row)?,
                ))
            })
            .collect()
    }
}

fn self_review_run_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<SelfReviewRunRecord> {
    let mode: String = row.try_get("mode")?;
    let scope: String = row.try_get("scope")?;
    let status: String = row.try_get("status")?;
    let disposition: Option<String> = row.try_get("disposition")?;
    Ok(SelfReviewRunRecord {
        tool_call_id: row.try_get("tool_call_id")?,
        started_at_ms: row.try_get("started_at_ms")?,
        mode: SelfReviewMode::parse(&mode)
            .with_context(|| format!("Unknown self-review mode {mode:?}"))?,
        scope: SelfReviewScope::from_label(&scope)
            .with_context(|| format!("Unknown self-review scope {scope:?}"))?,
        diff_line_count: read_u32(row, "diff_line_count")?,
        reviewer_duration_ms: read_u64(row, "reviewer_duration_ms")?,
        reviewer_prompt_tokens: read_u64(row, "reviewer_prompt_tokens")?,
        reviewer_completion_tokens: read_u64(row, "reviewer_completion_tokens")?,
        reviewer_cost_micros: row
            .try_get::<Option<i64>, _>("reviewer_cost_micros")?
            .map(|value| u64::try_from(value).context("Reviewer cost is out of range"))
            .transpose()?,
        status: SelfReviewRunStatus::from_label(&status)
            .with_context(|| format!("Unknown self-review status {status:?}"))?,
        result: row.try_get("result")?,
        findings: SelfReviewFindingCounts {
            blocker: read_u32(row, "blocker_count")?,
            major: read_u32(row, "major_count")?,
            minor: read_u32(row, "minor_count")?,
            nit: read_u32(row, "nit_count")?,
        },
        disposition: disposition
            .map(|value| {
                SelfReviewDisposition::from_label(&value)
                    .with_context(|| format!("Unknown self-review disposition {value:?}"))
            })
            .transpose()?,
    })
}

fn read_u32(row: &sqlx::sqlite::SqliteRow, column: &str) -> Result<u32> {
    u32::try_from(row.try_get::<i64, _>(column)?)
        .with_context(|| format!("Self-review {column} is out of range"))
}

fn read_u64(row: &sqlx::sqlite::SqliteRow, column: &str) -> Result<u64> {
    u64::try_from(row.try_get::<i64, _>(column)?)
        .with_context(|| format!("Self-review {column} is out of range"))
}
