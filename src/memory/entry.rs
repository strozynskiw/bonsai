//! One memory fact on disk: a `*.md` file with typed YAML frontmatter and a
//! markdown body. The file is the source of truth; parsing and rendering are
//! round-trip stable (`parse(render(entry)) == entry`) because content-hash
//! dedup and git diffs both depend on byte-identical re-emission.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::resource::frontmatter::{Parsed, parse_frontmatter};

/// Cap on a memory body folded into recall notes. Tighter than
/// [`crate::resource::MAX_RESOURCE_BODY_BYTES`]: one fact per file should stay
/// small, and recalled entries ride a ~500-token budget.
pub(crate) const MAX_MEMORY_BODY_BYTES: usize = 4 * 1024;
/// Cap on the one-line `description` frontmatter field.
pub(crate) const MAX_DESCRIPTION_CHARS: usize = 160;
/// Cap on the `name` slug (`[a-z0-9-]{1,64}`).
pub(crate) const MAX_NAME_CHARS: usize = 64;

/// Which store an entry lives in. Implied by directory, never serialized.
/// `User` sorts before `Project` (index and store snapshots order on it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum MemoryTier {
    /// `$BONSAI_HOME/memory/` — follows the user across projects.
    User,
    /// `<project>/.bonsai/memory/` — git-tracked, ships with the repo.
    Project,
}

impl MemoryTier {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }

    /// Inverse of [`Self::label`], for rows read back from the DB shadow.
    pub(crate) fn from_label(label: &str) -> Option<Self> {
        match label {
            "user" => Some(Self::User),
            "project" => Some(Self::Project),
            _ => None,
        }
    }
}

/// Typed entry kinds so recall can rank by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum MemoryEntryType {
    /// Who the user is (role, expertise).
    User,
    /// A standing user preference or correction.
    Preference,
    /// A project goal, constraint, or decision not derivable from the repo.
    Project,
    /// A pointer to an external resource (dashboard, ticket, doc).
    Reference,
}

impl MemoryEntryType {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Preference => "preference",
            Self::Project => "project",
            Self::Reference => "reference",
        }
    }
}

/// The typed frontmatter block of a memory file. `created`/`updated` are plain
/// `YYYY-MM-DD` strings (no calendar dependency); hand-authored files may omit
/// them.
#[derive(Debug, Deserialize)]
struct MemoryFrontmatter {
    name: String,
    description: String,
    #[serde(rename = "type")]
    entry_type: MemoryEntryType,
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    updated: Option<String>,
}

const fn default_enabled() -> bool {
    true
}

/// A loaded memory fact: typed frontmatter fields plus body and provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemoryEntry {
    pub tier: MemoryTier,
    /// Kebab-case slug; always equals the filename stem.
    pub name: String,
    /// One line, <= [`MAX_DESCRIPTION_CHARS`] chars.
    pub description: String,
    pub entry_type: MemoryEntryType,
    /// Whether the entry participates in recall and the diagnostics index.
    pub enabled: bool,
    /// `YYYY-MM-DD` (UTC); empty when a hand-authored file omitted it.
    pub created: String,
    pub updated: String,
    pub body: String,
    /// blake3 hex of the full file bytes — the DB shadow sync key.
    pub content_hash: String,
    pub path: PathBuf,
}

/// Whether `name` is a valid entry slug: `[a-z0-9-]{1,64}`, no leading or
/// trailing dash.
pub(crate) fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_CHARS
        && name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// Flatten free text into a single index-safe description line, capped on a
/// char boundary.
pub(crate) fn normalize_description(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX_DESCRIPTION_CHARS {
        return flat;
    }
    let mut capped: String = flat.chars().take(MAX_DESCRIPTION_CHARS - 1).collect();
    capped.push('…');
    capped
}

/// Normalize a body for storage: trim outer whitespace, cap to
/// [`MAX_MEMORY_BODY_BYTES`] on a char boundary, ensure one trailing newline so
/// rendered files end like every other text file in a repo.
pub(crate) fn normalize_body(body: &str) -> String {
    let trimmed = body.trim();
    let mut end = trimmed.len().min(MAX_MEMORY_BODY_BYTES);
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n", &trimmed[..end])
}

/// Parse one memory file. The filename stem is authoritative for `name`; a
/// mismatching frontmatter `name` resolves in favor of the filename (warn).
pub(crate) fn parse_memory_entry(
    tier: MemoryTier,
    path: &Path,
    content: &str,
) -> anyhow::Result<MemoryEntry> {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .with_context(|| format!("non-UTF-8 memory filename: {}", path.display()))?
        .to_string();
    if !is_valid_name(&stem) {
        bail!("memory filename is not a valid slug: {stem:?}");
    }

    let Parsed { frontmatter, body } = parse_frontmatter::<MemoryFrontmatter>(content)?;
    let MemoryFrontmatter {
        name,
        description,
        entry_type,
        enabled,
        created,
        updated,
    } = frontmatter;
    if name != stem {
        tracing::warn!(
            path = %path.display(),
            frontmatter_name = %name,
            "memory entry name mismatches filename; using the filename"
        );
    }

    Ok(MemoryEntry {
        tier,
        name: stem,
        description: normalize_description(&description),
        entry_type,
        enabled,
        created: created.unwrap_or_default(),
        updated: updated.unwrap_or_default(),
        body: normalize_body(&body),
        content_hash: blake3::hash(content.as_bytes()).to_hex().to_string(),
        path: path.to_path_buf(),
    })
}

