//! Persistence for the permission rules engine. Mirrors the `sessions`
//! CRUD style: raw sqlx against the `permission_rules` table, `now_ms()`
//! timestamps, and a row-mapper helper. Project rules carry a `project_id`;
//! global rules store `project_id = NULL`.

use super::*;
use crate::permissions::Permission;

/// Where a persisted rule applies. Session-scoped rules live only in memory and
/// are never written here, so only these two reach the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionScope {
    Project,
    Global,
}

crate::impl_db_enum!(PermissionScope {
    Project => "project",
    Global => "global",
} else Project);

/// Which resource a permission rule gates. Shares the `permission_rules` table
/// but partitions it so a bash-command rule and a WebFetch domain rule never
/// match each other. The canonical schema requires this namespace on every row.
///
/// Only the encoder is defined: `kind` is written (upsert) and used to filter
/// queries, but rows are never decoded back into a `RuleKind` — each manager
/// already knows its own kind — so a `from_db_str` would be dead code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleKind {
    Bash,
    Domain,
    /// Extension-tool rules: dotted ids (`mcp.github.*`) rather than
    /// shell commands or hosts, so an mcp rule never matches a bash or domain
    /// one.
    Mcp,
    /// Hook trust rules: `hook.<name>:<content-hash>` patterns for the
    /// load-time project-hook trust gate (`src/hooks/trust.rs`) — its own
    /// namespace so a hook rule never matches an mcp/bash/domain one.
    Hook,
    /// Workspace trust: one per-project record controlling whether
    /// project-owned config and instruction surfaces may become active.
    WorkspaceTrust,
}

impl RuleKind {
    pub(crate) fn as_db_str(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Domain => "domain",
            Self::Mcp => "mcp",
            Self::Hook => "hook",
            Self::WorkspaceTrust => "workspace_trust",
        }
    }
}

/// A permission rule as stored in the database.
#[derive(Debug, Clone)]
pub struct StoredPermissionRule {
    pub id: i64,
    pub pattern: String,
    pub decision: Permission,
    pub scope: PermissionScope,
}

impl Storage {
    /// Insert a rule, or update the decision of an existing one with the same
    /// `(scope, project_id, pattern)`. Returns the row id. Dedup is done here
    /// rather than via a UNIQUE index because SQLite treats NULL `project_id`
    /// (global rules) as distinct, which a UNIQUE constraint wouldn't collapse.
    pub async fn upsert_permission_rule(
        &self,
        project_id: Option<i64>,
        pattern: &str,
        decision: Permission,
        scope: PermissionScope,
        kind: RuleKind,
    ) -> Result<i64> {
        let now = now_ms();
        // Atomic upsert against the `idx_permission_rules_unique` invariant
        // (kind, scope, pattern, COALESCE(project_id, -1)), so concurrent or
        // repeated approvals collapse onto one row instead of racing a
        // SELECT-then-INSERT into duplicates. `created_at_ms` is preserved on
        // conflict. The ON CONFLICT target must match the index columns exactly.
        sqlx::query_scalar(
            r#"
            INSERT INTO permission_rules
              (project_id, pattern, decision, scope, kind, created_at_ms)
            VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT (kind, scope, pattern, COALESCE(project_id, -1))
            DO UPDATE SET decision = excluded.decision
            RETURNING id
            "#,
        )
        .bind(project_id)
        .bind(pattern)
        .bind(decision.as_db_str())
        .bind(scope.as_db_str())
        .bind(kind.as_db_str())
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .context("Failed to upsert permission rule")
    }

    /// All rules of `kind` that apply to a project: its own (scope `Project`)
    /// plus every global rule. Ordered project-first, then newest-first, matching
    /// the engine's priority within the user-rule bucket.
    pub async fn permission_rules_for_project(
        &self,
        project_id: i64,
        kind: RuleKind,
    ) -> Result<Vec<StoredPermissionRule>> {
        let rows = sqlx::query(
            r#"
            SELECT id, pattern, decision, scope
            FROM permission_rules
            WHERE kind = ? AND (project_id = ? OR project_id IS NULL)
            ORDER BY (project_id IS NULL), created_at_ms DESC
            "#,
        )
        .bind(kind.as_db_str())
        .bind(project_id)
        .fetch_all(&self.pool)
        .await
        .context("Failed to load permission rules")?;

        rows.into_iter().map(permission_rule_from_row).collect()
    }

    /// Delete a rule by id, but only one visible from `project_id`: the
    /// project's own rules or a global rule. This scopes `/permissions remove`
    /// so a guessed id belonging to another project can't be deleted.
    pub async fn delete_permission_rule(&self, id: i64, project_id: i64) -> Result<bool> {
        let result = sqlx::query(
            r#"
            DELETE FROM permission_rules
            WHERE id = ? AND (project_id = ? OR project_id IS NULL)
            "#,
        )
        .bind(id)
        .bind(project_id)
        .execute(&self.pool)
        .await
        .with_context(|| format!("Failed to delete permission rule {id}"))?;
        Ok(result.rows_affected() > 0)
    }
}

fn permission_rule_from_row(row: sqlx::sqlite::SqliteRow) -> Result<StoredPermissionRule> {
    Ok(StoredPermissionRule {
        id: row.try_get("id")?,
        pattern: row.try_get("pattern")?,
        decision: Permission::from_db_str(&row.try_get::<String, _>("decision")?),
        scope: PermissionScope::from_db_str(&row.try_get::<String, _>("scope")?),
    })
}
