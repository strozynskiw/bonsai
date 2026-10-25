//! Durable server authorization and audit data; never stores bearer material.
use super::*;

#[derive(Debug, Clone)]
pub(crate) struct ApiToken {
    pub id: String,
    pub workspace: String,
    pub sessions: Vec<i64>,
    pub scopes: Vec<String>,
    pub expires_at_ms: i64,
}
impl Storage {
    pub(crate) async fn api_artifact_insert(
        &self,
        workspace: &str,
        session: i64,
        artifact: &crate::server::artifacts::Artifact,
    ) -> Result<()> {
        sqlx::query("INSERT INTO server_artifacts(id,workspace,session_id,size,digest,name) VALUES(?,?,?,?,?,?)")
            .bind(&artifact.id).bind(workspace).bind(session).bind(artifact.size as i64).bind(&artifact.digest).bind(&artifact.name).execute(&self.pool).await?;
        Ok(())
    }
    pub(crate) async fn api_artifacts(
        &self,
        workspace: &str,
        session: i64,
        id: Option<&str>,
    ) -> Result<Vec<crate::server::artifacts::Artifact>> {
        let rows=sqlx::query("SELECT id,size,digest,name FROM server_artifacts WHERE workspace=? AND session_id=? AND (? IS NULL OR id=?) ORDER BY id LIMIT 100")
            .bind(workspace).bind(session).bind(id).bind(id).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|row| {
                Ok(crate::server::artifacts::Artifact {
                    id: row.try_get("id")?,
                    size: row.try_get::<i64, _>("size")? as u64,
                    digest: row.try_get("digest")?,
                    name: row.try_get("name")?,
                })
            })
            .collect()
    }
    pub(crate) async fn api_insert_token(&self, token: &ApiToken, digest: &str) -> Result<()> {
        sqlx::query("INSERT INTO server_tokens(id,digest,workspace,sessions_json,scopes_json,expires_at_ms) VALUES(?,?,?,?,?,?)")
            .bind(&token.id).bind(digest).bind(&token.workspace).bind(serde_json::to_string(&token.sessions)?)
            .bind(serde_json::to_string(&token.scopes)?).bind(token.expires_at_ms).execute(&self.pool).await?;
        Ok(())
    }
    pub(crate) async fn api_token(&self, digest: &str) -> Result<Option<ApiToken>> {
        let row = sqlx::query("SELECT id,workspace,sessions_json,scopes_json,expires_at_ms FROM server_tokens WHERE digest=? AND revoked=0 AND expires_at_ms>?")
            .bind(digest).bind(now_ms()).fetch_optional(&self.pool).await?;
        row.map(|r| {
            Ok(ApiToken {
                id: r.try_get("id")?,
                workspace: r.try_get("workspace")?,
                sessions: serde_json::from_str(&r.try_get::<String, _>("sessions_json")?)?,
                scopes: serde_json::from_str(&r.try_get::<String, _>("scopes_json")?)?,
                expires_at_ms: r.try_get("expires_at_ms")?,
            })
        })
        .transpose()
    }
    pub(crate) async fn api_revoke(&self, workspace: &str, id: &str) -> Result<()> {
        let result = sqlx::query("UPDATE server_tokens SET revoked=1 WHERE id=? AND workspace=?")
            .bind(id)
            .bind(workspace)
            .execute(&self.pool)
            .await?;
        anyhow::ensure!(result.rows_affected() == 1, "Unknown token");
        Ok(())
    }
    pub(crate) async fn api_audit(
        &self,
        token: Option<&str>,
        session: Option<i64>,
        run: Option<&str>,
        request: &str,
        action: &str,
        decision: &str,
    ) -> Result<()> {
        sqlx::query("INSERT INTO server_audit(at_ms,token_id,session_id,run_id,request_id,action,decision) VALUES(?,?,?,?,?,?,?)")
            .bind(now_ms()).bind(token).bind(session).bind(run).bind(request).bind(action).bind(decision).execute(&self.pool).await?;
        Ok(())
    }
    pub(crate) async fn api_mutation(
        &self,
        workspace: &str,
        operation: &str,
        session: i64,
        request: &str,
        digest: &str,
    ) -> Result<Option<Option<String>>> {
        let row = sqlx::query("SELECT digest,result_json FROM server_mutations WHERE workspace=? AND operation=? AND session_id=? AND request_id=?")
            .bind(workspace).bind(operation).bind(session).bind(request).fetch_optional(&self.pool).await?;
        row.map(|row| {
            anyhow::ensure!(
                row.try_get::<String, _>("digest")? == digest,
                "Request ID content conflict"
            );
            Ok(row.try_get("result_json")?)
        })
        .transpose()
    }
    pub(crate) async fn api_admit(
        &self,
        workspace: &str,
        operation: &str,
        session: i64,
        request: &str,
        digest: &str,
    ) -> Result<()> {
        sqlx::query("INSERT INTO server_mutations(workspace,operation,session_id,request_id,digest) VALUES(?,?,?,?,?)")
            .bind(workspace).bind(operation).bind(session).bind(request).bind(digest).execute(&self.pool).await?;
        Ok(())
    }
    pub(crate) async fn api_finish(
        &self,
        workspace: &str,
        operation: &str,
        session: i64,
        request: &str,
        result: &str,
    ) -> Result<()> {
        sqlx::query("UPDATE server_mutations SET result_json=? WHERE workspace=? AND operation=? AND session_id=? AND request_id=?")
            .bind(result).bind(workspace).bind(operation).bind(session).bind(request).execute(&self.pool).await?;
        Ok(())
    }
    pub(crate) async fn api_session_ids(
        &self,
        root: &str,
        after: i64,
        limit: i64,
        allowed: &[i64],
    ) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar("SELECT sessions.id FROM sessions JOIN projects ON projects.id=sessions.project_id WHERE projects.path=? AND sessions.id>? AND (?='[]' OR sessions.id IN (SELECT value FROM json_each(?))) ORDER BY sessions.id LIMIT ?")
            .bind(root).bind(after).bind(serde_json::to_string(allowed)?).bind(serde_json::to_string(allowed)?).bind(limit).fetch_all(&self.pool).await?)
    }
}
