//! Two-tier filesystem store for memory entries. Fixed paths — exactly
//! `$BONSAI_HOME/memory/` (user tier) and `<project>/.bonsai/memory/` (project
//! tier) — deliberately not `ResourceKind` discovery. Files are the source
//! of truth; the in-memory snapshot is a cache refreshed on load/reload and
//! updated synchronously by [`MemoryStore::write`]/[`MemoryStore::delete`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, bail};

use crate::memory::entry::{
    MemoryEntry, MemoryEntryType, MemoryTier, is_valid_name, normalize_body, normalize_description,
    parse_memory_entry, render_memory_file,
};
use crate::util::slug::slugify;
use crate::util::time::{now_ms, utc_date_string};

/// How many `-2`-style suffixes to try before giving up on a create collision.
const MAX_NAME_SUFFIX: usize = 100;
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The two-tier store. `entries` is kept sorted `(tier, name)` so every
/// consumer (index, list, retrieval) sees a deterministic order.
pub(crate) struct MemoryStore {
    user_dir: PathBuf,
    project_dir: PathBuf,
    entries: std::sync::RwLock<Vec<MemoryEntry>>,
    mutation_gate: std::sync::Mutex<()>,
}

/// Result of a [`MemoryStore::write`]: the stored entry, the previous file
/// content when it replaced one (for diff rendering), and whether it created a
/// new file.
#[derive(Debug)]
pub(crate) struct WrittenEntry {
    pub entry: MemoryEntry,
    pub previous: Option<String>,
    pub created: bool,
}

impl MemoryStore {
    /// Load both tiers. Never fails: missing directories are empty tiers and
    /// malformed files are skipped with a warning, so a broken entry can't take
    /// down startup. An empty `home_dir` (unresolvable home) disables the user
    /// tier the same way `resource::discovery` tolerates it.
    pub(crate) fn load(home_dir: &Path, project_root: &Path) -> Self {
        let user_dir = if home_dir.as_os_str().is_empty() {
            PathBuf::new()
        } else {
            home_dir.join("memory")
        };
        let store = Self {
            user_dir,
            project_dir: project_root.join(".bonsai/memory"),
            entries: std::sync::RwLock::new(Vec::new()),
            mutation_gate: std::sync::Mutex::new(()),
        };
        store.reload();
        store
    }

    /// Re-scan both tiers, replacing the in-memory snapshot.
    pub(crate) fn reload(&self) {
        let _mutation = self.mutation_gate.lock().expect("memory mutation lock");
        self.reload_inner();
    }

    fn reload_inner(&self) {
        let mut entries = Vec::new();
        for (tier, dir) in [
            (MemoryTier::User, &self.user_dir),
            (MemoryTier::Project, &self.project_dir),
        ] {
            load_dir(tier, dir, &mut entries);
        }
        entries.sort_by(|a, b| (a.tier, &a.name).cmp(&(b.tier, &b.name)));
        *self.entries.write().expect("memory store lock") = entries;
    }

    /// Snapshot of all entries, sorted `(tier, name)`.
    pub(crate) fn entries(&self) -> Vec<MemoryEntry> {
        self.entries.read().expect("memory store lock").clone()
    }

    /// Snapshot of only enabled entries, sorted `(tier, name)`.
    pub(crate) fn enabled_entries(&self) -> Vec<MemoryEntry> {
        self.entries
            .read()
            .expect("memory store lock")
            .iter()
            .filter(|entry| entry.enabled)
            .cloned()
            .collect()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries
            .read()
            .expect("memory store lock")
            .iter()
            .all(|entry| !entry.enabled)
    }

    /// Look up an entry by name. The project tier wins on a cross-tier clash
    /// (the more specific store overrides the roaming one).
    pub(crate) fn get(&self, name: &str) -> Option<MemoryEntry> {
        let entries = self.entries.read().expect("memory store lock");
        entries
            .iter()
            .filter(|entry| entry.name == name)
            .max_by_key(|entry| entry.tier)
            .cloned()
    }

    /// Look up an entry by exact `(tier, name)` identity.
    pub(crate) fn get_exact(&self, tier: MemoryTier, name: &str) -> Option<MemoryEntry> {
        self.entries
            .read()
            .expect("memory store lock")
            .iter()
            .find(|entry| entry.tier == tier && entry.name == name)
            .cloned()
    }

