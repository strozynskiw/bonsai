//! Episode ledger persistence: snapshot-replaced per session like the other
//! agent-state components. Both episode tables are written in one transaction
//! — parent `episodes` rows are deleted first, which cascade-clears
//! `episode_archive` before the fresh rows land.

use super::*;
use crate::episode::{
    ArchivedEpisodeItem, Episode, EpisodeCloseReason, EpisodeLedger, EpisodeStatus,
    PersistedEpisode,
};

impl Storage {
    pub async fn replace_episodes_snapshot(
        &self,
        session_id: SessionId,
        episodes: &[Episode],
    ) -> Result<()> {
        self.with_session_snapshot_tx("episodes snapshot", async move |tx, now| {
            self.replace_episodes_snapshot_in_tx(tx, session_id, episodes, now)
                .await
        })
        .await
    }

    pub(crate) async fn replace_episodes_snapshot_in_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        session_id: SessionId,
        episodes: &[Episode],
        now: i64,
    ) -> Result<()> {
        storage_op!(
            tx,
            "delete episodes",
            sqlx::query("DELETE FROM episodes WHERE session_id = ?").bind(session_id.as_i64()),
        )?;

        for episode in episodes {
            let episode_seq = i64::try_from(episode.seq())
                .context("Episode sequence does not fit SQLite INTEGER")?;
            let evicted_tokens = episode
                .evicted_tokens()
                .map(i64::try_from)
                .transpose()
                .context("Episode token count does not fit SQLite INTEGER")?;
            let recall_count = i64::try_from(episode.recall_count())
                .context("Episode recall count does not fit SQLite INTEGER")?;
            let files_touched_json = serde_json::to_string(episode.files_touched())
                .context("Failed to serialize episode files_touched")?;
            sqlx::query(
                r#"
                INSERT INTO episodes (
                  session_id, seq, title, status, goal, card_md, close_reason,
                  start_stable_id, end_stable_id, marker_stable_id,
                  files_touched_json, opened_at_ms, closed_at_ms, evicted_at_ms,
                  evicted_tokens, recall_count, completable
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#,
            )
            .bind(session_id.as_i64())
            .bind(episode_seq)
            .bind(episode.title())
            .bind(episode.status().as_db_str())
            .bind(episode.goal())
            .bind(episode.card_md())
            .bind(
                episode
                    .close_reason()
                    .map(EpisodeCloseReason::as_db_str)
                    .unwrap_or(""),
            )
            .bind(episode.start_stable_id())
            .bind(episode.end_stable_id())
            .bind(episode.marker_stable_id())
            .bind(files_touched_json)
            .bind(episode.opened_at_ms())
            .bind(episode.closed_at_ms())
            .bind(episode.evicted_at_ms())
            .bind(evicted_tokens)
            .bind(recall_count)
            .bind(i64::from(episode.is_completable()))
            .execute(&mut **tx)
            .await
            .with_context(|| format!("Failed to insert episode {}", episode.seq()))?;

            for (item_seq, item) in episode.archive().iter().enumerate() {
                let raw_json = serde_json::to_string(&item.message)
                    .context("Failed to serialize archived episode message")?;
                sqlx::query(
                    r#"
                    INSERT INTO episode_archive (
                      session_id, episode_seq, item_seq, stable_id, raw_json
                    )
                    VALUES (?, ?, ?, ?, ?)
                    "#,
                )
                .bind(session_id.as_i64())
                .bind(episode_seq)
                .bind(
                    i64::try_from(item_seq)
                        .context("Archive item sequence does not fit SQLite INTEGER")?,
                )
                .bind(&item.stable_id)
                .bind(raw_json)
                .execute(&mut **tx)
                .await
                .with_context(|| {
                    format!(
                        "Failed to insert archive item for episode {}",
                        episode.seq()
                    )
                })?;
            }
        }

        touch_session(tx, session_id, now).await
    }

    pub(crate) async fn increment_episode_recall_count(
        &self,
        session_id: SessionId,
        episode_seq: usize,
    ) -> Result<()> {
        let result = sqlx::query(
            "UPDATE episodes SET recall_count = recall_count + 1 \
             WHERE session_id = ? AND seq = ?",
        )
        .bind(session_id.as_i64())
        .bind(i64::try_from(episode_seq).context("Episode sequence does not fit SQLite INTEGER")?)
        .execute(&self.pool)
        .await
        .with_context(|| format!("Failed to record recall for episode {episode_seq}"))?;
        if result.rows_affected() != 1 {
            anyhow::bail!("Episode #{episode_seq} disappeared while recording recall");
        }
        Ok(())
    }

    /// Bounded, session-filtered substring scan over the archived episode
    /// bytes for the `recall` tool's query mode. The LIKE pattern is escaped so
    /// user text cannot smuggle wildcards; hits return the owning episode seq
    /// plus a content-only snippet extracted from the parsed message (never a
    /// raw-JSON excerpt).
    pub async fn search_episode_archive(
        &self,
        session_id: SessionId,
        query: &str,
        limit: i64,
    ) -> Result<Vec<(usize, String)>> {
        let escaped = query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let rows = sqlx::query(
            "SELECT episode_seq, raw_json FROM episode_archive \
             WHERE session_id = ? AND raw_json LIKE ? ESCAPE '\\' \
             ORDER BY episode_seq, item_seq LIMIT ?",
        )
        .bind(session_id.as_i64())
        .bind(pattern)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("Failed to search episode archive for session {session_id}"))?;

        let mut hits = Vec::new();
        for row in rows {
            let episode_seq = usize::try_from(row.try_get::<i64, _>("episode_seq")?)
                .context("Invalid negative episode sequence in archive search")?;
            let raw_json: String = row.try_get("raw_json")?;
            let Ok(message) = serde_json::from_str::<
                async_openai::types::chat::ChatCompletionRequestMessage,
            >(&raw_json) else {
                continue;
            };
            let (_role, text) = crate::context_view::describe_message_full(&message);
            if let Some(snippet) = content_snippet(&text, query) {
                hits.push((episode_seq, snippet));
            }
        }
        Ok(hits)
    }

    pub(crate) async fn load_episodes(&self, session_id: SessionId) -> Result<Vec<Episode>> {
        let rows = sqlx::query(
            "SELECT seq, title, status, goal, card_md, close_reason, \
             start_stable_id, end_stable_id, marker_stable_id, files_touched_json, \
             opened_at_ms, closed_at_ms, evicted_at_ms, evicted_tokens, \
             recall_count, completable \
             FROM episodes WHERE session_id = ? ORDER BY seq",
        )
        .bind(session_id.as_i64())
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("Failed to load episodes for session {session_id}"))?;

        let mut records = rows
            .into_iter()
            .map(|row| {
                let close_reason: String = row.try_get("close_reason")?;
                let files_touched_json: String = row.try_get("files_touched_json")?;
                Ok(PersistedEpisode {
                    seq: usize::try_from(row.try_get::<i64, _>("seq")?)
                        .context("Invalid negative episode sequence")?,
                    title: row.try_get("title")?,
                    status: EpisodeStatus::from_db_str(&row.try_get::<String, _>("status")?),
                    goal: row.try_get("goal")?,
                    card_md: row.try_get("card_md")?,
                    close_reason: (!close_reason.is_empty())
                        .then(|| EpisodeCloseReason::from_db_str(&close_reason)),
                    start_stable_id: row.try_get("start_stable_id")?,
                    end_stable_id: row.try_get("end_stable_id")?,
                    marker_stable_id: row.try_get("marker_stable_id")?,
                    files_touched: serde_json::from_str(&files_touched_json)
                        .context("Failed to parse episode files_touched")?,
                    opened_at_ms: row.try_get("opened_at_ms")?,
                    closed_at_ms: row.try_get("closed_at_ms")?,
                    evicted_at_ms: row.try_get("evicted_at_ms")?,
                    evicted_tokens: row
                        .try_get::<Option<i64>, _>("evicted_tokens")?
                        .and_then(|tokens| usize::try_from(tokens).ok()),
                    recall_count: usize::try_from(row.try_get::<i64, _>("recall_count")?)
                        .context("Invalid negative episode recall count")?,
                    completable: row.try_get::<i64, _>("completable")? != 0,
                    archive: Vec::new(),
                })
            })
            .collect::<Result<Vec<PersistedEpisode>>>()?;

        let archive_rows = sqlx::query(
            "SELECT episode_seq, stable_id, raw_json \
             FROM episode_archive WHERE session_id = ? ORDER BY episode_seq, item_seq",
        )
        .bind(session_id.as_i64())
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("Failed to load episode archive for session {session_id}"))?;

        let mut corrupted_episode_seqs = std::collections::HashSet::new();
        for row in archive_rows {
            let episode_seq = usize::try_from(row.try_get::<i64, _>("episode_seq")?)
                .context("Invalid negative archived episode sequence")?;
            let stable_id: String = row.try_get("stable_id")?;
            let raw_json: String = row.try_get("raw_json")?;
            let message = match serde_json::from_str(&raw_json) {
                Ok(message) => message,
                Err(err) => {
                    corrupted_episode_seqs.insert(episode_seq);
                    tracing::warn!(
                        session_id = %session_id,
                        episode_seq,
                        error = %err,
                        "skipping malformed archived episode message"
                    );
                    continue;
                }
            };
            if let Some(record) = records.iter_mut().find(|record| record.seq == episode_seq) {
                record
                    .archive
                    .push(ArchivedEpisodeItem { stable_id, message });
            }
        }
        let mut ledger = EpisodeLedger::default();
        ledger.restore(records.into_iter().map(Episode::from_persisted).collect());
        let now = crate::util::time::now_ms();
        for episode_seq in corrupted_episode_seqs {
            ledger.mark_episode_repaired(episode_seq, now);
        }

        Ok(ledger.snapshot())
    }
}

