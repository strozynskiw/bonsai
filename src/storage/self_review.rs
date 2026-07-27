use super::*;
use crate::self_review::{
    SelfReviewDisposition, SelfReviewFindingCounts, SelfReviewMode, SelfReviewRunRecord,
    SelfReviewScope, SelfReviewStats,
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
                      session_id, seq, started_at_ms, mode, scope, diff_line_count,
                      reviewer_duration_ms, reviewer_prompt_tokens,
                      reviewer_completion_tokens, reviewer_cost_micros,
                      blocker_count, major_count, minor_count, nit_count, disposition
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                    "#,
                )
                .bind(session_id.as_i64())
                .bind(i64::try_from(seq).context("Self-review sequence is out of range")?)
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
            SELECT started_at_ms, mode, scope, diff_line_count,
                   reviewer_duration_ms, reviewer_prompt_tokens,
                   reviewer_completion_tokens, reviewer_cost_micros,
                   blocker_count, major_count, minor_count, nit_count, disposition
            FROM self_review_runs WHERE session_id = ? ORDER BY seq
            "#,
        )
        .bind(session_id.as_i64())
        .fetch_all(&self.pool)
        .await
        .context("Failed to load self-review runs")?;

        rows.into_iter()
            .map(|row| {
                let mode: String = row.try_get("mode")?;
                let scope: String = row.try_get("scope")?;
                let disposition: Option<String> = row.try_get("disposition")?;
                Ok(SelfReviewRunRecord {
                    started_at_ms: row.try_get("started_at_ms")?,
                    mode: SelfReviewMode::parse(&mode)
                        .with_context(|| format!("Unknown self-review mode {mode:?}"))?,
                    scope: SelfReviewScope::from_label(&scope)
                        .with_context(|| format!("Unknown self-review scope {scope:?}"))?,
                    diff_line_count: read_u32(&row, "diff_line_count")?,
                    reviewer_duration_ms: read_u64(&row, "reviewer_duration_ms")?,
                    reviewer_prompt_tokens: read_u64(&row, "reviewer_prompt_tokens")?,
                    reviewer_completion_tokens: read_u64(&row, "reviewer_completion_tokens")?,
                    reviewer_cost_micros: row
                        .try_get::<Option<i64>, _>("reviewer_cost_micros")?
                        .map(|value| u64::try_from(value).context("Reviewer cost is out of range"))
                        .transpose()?,
                    findings: SelfReviewFindingCounts {
                        blocker: read_u32(&row, "blocker_count")?,
                        major: read_u32(&row, "major_count")?,
                        minor: read_u32(&row, "minor_count")?,
                        nit: read_u32(&row, "nit_count")?,
                    },
                    disposition: disposition
                        .map(|value| {
                            SelfReviewDisposition::from_label(&value).with_context(|| {
                                format!("Unknown self-review disposition {value:?}")
                            })
                        })
                        .transpose()?,
                })
            })
            .collect()
    }

    pub(super) async fn load_self_review_stats(&self) -> Result<SelfReviewStats> {
        let row = sqlx::query(
            r#"
            SELECT COUNT(*) AS runs,
                   COALESCE(SUM(CASE WHEN blocker_count + major_count + minor_count + nit_count > 0 THEN 1 ELSE 0 END), 0) AS runs_with_findings,
                   COALESCE(SUM(CASE WHEN disposition = 'fixed' THEN 1 ELSE 0 END), 0) AS fixed,
                   COALESCE(SUM(CASE WHEN disposition = 'none_needed' THEN 1 ELSE 0 END), 0) AS none_needed,
                   COALESCE(SUM(CASE WHEN disposition = 'rebutted' THEN 1 ELSE 0 END), 0) AS rebutted,
                   COALESCE(SUM(blocker_count + major_count + minor_count + nit_count), 0) AS findings,
                   COALESCE(SUM(reviewer_duration_ms), 0) AS reviewer_duration_ms,
                   COALESCE(SUM(reviewer_cost_micros), 0) AS reviewer_cost_micros
            FROM self_review_runs
            "#,
        )
        .fetch_one(&self.pool)
        .await
        .context("Failed to load self-review stats")?;
        Ok(SelfReviewStats {
            runs: row.try_get("runs")?,
            runs_with_findings: row.try_get("runs_with_findings")?,
            fixed: row.try_get("fixed")?,
            none_needed: row.try_get("none_needed")?,
            rebutted: row.try_get("rebutted")?,
            findings: row.try_get("findings")?,
            reviewer_duration_ms: row.try_get("reviewer_duration_ms")?,
            reviewer_cost_micros: row.try_get("reviewer_cost_micros")?,
        })
    }
}

fn read_u32(row: &sqlx::sqlite::SqliteRow, column: &str) -> Result<u32> {
    u32::try_from(row.try_get::<i64, _>(column)?)
        .with_context(|| format!("Self-review {column} is out of range"))
}

fn read_u64(row: &sqlx::sqlite::SqliteRow, column: &str) -> Result<u64> {
    u64::try_from(row.try_get::<i64, _>(column)?)
        .with_context(|| format!("Self-review {column} is out of range"))
}