    /// The on-disk directory for a tier (for user-facing path reporting).
    pub(crate) fn dir(&self, tier: MemoryTier) -> &Path {
        match tier {
            MemoryTier::User => &self.user_dir,
            MemoryTier::Project => &self.project_dir,
        }
    }

    /// Create or update an entry.
    ///
    /// With `name: Some(..)` an existing same-tier entry of that name is
    /// updated in place (preserving `created`, bumping `updated`); otherwise a
    /// file of that name is created. With `name: None` the slug derives from
    /// the description, and a collision picks the first free `-2`-style suffix
    /// instead of silently overwriting a different fact.
    pub(crate) fn write(
        &self,
        tier: MemoryTier,
        entry_type: MemoryEntryType,
        name: Option<&str>,
        description: &str,
        body: &str,
        enabled: Option<bool>,
    ) -> anyhow::Result<WrittenEntry> {
        let _mutation = self.mutation_gate.lock().expect("memory mutation lock");
        if tier == MemoryTier::User && self.user_dir.as_os_str().is_empty() {
            bail!("user-tier memory is unavailable: no bonsai home directory");
        }
        let description = normalize_description(description);
        if description.is_empty() {
            bail!("memory entries need a non-empty description");
        }

        let name = match name {
            Some(name) => {
                if !is_valid_name(name) {
                    bail!("not a valid memory name (kebab-case, max 64 chars): {name:?}");
                }
                name.to_string()
            }
            None => self.free_name(tier, &slugify(&description))?,
        };

        let existing = self
            .entries
            .read()
            .expect("memory store lock")
            .iter()
            .find(|entry| entry.tier == tier && entry.name == name)
            .cloned();

        let today = utc_date_string(now_ms());
        let enabled =
            enabled.unwrap_or_else(|| existing.as_ref().map(|entry| entry.enabled).unwrap_or(true));
        let entry = MemoryEntry {
            tier,
            name: name.clone(),
            description,
            entry_type,
            enabled,
            created: existing
                .as_ref()
                .map(|entry| entry.created.clone())
                .filter(|created| !created.is_empty())
                .unwrap_or_else(|| today.clone()),
            updated: today,
            body: normalize_body(body),
            content_hash: String::new(),
            path: self.dir(tier).join(format!("{name}.md")),
        };
        let rendered = render_memory_file(&entry);
        let entry = MemoryEntry {
            content_hash: blake3::hash(rendered.as_bytes()).to_hex().to_string(),
            ..entry
        };

        let previous = existing.map(|entry| render_memory_file(&entry));
        write_atomically(&entry.path, &rendered)?;

        let mut entries = self.entries.write().expect("memory store lock");
        entries.retain(|other| !(other.tier == tier && other.name == name));
        entries.push(entry.clone());
        entries.sort_by(|a, b| (a.tier, &a.name).cmp(&(b.tier, &b.name)));

        Ok(WrittenEntry {
            entry,
            created: previous.is_none(),
            previous,
        })
    }

    /// Create an entry without replacing any file chosen concurrently by
    /// another process. Name collisions advance to the next numeric suffix.
    pub(crate) fn create(
        &self,
        tier: MemoryTier,
        entry_type: MemoryEntryType,
        description: &str,
        body: &str,
        enabled: Option<bool>,
    ) -> anyhow::Result<WrittenEntry> {
        let _mutation = self.mutation_gate.lock().expect("memory mutation lock");
        let description = normalize_description(description);
        if description.is_empty() {
            bail!("memory entries need a non-empty description");
        }
        let slug = slugify(&description);
        if !is_valid_name(&slug) {
            bail!("description does not reduce to a usable name: {slug:?}");
        }
        for suffix in 1..=MAX_NAME_SUFFIX {
            let name = if suffix == 1 {
                slug.clone()
            } else {
                format!("{slug}-{suffix}")
            };
            if name.len() > crate::memory::entry::MAX_NAME_CHARS {
                continue;
            }
            if let Some(written) = self.create_exact_inner(
                tier,
                entry_type,
                &name,
                &description,
                body,
                enabled.unwrap_or(true),
            )? {
                return Ok(written);
            }
        }
        bail!("too many memory entries named like {slug:?}")
    }