/// A short window of the parsed message text around the first (case-
/// insensitive) match of `query`. `None` when the match only existed in JSON
/// framing — a raw-JSON hit must never leak encoder syntax into snippets.
pub(crate) fn content_snippet(text: &str, query: &str) -> Option<String> {
    const SNIPPET_CONTEXT_CHARS: usize = 80;
    let chars = text.chars().collect::<Vec<_>>();
    let mut lower_text = String::new();
    let mut folded_to_original = Vec::new();
    for (original_index, ch) in chars.iter().copied().enumerate() {
        for folded in ch.to_lowercase() {
            lower_text.push(folded);
            folded_to_original.push(original_index);
        }
    }
    let lower_query = query
        .trim()
        .chars()
        .flat_map(char::to_lowercase)
        .collect::<String>();
    if lower_query.is_empty() {
        return None;
    }
    let byte_start = lower_text.find(&lower_query)?;
    let folded_start = lower_text[..byte_start].chars().count();
    let folded_end = folded_start + lower_query.chars().count();
    let char_start = *folded_to_original.get(folded_start)?;
    let match_end = folded_to_original
        .get(folded_end.saturating_sub(1))?
        .saturating_add(1);
    let window_start = char_start.saturating_sub(SNIPPET_CONTEXT_CHARS);
    let window_end = match_end
        .saturating_add(SNIPPET_CONTEXT_CHARS)
        .min(chars.len());
    let mut snippet: String = chars[window_start..window_end].iter().collect();
    snippet = snippet.split_whitespace().collect::<Vec<_>>().join(" ");
    if window_start > 0 {
        snippet = format!("…{snippet}");
    }
    if window_end < chars.len() {
        snippet.push('…');
    }
    Some(snippet)
}

