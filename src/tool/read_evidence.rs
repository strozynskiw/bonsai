use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

use tokio::fs;
use tokio::io::AsyncReadExt;

const MAX_FRESHNESS_DIGEST_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReadCoverage {
    Full,
    Partial,
}

impl ReadCoverage {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Partial => "partial",
        }
    }

    pub(crate) fn from_label(label: &str) -> Option<Self> {
        match label {
            "full" => Some(Self::Full),
            "partial" => Some(Self::Partial),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReadFreshness {
    Fresh,
    Stale,
    Deleted,
    Unknown,
}

impl ReadFreshness {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Stale => "stale",
            Self::Deleted => "deleted",
            Self::Unknown => "unknown",
        }
    }

    pub const fn requires_marker(self) -> bool {
        !matches!(self, Self::Fresh)
    }

    pub(crate) fn from_label(label: &str) -> Option<Self> {
        match label {
            "fresh" => Some(Self::Fresh),
            "stale" => Some(Self::Stale),
            "deleted" => Some(Self::Deleted),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// A file's state as captured at read time: the triple every staleness check
/// compares the current on-disk file against.
#[derive(Debug, Clone, Copy)]
pub struct FileSnapshot {
    pub len: u64,
    pub modified: Option<SystemTime>,
    /// Digest of the bytes that were read, when captured. The only way to catch
    /// an in-place edit that preserves length and mtime granularity.
    pub file_digest: Option<blake3::Hash>,
}

/// How the on-disk file compares to a captured [`FileSnapshot`]. The single
/// staleness primitive shared by the write-guard
/// ([`ReadTracker`](crate::tool::ReadTracker)) and the `/ctx` freshness ledger
/// ([`ReadEvidence`]), so the two can never disagree on what "changed since
/// read" means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotComparison {
    /// Same bytes (digest match) or same len+mtime.
    Unchanged,
    /// Different content, a non-regular file, or a swapped path identity.
    Changed,
    /// The file no longer exists.
    Deleted,
    /// No evidence either way: the snapshot is too large to re-digest, or
    /// mtimes are unavailable for the digestless comparison.
    Unverifiable,
    /// A momentary stat/open error (EINTR, a permission hiccup, fd
    /// exhaustion, an AV/lock blip) is not evidence the file changed; callers
    /// keep their prior verdict rather than flapping — a spurious stale
    /// marker would rewrite mid-history bytes and break the provider prompt
    /// cache for a file that never changed.
    TransientError,
}

/// Compare the file at `path` to a captured snapshot. When
/// `expected_canonical` is given, the path's current canonicalization must
/// still resolve to it — this catches a symlink swapped underneath a
/// previously-canonicalized path (the ledger's case; the write-guard tracks
/// caller-supplied keys, so it passes `None`).
pub async fn compare_snapshot(
    path: &std::path::Path,
    expected_canonical: Option<&std::path::Path>,
    snapshot: FileSnapshot,
) -> SnapshotComparison {
    let metadata = match fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return SnapshotComparison::Deleted;
        }
        Err(_) => return SnapshotComparison::TransientError,
    };

    if !metadata.file_type().is_file() {
        return SnapshotComparison::Changed;
    }

    if let Some(expected) = expected_canonical {
        match fs::canonicalize(path).await {
            Ok(current) if current == expected => {}
            Ok(_) => return SnapshotComparison::Changed,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return SnapshotComparison::Deleted;
            }
            Err(_) => return SnapshotComparison::TransientError,
        }
    }

    if metadata.len() != snapshot.len {
        return SnapshotComparison::Changed;
    }

    if let Some(expected_digest) = snapshot.file_digest {
        return compare_digest(path, snapshot.len, expected_digest).await;
    }

    // No captured digest (a large-file window read, or a bash `cat`-style
    // marker): fall back to mtime. A changed mtime is treated conservatively
    // as changed — warning the model to re-read is safer than missing a real
    // change we can't otherwise detect.
    match (metadata.modified().ok(), snapshot.modified) {
        (Some(current), Some(previous)) if current == previous => SnapshotComparison::Unchanged,
        (Some(_), Some(_)) => SnapshotComparison::Changed,
        _ => SnapshotComparison::Unverifiable,
    }
}