    /// Create one exact name, refusing to replace an existing destination.
    pub(crate) fn create_exact(
        &self,
        tier: MemoryTier,
        entry_type: MemoryEntryType,
        name: &str,
        description: &str,
        body: &str,
        enabled: bool,
    ) -> anyhow::Result<WrittenEntry> {
        let _mutation = self.mutation_gate.lock().expect("memory mutation lock");
        if !is_valid_name(name) {
            bail!("not a valid memory name (kebab-case, max 64 chars): {name:?}");
        }
        let description = normalize_description(description);
        if description.is_empty() {
            bail!("memory entries need a non-empty description");
        }
        self.create_exact_inner(tier, entry_type, name, &description, body, enabled)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "a {} memory entry named {name:?} already exists",
                    tier.label()
                )
            })
    }

    fn create_exact_inner(
        &self,
        tier: MemoryTier,
        entry_type: MemoryEntryType,
        name: &str,
        description: &str,
        body: &str,
        enabled: bool,
    ) -> anyhow::Result<Option<WrittenEntry>> {
        if tier == MemoryTier::User && self.user_dir.as_os_str().is_empty() {
            bail!("user-tier memory is unavailable: no bonsai home directory");
        }
        let today = utc_date_string(now_ms());
        let entry = MemoryEntry {
            tier,
            name: name.to_string(),
            description: description.to_string(),
            entry_type,
            enabled,
            created: today.clone(),
            updated: today,
            body: normalize_body(body),
            content_hash: String::new(),
            path: self.dir(tier).join(format!("{name}.md")),
        };
        let rendered = render_memory_file(&entry);
        let entry = MemoryEntry {
            content_hash: blake3::hash(rendered.as_bytes()).to_hex().to_string(),
            ..entry
        };
        if !write_new_atomically(&entry.path, &rendered)? {
            return Ok(None);
        }
        let mut entries = self.entries.write().expect("memory store lock");
        entries.retain(|other| !(other.tier == tier && other.name == name));
        entries.push(entry.clone());
        entries.sort_by(|a, b| (a.tier, &a.name).cmp(&(b.tier, &b.name)));
        Ok(Some(WrittenEntry {
            entry,
            previous: None,
            created: true,
        }))
    }

    /// Delete an entry by name (project tier wins on a clash, mirroring
    /// [`Self::get`]). Returns the removed file's path.
    pub(crate) fn delete(&self, name: &str) -> anyhow::Result<PathBuf> {
        let _mutation = self.mutation_gate.lock().expect("memory mutation lock");
        let Some(entry) = self.get(name) else {
            bail!("no memory entry named {name:?}");
        };
        std::fs::remove_file(&entry.path)
            .with_context(|| format!("deleting {}", entry.path.display()))?;
        self.entries
            .write()
            .expect("memory store lock")
            .retain(|other| !(other.tier == entry.tier && other.name == entry.name));
        Ok(entry.path)
    }

    /// Delete an entry by exact `(tier, name)` identity.
    pub(crate) fn delete_exact(&self, tier: MemoryTier, name: &str) -> anyhow::Result<PathBuf> {
        let _mutation = self.mutation_gate.lock().expect("memory mutation lock");
        let Some(entry) = self.get_exact(tier, name) else {
            bail!("no memory entry named {name:?} in {tier:?} tier");
        };
        std::fs::remove_file(&entry.path)
            .with_context(|| format!("deleting {}", entry.path.display()))?;
        self.entries
            .write()
            .expect("memory store lock")
            .retain(|other| !(other.tier == tier && other.name == name));
        Ok(entry.path)
    }

    /// First free name for a create: the slug itself, else `slug-2`, `slug-3`, …
    fn free_name(&self, tier: MemoryTier, slug: &str) -> anyhow::Result<String> {
        if !is_valid_name(slug) {
            bail!("description does not reduce to a usable name: {slug:?}");
        }
        let entries = self.entries.read().expect("memory store lock");
        let taken = |name: &str| {
            entries
                .iter()
                .any(|entry| entry.tier == tier && entry.name == name)
        };
        if !taken(slug) {
            return Ok(slug.to_string());
        }
        for suffix in 2..=MAX_NAME_SUFFIX {
            let candidate = format!("{slug}-{suffix}");
            if candidate.len() <= crate::memory::entry::MAX_NAME_CHARS && !taken(&candidate) {
                return Ok(candidate);
            }
        }
        bail!("too many memory entries named like {slug:?}");
    }
}

