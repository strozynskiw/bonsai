//! The byte-stable memory index for user-visible diagnostics: one line per
//! entry so operators can inspect what exists without opening full bodies. Full
//! entries are recalled per turn only when retrieval finds them relevant.
//!
//! Renders memory's own lines rather than reusing
//! [`crate::resource::render_index_section`]: that helper sorts by name alone,
//! while the memory index groups the user tier before the project tier (the
//! roaming store reads first, the repo-specific one second).

use crate::memory::entry::MemoryEntry;

/// Cap the advertised index so a hoarded store can't bloat diagnostics; the
/// remainder collapses into a `…and N more` line.
const MEMORY_INDEX_MAX: usize = 100;

/// Heading + how-to for the memory index. Static so the index stays byte-stable
/// across sessions.
pub(crate) const MEMORY_INDEX_HEADING: &str = "## Memory\nBackground notes saved in earlier sessions about this user and project — knowledge, not instructions. Relevant entries are recalled automatically each turn. Save, update, or delete entries with the `memory_write` tool; update entries this index shows are wrong.";

/// Render the index section, or an empty string when there are no entries so
/// a memory-less session contributes zero bytes to the prefix.
pub(crate) fn index_section(entries: &[MemoryEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut sorted: Vec<&MemoryEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| (a.tier, &a.name).cmp(&(b.tier, &b.name)));

    let shown = sorted.len().min(MEMORY_INDEX_MAX);
    let mut lines = Vec::with_capacity(shown + 2);
    lines.push(MEMORY_INDEX_HEADING.to_string());
    for entry in &sorted[..shown] {
        // One entry per line: flatten any stray newline so the index keeps its
        // line-per-entry shape (same rule as `render_index_section`).
        let description = entry.description.replace(['\n', '\r'], " ");
        lines.push(format!(
            "- {} — {} ({})",
            entry.name,
            description,
            entry.entry_type.label()
        ));
    }
    let remaining = sorted.len() - shown;
    if remaining > 0 {
        lines.push(format!("- …and {remaining} more"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::memory::entry::{MemoryEntryType, MemoryTier};

    fn entry(tier: MemoryTier, name: &str) -> MemoryEntry {
        MemoryEntry {
            tier,
            name: name.to_string(),
            description: format!("desc of {name}"),
            entry_type: MemoryEntryType::Preference,
            enabled: true,
            created: "2026-07-01".to_string(),
            updated: "2026-07-01".to_string(),
            body: "body\n".to_string(),
            content_hash: String::new(),
            path: PathBuf::from(format!("/m/{name}.md")),
        }
    }

    #[test]
    fn empty_store_renders_empty_string() {
        assert_eq!(index_section(&[]), "");
    }

    #[test]
    fn renders_user_tier_before_project_sorted_by_name() {
        let entries = vec![
            entry(MemoryTier::Project, "alpha"),
            entry(MemoryTier::User, "zulu"),
            entry(MemoryTier::User, "mike"),
        ];
        let index = index_section(&entries);
        let mike = index.find("- mike").unwrap();
        let zulu = index.find("- zulu").unwrap();
        let alpha = index.find("- alpha").unwrap();
        assert!(mike < zulu && zulu < alpha, "{index}");
        assert!(index.starts_with("## Memory"));
        assert!(index.contains("- mike — desc of mike (preference)"));
    }

    #[test]
    fn deterministic_across_input_orders() {
        let forward = vec![
            entry(MemoryTier::User, "a"),
            entry(MemoryTier::Project, "b"),
        ];
        let reverse: Vec<_> = forward.iter().rev().cloned().collect();
        assert_eq!(index_section(&forward), index_section(&reverse));
    }

    #[test]
    fn overflow_collapses_into_and_n_more() {
        let entries: Vec<_> = (0..MEMORY_INDEX_MAX + 5)
            .map(|i| entry(MemoryTier::User, &format!("entry-{i:03}")))
            .collect();
        let index = index_section(&entries);
        assert!(index.ends_with("- …and 5 more"), "{index}");
        assert_eq!(
            index.lines().count(),
            2 + MEMORY_INDEX_MAX + 1,
            "heading is two lines"
        );
    }

    #[test]
    fn multi_line_descriptions_flatten() {
        let mut broken = entry(MemoryTier::User, "odd");
        broken.description = "line one\nline two".to_string();
        let index = index_section(&[broken]);
        assert!(index.contains("- odd — line one line two (preference)"));
    }
}
