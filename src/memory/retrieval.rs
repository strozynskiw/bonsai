//! Relevance-gated recall: sync the file stores into the DB shadow, then fuse
//! FTS5 BM25 and embedding similarity with reciprocal-rank fusion. The
//! threshold is membership — an entry must surface in one of the top-10
//! candidate lists — so an unrelated turn injects nothing. Every failure path
//! degrades (BM25-only, or no recall this turn) and never fails the turn.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::memory::embedding::{Embedder, EmbedderAvailability};
use crate::memory::entry::{MemoryEntry, MemoryTier};
use crate::memory::store::MemoryStore;
use crate::storage::Storage;
use crate::util::time::{days_since_utc_date, now_ms};

/// At most this many entries are recalled per turn (before the token budget).
pub(crate) const RECALL_MAX_ENTRIES: usize = 3;
/// Depth of each candidate list (BM25 and cosine) fed into RRF.
const CANDIDATES_PER_LIST: usize = 10;
/// Cosine floor below which an embedding hit is noise, not similarity.
const COSINE_FLOOR: f32 = 0.35;
/// Standard RRF constant: `score += 1 / (60 + rank)`.
const RRF_K: f64 = 60.0;
/// Small prior for project-tier entries (repo-specific beats roaming).
const PROJECT_TIER_PRIOR: f64 = 0.010;
/// Small recency prior, decaying with ~90-day e-folding on `updated`.
const RECENCY_PRIOR: f64 = 0.010;
const RECENCY_DECAY_DAYS: f64 = 90.0;
/// Cap query tokens so a pasted wall of text stays a bounded FTS query.
const MAX_QUERY_TOKENS: usize = 16;
/// English function words dropped from FTS queries. The default FTS5
/// tokenizer indexes them, and with OR'd tokens a single shared "the" would
/// make every English sentence recall something — exactly the noise the
/// relevance gate exists to prevent.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "been", "but", "by", "can", "could", "did", "do",
    "does", "for", "from", "had", "has", "have", "how", "if", "in", "into", "is", "it", "its",
    "just", "me", "my", "no", "not", "of", "on", "or", "our", "should", "so", "that", "the",
    "their", "them", "then", "there", "these", "they", "this", "to", "was", "we", "were", "what",
    "when", "where", "which", "who", "why", "will", "with", "would", "you", "your",
];

pub(crate) struct MemoryRetrieval {
    storage: Storage,
    project_id: i64,
    embedder: Arc<dyn Embedder>,
    /// Fingerprint of both store dirs' file metadata at the last completed DB
    /// sync — the per-turn fast path: unchanged dirs skip reload and DB work.
    last_scan: Mutex<Option<blake3::Hash>>,
    /// Whether every current entry has a cached embedding for the active
    /// model. Reset when the stores change; retried once the embedder is Ready
    /// (it may still be Loading when the fingerprint is first recorded).
    embeddings_complete: Mutex<bool>,
}

impl MemoryRetrieval {
    pub(crate) fn new(storage: Storage, project_id: i64, embedder: Arc<dyn Embedder>) -> Self {
        Self {
            storage,
            project_id,
            embedder,
            last_scan: Mutex::new(None),
            embeddings_complete: Mutex::new(false),
        }
    }

    /// Reconcile fs → DB shadow. Runs at the top of each user turn; the
    /// metadata scan below keeps the common no-change case at readdir cost
    /// (the same turn already runs a git subprocess for volatile state).
    pub(crate) async fn sync(&self, store: &MemoryStore) {
        let fingerprint = store_fingerprint(store);
        let changed = *self.last_scan.lock().expect("memory scan lock") != Some(fingerprint);
        if changed {
            // Pick up external edits ($EDITOR, the edit tool on project-tier
            // files, another bonsai instance) before mirroring to the DB.
            store.reload();
            let entries = store.enabled_entries();
            if let Err(err) = self
                .storage
                .sync_memory_entries(self.project_id, &entries)
                .await
            {
                tracing::warn!(%err, "memory shadow sync failed; will retry next turn");
                return; // fingerprint not recorded => retried next turn
            }
            *self.embeddings_complete.lock().expect("memory embed lock") = false;
            *self.last_scan.lock().expect("memory scan lock") = Some(fingerprint);
        }

        let entries = store.enabled_entries();
        if entries.is_empty() {
            return;
        }
        // Lazy model load: kicked only by a non-empty store, so fresh installs
        // never download anything.
        self.embedder.ensure_loading();
        let complete = *self.embeddings_complete.lock().expect("memory embed lock");
        if !complete && self.ensure_embeddings(&entries).await {
            *self.embeddings_complete.lock().expect("memory embed lock") = true;
        }
    }

