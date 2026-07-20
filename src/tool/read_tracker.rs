use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::Mutex;

// Single source of truth for read digests and staleness comparison, shared
// with the freshness ledger so the write-guard and `/ctx` staleness can never
// disagree on the algorithm or on what "changed since read" means.
#[cfg(test)]
use crate::tool::read_evidence::digest_content;
use crate::tool::read_evidence::{FileSnapshot, SnapshotComparison, compare_snapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadState {
    pub modified: Option<SystemTime>,
    pub len: u64,
    /// Hash of the bytes that were actually read, when known. Captured at read
    /// time so an in-place edit that preserves length and mtime granularity can
    /// still be detected as a change.
    pub file_digest: Option<blake3::Hash>,
    /// Whether the read is treated as covering the entire file. Only the read
    /// tool's *explicit partial window* (an offset/limit that doesn't span the
    /// file, or large-file paging) sets this `false`, so a `write` that would
    /// clobber the unseen remainder is rejected. Bash reads (`cat`/`head`/etc.)
    /// and the generic marker assume full coverage — `cat` is the common case
    /// and over-blocking it would be a false positive.
    pub full: bool,
}

#[derive(Clone)]
pub struct ReadTracker {
    files: Arc<Mutex<HashMap<PathBuf, ReadState>>>,
}

impl ReadTracker {
    pub fn new() -> Self {
        Self {
            files: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Record a read with no file digest, treated as full coverage. Used by
    /// bash read-tracking (`cat`/`head`/`tail`/`grep`) and as the generic "was
    /// read" marker. Coverage is assumed full because `cat` (the common case) is
    /// a whole-file read and over-blocking it would be a false positive; the
    /// rare grep-then-write hole is accepted.
    pub async fn mark_read(&self, path: &std::path::Path) {
        self.mark_read_with_coverage(path, true).await;
    }

    /// Record an explicitly *partial* read (a read-tool large-file page) — leaves
    /// coverage `false` so a later `write` is rejected.
    pub async fn mark_read_partial(&self, path: &std::path::Path) {
        self.mark_read_with_coverage(path, false).await;
    }

    async fn mark_read_with_coverage(&self, path: &std::path::Path, full: bool) {
        let metadata = tokio::fs::metadata(path).await.ok();
        let state = ReadState {
            modified: metadata.as_ref().and_then(|m| m.modified().ok()),
            len: metadata.as_ref().map(|m| m.len()).unwrap_or(0),
            file_digest: None,
            full,
        };

        self.files.lock().await.insert(path.to_path_buf(), state);
    }

    /// Record a read together with the bytes that were actually read, digesting
    /// them here. Test-only convenience; production callers (the `read` tool)
    /// already hold a digest and use [`mark_read_with_file_digest`] directly to
    /// avoid digesting the same bytes twice.
    #[cfg(test)]
    pub async fn mark_read_with_content(&self, path: &std::path::Path, content: &[u8], full: bool) {
        self.mark_read_with_file_digest(path, digest_content(content), full)
            .await;
    }

    /// Record a read whose file digest the caller already computed (the `read`
    /// tool digests once and shares the value with the freshness ledger), so the
    /// same bytes are not digested twice per read.
    pub async fn mark_read_with_file_digest(
        &self,
        path: &std::path::Path,
        file_digest: blake3::Hash,
        full: bool,
    ) {
        let metadata = tokio::fs::metadata(path).await.ok();
        let state = ReadState {
            modified: metadata.as_ref().and_then(|m| m.modified().ok()),
            len: metadata.as_ref().map(|m| m.len()).unwrap_or(0),
            file_digest: Some(file_digest),
            full,
        };

        self.files.lock().await.insert(path.to_path_buf(), state);
    }

    /// Whether the file's last recorded read covered the whole file (not a
    /// partial window). `false` when never read.
    pub async fn was_fully_read(&self, path: &std::path::Path) -> bool {
        self.last_read_state(path)
            .await
            .map(|state| state.full)
            .unwrap_or(false)
    }

    pub async fn is_read(&self, path: &std::path::Path) -> bool {
        self.files.lock().await.contains_key(path)
    }

    pub async fn last_read_state(&self, path: &std::path::Path) -> Option<ReadState> {
        self.files.lock().await.get(path).copied()
    }

    /// Whether the file provably still holds the bytes of the last recorded
    /// read. Delegates to the shared [`compare_snapshot`] primitive (also
    /// backing the `/ctx` freshness ledger); the write-guard fails closed, so
    /// anything but a definite `Unchanged` — including a transient probe error
    /// or an unverifiable oversized file — counts as changed. No canonical
    /// identity check: the tracker keys on caller-supplied paths, which need
    /// not be canonical.
    pub async fn is_unchanged_since_read(&self, path: &std::path::Path) -> bool {
        let Some(state) = self.last_read_state(path).await else {
            return false;
        };
        let snapshot = FileSnapshot {
            len: state.len,
            modified: state.modified,
            file_digest: state.file_digest,
        };
        compare_snapshot(path, None, snapshot).await == SnapshotComparison::Unchanged
    }

    pub async fn clear(&self) {
        self.files.lock().await.clear();
    }
}

impl Default for ReadTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[tokio::test]
    async fn test_mark_read_and_is_read() {
        let tracker = ReadTracker::new();
        let path = Path::new("/tmp/test.txt");

        assert!(!tracker.is_read(path).await);
        tracker.mark_read(path).await;
        assert!(tracker.is_read(path).await);
    }

    #[tokio::test]
    async fn test_last_read_state_returns_none_before_mark() {
        let tracker = ReadTracker::new();
        let path = Path::new("/tmp/test.txt");

        assert!(tracker.last_read_state(path).await.is_none());
    }

    #[tokio::test]
    async fn test_last_read_state_returns_some_after_mark() {
        let tracker = ReadTracker::new();
        let path = Path::new("/tmp/test.txt");

        tracker.mark_read(path).await;
        assert!(tracker.last_read_state(path).await.is_some());
    }

    #[tokio::test]
    async fn test_clear_removes_all_entries() {
        let tracker = ReadTracker::new();
        let path1 = Path::new("/tmp/test1.txt");
        let path2 = Path::new("/tmp/test2.txt");

        tracker.mark_read(path1).await;
        tracker.mark_read(path2).await;
        assert!(tracker.is_read(path1).await);
        assert!(tracker.is_read(path2).await);

        tracker.clear().await;
        assert!(!tracker.is_read(path1).await);
        assert!(!tracker.is_read(path2).await);
    }

    #[tokio::test]
    async fn test_concurrent_access_via_clone() {
        let tracker = ReadTracker::new();
        let tracker_clone = tracker.clone();
        let path = Path::new("/tmp/test.txt");

        tracker.mark_read(path).await;
        assert!(tracker_clone.is_read(path).await);
    }

    fn unique_temp_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "bonsai_read_tracker_{label}_{}_{nanos}.txt",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn test_is_unchanged_since_read_true_for_untouched_file() {
        let path = unique_temp_path("untouched");
        tokio::fs::write(&path, b"hello world").await.unwrap();

        let tracker = ReadTracker::new();
        tracker
            .mark_read_with_content(&path, b"hello world", true)
            .await;
        assert!(tracker.is_unchanged_since_read(&path).await);

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_is_unchanged_since_read_false_after_same_length_edit() {
        let path = unique_temp_path("inplace");
        tokio::fs::write(&path, b"aaaaa").await.unwrap();

        let tracker = ReadTracker::new();
        tracker.mark_read_with_content(&path, b"aaaaa", true).await;
        assert!(tracker.is_unchanged_since_read(&path).await);

        // Same length, different content (mtime granularity may not advance).
        tokio::fs::write(&path, b"bbbbb").await.unwrap();
        assert!(!tracker.is_unchanged_since_read(&path).await);

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_is_unchanged_since_read_false_after_real_modification() {
        let path = unique_temp_path("modified");
        tokio::fs::write(&path, b"original").await.unwrap();

        let tracker = ReadTracker::new();
        tracker
            .mark_read_with_content(&path, b"original", true)
            .await;

        tokio::fs::write(&path, b"original content extended")
            .await
            .unwrap();
        assert!(!tracker.is_unchanged_since_read(&path).await);

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_is_unchanged_since_read_false_for_never_read_path() {
        let tracker = ReadTracker::new();
        let path = Path::new("/tmp/bonsai_never_read_path_xyz.txt");
        assert!(!tracker.is_unchanged_since_read(path).await);
    }

    #[tokio::test]
    async fn test_is_unchanged_since_read_false_for_non_regular_file() {
        // The shared compare_snapshot primitive rejects non-regular files, so
        // a tracked path that turned into (or always was) a directory fails
        // closed instead of comparing directory metadata as if it were content
        // (the pre-consolidation write-guard skipped this check).
        let dir = std::env::temp_dir().join(format!(
            "bonsai_read_tracker_dir_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir(&dir).await.unwrap();

        let tracker = ReadTracker::new();
        tracker.mark_read(&dir).await;
        assert!(!tracker.is_unchanged_since_read(&dir).await);

        let _ = tokio::fs::remove_dir(&dir).await;
    }
}
