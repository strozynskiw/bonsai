use super::*;

impl Storage {
    pub(crate) async fn replace_plan_snapshot_in_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        session_id: SessionId,
        plan: &PlanDoc,
        now: i64,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO plans (session_id, title, revision, updated_at_ms)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(session_id) DO UPDATE SET
              title = excluded.title,
              revision = excluded.revision,
              updated_at_ms = excluded.updated_at_ms
            "#,
        )
        .bind(session_id.as_i64())
        .bind(&plan.title)
        .bind(plan.revision as i64)
        .bind(now)
        .execute(&mut **tx)
        .await
        .context("Failed to upsert plan")?;
        replace_plan_rows(tx, session_id.as_i64(), plan).await?;
        touch_session(tx, session_id, now).await
    }

    /// Freeze the current plan into the library as an immutable JSON snapshot.
    /// `saved_plan_id: None` creates a new entry; `Some` overwrites that
    /// entry's content in place (the re-save flow). Later live edits never
    /// touch the snapshot — saving again is the only way to update it.
    pub async fn save_plan_to_library(
        &self,
        current_session_id: SessionId,
        saved_plan_id: Option<SavedPlanId>,
        plan: &PlanDoc,
        branch: Option<&str>,
    ) -> Result<SavedPlanSummary> {
        let now = now_ms();
        let doc_json = serde_json::to_string(plan).context("Failed to serialize plan snapshot")?;
        let section_count = plan.sections.len() as i64;
        let task_count = (plan.tasks.len()
            + plan
                .phases
                .iter()
                .map(|phase| phase.tasks.len())
                .sum::<usize>()) as i64;

        let plan_id = match saved_plan_id {
            Some(plan_id) => {
                let result = sqlx::query(
                    r#"
                    UPDATE saved_plans
                    SET title = ?, branch = ?, saved_at_ms = ?, updated_at_ms = ?,
                        section_count = ?, task_count = ?, doc_json = ?
                    WHERE id = ?
                    "#,
                )
                .bind(&plan.title)
                .bind(branch.map(str::to_string))
                .bind(now)
                .bind(now)
                .bind(section_count)
                .bind(task_count)
                .bind(&doc_json)
                .bind(plan_id.as_i64())
                .execute(&self.pool)
                .await
                .with_context(|| format!("Failed to update saved plan #{plan_id}"))?;
                if result.rows_affected() == 0 {
                    anyhow::bail!(
                        "Saved plan #{plan_id} no longer exists; save again to recreate it"
                    );
                }
                plan_id
            }
            None => {
                let row = sqlx::query(
                    r#"
                    INSERT INTO saved_plans (
                      project_id, title, source_session_id, branch, status,
                      saved_at_ms, updated_at_ms, section_count, task_count, doc_json
                    )
                    SELECT sessions.project_id, ?, ?, ?, ?, ?, ?, ?, ?, ?
                    FROM sessions WHERE sessions.id = ?
                    RETURNING id
                    "#,
                )
                .bind(&plan.title)
                .bind(current_session_id.as_i64())
                .bind(branch.map(str::to_string))
                .bind(SavedPlanStatus::Draft.as_db_str())
                .bind(now)
                .bind(now)
                .bind(section_count)
                .bind(task_count)
                .bind(&doc_json)
                .bind(current_session_id.as_i64())
                .fetch_optional(&self.pool)
                .await
                .context("Failed to save plan to the library")?
                .ok_or_else(|| {
                    anyhow::anyhow!("Session #{current_session_id} was not found for plan save")
                })?;
                SavedPlanId::from_raw(row.try_get("id")?)
            }
        };

        self.saved_plan_summary(plan_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Saved plan #{plan_id} was not found"))
    }

    pub async fn saved_plans_for_project(
        &self,
        project_path: &Path,
        limit: i64,
    ) -> Result<Vec<SavedPlanSummary>> {
        let project_path = canonical_project_path(project_path);
        let query = saved_plan_summary_query(
            "projects.path = ?",
            "ORDER BY saved_plans.saved_at_ms DESC, saved_plans.updated_at_ms DESC\n            LIMIT ?",
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(project_path)
            .bind(limit.max(1))
            .fetch_all(&self.pool)
            .await
            .context("Failed to list saved plans")?;

        rows.into_iter().map(saved_plan_summary_from_row).collect()
    }

    pub async fn load_saved_plan(&self, plan_id: SavedPlanId) -> Result<Option<SavedPlanSnapshot>> {
        let Some(summary) = self.saved_plan_summary(plan_id).await? else {
            return Ok(None);
        };
        let doc_json: String = sqlx::query_scalar("SELECT doc_json FROM saved_plans WHERE id = ?")
            .bind(plan_id.as_i64())
            .fetch_one(&self.pool)
            .await
            .with_context(|| format!("Failed to load saved plan #{plan_id} content"))?;
        let plan = serde_json::from_str(&doc_json)
            .with_context(|| format!("Failed to parse saved plan #{plan_id} snapshot"))?;
        Ok(Some(SavedPlanSnapshot { summary, plan }))
    }

    pub async fn delete_saved_plan(&self, plan_id: SavedPlanId) -> Result<bool> {
        let mut tx = self
            .pool
            .begin()
            .await
            .context("Failed to begin saved-plan delete transaction")?;
        let now = now_ms();
        sqlx::query(
            r#"
            UPDATE sessions
            SET source_plan_id = NULL, updated_at_ms = ?
            WHERE source_plan_id = ?
            "#,
        )
        .bind(now)
        .bind(plan_id.as_i64())
        .execute(&mut *tx)
        .await?;
        // Saved plans own their snapshot outright (no shared rows with any
        // session's live plan), so deleting one is a plain DELETE.
        let result = sqlx::query("DELETE FROM saved_plans WHERE id = ?")
            .bind(plan_id.as_i64())
            .execute(&mut *tx)
            .await
            .with_context(|| format!("Failed to delete saved plan #{plan_id}"))?;
        tx.commit()
            .await
            .context("Failed to commit saved-plan delete transaction")?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn mark_plan_started(
        &self,
        plan_id: SavedPlanId,
        execution_session_id: SessionId,
    ) -> Result<bool> {
        let mut tx = self
            .pool
            .begin()
            .await
            .context("Failed to begin saved-plan start transaction")?;
        let now = now_ms();
        let result = sqlx::query(
            r#"
            UPDATE saved_plans
            SET status = ?, execution_session_id = ?, updated_at_ms = ?
            WHERE id = ?
            "#,
        )
        .bind(SavedPlanStatus::Started.as_db_str())
        .bind(execution_session_id.as_i64())
        .bind(now)
        .bind(plan_id.as_i64())
        .execute(&mut *tx)
        .await
        .with_context(|| format!("Failed to mark saved plan #{plan_id} as started"))?;
        if result.rows_affected() == 0 {
            tx.commit()
                .await
                .context("Failed to commit saved-plan start transaction")?;
            return Ok(false);
        }

        sqlx::query(
            r#"
            UPDATE sessions
            SET source_plan_id = ?, updated_at_ms = ?
            WHERE id = ?
            "#,
        )
        .bind(plan_id.as_i64())
        .bind(now)
        .bind(execution_session_id.as_i64())
        .execute(&mut *tx)
        .await
        .with_context(|| {
            format!("Failed to link session #{execution_session_id} to saved plan #{plan_id}")
        })?;

        tx.commit()
            .await
            .context("Failed to commit saved-plan start transaction")?;
        Ok(true)
    }

    pub async fn set_session_source_plan(
        &self,
        session_id: SessionId,
        plan_id: Option<SavedPlanId>,
    ) -> Result<()> {
        let now = now_ms();
        sqlx::query(
            r#"
            UPDATE sessions
            SET source_plan_id = ?, updated_at_ms = ?
            WHERE id = ?
            "#,
        )
        .bind(plan_id.map(SavedPlanId::as_i64))
        .bind(now)
        .bind(session_id.as_i64())
        .execute(&self.pool)
        .await
        .with_context(|| format!("Failed to set source plan for session #{session_id}"))?;
        Ok(())
    }

    async fn saved_plan_summary(&self, plan_id: SavedPlanId) -> Result<Option<SavedPlanSummary>> {
        let query = saved_plan_summary_query("saved_plans.id = ?", "");
        let row = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(plan_id.as_i64())
            .fetch_optional(&self.pool)
            .await
            .with_context(|| format!("Failed to load saved plan #{plan_id}"))?;

        row.map(saved_plan_summary_from_row).transpose()
    }
    pub(super) async fn load_plan_snapshot(&self, session_id: SessionId) -> Result<PlanDoc> {
        let plan_row = sqlx::query("SELECT title, revision FROM plans WHERE session_id = ?")
            .bind(session_id.as_i64())
            .fetch_optional(&self.pool)
            .await
            .with_context(|| format!("Failed to load plan for session {session_id}"))?;

        let Some(plan_row) = plan_row else {
            return Ok(PlanDoc::default());
        };

        let section_rows = sqlx::query(
            "SELECT heading, body FROM plan_sections WHERE session_id = ? ORDER BY seq",
        )
        .bind(session_id.as_i64())
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("Failed to load plan sections for session {session_id}"))?;
        let question_rows =
            sqlx::query("SELECT text FROM plan_questions WHERE session_id = ? ORDER BY seq")
                .bind(session_id.as_i64())
                .fetch_all(&self.pool)
                .await
                .with_context(|| {
                    format!("Failed to load plan questions for session {session_id}")
                })?;
        let task_rows = sqlx::query(
            "SELECT text, done, phase_seq FROM plan_tasks WHERE session_id = ? ORDER BY seq",
        )
        .bind(session_id.as_i64())
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("Failed to load plan tasks for session {session_id}"))?;
        let phase_rows =
            sqlx::query("SELECT name FROM plan_phases WHERE session_id = ? ORDER BY seq")
                .bind(session_id.as_i64())
                .fetch_all(&self.pool)
                .await
                .with_context(|| format!("Failed to load plan phases for session {session_id}"))?;
        let finding_rows = sqlx::query(
            "SELECT severity, file, line, issue, required_fix, acceptance_tests, source_ids, task, resolved \
             FROM plan_findings WHERE session_id = ? ORDER BY seq",
        )
        .bind(session_id.as_i64())
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("Failed to load plan findings for session {session_id}"))?;

        let mut phases = phase_rows
            .into_iter()
            .map(|row| {
                Ok(crate::plan::PlanPhase {
                    name: row.try_get("name")?,
                    tasks: Vec::new(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        // Bucket each task into its owning phase by phase_seq; NULL (or an
        // out-of-range index from a corrupt row) stays a flat, unphased task.
        let mut tasks = Vec::new();
        for row in task_rows {
            let task = crate::plan::PlanTask {
                text: row.try_get("text")?,
                done: row.try_get::<i64, _>("done")? != 0,
            };
            match row.try_get::<Option<i64>, _>("phase_seq")? {
                Some(phase_seq) if (phase_seq as usize) < phases.len() => {
                    phases[phase_seq as usize].tasks.push(task);
                }
                _ => tasks.push(task),
            }
        }

        let findings = finding_rows
            .into_iter()
            .map(|row| {
                let severity =
                    crate::plan::Severity::from_db_str(&row.try_get::<String, _>("severity")?)
                        .unwrap_or(crate::plan::Severity::Major);
                let acceptance_tests: Vec<String> =
                    serde_json::from_str(&row.try_get::<String, _>("acceptance_tests")?)
                        .unwrap_or_default();
                let source_ids: Vec<String> =
                    serde_json::from_str(&row.try_get::<String, _>("source_ids")?)
                        .unwrap_or_default();
                Ok(crate::plan::Finding {
                    severity,
                    file: row.try_get::<Option<String>, _>("file")?,
                    line: row
                        .try_get::<Option<i64>, _>("line")?
                        .map(|line| line as u32),
                    issue: row.try_get("issue")?,
                    required_fix: row.try_get("required_fix")?,
                    acceptance_tests,
                    source_ids,
                    task: row.try_get::<Option<String>, _>("task")?,
                    resolved: row.try_get::<i64, _>("resolved")? != 0,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(PlanDoc {
            title: plan_row.try_get("title")?,
            sections: section_rows
                .into_iter()
                .map(|row| {
                    Ok(crate::plan::PlanSection {
                        heading: row.try_get("heading")?,
                        body: row.try_get("body")?,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            questions: question_rows
                .into_iter()
                .map(|row| row.try_get("text").map_err(anyhow::Error::from))
                .collect::<Result<Vec<String>>>()?,
            tasks,
            phases,
            findings,
            revision: plan_row.try_get::<i64, _>("revision")?.max(0) as u64,
        })
    }
}

fn saved_plan_summary_query(where_clause: &str, suffix: &str) -> String {
    format!(
        r#"
            SELECT
{SAVED_PLAN_SUMMARY_PROJECTION}
            FROM saved_plans
            JOIN projects ON projects.id = saved_plans.project_id
            WHERE {where_clause}
            {suffix}
            "#
    )
}

fn saved_plan_summary_from_row(row: sqlx::sqlite::SqliteRow) -> Result<SavedPlanSummary> {
    Ok(SavedPlanSummary {
        id: SavedPlanId::from_raw(row.try_get("id")?),
        project_path: row.try_get("project_path")?,
        title: row.try_get("title")?,
        source_session_id: row
            .try_get::<Option<i64>, _>("source_session_id")?
            .map(SessionId::from_raw),
        branch: row.try_get("branch")?,
        status: SavedPlanStatus::from_db_str(&row.try_get::<String, _>("status")?),
        execution_session_id: row
            .try_get::<Option<i64>, _>("execution_session_id")?
            .map(SessionId::from_raw),
        saved_at_ms: row.try_get("saved_at_ms")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
        section_count: row.try_get("section_count")?,
        task_count: row.try_get("task_count")?,
    })
}

async fn replace_plan_rows(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: i64,
    plan: &PlanDoc,
) -> Result<()> {
    sqlx::query("DELETE FROM plan_sections WHERE session_id = ?")
        .bind(session_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM plan_questions WHERE session_id = ?")
        .bind(session_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM plan_tasks WHERE session_id = ?")
        .bind(session_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM plan_phases WHERE session_id = ?")
        .bind(session_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM plan_findings WHERE session_id = ?")
        .bind(session_id)
        .execute(&mut **tx)
        .await?;
    for (seq, section) in plan.sections.iter().enumerate() {
        sqlx::query(
            "INSERT INTO plan_sections (session_id, seq, heading, body) VALUES (?, ?, ?, ?)",
        )
        .bind(session_id)
        .bind(seq as i64)
        .bind(&section.heading)
        .bind(&section.body)
        .execute(&mut **tx)
        .await?;
    }
    for (seq, question) in plan.questions.iter().enumerate() {
        sqlx::query("INSERT INTO plan_questions (session_id, seq, text) VALUES (?, ?, ?)")
            .bind(session_id)
            .bind(seq as i64)
            .bind(question)
            .execute(&mut **tx)
            .await?;
    }
    for (seq, phase) in plan.phases.iter().enumerate() {
        sqlx::query("INSERT INTO plan_phases (session_id, seq, name) VALUES (?, ?, ?)")
            .bind(session_id)
            .bind(seq as i64)
            .bind(&phase.name)
            .execute(&mut **tx)
            .await?;
    }
    // plan_tasks keeps one row per task across the whole plan (UNIQUE seq), with
    // phase_seq tying each row to its owning phase (NULL = flat task). Flat tasks
    // come first, then each phase's tasks in order, so the global seq preserves
    // both flat order and per-phase order on load.
    let mut task_seq: i64 = 0;
    for task in &plan.tasks {
        insert_plan_task(tx, session_id, task_seq, task, None).await?;
        task_seq += 1;
    }
    for (phase_seq, phase) in plan.phases.iter().enumerate() {
        for task in &phase.tasks {
            insert_plan_task(tx, session_id, task_seq, task, Some(phase_seq as i64)).await?;
            task_seq += 1;
        }
    }
    for (seq, finding) in plan.findings.iter().enumerate() {
        sqlx::query(
            "INSERT INTO plan_findings \
             (session_id, seq, severity, file, line, issue, required_fix, acceptance_tests, source_ids, task, resolved) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(session_id)
        .bind(seq as i64)
        .bind(finding.severity.as_db_str())
        .bind(finding.file.as_deref())
        .bind(finding.line.map(|line| line as i64))
        .bind(&finding.issue)
        .bind(&finding.required_fix)
        .bind(serde_json::to_string(&finding.acceptance_tests)?)
        .bind(serde_json::to_string(&finding.source_ids)?)
        .bind(finding.task.as_deref())
        .bind(finding.resolved as i64)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn insert_plan_task(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: i64,
    seq: i64,
    task: &crate::plan::PlanTask,
    phase_seq: Option<i64>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO plan_tasks (session_id, seq, text, done, phase_seq) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(session_id)
    .bind(seq)
    .bind(&task.text)
    .bind(if task.done { 1_i64 } else { 0_i64 })
    .bind(phase_seq)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
