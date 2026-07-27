use super::*;

impl Storage {
    pub(crate) async fn replace_todos_snapshot_in_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        session_id: SessionId,
        todos: &[TodoItem],
        now: i64,
    ) -> Result<()> {
        storage_op!(
            tx,
            "delete todos",
            sqlx::query("DELETE FROM todos WHERE session_id = ?").bind(session_id.as_i64()),
        )?;
        for (seq, todo) in todos.iter().enumerate() {
            sqlx::query("INSERT INTO todos (session_id, seq, content, status) VALUES (?, ?, ?, ?)")
                .bind(session_id.as_i64())
                .bind(seq as i64)
                .bind(&todo.content)
                .bind(todo.status.as_db_str())
                .execute(&mut **tx)
                .await?;
        }
        touch_session(tx, session_id, now).await
    }
    pub(super) async fn load_todos_snapshot(&self, session_id: SessionId) -> Result<Vec<TodoItem>> {
        let rows =
            sqlx::query("SELECT content, status FROM todos WHERE session_id = ? ORDER BY seq")
                .bind(session_id.as_i64())
                .fetch_all(&self.pool)
                .await
                .with_context(|| format!("Failed to load todos for session {session_id}"))?;

        rows.into_iter()
            .map(|row| {
                let status: String = row.try_get("status")?;
                Ok(TodoItem {
                    content: row.try_get("content")?,
                    status: TodoStatus::from_db_str(&status),
                })
            })
            .collect()
    }
}
