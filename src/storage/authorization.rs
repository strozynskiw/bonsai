//! Compact, bounded authorization-decision persistence.

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::Row;

use super::{SessionId, Storage};
use crate::util::time::now_ms;

const MAX_DECISIONS_PER_SESSION: i64 = 500;

#[derive(Debug, Clone)]
pub(crate) struct NewAuthorizationDecision<'a> {
    pub(crate) tool_call_id: Option<&'a str>,
    pub(crate) surface: &'a str,
    pub(crate) subject: &'a str,
    pub(crate) effects: &'a [&'a str],
    pub(crate) risk_tier: &'a str,
    pub(crate) rule_source: &'a str,
    pub(crate) autonomy_level: &'a str,
    pub(crate) sandbox_posture: &'a str,
    pub(crate) decision: &'a str,
    pub(crate) reason: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthorizationDecisionRecord {
    pub(crate) id: i64,
    pub(crate) tool_call_id: Option<String>,
    pub(crate) surface: String,
    pub(crate) subject: String,
    pub(crate) effects: Vec<String>,
    pub(crate) risk_tier: String,
    pub(crate) rule_source: String,
    pub(crate) autonomy_level: String,
    pub(crate) sandbox_posture: String,
    pub(crate) decision: String,
    pub(crate) reason: String,
    pub(crate) created_at_ms: i64,
}

impl Storage {
    pub(crate) async fn record_authorization_decision(
        &self,
        session_id: SessionId,
        decision: NewAuthorizationDecision<'_>,
    ) -> Result<()> {
        let effects_json = serde_json::to_string(decision.effects)
            .context("Failed to serialize authorization effects")?;
        let mut transaction = self
            .begin_write()
            .await
            .context("Failed to begin authorization-decision transaction")?;
        sqlx::query(
            r#"
            INSERT INTO authorization_decisions (
              session_id, tool_call_id, surface, subject, effects_json,
              risk_tier, rule_source, autonomy_level, sandbox_posture,
              decision, reason, created_at_ms
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(session_id.as_i64())
        .bind(decision.tool_call_id)
        .bind(decision.surface)
        .bind(decision.subject)
        .bind(effects_json)
        .bind(decision.risk_tier)
        .bind(decision.rule_source)
        .bind(decision.autonomy_level)
        .bind(decision.sandbox_posture)
        .bind(decision.decision)
        .bind(decision.reason)
        .bind(now_ms())
        .execute(&mut *transaction)
        .await
        .context("Failed to persist authorization decision")?;

        sqlx::query(
            r#"
            DELETE FROM authorization_decisions
            WHERE session_id = ?
              AND id NOT IN (
                SELECT id FROM authorization_decisions
                WHERE session_id = ?
                ORDER BY created_at_ms DESC, id DESC
                LIMIT ?
              )
            "#,
        )
        .bind(session_id.as_i64())
        .bind(session_id.as_i64())
        .bind(MAX_DECISIONS_PER_SESSION)
        .execute(&mut *transaction)
        .await
        .context("Failed to enforce authorization-decision retention")?;
        transaction
            .commit()
            .await
            .context("Failed to commit authorization decision")
    }

    pub(crate) async fn recent_authorization_decisions(
        &self,
        session_id: SessionId,
        limit: usize,
    ) -> Result<Vec<AuthorizationDecisionRecord>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = sqlx::query(
            r#"
            SELECT id, tool_call_id, surface, subject, effects_json, risk_tier,
                   rule_source, autonomy_level, sandbox_posture, decision,
                   reason, created_at_ms
            FROM authorization_decisions
            WHERE session_id = ?
            ORDER BY created_at_ms DESC, id DESC
            LIMIT ?
            "#,
        )
        .bind(session_id.as_i64())
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("Failed to load authorization decisions")?;

        rows.into_iter()
            .map(|row| {
                let effects_json: String = row.try_get("effects_json")?;
                let effects = serde_json::from_str::<Value>(&effects_json)
                    .ok()
                    .and_then(|value| value.as_array().cloned())
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|value| value.as_str().map(ToString::to_string))
                    .collect();
                Ok(AuthorizationDecisionRecord {
                    id: row.try_get("id")?,
                    tool_call_id: row.try_get("tool_call_id")?,
                    surface: row.try_get("surface")?,
                    subject: row.try_get("subject")?,
                    effects,
                    risk_tier: row.try_get("risk_tier")?,
                    rule_source: row.try_get("rule_source")?,
                    autonomy_level: row.try_get("autonomy_level")?,
                    sandbox_posture: row.try_get("sandbox_posture")?,
                    decision: row.try_get("decision")?,
                    reason: row.try_get("reason")?,
                    created_at_ms: row.try_get("created_at_ms")?,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn decisions_round_trip_in_newest_first_order() {
        let fixture = crate::storage::test_utils::TestStorage::new().await;
        let session = fixture.start_session().await;
        for subject in ["first", "second"] {
            fixture
                .storage
                .record_authorization_decision(
                    session,
                    NewAuthorizationDecision {
                        tool_call_id: Some("call-1"),
                        surface: "bash",
                        subject,
                        effects: &["code-execution"],
                        risk_tier: "medium",
                        rule_source: "default",
                        autonomy_level: "balanced",
                        sandbox_posture: "seatbelt:on,network:allow",
                        decision: "allow",
                        reason: "autonomy ceiling",
                    },
                )
                .await
                .unwrap();
        }

        let records = fixture
            .storage
            .recent_authorization_decisions(session, 10)
            .await
            .unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].subject, "second");
        assert_eq!(records[1].subject, "first");
        assert_eq!(records[0].effects, ["code-execution"]);
    }
}