fn load_dir(tier: MemoryTier, dir: &Path, entries: &mut Vec<MemoryEntry>) {
    if dir.as_os_str().is_empty() {
        return;
    }
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return; // Missing dir == empty tier; created lazily on first write.
    };
    for dir_entry in read_dir.flatten() {
        let path = dir_entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "skipping unreadable memory file");
                continue;
            }
        };
        match parse_memory_entry(tier, &path, &content) {
            Ok(entry) => entries.push(entry),
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "skipping malformed memory file");
            }
        }
    }
}

/// Write via a same-directory temp file + `rename` so a concurrent bonsai
/// instance never observes a half-written entry. The temp name carries the pid
/// so two instances can't clobber each other's staging file.
fn write_atomically(path: &Path, content: &str) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("memory path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("non-UTF-8 memory path: {}", path.display()))?;
    let tmp = dir.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming into place: {}", path.display()))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
}

/// Publish a complete new file atomically, returning false when another
/// process already created the destination.
fn write_new_atomically(path: &Path, content: &str) -> anyhow::Result<bool> {
    let dir = path
        .parent()
        .with_context(|| format!("memory path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("non-UTF-8 memory path: {}", path.display()))?;
    let tmp = dir.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
    let linked = match std::fs::hard_link(&tmp, path) {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(err).with_context(|| format!("creating {}", path.display()));
        }
    };
    let _ = std::fs::remove_file(&tmp);
    Ok(linked)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stores() -> (tempfile::TempDir, tempfile::TempDir, MemoryStore) {
        let home = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        let store = MemoryStore::load(home.path(), project.path());
        (home, project, store)
    }

    #[test]
    fn write_creates_file_and_snapshot_entry() {
        let (_home, _project, store) = stores();
        let written = store
            .write(
                MemoryTier::User,
                MemoryEntryType::Preference,
                None,
                "Use --no-verify for partial commits",
                "Hook fails on the unstaged remainder.",
                None,
            )
            .unwrap();
        assert!(written.created);
        assert!(written.previous.is_none());
        assert_eq!(written.entry.name, "use-no-verify-for-partial-commits");
        assert!(written.entry.path.is_file());
        assert_eq!(store.entries().len(), 1);
        assert!(store.get(&written.entry.name).is_some());

        // No staging files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(written.entry.path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().extension().and_then(|e| e.to_str()) == Some("tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn update_preserves_created_and_bumps_updated() {
        let (_home, project, store) = stores();
        let dir = project.path().join(".bonsai/memory");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("style.md"),
            "---\nname: style\ndescription: \"old\"\ntype: project\ncreated: 2020-01-02\nupdated: 2020-01-02\n---\nold body\n",
        )
        .unwrap();
        store.reload();

        let written = store
            .write(
                MemoryTier::Project,
                MemoryEntryType::Project,
                Some("style"),
                "new description",
                "new body",
                None,
            )
            .unwrap();
        assert!(!written.created);
        assert_eq!(written.entry.created, "2020-01-02");
        assert_eq!(written.entry.updated, utc_date_string(now_ms()));
        assert!(written.previous.unwrap().contains("old body"));
        assert_eq!(store.entries().len(), 1);
        let on_disk = std::fs::read_to_string(&written.entry.path).unwrap();
        assert!(on_disk.contains("new body"));
        assert!(on_disk.contains("created: 2020-01-02"));
    }

    #[test]
    fn create_collision_gets_numeric_suffix() {
        let (_home, _project, store) = stores();
        let first = store
            .create(
                MemoryTier::User,
                MemoryEntryType::User,
                "Same fact",
                "a",
                None,
            )
            .unwrap();
        let second = store
            .create(
                MemoryTier::User,
                MemoryEntryType::User,
                "Same fact",
                "b",
                None,
            )
            .unwrap();
        assert_eq!(first.entry.name, "same-fact");
        assert_eq!(second.entry.name, "same-fact-2");
        assert!(second.created);
    }

    #[test]
    fn concurrent_creates_with_same_description_do_not_clobber() {
        let (home, project, _) = stores();
        let first_store = MemoryStore::load(home.path(), project.path());
        let second_store = MemoryStore::load(home.path(), project.path());
        let first = std::thread::spawn(move || {
            first_store.create(
                MemoryTier::Project,
                MemoryEntryType::Project,
                "Concurrent fact",
                "first",
                None,
            )
        });
        let second = std::thread::spawn(move || {
            second_store.create(
                MemoryTier::Project,
                MemoryEntryType::Project,
                "Concurrent fact",
                "second",
                None,
            )
        });

        let first = first.join().unwrap().unwrap();
        let second = second.join().unwrap().unwrap();
        assert_ne!(first.entry.name, second.entry.name);
        assert!(first.entry.path.is_file());
        assert!(second.entry.path.is_file());
    }

    #[test]
    fn exact_create_refuses_to_replace_existing_file() {
        let (_home, _project, store) = stores();
        let first = store
            .create_exact(
                MemoryTier::Project,
                MemoryEntryType::Project,
                "shared-name",
                "first",
                "first body",
                true,
            )
            .unwrap();

        let err = store
            .create_exact(
                MemoryTier::Project,
                MemoryEntryType::Project,
                "shared-name",
                "second",
                "second body",
                true,
            )
            .unwrap_err();

        assert!(err.to_string().contains("already exists"));
        assert!(
            std::fs::read_to_string(first.entry.path)
                .unwrap()
                .contains("first body")
        );
    }

    #[test]
    fn get_prefers_project_tier_on_clash() {
        let (_home, _project, store) = stores();
        store
            .write(
                MemoryTier::User,
                MemoryEntryType::User,
                Some("clash"),
                "user fact",
                "u",
                None,
            )
            .unwrap();
        store
            .write(
                MemoryTier::Project,
                MemoryEntryType::Project,
                Some("clash"),
                "project fact",
                "p",
                None,
            )
            .unwrap();
        assert_eq!(store.get("clash").unwrap().tier, MemoryTier::Project);
    }

    #[test]
    fn delete_removes_file_and_snapshot_entry() {
        let (_home, _project, store) = stores();
        let written = store
            .write(
                MemoryTier::User,
                MemoryEntryType::Reference,
                None,
                "Dashboard link",
                "url",
                None,
            )
            .unwrap();
        let path = store.delete(&written.entry.name).unwrap();
        assert_eq!(path, written.entry.path);
        assert!(!path.exists());
        assert!(store.is_empty());
        assert!(store.delete("gone").is_err());
    }

    #[test]
    fn reload_picks_up_external_edits_and_skips_malformed() {
        let (home, _project, store) = stores();
        let dir = home.path().join("memory");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("good.md"),
            "---\nname: good\ndescription: \"fine\"\ntype: user\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(dir.join("broken.md"), "no frontmatter here").unwrap();
        std::fs::write(dir.join("notes.txt"), "not markdown").unwrap();
        store.reload();
        let entries = store.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "good");
    }

    #[test]
    fn entries_sort_user_tier_before_project() {
        let (_home, _project, store) = stores();
        store
            .write(
                MemoryTier::Project,
                MemoryEntryType::Project,
                Some("bbb"),
                "b",
                "b",
                None,
            )
            .unwrap();
        store
            .write(
                MemoryTier::User,
                MemoryEntryType::User,
                Some("zzz"),
                "z",
                "z",
                None,
            )
            .unwrap();
        let names: Vec<_> = store
            .entries()
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(names, vec!["zzz".to_string(), "bbb".to_string()]);
    }

    #[test]
    fn empty_home_disables_user_tier_writes() {
        let project = tempfile::TempDir::new().unwrap();
        let store = MemoryStore::load(Path::new(""), project.path());
        assert!(
            store
                .write(
                    MemoryTier::User,
                    MemoryEntryType::User,
                    None,
                    "fact",
                    "f",
                    None
                )
                .is_err()
        );
        assert!(
            store
                .write(
                    MemoryTier::Project,
                    MemoryEntryType::Project,
                    None,
                    "fact",
                    "f",
                    None
                )
                .is_ok()
        );
    }
}