    /// Backfill embedding rows for entries missing one under the active model.
    /// Returns true when the cache is complete for the current entry set.
    async fn ensure_embeddings(&self, entries: &[MemoryEntry]) -> bool {
        if !matches!(self.embedder.availability(), EmbedderAvailability::Ready) {
            return false;
        }
        let cached: HashSet<String> =
            match self.storage.memory_vectors(self.embedder.model_id()).await {
                Ok(rows) => rows.into_iter().map(|row| row.content_hash).collect(),
                Err(err) => {
                    tracing::warn!(%err, "failed to load memory embedding cache");
                    return false;
                }
            };
        let missing: Vec<&MemoryEntry> = entries
            .iter()
            .filter(|entry| !cached.contains(&entry.content_hash))
            .collect();
        if missing.is_empty() {
            return true;
        }
        let texts: Vec<String> = missing.iter().map(|entry| embedding_text(entry)).collect();
        let vectors = match self.embedder.encode(&texts) {
            Ok(vectors) => vectors,
            Err(err) => {
                tracing::warn!(%err, "memory embedding encode failed");
                return false;
            }
        };
        for (entry, vector) in missing.iter().zip(vectors) {
            if let Err(err) = self
                .storage
                .upsert_memory_embedding(&entry.content_hash, self.embedder.model_id(), &vector)
                .await
            {
                tracing::warn!(%err, "failed to cache memory embedding");
                return false;
            }
        }
        true
    }

