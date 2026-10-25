use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerCommand {
    pub(crate) run_id: String,
    pub(crate) state: String,
}

impl Storage {
    pub(crate) async fn server_command(
        &self,
        session: SessionId,
        request: &str,
        prompt: &str,
    ) -> Result<Option<ServerCommand>> {
        let row = sqlx::query("SELECT prompt, run_id, state FROM server_commands WHERE session_id = ? AND request_id = ?")
            .bind(session.as_i64()).bind(request).fetch_optional(&self.pool).await?;
        row.map(|row| {
            anyhow::ensure!(
                row.try_get::<String, _>("prompt")? == prompt,
                "Request ID was already used with different content"
            );
            Ok(ServerCommand {
                run_id: row.try_get("run_id")?,
                state: row.try_get("state")?,
            })
        })
        .transpose()
    }

    pub(crate) async fn admit_server_command(
        &self,
        session: SessionId,
        request: &str,
        prompt: &str,
        run: &str,
    ) -> Result<()> {
        sqlx::query("INSERT INTO server_commands(session_id, request_id, prompt, run_id, state) VALUES (?, ?, ?, ?, 'accepted')")
            .bind(session.as_i64()).bind(request).bind(prompt).bind(run).execute(&self.pool).await?;
        Ok(())
    }

    pub(crate) async fn finish_server_command(&self, session: SessionId, run: &str) -> Result<()> {
        sqlx::query("UPDATE server_commands SET state = 'finished' WHERE session_id = ? AND run_id = ? AND state = 'accepted'")
            .bind(session.as_i64()).bind(run).execute(&self.pool).await?;
        Ok(())
    }

    /// Called only after acquiring the session claim. Never replays admitted work.
    pub(crate) async fn interrupt_abandoned_server_commands(
        &self,
        session: SessionId,
    ) -> Result<()> {
        sqlx::query("UPDATE server_commands SET state = 'interrupted' WHERE session_id = ? AND state = 'accepted'")
            .bind(session.as_i64()).execute(&self.pool).await?;
        Ok(())
    }
}