/// Re-digest the current file bytes and compare. The open/metadata/read
/// sequence re-checks identity and length at each step so a file replaced
/// mid-probe reads as changed, not as a transient error.
async fn compare_digest(
    path: &std::path::Path,
    expected_len: u64,
    expected_digest: blake3::Hash,
) -> SnapshotComparison {
    if expected_len > MAX_FRESHNESS_DIGEST_BYTES {
        return SnapshotComparison::Unverifiable;
    }

    let file = match fs::File::open(path).await {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return SnapshotComparison::Deleted;
        }
        Err(_) => return SnapshotComparison::TransientError,
    };

    let metadata = match file.metadata().await {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return SnapshotComparison::Deleted;
        }
        Err(_) => return SnapshotComparison::TransientError,
    };

    if !metadata.file_type().is_file() || metadata.len() != expected_len {
        return SnapshotComparison::Changed;
    }

    let Ok(capacity) = usize::try_from(expected_len) else {
        return SnapshotComparison::TransientError;
    };
    let mut content = Vec::with_capacity(capacity);
    let mut reader = file.take(expected_len.saturating_add(1));
    match reader.read_to_end(&mut content).await {
        Ok(_) => {
            if content.len() as u64 != expected_len {
                SnapshotComparison::Changed
            } else if digest_content(&content) == expected_digest {
                SnapshotComparison::Unchanged
            } else {
                SnapshotComparison::Changed
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => SnapshotComparison::Deleted,
        Err(_) => SnapshotComparison::TransientError,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReadWindow {
    pub requested_offset: usize,
    pub requested_limit: usize,
    pub start_line: usize,
    pub end_line: Option<usize>,
    pub total_lines: Option<usize>,
}

impl ReadWindow {
    pub fn label(&self) -> String {
        match (self.end_line, self.total_lines) {
            (Some(end), Some(total)) => format!("lines {}-{end}/{total}", self.start_line),
            (Some(end), None) => format!("lines {}-{end}", self.start_line),
            (None, Some(total)) => format!("line {} past EOF/{total}", self.start_line),
            (None, None) => format!("line {}", self.start_line),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReadObservation {
    display_path: String,
    canonical_path: PathBuf,
    window: ReadWindow,
    coverage: ReadCoverage,
    visible_digest: blake3::Hash,
    visible_chars: usize,
    file_digest_at_read: Option<blake3::Hash>,
}

impl ReadObservation {
    pub fn display_path(&self) -> &str {
        &self.display_path
    }

    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    pub const fn window(&self) -> &ReadWindow {
        &self.window
    }

    pub const fn coverage(&self) -> ReadCoverage {
        self.coverage
    }

    pub const fn visible_digest(&self) -> blake3::Hash {
        self.visible_digest
    }

    pub const fn visible_chars(&self) -> usize {
        self.visible_chars
    }

    pub const fn file_digest_at_read(&self) -> Option<blake3::Hash> {
        self.file_digest_at_read
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileFreshnessBaseline {
    canonical_path: PathBuf,
    modified: Option<SystemTime>,
    len: u64,
    current_file_digest: Option<blake3::Hash>,
    status: ReadFreshness,
    observation_is_current: bool,
}

impl FileFreshnessBaseline {
    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    pub const fn modified(&self) -> Option<SystemTime> {
        self.modified
    }

    pub const fn len(&self) -> u64 {
        self.len
    }

    pub const fn current_file_digest(&self) -> Option<blake3::Hash> {
        self.current_file_digest
    }

    pub const fn status(&self) -> ReadFreshness {
        self.status
    }

    pub const fn observation_is_current(&self) -> bool {
        self.observation_is_current
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReadEvidence {
    observation: ReadObservation,
    freshness_baseline: FileFreshnessBaseline,
}

impl ReadEvidence {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        display_path: impl Into<String>,
        canonical_path: PathBuf,
        window: ReadWindow,
        coverage: ReadCoverage,
        visible_content: &str,
        modified: Option<SystemTime>,
        len: u64,
        file_digest_at_read: Option<blake3::Hash>,
    ) -> Self {
        Self {
            observation: ReadObservation {
                display_path: display_path.into(),
                canonical_path: canonical_path.clone(),
                window,
                coverage,
                visible_digest: digest_content(visible_content.as_bytes()),
                visible_chars: visible_content.chars().count(),
                file_digest_at_read,
            },
            freshness_baseline: FileFreshnessBaseline {
                canonical_path,
                modified,
                len,
                current_file_digest: file_digest_at_read,
                status: ReadFreshness::Fresh,
                observation_is_current: true,
            },
        }
    }

    pub const fn observation(&self) -> &ReadObservation {
        &self.observation
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_persisted_parts(
        display_path: String,
        canonical_path: PathBuf,
        window: ReadWindow,
        coverage: ReadCoverage,
        visible_digest: blake3::Hash,
        visible_chars: usize,
        file_digest_at_read: Option<blake3::Hash>,
        modified: Option<SystemTime>,
        len: u64,
        current_file_digest: Option<blake3::Hash>,
        status: ReadFreshness,
        observation_is_current: bool,
    ) -> Self {
        Self {
            observation: ReadObservation {
                display_path,
                canonical_path: canonical_path.clone(),
                window,
                coverage,
                visible_digest,
                visible_chars,
                file_digest_at_read,
            },
            freshness_baseline: FileFreshnessBaseline {
                canonical_path,
                modified,
                len,
                current_file_digest,
                status,
                observation_is_current,
            },
        }
    }

    #[cfg(test)]
    pub fn display_path(&self) -> &str {
        self.observation.display_path()
    }

    #[cfg(test)]
    pub fn canonical_path(&self) -> &Path {
        self.observation.canonical_path()
    }

    #[cfg(test)]
    pub const fn window(&self) -> &ReadWindow {
        self.observation.window()
    }

    #[cfg(test)]
    pub const fn coverage(&self) -> ReadCoverage {
        self.observation.coverage()
    }

    #[cfg(test)]
    pub const fn visible_digest(&self) -> blake3::Hash {
        self.observation.visible_digest()
    }

    #[cfg(test)]
    pub const fn file_digest_at_read(&self) -> Option<blake3::Hash> {
        self.observation.file_digest_at_read()
    }

    pub const fn freshness_baseline(&self) -> &FileFreshnessBaseline {
        &self.freshness_baseline
    }

    pub const fn freshness(&self) -> ReadFreshness {
        self.freshness_baseline.status()
    }

    pub fn observation_is_current(&self) -> bool {
        self.freshness() == ReadFreshness::Fresh && self.freshness_baseline.observation_is_current()
    }

    pub async fn refresh_freshness(&mut self) {
        let snapshot = FileSnapshot {
            len: self.freshness_baseline.len(),
            modified: self.freshness_baseline.modified(),
            file_digest: self.freshness_baseline.current_file_digest(),
        };
        let comparison = compare_snapshot(
            self.freshness_baseline.canonical_path(),
            Some(self.freshness_baseline.canonical_path()),
            snapshot,
        )
        .await;
        self.freshness_baseline.status = match comparison {
            SnapshotComparison::Unchanged => ReadFreshness::Fresh,
            SnapshotComparison::Changed => {
                self.freshness_baseline.observation_is_current = false;
                ReadFreshness::Stale
            }
            SnapshotComparison::Deleted => {
                self.freshness_baseline.observation_is_current = false;
                ReadFreshness::Deleted
            }
            SnapshotComparison::Unverifiable => {
                self.freshness_baseline.observation_is_current = false;
                ReadFreshness::Unknown
            }
            // Transient probe failure: keep the prior verdict rather than
            // downgrading to Unknown and flapping a marker.
            SnapshotComparison::TransientError => return,
        };
    }

    /// Advance only the mutable filesystem baseline after an agent mutation.
    /// The observation digests remain the identity of the bytes that the old
    /// message actually displayed and must never change.
    pub async fn refresh_baseline_after_agent_mutation(&mut self) {
        let path = &self.freshness_baseline.canonical_path;
        let Ok(metadata) = fs::symlink_metadata(path).await else {
            return;
        };
        if !metadata.file_type().is_file() {
            return;
        }
        if metadata.len() <= MAX_FRESHNESS_DIGEST_BYTES {
            let Ok(bytes) = fs::read(path).await else {
                return;
            };
            self.freshness_baseline.len = bytes.len() as u64;
            self.freshness_baseline.current_file_digest = Some(digest_content(&bytes));
            // Re-stat after the read so mtime describes the same (or newer)
            // snapshot; a change in the sub-read window self-corrects next turn.
            self.freshness_baseline.modified = fs::symlink_metadata(path)
                .await
                .ok()
                .and_then(|meta| meta.modified().ok());
        } else {
            // Above the digest ceiling: mtime baseline only, matching the
            // digestless partial-read path in `refresh_freshness`.
            self.freshness_baseline.len = metadata.len();
            self.freshness_baseline.current_file_digest = None;
            self.freshness_baseline.modified = metadata.modified().ok();
        }
        self.freshness_baseline.status = ReadFreshness::Fresh;
        self.freshness_baseline.observation_is_current = false;
    }

    pub fn ledger_label(&self) -> String {
        format!(
            "{} {} {}",
            self.freshness().label(),
            self.observation.coverage.label(),
            self.observation.window.label()
        )
    }

    pub fn ledger_text(&self) -> String {
        format!(
            "path: {}\nstatus: {}\ncoverage: {}\nwindow: {}\nbytes_at_read: {}",
            self.observation.display_path,
            self.freshness().label(),
            self.observation.coverage.label(),
            self.observation.window.label(),
            self.freshness_baseline.len()
        )
    }

    /// One bullet for the volatile-tail stale-read advisory. Advisories ride
    /// in the per-request volatile section instead of being wrapped around the
    /// historical read output, so flagging a stale read never rewrites
    /// mid-history bytes (which would break the provider prompt cache).
    pub fn advisory_line(&self) -> String {
        let status = match self.freshness() {
            ReadFreshness::Stale => "changed after your read",
            ReadFreshness::Deleted => "deleted after your read",
            ReadFreshness::Unknown => "freshness could not be verified",
            ReadFreshness::Fresh => "still fresh",
        };
        format!(
            "- {} — {status} ({} read, {})",
            self.observation.display_path,
            self.observation.coverage.label(),
            self.observation.window.label()
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ReadProvenance {
    ParentVisible,
    MentionVisible,
    DelegatedObserved,
}

impl ReadProvenance {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::ParentVisible => "parent_visible",
            Self::MentionVisible => "mention_visible",
            Self::DelegatedObserved => "delegated_observed",
        }
    }

    pub(crate) fn from_label(label: &str) -> Option<Self> {
        match label {
            "parent_visible" => Some(Self::ParentVisible),
            "mention_visible" => Some(Self::MentionVisible),
            "delegated_observed" => Some(Self::DelegatedObserved),
            _ => None,
        }
    }
}

/// Fixed-size evidence handed from a completed child conversation to its
/// parent. It deliberately carries no file body and never enters the parent's
/// read-before-write tracker.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DelegatedReadEvidence {
    pub(crate) subtask_id: String,
    pub(crate) launch_group_id: Option<String>,
    pub(crate) source_id: String,
    pub(crate) cited_in_result: bool,
    pub(crate) evidence: ReadEvidence,
}

/// Normalized session snapshot for one model-visible file observation. It
/// stores only identity, coverage, and fixed-size validation data; file bodies
/// remain in their existing context messages.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ReadEvidenceRecord {
    pub(crate) source_id: String,
    pub(crate) provenance: ReadProvenance,
    pub(crate) target_message_id: String,
    pub(crate) target_content_digest: blake3::Hash,
    pub(crate) target_tool_call_id: Option<String>,
    pub(crate) tool_name: Option<String>,
    pub(crate) tool_arguments: Option<String>,
    pub(crate) target_live: bool,
    pub(crate) target_stubbed: bool,
    pub(crate) evidence: ReadEvidence,
    pub(crate) admission_outcome: String,
    pub(crate) admission_reason: String,
    pub(crate) requested_chars: usize,
    pub(crate) returned_chars: usize,
    pub(crate) avoided_chars: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum InspectionOutcome {
    Executed,
    Reused,
    Rejected,
}

impl InspectionOutcome {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Executed => "executed",
            Self::Reused => "reused",
            Self::Rejected => "rejected",
        }
    }

    pub(crate) fn from_label(label: &str) -> Option<Self> {
        match label {
            "executed" => Some(Self::Executed),
            "reused" => Some(Self::Reused),
            "rejected" => Some(Self::Rejected),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum InspectionReason {
    FreshVisibleCoverage,
    NoFreshVisibleCoverage,
    NotReusable,
    ToolFailed,
    RepeatedFreshReuse,
    MissingPathEvidence,
}

impl InspectionReason {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::FreshVisibleCoverage => "fresh_visible_coverage",
            Self::NoFreshVisibleCoverage => "no_fresh_visible_coverage",
            Self::NotReusable => "not_reusable",
            Self::ToolFailed => "tool_failed",
            Self::RepeatedFreshReuse => "repeated_fresh_reuse",
            Self::MissingPathEvidence => "missing_path_evidence",
        }
    }

    pub(crate) fn from_label(label: &str) -> Option<Self> {
        match label {
            "fresh_visible_coverage" => Some(Self::FreshVisibleCoverage),
            "no_fresh_visible_coverage" => Some(Self::NoFreshVisibleCoverage),
            "not_reusable" => Some(Self::NotReusable),
            "tool_failed" => Some(Self::ToolFailed),
            "repeated_fresh_reuse" => Some(Self::RepeatedFreshReuse),
            "missing_path_evidence" => Some(Self::MissingPathEvidence),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ReadAdmissionMetadata {
    pub(crate) outcome: InspectionOutcome,
    pub(crate) reason: InspectionReason,
    pub(crate) reuse_target_tool_call_id: Option<String>,
    pub(crate) requested_chars: usize,
    pub(crate) returned_chars: usize,
    pub(crate) avoided_chars: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct InspectionEventRecord {
    pub(crate) call_id: String,
    pub(crate) target_message_id: String,
    pub(crate) target_content_digest: blake3::Hash,
    pub(crate) tool_name: String,
    pub(crate) tool_arguments: String,
    pub(crate) target_live: bool,
    pub(crate) target_stubbed: bool,
    pub(crate) admission: ReadAdmissionMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LineRange {
    start: usize,
    end: usize,
}

impl LineRange {
    pub(crate) const fn new(start: usize, end: usize) -> Option<Self> {
        if start == 0 || end < start {
            return None;
        }
        Some(Self { start, end })
    }

    #[cfg(test)]
    const fn covers(self, other: Self) -> bool {
        self.start <= other.start && self.end >= other.end
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexedRange {
    Full,
    Lines(LineRange),
}

impl IndexedRange {
    #[cfg(test)]
    const fn covers(self, requested: LineRange) -> bool {
        match self {
            Self::Full => true,
            Self::Lines(range) => range.covers(requested),
        }
    }
}

#[derive(Debug)]
struct IndexedObservation {
    target_id: String,
    provenance: ReadProvenance,
    range: IndexedRange,
    cited_in_result: bool,
}

#[derive(Debug)]
struct IndexedPath {
    display_path: String,
    observations: Vec<IndexedObservation>,
}

#[derive(Debug, Default)]
pub(crate) struct ReadEvidenceIndex {
    paths: BTreeMap<PathBuf, IndexedPath>,
}

impl ReadEvidenceIndex {
    pub(crate) fn insert(
        &mut self,
        target_id: impl Into<String>,
        provenance: ReadProvenance,
        evidence: &ReadEvidence,
    ) {
        if !evidence.observation_is_current() {
            return;
        }
        let observation = evidence.observation();
        let range = match observation.coverage() {
            ReadCoverage::Full => IndexedRange::Full,
            ReadCoverage::Partial => {
                let Some(end) = observation.window().end_line else {
                    return;
                };
                let Some(range) = LineRange::new(observation.window().start_line, end) else {
                    return;
                };
                IndexedRange::Lines(range)
            }
        };
        let path = self
            .paths
            .entry(observation.canonical_path().to_path_buf())
            .or_insert_with(|| IndexedPath {
                display_path: observation.display_path().to_string(),
                observations: Vec::new(),
            });
        path.observations.push(IndexedObservation {
            target_id: target_id.into(),
            provenance,
            range,
            cited_in_result: false,
        });
    }

    pub(crate) fn insert_delegated(&mut self, delegated: &DelegatedReadEvidence) {
        let evidence = &delegated.evidence;
        if !evidence.observation_is_current() {
            return;
        }
        let observation = evidence.observation();
        let range = match observation.coverage() {
            ReadCoverage::Full => IndexedRange::Full,
            ReadCoverage::Partial => {
                let Some(end) = observation.window().end_line else {
                    return;
                };
                let Some(range) = LineRange::new(observation.window().start_line, end) else {
                    return;
                };
                IndexedRange::Lines(range)
            }
        };
        let path = self
            .paths
            .entry(observation.canonical_path().to_path_buf())
            .or_insert_with(|| IndexedPath {
                display_path: observation.display_path().to_string(),
                observations: Vec::new(),
            });
        path.observations.push(IndexedObservation {
            target_id: delegated
                .launch_group_id
                .as_ref()
                .map(|group| format!("{group}/{}", delegated.subtask_id))
                .unwrap_or_else(|| delegated.subtask_id.clone()),
            provenance: ReadProvenance::DelegatedObserved,
            range,
            cited_in_result: delegated.cited_in_result,
        });
    }

    #[cfg(test)]
    pub(crate) fn covers(&self, canonical_path: &Path, requested: LineRange) -> bool {
        self.paths.get(canonical_path).is_some_and(|path| {
            path.observations
                .iter()
                .filter(|observation| visible_to_parent(observation.provenance))
                .any(|observation| observation.range.covers(requested))
                || merged_ranges(path, visible_to_parent)
                    .is_some_and(|ranges| ranges.iter().any(|range| range.covers(requested)))
        })
    }

    pub(crate) fn render_volatile_coverage(&self) -> String {
        const MAX_PATHS: usize = 12;
        const MAX_RANGES_PER_PATH: usize = 4;
        const MAX_CHARS: usize = 2_400;

        let mut lines = Vec::new();
        let mut rendered_paths = 0usize;
        let mut omitted_paths = 0usize;
        let mut current_lines = vec!["### Current read coverage".to_string()];
        for path in self.paths.values() {
            let coverage = if path.observations.iter().any(|observation| {
                visible_to_parent(observation.provenance)
                    && matches!(observation.range, IndexedRange::Full)
            }) {
                "full file".to_string()
            } else {
                let Some(ranges) = merged_ranges(path, visible_to_parent) else {
                    continue;
                };
                ranges
                    .iter()
                    .take(MAX_RANGES_PER_PATH)
                    .map(|range| format!("{}-{}", range.start, range.end))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            if rendered_paths >= MAX_PATHS {
                omitted_paths = omitted_paths.saturating_add(1);
                continue;
            }
            let line = format!("- {}: {coverage} (fresh, visible)", path.display_path);
            let projected_chars = current_lines
                .iter()
                .map(|line| line.chars().count().saturating_add(1))
                .sum::<usize>()
                .saturating_add(line.chars().count());
            if projected_chars > MAX_CHARS {
                omitted_paths = omitted_paths.saturating_add(1);
                continue;
            }
            current_lines.push(line);
            rendered_paths = rendered_paths.saturating_add(1);
        }
        if rendered_paths > 0 {
            if omitted_paths > 0 {
                current_lines.push(format!("- {omitted_paths} additional path(s) omitted"));
            }
            current_lines.push(
                "Covered visible ranges do not need another read unless a stale-files notice says otherwise."
                    .to_string(),
            );
            lines.extend(current_lines);
        }

        let mut delegated_lines = vec!["### Delegated read coverage".to_string()];
        let mut delegated_paths = 0usize;
        for path in self.paths.values() {
            let observations = path
                .observations
                .iter()
                .filter(|observation| observation.provenance == ReadProvenance::DelegatedObserved)
                .collect::<Vec<_>>();
            if observations.is_empty() || delegated_paths >= MAX_PATHS {
                continue;
            }
            let coverage = if observations
                .iter()
                .any(|observation| matches!(observation.range, IndexedRange::Full))
            {
                "full file".to_string()
            } else {
                let Some(ranges) = merged_ranges(path, |provenance| {
                    provenance == ReadProvenance::DelegatedObserved
                }) else {
                    continue;
                };
                ranges
                    .iter()
                    .take(MAX_RANGES_PER_PATH)
                    .map(|range| format!("{}-{}", range.start, range.end))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let mut subtasks = observations
                .iter()
                .map(|observation| observation.target_id.as_str())
                .collect::<Vec<_>>();
            subtasks.sort_unstable();
            subtasks.dedup();
            let citation = if observations
                .iter()
                .any(|observation| observation.cited_in_result)
            {
                "; cited in result"
            } else {
                ""
            };
            let line = format!(
                "- {}: {coverage} (observed by {}{citation})",
                path.display_path,
                subtasks.join(", ")
            );
            let projected_chars = lines
                .iter()
                .chain(delegated_lines.iter())
                .map(|line| line.chars().count().saturating_add(1))
                .sum::<usize>()
                .saturating_add(line.chars().count());
            if projected_chars > MAX_CHARS {
                break;
            }
            delegated_lines.push(line);
            delegated_paths = delegated_paths.saturating_add(1);
        }
        if delegated_paths > 0 {
            delegated_lines.push(
                "Delegated coverage is observational only: use a narrow read for cited or risky regions; it does not authorize parent edits."
                    .to_string(),
            );
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.extend(delegated_lines);
        }
        lines.join("\n")
    }
}

fn visible_to_parent(provenance: ReadProvenance) -> bool {
    matches!(
        provenance,
        ReadProvenance::ParentVisible | ReadProvenance::MentionVisible
    )
}

fn merged_ranges(
    path: &IndexedPath,
    include: impl Fn(ReadProvenance) -> bool,
) -> Option<Vec<LineRange>> {
    let mut ranges = path
        .observations
        .iter()
        .filter(|observation| include(observation.provenance))
        .filter_map(|observation| match observation.range {
            IndexedRange::Full => None,
            IndexedRange::Lines(range) => Some(range),
        })
        .collect::<Vec<_>>();
    if ranges.is_empty() {
        return None;
    }
    ranges.sort_unstable_by_key(|range| range.start);
    let mut merged = Vec::<LineRange>::with_capacity(ranges.len());
    for range in ranges {
        if let Some(previous) = merged.last_mut()
            && range.start <= previous.end.saturating_add(1)
        {
            previous.end = previous.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    Some(merged)
}

pub(crate) fn digest_content(content: &[u8]) -> blake3::Hash {
    blake3::hash(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "bonsai_read_evidence_{label}_{}_{nanos}.txt",
            std::process::id()
        ))
    }

    fn window() -> ReadWindow {
        ReadWindow {
            requested_offset: 1,
            requested_limit: 2000,
            start_line: 1,
            end_line: Some(1),
            total_lines: Some(1),
        }
    }

    fn partial_evidence(
        display_path: &str,
        canonical_path: &Path,
        start_line: usize,
        end_line: usize,
    ) -> ReadEvidence {
        ReadEvidence::new(
            display_path,
            canonical_path.to_path_buf(),
            ReadWindow {
                requested_offset: start_line,
                requested_limit: end_line.saturating_sub(start_line).saturating_add(1),
                start_line,
                end_line: Some(end_line),
                total_lines: Some(100),
            },
            ReadCoverage::Partial,
            &format!("{start_line}: first\n{end_line}: last\n"),
            None,
            0,
            None,
        )
    }

    #[test]
    fn any_non_fresh_state_requires_a_marker() {
        assert!(!ReadFreshness::Fresh.requires_marker());
        assert!(ReadFreshness::Stale.requires_marker());
        assert!(ReadFreshness::Deleted.requires_marker());
        assert!(ReadFreshness::Unknown.requires_marker());
    }

    #[test]
    fn evidence_index_merges_adjacent_ranges_across_path_aliases() {
        let canonical = PathBuf::from("/tmp/project/src/lib.rs");
        let first = partial_evidence("src/lib.rs", &canonical, 1, 10);
        let second = partial_evidence("./src/lib.rs", &canonical, 11, 20);
        let mut index = ReadEvidenceIndex::default();

        index.insert("call-1", ReadProvenance::ParentVisible, &first);
        index.insert("msg-2", ReadProvenance::MentionVisible, &second);

        assert!(index.covers(&canonical, LineRange::new(1, 20).expect("valid range")));
        assert!(!index.covers(&canonical, LineRange::new(1, 21).expect("valid range")));
        let rendered = index.render_volatile_coverage();
        assert!(rendered.contains("- src/lib.rs: 1-20 (fresh, visible)"));
        assert_eq!(rendered.matches("src/lib.rs").count(), 1);
    }

    #[test]
    fn delegated_coverage_renders_separately_and_never_counts_as_parent_visible() {
        let canonical = PathBuf::from("/tmp/project/src/delegated.rs");
        let evidence = partial_evidence("src/delegated.rs", &canonical, 20, 40);
        let mut index = ReadEvidenceIndex::default();
        index.insert_delegated(&DelegatedReadEvidence {
            subtask_id: "sub-2".to_string(),
            launch_group_id: Some("group-1".to_string()),
            source_id: "tool:read-1".to_string(),
            cited_in_result: true,
            evidence,
        });

        assert!(
            !index.covers(
                &canonical,
                LineRange::new(20, 40).expect("valid delegated range")
            ),
            "delegated observations must not satisfy parent-visible coverage"
        );
        let rendered = index.render_volatile_coverage();
        assert!(rendered.contains("### Delegated read coverage"));
        assert!(rendered.contains("src/delegated.rs: 20-40"));
        assert!(rendered.contains("observed by group-1/sub-2; cited in result"));
        assert!(rendered.contains("does not authorize parent edits"));
        assert!(!rendered.contains("fresh, visible"));
    }

    #[tokio::test]
    async fn unchanged_digested_read_is_fresh() {
        let path = temp_path("fresh");
        std::fs::write(&path, b"abc").unwrap();
        let canonical = path.canonicalize().unwrap();
        let metadata = std::fs::metadata(&canonical).unwrap();
        let mut evidence = ReadEvidence::new(
            "f.txt",
            canonical,
            window(),
            ReadCoverage::Full,
            "1: abc\n",
            metadata.modified().ok(),
            metadata.len(),
            Some(digest_content(b"abc")),
        );
        evidence.refresh_freshness().await;
        assert_eq!(evidence.freshness(), ReadFreshness::Fresh);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn same_length_edit_is_stale_regardless_of_mtime() {
        // The captured digest must catch a same-length in-place edit even if the
        // filesystem's mtime granularity wouldn't distinguish the two writes.
        let path = temp_path("samelen");
        std::fs::write(&path, b"aaaaa").unwrap();
        let canonical = path.canonicalize().unwrap();
        let metadata = std::fs::metadata(&canonical).unwrap();
        let mut evidence = ReadEvidence::new(
            "f.txt",
            canonical,
            window(),
            ReadCoverage::Full,
            "1: aaaaa\n",
            metadata.modified().ok(),
            metadata.len(),
            Some(digest_content(b"aaaaa")),
        );
        std::fs::write(&path, b"bbbbb").unwrap();
        evidence.refresh_freshness().await;
        assert_eq!(evidence.freshness(), ReadFreshness::Stale);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn agent_mutation_advances_freshness_without_changing_observation_identity() {
        let path = temp_path("rebaseline");
        std::fs::write(&path, b"aaaaa").unwrap();
        let canonical = path.canonicalize().unwrap();
        let metadata = std::fs::metadata(&canonical).unwrap();
        let mut evidence = ReadEvidence::new(
            "f.txt",
            canonical,
            window(),
            ReadCoverage::Full,
            "1: aaaaa\n",
            metadata.modified().ok(),
            metadata.len(),
            Some(digest_content(b"aaaaa")),
        );
        let visible_digest = evidence.visible_digest();
        let file_digest_at_read = evidence.observation().file_digest_at_read();

        // A same-length in-place edit makes the read stale.
        std::fs::write(&path, b"bbbbb").unwrap();
        evidence.refresh_freshness().await;
        assert_eq!(evidence.freshness(), ReadFreshness::Stale);

        evidence.refresh_baseline_after_agent_mutation().await;
        assert_eq!(evidence.freshness(), ReadFreshness::Fresh);
        assert!(!evidence.observation_is_current());
        let mut index = ReadEvidenceIndex::default();
        index.insert("call-1", ReadProvenance::ParentVisible, &evidence);
        assert!(
            index.render_volatile_coverage().is_empty(),
            "a pre-edit observation must not advertise post-edit coverage"
        );
        assert_eq!(
            evidence.freshness_baseline().current_file_digest(),
            Some(digest_content(b"bbbbb"))
        );
        assert_eq!(evidence.visible_digest(), visible_digest);
        assert_eq!(
            evidence.observation().file_digest_at_read(),
            file_digest_at_read
        );
        evidence.refresh_freshness().await;
        assert_eq!(evidence.freshness(), ReadFreshness::Fresh);

        // A subsequent real change is still detected (re-baseline didn't blind it).
        std::fs::write(&path, b"ccccc").unwrap();
        evidence.refresh_freshness().await;
        assert_eq!(evidence.freshness(), ReadFreshness::Stale);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn mutation_baseline_refresh_on_deleted_file_is_a_noop() {
        let path = temp_path("rebaseline_gone");
        std::fs::write(&path, b"abc").unwrap();
        let canonical = path.canonicalize().unwrap();
        let metadata = std::fs::metadata(&canonical).unwrap();
        let mut evidence = ReadEvidence::new(
            "f.txt",
            canonical,
            window(),
            ReadCoverage::Full,
            "1: abc\n",
            metadata.modified().ok(),
            metadata.len(),
            Some(digest_content(b"abc")),
        );
        std::fs::remove_file(&path).unwrap();
        // Best-effort: a missing file leaves the row untouched for refresh to judge.
        evidence.refresh_baseline_after_agent_mutation().await;
        assert_eq!(
            evidence.freshness_baseline().current_file_digest(),
            Some(digest_content(b"abc"))
        );
        evidence.refresh_freshness().await;
        assert_eq!(evidence.freshness(), ReadFreshness::Deleted);
    }

    #[tokio::test]
    async fn digestless_read_with_advanced_mtime_is_stale() {
        let path = temp_path("digestless");
        std::fs::write(&path, b"abc").unwrap();
        let canonical = path.canonicalize().unwrap();
        let len = std::fs::metadata(&canonical).unwrap().len();
        let mut evidence = ReadEvidence::new(
            "f.txt",
            canonical,
            window(),
            ReadCoverage::Partial,
            "1: abc\n",
            // An mtime that won't match the real file, with no captured digest
            // (a large-file window read): conservatively Stale.
            Some(SystemTime::UNIX_EPOCH),
            len,
            None,
        );
        evidence.refresh_freshness().await;
        assert_eq!(evidence.freshness(), ReadFreshness::Stale);
        let _ = std::fs::remove_file(&path);
    }
}