    /// Hybrid retrieval for one query. `exclude` holds names already in the
    /// conversation this session (recalled earlier or freshly captured).
    pub(crate) async fn retrieve(
        &self,
        query: &str,
        exclude: &HashSet<String>,
        store: &MemoryStore,
    ) -> Vec<MemoryEntry> {
        let entries = store.enabled_entries();
        if entries.is_empty() || query.trim().is_empty() {
            return Vec::new();
        }

        let mut rrf: HashMap<(MemoryTier, String), f64> = HashMap::new();

        let fts_query = sanitize_fts_query(query);
        if !fts_query.is_empty() {
            match self
                .storage
                .memory_bm25(self.project_id, &fts_query, CANDIDATES_PER_LIST as i64)
                .await
            {
                Ok(hits) => {
                    for (rank, hit) in hits.iter().enumerate() {
                        let Some(tier) = MemoryTier::from_label(&hit.tier) else {
                            continue;
                        };
                        *rrf.entry((tier, hit.name.clone())).or_default() +=
                            1.0 / (RRF_K + rank as f64 + 1.0);
                    }
                }
                Err(err) => {
                    tracing::warn!(%err, "memory BM25 query failed; skipping recall lane");
                }
            }
        }

        for (rank, entry) in self
            .cosine_candidates(query, &entries)
            .await
            .iter()
            .enumerate()
        {
            *rrf.entry((entry.tier, entry.name.clone())).or_default() +=
                1.0 / (RRF_K + rank as f64 + 1.0);
        }

        if rrf.is_empty() {
            return Vec::new();
        }

        let now = now_ms();
        let mut ranked: Vec<(f64, &MemoryEntry)> = entries
            .iter()
            .filter(|entry| !exclude.contains(&entry.name))
            .filter_map(|entry| {
                let base = rrf.get(&(entry.tier, entry.name.clone()))?;
                let mut score = *base;
                if entry.tier == MemoryTier::Project {
                    score += PROJECT_TIER_PRIOR;
                }
                if let Some(age_days) = days_since_utc_date(&entry.updated, now) {
                    score += RECENCY_PRIOR * (-age_days / RECENCY_DECAY_DAYS).exp();
                }
                Some((score, entry))
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.name.cmp(&b.1.name))
        });
        ranked
            .into_iter()
            .take(RECALL_MAX_ENTRIES)
            .map(|(_, entry)| entry.clone())
            .collect()
    }

    /// The embedding lane: brute-force cosine over the cached vectors (a few
    /// hundred 256-dim rows at most — no vector index needed). Empty whenever
    /// the embedder isn't Ready, which is the silent BM25-only degradation.
    async fn cosine_candidates<'a>(
        &self,
        query: &str,
        entries: &'a [MemoryEntry],
    ) -> Vec<&'a MemoryEntry> {
        if !matches!(self.embedder.availability(), EmbedderAvailability::Ready) {
            return Vec::new();
        }
        let query_vector = match self
            .embedder
            .encode(std::slice::from_ref(&query.to_string()))
        {
            Ok(mut vectors) if !vectors.is_empty() => vectors.remove(0),
            Ok(_) => return Vec::new(),
            Err(err) => {
                tracing::warn!(%err, "memory query embedding failed; BM25-only this turn");
                return Vec::new();
            }
        };
        let rows = match self.storage.memory_vectors(self.embedder.model_id()).await {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(%err, "memory embedding lookup failed; BM25-only this turn");
                return Vec::new();
            }
        };
        let by_hash: HashMap<&str, &Vec<f32>> = rows
            .iter()
            .map(|row| (row.content_hash.as_str(), &row.vector))
            .collect();
        let mut hits: Vec<(f32, &MemoryEntry)> = entries
            .iter()
            .filter_map(|entry| {
                let vector = by_hash.get(entry.content_hash.as_str())?;
                let similarity = cosine(&query_vector, vector);
                (similarity >= COSINE_FLOOR).then_some((similarity, entry))
            })
            .collect();
        hits.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.name.cmp(&b.1.name))
        });
        hits.into_iter()
            .take(CANDIDATES_PER_LIST)
            .map(|(_, entry)| entry)
            .collect()
    }
}

/// What gets embedded per entry: the name (de-slugged), the index line, and
/// the fact body — the same text surfaces BM25 hits.
fn embedding_text(entry: &MemoryEntry) -> String {
    format!(
        "{}\n{}\n{}",
        entry.name.replace('-', " "),
        entry.description,
        entry.body
    )
}

/// Reduce free text to a safe FTS5 query: lowercase alphanumeric tokens,
/// quoted (so FTS operators like NEAR/AND/* stay literal), OR'd, deduped,
/// bounded. Single characters are dropped as noise.
fn sanitize_fts_query(query: &str) -> String {
    let mut seen = HashSet::new();
    let mut tokens = Vec::new();
    for token in query
        .to_lowercase()
        .split(|ch: char| !ch.is_ascii_alphanumeric())
    {
        if token.chars().count() < 2
            || STOPWORDS.contains(&token)
            || !seen.insert(token.to_string())
        {
            continue;
        }
        tokens.push(format!("\"{token}\""));
        if tokens.len() == MAX_QUERY_TOKENS {
            break;
        }
    }
    tokens.join(" OR ")
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a.sqrt() * norm_b.sqrt())
    }
}

