//! Retrieval shadow for persistent memory. The file stores are the
//! source of truth; these rows exist so recall can run FTS5 BM25 and cached
//! embedding similarity without re-reading the stores. Rows are reconciled by
//! blake3 content hash — an unchanged store is a no-op sync. User-tier rows
//! use `project_id = 0`, project-tier rows the real `projects.id`, so
//! `UNIQUE(tier, project_id, name)` is total and one DB serves all projects.

use super::*;
use crate::memory::entry::{MemoryEntry, MemoryTier};

/// One BM25 hit: the entry key plus its raw `bm25()` score (lower = better).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MemoryBm25Hit {
    pub tier: String,
    pub name: String,
    pub score: f64,
}

/// One cached embedding row for the active model.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MemoryEmbeddingRow {
    pub content_hash: String,
    pub vector: Vec<f32>,
}

fn tier_project_id(tier: MemoryTier, project_id: i64) -> i64 {
    match tier {
        MemoryTier::User => 0,
        MemoryTier::Project => project_id,
    }
}

impl Storage {
    /// Reconcile the shadow rows visible to `project_id` (its own rows plus the
    /// shared user tier) with the current fs snapshot: delete gone entries,
    /// upsert new/changed ones (hash-keyed, so unchanged entries are no-ops).
    /// The FTS triggers keep `memory_fts` in step.
    pub(crate) async fn sync_memory_entries(
        &self,
        project_id: i64,
        entries: &[MemoryEntry],
    ) -> Result<()> {
        let existing: Vec<(String, i64, String, String)> = sqlx::query_as(
            "SELECT tier, project_id, name, content_hash FROM memory_entries WHERE project_id IN (0, ?)",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await
        .context("Failed to load memory shadow rows")?;

        let now = now_ms();
        let current: std::collections::HashMap<(String, i64, String), &MemoryEntry> = entries
            .iter()
            .map(|entry| {
                (
                    (
                        entry.tier.label().to_string(),
                        tier_project_id(entry.tier, project_id),
                        entry.name.clone(),
                    ),
                    entry,
                )
            })
            .collect();

        let mut unchanged = std::collections::HashSet::new();
        for (tier, row_project_id, name, hash) in &existing {
            let key = (tier.clone(), *row_project_id, name.clone());
            match current.get(&key) {
                None => {
                    sqlx::query(
                        "DELETE FROM memory_entries WHERE tier = ? AND project_id = ? AND name = ?",
                    )
                    .bind(tier)
                    .bind(row_project_id)
                    .bind(name)
                    .execute(&self.pool)
                    .await
                    .context("Failed to delete stale memory shadow row")?;
                }
                Some(entry) if &entry.content_hash == hash => {
                    unchanged.insert(key);
                }
                Some(_) => {}
            }
        }

        for (key, entry) in &current {
            if unchanged.contains(key) {
                continue;
            }
            sqlx::query(
                r#"
                INSERT INTO memory_entries (
                  tier, project_id, name, entry_type, description, body,
                  content_hash, created_at_ms, updated_at_ms
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT(tier, project_id, name) DO UPDATE SET
                  entry_type = excluded.entry_type,
                  description = excluded.description,
                  body = excluded.body,
                  content_hash = excluded.content_hash,
                  updated_at_ms = excluded.updated_at_ms
                "#,
            )
            .bind(&key.0)
            .bind(key.1)
            .bind(&key.2)
            .bind(entry.entry_type.label())
            .bind(&entry.description)
            .bind(&entry.body)
            .bind(&entry.content_hash)
            .bind(now)
            .bind(now)
            .execute(&self.pool)
            .await
            .context("Failed to upsert memory shadow row")?;
        }
        Ok(())
    }

    /// Top BM25 matches for a sanitized FTS query, scoped to the user tier and
    /// this project. `bm25()` is more-negative-is-better, hence `ORDER BY ASC`.
    pub(crate) async fn memory_bm25(
        &self,
        project_id: i64,
        fts_query: &str,
        limit: i64,
    ) -> Result<Vec<MemoryBm25Hit>> {
        let rows: Vec<(String, String, f64)> = sqlx::query_as(
            r#"
            SELECT m.tier, m.name, bm25(memory_fts, 4.0, 3.0, 1.0) AS score
            FROM memory_fts
            JOIN memory_entries m ON m.id = memory_fts.rowid
            WHERE memory_fts MATCH ? AND m.project_id IN (0, ?) AND m.enabled = 1
            ORDER BY score ASC
            LIMIT ?
            "#,
        )
        .bind(fts_query)
        .bind(project_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("Failed to run memory BM25 query")?;
        Ok(rows
            .into_iter()
            .map(|(tier, name, score)| MemoryBm25Hit { tier, name, score })
            .collect())
    }

    /// All cached embedding vectors for `model_id`. The table stays small
    /// (hundreds of entries), so callers filter by hash in memory.
    pub(crate) async fn memory_vectors(&self, model_id: &str) -> Result<Vec<MemoryEmbeddingRow>> {
        let rows: Vec<(String, i64, Vec<u8>)> = sqlx::query_as(
            "SELECT content_hash, dims, vector FROM memory_embeddings WHERE model_id = ?",
        )
        .bind(model_id)
        .fetch_all(&self.pool)
        .await
        .context("Failed to load memory embeddings")?;
        Ok(rows
            .into_iter()
            .filter_map(|(content_hash, dims, blob)| {
                let vector = decode_vector(&blob, dims)?;
                Some(MemoryEmbeddingRow {
                    content_hash,
                    vector,
                })
            })
            .collect())
    }

    pub(crate) async fn upsert_memory_embedding(
        &self,
        content_hash: &str,
        model_id: &str,
        vector: &[f32],
    ) -> Result<()> {
        let mut blob = Vec::with_capacity(vector.len() * 4);
        for value in vector {
            blob.extend_from_slice(&value.to_le_bytes());
        }
        sqlx::query(
            r#"
            INSERT INTO memory_embeddings (content_hash, model_id, dims, vector)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(content_hash, model_id) DO UPDATE SET
              dims = excluded.dims,
              vector = excluded.vector
            "#,
        )
        .bind(content_hash)
        .bind(model_id)
        .bind(vector.len() as i64)
        .bind(blob)
        .execute(&self.pool)
        .await
        .context("Failed to upsert memory embedding")?;
        Ok(())
    }

    /// Drop one shadow row immediately (`/memory forget`); the FTS
    /// delete-trigger cleans `memory_fts`. Embedding rows are keyed by content
    /// hash and harmlessly orphan until the next model change.
    pub(crate) async fn delete_memory_entry(
        &self,
        project_id: i64,
        tier: MemoryTier,
        name: &str,
    ) -> Result<()> {
        sqlx::query("DELETE FROM memory_entries WHERE tier = ? AND project_id = ? AND name = ?")
            .bind(tier.label())
            .bind(tier_project_id(tier, project_id))
            .bind(name)
            .execute(&self.pool)
            .await
            .context("Failed to delete memory shadow row")?;
        Ok(())
    }
}

/// Decode a little-endian f32 blob, rejecting corrupt rows whose byte length
/// disagrees with `dims` (no bytemuck in tree; `as_chunks` is enough once the
/// length check rules out a trailing partial chunk).
fn decode_vector(blob: &[u8], dims: i64) -> Option<Vec<f32>> {
    if dims < 0 || blob.len() != (dims as usize) * 4 {
        return None;
    }
    let (chunks, remainder) = blob.as_chunks::<4>();
    debug_assert!(
        remainder.is_empty(),
        "length check above guarantees a whole number of f32s"
    );
    Some(
        chunks
            .iter()
            .map(|chunk| f32::from_le_bytes(*chunk))
            .collect(),
    )
}

#[cfg(test)]
mod memory_tests {
    use super::*;
    use crate::memory::entry::MemoryEntryType;
    use std::path::PathBuf;

    async fn new_storage() -> (tempfile::TempDir, Storage) {
        let dir = tempfile::TempDir::new().unwrap();
        let storage = Storage::open_at(dir.path().join("bonsai.db"))
            .await
            .unwrap();
        (dir, storage)
    }

    fn entry(tier: MemoryTier, name: &str, body: &str) -> MemoryEntry {
        MemoryEntry {
            tier,
            name: name.to_string(),
            description: format!("about {name}"),
            entry_type: MemoryEntryType::Preference,
            enabled: true,
            created: "2026-07-01".to_string(),
            updated: "2026-07-01".to_string(),
            body: format!("{body}\n"),
            content_hash: blake3::hash(body.as_bytes()).to_hex().to_string(),
            path: PathBuf::from(format!("/m/{name}.md")),
        }
    }

    async fn shadow_rows(storage: &Storage) -> Vec<(String, i64, String, String)> {
        sqlx::query_as(
            "SELECT tier, project_id, name, content_hash FROM memory_entries ORDER BY tier, name",
        )
        .fetch_all(&storage.pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn sync_adds_updates_and_deletes_rows() {
        let (_dir, storage) = new_storage().await;
        let project_id = storage
            .ensure_project(std::path::Path::new("/tmp/p"))
            .await
            .unwrap();

        let mut entries = vec![
            entry(MemoryTier::User, "alpha", "first body"),
            entry(MemoryTier::Project, "beta", "second body"),
        ];
        storage
            .sync_memory_entries(project_id, &entries)
            .await
            .unwrap();
        let rows = shadow_rows(&storage).await;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1, project_id, "project tier rows use the real id");
        assert_eq!(rows[1].1, 0, "user tier rows use project_id 0");

        // Modify one, drop the other.
        entries[0] = entry(MemoryTier::User, "alpha", "changed body");
        entries.remove(1);
        storage
            .sync_memory_entries(project_id, &entries)
            .await
            .unwrap();
        let rows = shadow_rows(&storage).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].2, "alpha");
        assert_eq!(
            rows[0].3,
            blake3::hash(b"changed body").to_hex().to_string()
        );

        // Unchanged sync is a no-op (updated_at_ms stays put).
        let before: (i64,) =
            sqlx::query_as("SELECT updated_at_ms FROM memory_entries WHERE name = 'alpha'")
                .fetch_one(&storage.pool)
                .await
                .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        storage
            .sync_memory_entries(project_id, &entries)
            .await
            .unwrap();
        let after: (i64,) =
            sqlx::query_as("SELECT updated_at_ms FROM memory_entries WHERE name = 'alpha'")
                .fetch_one(&storage.pool)
                .await
                .unwrap();
        assert_eq!(before, after, "hash-unchanged row must not be rewritten");
    }

    #[tokio::test]
    async fn sync_scopes_deletes_to_the_current_project() {
        let (_dir, storage) = new_storage().await;
        let project_a = storage
            .ensure_project(std::path::Path::new("/tmp/a"))
            .await
            .unwrap();
        let project_b = storage
            .ensure_project(std::path::Path::new("/tmp/b"))
            .await
            .unwrap();

        storage
            .sync_memory_entries(project_a, &[entry(MemoryTier::Project, "a-fact", "a")])
            .await
            .unwrap();
        // Project B syncs an empty store: A's rows must survive.
        storage.sync_memory_entries(project_b, &[]).await.unwrap();
        assert_eq!(shadow_rows(&storage).await.len(), 1);
    }

    #[tokio::test]
    async fn bm25_ranks_better_matches_first() {
        let (_dir, storage) = new_storage().await;
        let project_id = storage
            .ensure_project(std::path::Path::new("/tmp/p"))
            .await
            .unwrap();
        storage
            .sync_memory_entries(
                project_id,
                &[
                    entry(
                        MemoryTier::User,
                        "commit-hooks",
                        "use no-verify for partial commit hooks commit",
                    ),
                    entry(
                        MemoryTier::User,
                        "editor-theme",
                        "the editor theme is gruvbox",
                    ),
                ],
            )
            .await
            .unwrap();

        let hits = storage
            .memory_bm25(project_id, "\"commit\" OR \"hooks\"", 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "unrelated entry must not match");
        assert_eq!(hits[0].name, "commit-hooks");
        assert!(
            hits[0].score < 0.0,
            "bm25 scores are negative when relevant"
        );

        // Ranking direction: an entry matching both terms outranks a weaker one.
        storage
            .sync_memory_entries(
                project_id,
                &[
                    entry(
                        MemoryTier::User,
                        "commit-hooks",
                        "use no-verify for partial commit hooks commit",
                    ),
                    entry(
                        MemoryTier::User,
                        "commit-style",
                        "commit messages use imperative mood",
                    ),
                    entry(
                        MemoryTier::User,
                        "editor-theme",
                        "the editor theme is gruvbox",
                    ),
                ],
            )
            .await
            .unwrap();
        let hits = storage
            .memory_bm25(project_id, "\"commit\" OR \"hooks\"", 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].name, "commit-hooks",
            "double match must rank first: {hits:?}"
        );
    }

    #[tokio::test]
    async fn bm25_stems_query_and_entry_tokens() {
        let (_dir, storage) = new_storage().await;
        let project_id = storage
            .ensure_project(std::path::Path::new("/tmp/p"))
            .await
            .unwrap();
        storage
            .sync_memory_entries(
                project_id,
                &[entry(
                    MemoryTier::User,
                    "partial-commits",
                    "use no-verify for partial commits",
                )],
            )
            .await
            .unwrap();
        // Singular query, plural entry — porter stemming bridges them.
        let hits = storage
            .memory_bm25(project_id, "\"commit\"", 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        let hits = storage
            .memory_bm25(project_id, "\"committing\"", 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
    }

    #[tokio::test]
    async fn embeddings_round_trip_and_reject_corrupt_rows() {
        let (_dir, storage) = new_storage().await;
        storage
            .upsert_memory_embedding("hash-1", "test-model", &[0.5, -1.0, 2.0])
            .await
            .unwrap();
        let rows = storage.memory_vectors("test-model").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].vector, vec![0.5, -1.0, 2.0]);
        assert!(
            storage
                .memory_vectors("other-model")
                .await
                .unwrap()
                .is_empty()
        );

        // Corrupt the blob: the row is skipped, not a panic.
        sqlx::query("UPDATE memory_embeddings SET vector = x'0102' WHERE content_hash = 'hash-1'")
            .execute(&storage.pool)
            .await
            .unwrap();
        assert!(
            storage
                .memory_vectors("test-model")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn delete_memory_entry_cleans_fts() {
        let (_dir, storage) = new_storage().await;
        let project_id = storage
            .ensure_project(std::path::Path::new("/tmp/p"))
            .await
            .unwrap();
        storage
            .sync_memory_entries(
                project_id,
                &[entry(MemoryTier::User, "gone-soon", "findable body")],
            )
            .await
            .unwrap();
        assert_eq!(
            storage
                .memory_bm25(project_id, "\"findable\"", 10)
                .await
                .unwrap()
                .len(),
            1
        );
        storage
            .delete_memory_entry(project_id, MemoryTier::User, "gone-soon")
            .await
            .unwrap();
        assert!(
            storage
                .memory_bm25(project_id, "\"findable\"", 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(shadow_rows(&storage).await.is_empty());
    }
}