/// Render an entry back to file bytes in fixed field order. Round-trip
/// identity with [`parse_memory_entry`] is load-bearing: the store re-hashes
/// rendered bytes for the DB shadow, and stable emission keeps git diffs
/// minimal.
pub(crate) fn render_memory_file(entry: &MemoryEntry) -> String {
    format!(
        "---\nname: {}\ndescription: {}\ntype: {}\nenabled: {}\ncreated: {}\nupdated: {}\n---\n{}",
        entry.name,
        yaml_quote(&entry.description),
        entry.entry_type.label(),
        entry.enabled,
        entry.created,
        entry.updated,
        entry.body,
    )
}

/// Quote a YAML scalar defensively so free-text descriptions (colons, quotes,
/// leading specials) always parse back to the same string. Local copy of the
/// agent-composer helper — the shape is 4 lines and memory must not depend on
/// TUI internals.
fn yaml_quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> MemoryEntry {
        MemoryEntry {
            tier: MemoryTier::User,
            name: "prefers-no-verify".to_string(),
            description: "Use --no-verify when committing a subset: it skips hooks".to_string(),
            entry_type: MemoryEntryType::Preference,
            enabled: true,
            created: "2026-07-01".to_string(),
            updated: "2026-07-03".to_string(),
            body: "Pass `--no-verify` on partial commits.\n".to_string(),
            content_hash: String::new(),
            path: PathBuf::from("/home/memory/prefers-no-verify.md"),
        }
    }

    #[test]
    fn render_parse_round_trip_is_identity() {
        let entry = sample();
        let rendered = render_memory_file(&entry);
        let parsed =
            parse_memory_entry(MemoryTier::User, &entry.path, &rendered).expect("parses back");
        assert_eq!(
            parsed.content_hash,
            blake3::hash(rendered.as_bytes()).to_hex().to_string()
        );
        let parsed = MemoryEntry {
            content_hash: String::new(),
            ..parsed
        };
        assert_eq!(parsed, entry);
        // Emission is stable: rendering the parsed entry is byte-identical.
        assert_eq!(render_memory_file(&parsed), rendered);
    }

    #[test]
    fn filename_wins_over_frontmatter_name() {
        let content = "---\nname: other-name\ndescription: d\ntype: project\n---\nbody\n";
        let entry = parse_memory_entry(
            MemoryTier::Project,
            Path::new("/repo/.bonsai/memory/actual-name.md"),
            content,
        )
        .unwrap();
        assert_eq!(entry.name, "actual-name");
    }

    #[test]
    fn invalid_filename_slug_is_rejected() {
        let content = "---\nname: a\ndescription: d\ntype: user\n---\nbody";
        for bad in ["Bad Name.md", "-leading.md", "trailing-.md", "über.md"] {
            let path = PathBuf::from("/m").join(bad);
            assert!(
                parse_memory_entry(MemoryTier::User, &path, content).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn missing_dates_default_to_empty() {
        let content = "---\nname: a\ndescription: d\ntype: reference\n---\nbody\n";
        let entry = parse_memory_entry(MemoryTier::User, Path::new("/m/a.md"), content).unwrap();
        assert_eq!(entry.created, "");
        assert_eq!(entry.updated, "");
        assert!(entry.enabled);
    }

    #[test]
    fn enabled_round_trips_through_frontmatter() {
        let entry = MemoryEntry {
            enabled: false,
            ..sample()
        };
        let rendered = render_memory_file(&entry);
        assert!(rendered.contains("enabled: false"));
        let parsed = parse_memory_entry(MemoryTier::User, &entry.path, &rendered).unwrap();
        assert!(!parsed.enabled);
    }

    #[test]
    fn body_is_capped_on_load() {
        let long = "x".repeat(MAX_MEMORY_BODY_BYTES + 100);
        let content = format!("---\nname: a\ndescription: d\ntype: user\n---\n{long}");
        let entry = parse_memory_entry(MemoryTier::User, Path::new("/m/a.md"), &content).unwrap();
        assert!(
            entry.body.len() <= MAX_MEMORY_BODY_BYTES + 1,
            "cap plus trailing newline"
        );
    }

    #[test]
    fn description_is_flattened_and_capped() {
        assert_eq!(
            normalize_description("  a\nmulti\r\nline  desc "),
            "a multi line desc"
        );
        let long = "word ".repeat(100);
        let capped = normalize_description(&long);
        assert!(capped.chars().count() <= MAX_DESCRIPTION_CHARS);
        assert!(capped.ends_with('…'));
    }

    #[test]
    fn description_with_yaml_specials_round_trips() {
        let entry = MemoryEntry {
            description: "Deploy: use \"blue\\green\" cutover".to_string(),
            ..sample()
        };
        let rendered = render_memory_file(&entry);
        let parsed = parse_memory_entry(MemoryTier::User, &entry.path, &rendered).unwrap();
        assert_eq!(parsed.description, entry.description);
    }

    #[test]
    fn name_slug_rules() {
        assert!(is_valid_name("a"));
        assert!(is_valid_name("two-words-2"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("Upper"));
        assert!(!is_valid_name("under_score"));
        assert!(!is_valid_name("-lead"));
        assert!(!is_valid_name("trail-"));
        assert!(!is_valid_name(&"a".repeat(MAX_NAME_CHARS + 1)));
    }
}