/// Fingerprint both store dirs by entry metadata (name, mtime, size) — enough
/// to catch adds, deletes, renames, and in-place edits without reading bodies.
fn store_fingerprint(store: &MemoryStore) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    for tier in [MemoryTier::User, MemoryTier::Project] {
        let dir = store.dir(tier);
        hasher.update(dir.as_os_str().as_encoded_bytes());
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut rows: Vec<(String, i64, u64)> = read_dir
            .flatten()
            .filter(|dir_entry| {
                dir_entry.path().extension().and_then(|ext| ext.to_str()) == Some("md")
            })
            .filter_map(|dir_entry| {
                let metadata = dir_entry.metadata().ok()?;
                let mtime = metadata
                    .modified()
                    .ok()
                    .and_then(crate::util::time::system_time_to_ms)
                    .unwrap_or(0);
                Some((
                    dir_entry.file_name().to_string_lossy().to_string(),
                    mtime,
                    metadata.len(),
                ))
            })
            .collect();
        rows.sort();
        for (name, mtime, len) in rows {
            hasher.update(name.as_bytes());
            hasher.update(&mtime.to_le_bytes());
            hasher.update(&len.to_le_bytes());
        }
    }
    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::embedding::NoopEmbedder;
    use crate::memory::embedding::test_utils::FakeEmbedder;
    use crate::memory::entry::MemoryEntryType;

    async fn fixture(
        embedder: Arc<dyn Embedder>,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        tempfile::TempDir,
        MemoryStore,
        MemoryRetrieval,
    ) {
        let home = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        let db = tempfile::TempDir::new().unwrap();
        let storage = Storage::open_at(db.path().join("bonsai.db")).await.unwrap();
        let store = MemoryStore::load(home.path(), project.path());
        let retrieval = MemoryRetrieval::new(storage, 7, embedder);
        (home, project, db, store, retrieval)
    }

    fn write_entry(store: &MemoryStore, tier: MemoryTier, description: &str, body: &str) {
        store
            .write(
                tier,
                MemoryEntryType::Preference,
                None,
                description,
                body,
                None,
            )
            .unwrap();
    }

    #[tokio::test]
    async fn sync_fast_path_skips_unchanged_dirs() {
        let (_h, _p, _d, store, retrieval) = fixture(Arc::new(NoopEmbedder)).await;
        write_entry(
            &store,
            MemoryTier::User,
            "commit hooks fact",
            "use no-verify",
        );
        retrieval.sync(&store).await;
        let first = *retrieval.last_scan.lock().unwrap();
        assert!(first.is_some());
        retrieval.sync(&store).await;
        assert_eq!(*retrieval.last_scan.lock().unwrap(), first);

        // A new file changes the fingerprint and re-syncs.
        write_entry(&store, MemoryTier::User, "editor theme fact", "gruvbox");
        retrieval.sync(&store).await;
        assert_ne!(*retrieval.last_scan.lock().unwrap(), first);
    }

    #[tokio::test]
    async fn bm25_lane_finds_related_and_ignores_unrelated_queries() {
        let (_h, _p, _d, store, retrieval) = fixture(Arc::new(NoopEmbedder)).await;
        write_entry(
            &store,
            MemoryTier::User,
            "partial commits need no-verify",
            "The pre-commit hook fails on the unstaged remainder.",
        );
        retrieval.sync(&store).await;

        let hits = retrieval
            .retrieve(
                "how do I commit only part of my changes",
                &HashSet::new(),
                &store,
            )
            .await;
        assert_eq!(hits.len(), 1, "related query should recall the entry");

        let hits = retrieval
            .retrieve(
                "what color is the dashboard sidebar",
                &HashSet::new(),
                &store,
            )
            .await;
        assert!(
            hits.is_empty(),
            "unrelated query must inject nothing: {hits:?}"
        );
    }

    #[tokio::test]
    async fn exclusion_prevents_reinjection() {
        let (_h, _p, _d, store, retrieval) = fixture(Arc::new(NoopEmbedder)).await;
        write_entry(
            &store,
            MemoryTier::User,
            "partial commits need no-verify",
            "hook detail",
        );
        retrieval.sync(&store).await;

        let first = retrieval
            .retrieve("partial commits", &HashSet::new(), &store)
            .await;
        assert_eq!(first.len(), 1);
        let exclude: HashSet<String> = first.iter().map(|entry| entry.name.clone()).collect();
        let second = retrieval
            .retrieve("partial commits", &exclude, &store)
            .await;
        assert!(second.is_empty(), "recalled names must not re-inject");
    }

    #[tokio::test]
    async fn embedding_lane_recalls_semantic_matches_without_shared_tokens() {
        let embedder = Arc::new(FakeEmbedder {
            table: vec![
                ("staged subset", vec![1.0, 0.0, 0.0, 0.0]),
                ("commit only some", vec![0.9, 0.1, 0.0, 0.0]),
                ("gruvbox", vec![0.0, 1.0, 0.0, 0.0]),
            ],
        });
        let (_h, _p, _d, store, retrieval) = fixture(embedder).await;
        write_entry(
            &store,
            MemoryTier::User,
            "staged subset workflow",
            "use no-verify",
        );
        write_entry(&store, MemoryTier::User, "gruvbox theme", "editor colors");
        retrieval.sync(&store).await;

        // No lexical overlap with either entry; only the fake vectors align.
        let hits = retrieval
            .retrieve("commit only some", &HashSet::new(), &store)
            .await;
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].name, "staged-subset-workflow");
    }

    #[tokio::test]
    async fn rrf_fusion_ranks_double_lane_hits_first() {
        let embedder = Arc::new(FakeEmbedder {
            // Only the checklist entry (and the query) get an aligned vector;
            // the notes entry embeds orthogonally.
            table: vec![("checklist", vec![1.0, 0.0, 0.0, 0.0])],
        });
        let (_h, _p, _d, store, retrieval) = fixture(embedder).await;
        // Entry A hits both lanes (lexical "release" + vector); entry B only
        // matches lexically.
        write_entry(
            &store,
            MemoryTier::User,
            "release checklist steps",
            "tag then publish",
        );
        write_entry(
            &store,
            MemoryTier::User,
            "release notes format",
            "keep a changelog",
        );
        retrieval.sync(&store).await;

        let hits = retrieval
            .retrieve("release checklist", &HashSet::new(), &store)
            .await;
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].name, "release-checklist-steps",
            "double-lane entry must outrank single-lane: {hits:?}"
        );

        // Hand-computed RRF sanity for this fixture (same tier and day, so
        // priors cancel): two lane-1 memberships beat one.
        let two_lane = 2.0 / (RRF_K + 1.0);
        let one_lane = 1.0 / (RRF_K + 1.0);
        assert!(two_lane > one_lane);
    }

    #[tokio::test]
    async fn project_tier_prior_breaks_lexical_ties() {
        let (_h, _p, _d, store, retrieval) = fixture(Arc::new(NoopEmbedder)).await;
        write_entry(
            &store,
            MemoryTier::User,
            "deploy pipeline alpha",
            "generic fact",
        );
        write_entry(
            &store,
            MemoryTier::Project,
            "deploy pipeline beta",
            "repo fact",
        );
        retrieval.sync(&store).await;

        let hits = retrieval
            .retrieve("deploy pipeline", &HashSet::new(), &store)
            .await;
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].tier,
            MemoryTier::Project,
            "project tier should win a tie: {hits:?}"
        );
    }

    #[tokio::test]
    async fn recall_caps_at_three_entries() {
        let (_h, _p, _d, store, retrieval) = fixture(Arc::new(NoopEmbedder)).await;
        for index in 0..6 {
            write_entry(
                &store,
                MemoryTier::User,
                &format!("deploy step number {index}"),
                "detail",
            );
        }
        retrieval.sync(&store).await;
        let hits = retrieval
            .retrieve("deploy step", &HashSet::new(), &store)
            .await;
        assert_eq!(hits.len(), RECALL_MAX_ENTRIES);
    }

    #[test]
    fn fts_queries_are_sanitized() {
        // Operators are neutralized by quoting; stopwords and one-char tokens
        // are dropped; duplicates collapse.
        assert_eq!(
            sanitize_fts_query("How do I commit* AND (NEAR) stuff stuff?"),
            "\"commit\" OR \"near\" OR \"stuff\""
        );
        assert_eq!(sanitize_fts_query("a ? !"), "");
        assert_eq!(sanitize_fts_query("what is the that"), "");
        let long = "word ".repeat(50);
        assert_eq!(sanitize_fts_query(&long), "\"word\"");
    }

    #[test]
    fn cosine_handles_degenerate_inputs() {
        assert_eq!(cosine(&[], &[]), 0.0);
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
    }
}
