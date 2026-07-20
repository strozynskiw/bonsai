use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct RecoveryId(String);

impl RecoveryId {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        let valid = !value.is_empty()
            && value.len() <= 96
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
        anyhow::ensure!(valid, "Invalid recovery id {value:?}");
        Ok(Self(value.to_string()))
    }

    pub(crate) fn from_generated(value: String) -> Self {
        Self(value)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RecoveryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryState {
    Active,
    Ready,
    Merged,
    Kept,
    Discarded,
    Failed,
}

impl RecoveryState {
    pub(crate) const fn as_db_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Ready => "ready",
            Self::Merged => "merged",
            Self::Kept => "kept",
            Self::Discarded => "discarded",
            Self::Failed => "failed",
        }
    }

    fn parse_db(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "ready" => Some(Self::Ready),
            "merged" => Some(Self::Merged),
            "kept" => Some(Self::Kept),
            "discarded" => Some(Self::Discarded),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecoveryPoint {
    pub id: RecoveryId,
    pub session_id: Option<SessionId>,
    pub project_path: PathBuf,
    pub repository_path: PathBuf,
    pub worktree_path: Option<PathBuf>,
    pub baseline_ref: String,
    pub result_ref: Option<String>,
    pub source_index_tree: String,
    pub state: RecoveryState,
    pub branch_name: Option<String>,
    pub error: Option<String>,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct NewRecoveryPoint<'a> {
    pub id: &'a RecoveryId,
    pub project_path: &'a Path,
    pub repository_path: &'a Path,
    pub worktree_path: &'a Path,
    pub baseline_ref: &'a str,
    pub source_index_tree: &'a str,
}

impl Storage {
    pub(crate) async fn insert_recovery_point(&self, point: NewRecoveryPoint<'_>) -> Result<()> {
        let now = now_ms();
        sqlx::query(
            r#"
            INSERT INTO recovery_points (
              id, project_path, repository_path, worktree_path, baseline_ref,
              source_index_tree, state, created_at_ms, updated_at_ms
            ) VALUES (?, ?, ?, ?, ?, ?, 'active', ?, ?)
            "#,
        )
        .bind(point.id.as_str())
        .bind(path_for_storage(point.project_path))
        .bind(path_for_storage(point.repository_path))
        .bind(path_for_storage(point.worktree_path))
        .bind(point.baseline_ref)
        .bind(point.source_index_tree)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .context("Failed to insert recovery point")?;
        Ok(())
    }

    pub(crate) async fn attach_recovery_session(
        &self,
        id: &RecoveryId,
        session_id: SessionId,
    ) -> Result<()> {
        let result = sqlx::query(
            "UPDATE recovery_points SET session_id = ?, updated_at_ms = ? WHERE id = ?",
        )
        .bind(session_id.as_i64())
        .bind(now_ms())
        .bind(id.as_str())
        .execute(&self.pool)
        .await
        .context("Failed to attach recovery session")?;
        ensure_updated(id, result.rows_affected())
    }

    pub(crate) async fn attach_recovery_workspace_session(
        &self,
        workspace_path: &Path,
        session_id: SessionId,
    ) -> Result<bool> {
        let mut tx = self
            .begin_write()
            .await
            .context("Failed to begin recovery attachment transaction")?;
        let attached = self
            .attach_recovery_workspace_session_in_tx(&mut tx, workspace_path, session_id, now_ms())
            .await?;
        tx.commit()
            .await
            .context("Failed to commit recovery attachment")?;
        Ok(attached)
    }

    pub(super) async fn attach_recovery_workspace_session_in_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        workspace_path: &Path,
        session_id: SessionId,
        now: i64,
    ) -> Result<bool> {
        let workspace_path = workspace_path
            .canonicalize()
            .unwrap_or_else(|_| workspace_path.to_path_buf());
        let rows = sqlx::query(
            "SELECT id, worktree_path FROM recovery_points \
             WHERE state = 'active' AND worktree_path IS NOT NULL",
        )
        .fetch_all(&mut **tx)
        .await
        .context("Failed to find active recovery workspace during session switch")?;
        let matching_id = rows
            .into_iter()
            .filter_map(|row| {
                let id = row.try_get::<String, _>("id").ok()?;
                let worktree = PathBuf::from(row.try_get::<String, _>("worktree_path").ok()?);
                let worktree = worktree.canonicalize().unwrap_or(worktree);
                workspace_path
                    .starts_with(&worktree)
                    .then_some((worktree.components().count(), id))
            })
            .max_by_key(|(depth, _)| *depth)
            .map(|(_, id)| id);
        let Some(id) = matching_id else {
            return Ok(false);
        };
        let updated = sqlx::query(
            "UPDATE recovery_points SET session_id = ?, updated_at_ms = ? WHERE id = ?",
        )
        .bind(session_id.as_i64())
        .bind(now)
        .bind(id)
        .execute(&mut **tx)
        .await
        .context("Failed to attach recovery workspace during session switch")?;
        anyhow::ensure!(
            updated.rows_affected() == 1,
            "Recovery workspace disappeared during session switch"
        );
        Ok(true)
    }

    pub(crate) async fn mark_recovery_ready(
        &self,
        id: &RecoveryId,
        result_ref: &str,
    ) -> Result<()> {
        let result = sqlx::query(
            "UPDATE recovery_points SET result_ref = ?, state = 'ready', error = NULL, updated_at_ms = ? WHERE id = ?",
        )
        .bind(result_ref)
        .bind(now_ms())
        .bind(id.as_str())
        .execute(&self.pool)
        .await
        .context("Failed to mark recovery ready")?;
        ensure_updated(id, result.rows_affected())
    }

    pub(crate) async fn mark_recovery_state(
        &self,
        id: &RecoveryId,
        state: RecoveryState,
        branch_name: Option<&str>,
        error: Option<&str>,
        clear_worktree: bool,
    ) -> Result<()> {
        let result = if clear_worktree {
            sqlx::query(
                "UPDATE recovery_points SET state = ?, branch_name = ?, error = ?, updated_at_ms = ?, worktree_path = NULL WHERE id = ?",
            )
            .bind(state.as_db_str())
            .bind(branch_name)
            .bind(error)
            .bind(now_ms())
            .bind(id.as_str())
            .execute(&self.pool)
            .await
        } else {
            sqlx::query(
                "UPDATE recovery_points SET state = ?, branch_name = ?, error = ?, updated_at_ms = ? WHERE id = ?",
            )
            .bind(state.as_db_str())
            .bind(branch_name)
            .bind(error)
            .bind(now_ms())
            .bind(id.as_str())
            .execute(&self.pool)
            .await
        }
        .context("Failed to update recovery point state")?;
        ensure_updated(id, result.rows_affected())
    }

    pub(crate) async fn recovery_point(&self, id: &RecoveryId) -> Result<RecoveryPoint> {
        let row = sqlx::query(
            r#"
            SELECT id, session_id, project_path, repository_path, worktree_path,
                   baseline_ref, result_ref, source_index_tree, state, branch_name,
                   error, updated_at_ms
            FROM recovery_points WHERE id = ?
            "#,
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await
        .context("Failed to load recovery point")?
        .with_context(|| format!("Recovery point {id} does not exist"))?;
        recovery_point_from_row(&row)
    }

    pub(crate) async fn list_recovery_points(&self, limit: usize) -> Result<Vec<RecoveryPoint>> {
        let limit = i64::try_from(limit).context("Recovery list limit is out of range")?;
        let rows = sqlx::query(
            r#"
            SELECT id, session_id, project_path, repository_path, worktree_path,
                   baseline_ref, result_ref, source_index_tree, state, branch_name,
                   error, updated_at_ms
            FROM recovery_points ORDER BY created_at_ms DESC LIMIT ?
            "#,
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list recovery points")?;
        rows.iter().map(recovery_point_from_row).collect()
    }
}

fn ensure_updated(id: &RecoveryId, rows_affected: u64) -> Result<()> {
    anyhow::ensure!(rows_affected == 1, "Recovery point {id} does not exist");
    Ok(())
}

fn recovery_point_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<RecoveryPoint> {
    let id: String = row.try_get("id")?;
    let state: String = row.try_get("state")?;
    Ok(RecoveryPoint {
        id: RecoveryId::parse(&id)?,
        session_id: row
            .try_get::<Option<i64>, _>("session_id")?
            .map(SessionId::from_raw),
        project_path: PathBuf::from(row.try_get::<String, _>("project_path")?),
        repository_path: PathBuf::from(row.try_get::<String, _>("repository_path")?),
        worktree_path: row
            .try_get::<Option<String>, _>("worktree_path")?
            .map(PathBuf::from),
        baseline_ref: row.try_get("baseline_ref")?,
        result_ref: row.try_get("result_ref")?,
        source_index_tree: row.try_get("source_index_tree")?,
        state: RecoveryState::parse_db(&state)
            .with_context(|| format!("Unknown recovery state {state:?}"))?,
        branch_name: row.try_get("branch_name")?,
        error: row.try_get("error")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
    })
}

fn path_for_storage(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