#[cfg(test)]
mod tests {
    use async_openai::types::chat::ChatCompletionRequestMessage;

    use super::*;

    #[test]
    fn content_snippet_handles_expanding_unicode_casefold_before_match() {
        let text = format!("{}x tail", "İ".repeat(100));
        let snippet = content_snippet(&text, "x").expect("match remains addressable");
        assert!(snippet.contains('x'), "{snippet}");
    }

    #[tokio::test]
    async fn malformed_archive_item_repairs_only_its_episode() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Storage::open_at(temp.path().join("bonsai.db"))
            .await
            .unwrap();
        let session_id = storage
            .start_session(
                temp.path(),
                "codex",
                "test-model",
                crate::provider::ReasoningSelection::default(),
            )
            .await
            .unwrap();
        let episode = Episode::from_persisted(PersistedEpisode {
            seq: 1,
            title: "archived".to_string(),
            status: EpisodeStatus::Evicted,
            goal: "goal".to_string(),
            card_md: "## Episode card".to_string(),
            close_reason: Some(EpisodeCloseReason::TitleChange),
            start_stable_id: "msg-1".to_string(),
            end_stable_id: Some("msg-2".to_string()),
            marker_stable_id: Some("msg-3".to_string()),
            files_touched: Vec::new(),
            opened_at_ms: 1,
            closed_at_ms: Some(2),
            evicted_at_ms: Some(3),
            evicted_tokens: Some(9_000),
            recall_count: 0,
            completable: false,
            archive: vec![ArchivedEpisodeItem {
                stable_id: "msg-1".to_string(),
                message: ChatCompletionRequestMessage::User(
                    async_openai::types::chat::ChatCompletionRequestUserMessage {
                        content: "real bytes".into(),
                        name: None,
                    },
                ),
            }],
        });
        storage
            .replace_episodes_snapshot(session_id, &[episode])
            .await
            .unwrap();
        sqlx::query(
            "UPDATE episode_archive SET raw_json = '{not json' \
             WHERE session_id = ? AND episode_seq = 1",
        )
        .bind(session_id.as_i64())
        .execute(&storage.pool)
        .await
        .unwrap();

        let loaded = storage.load_episodes(session_id).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].status(), EpisodeStatus::Restored);
        assert!(loaded[0].marker_stable_id().is_none());
        assert!(loaded[0].archive().is_empty());
    }
}
