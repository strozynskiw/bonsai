//! `/remember` + `/memory` — persistent-memory commands. `/remember`
//! pins one fact explicitly (deterministic slug, no model round-trip);
//! `/memory` lists, views, and forgets entries. Pruning is `/memory forget
//! <name>` because top-level `/forget` already deletes saved sessions.

use std::path::Path;

use crate::memory::entry::{MemoryEntry, MemoryTier, render_memory_file};

pub(crate) const REMEMBER_USAGE: &str = "Usage: /remember [project] <fact>";
pub(crate) const MEMORY_USAGE: &str = "Usage: /memory [<name>|forget <name>]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemoryCommandRequest {
    List,
    View { name: String },
    Forget { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RememberRequest {
    pub tier: MemoryTier,
    pub fact: String,
}

pub(crate) fn parse_remember_command(input: &str) -> Result<RememberRequest, String> {
    let rest = input
        .trim()
        .strip_prefix("/remember")
        .ok_or_else(|| REMEMBER_USAGE.to_string())?
        .trim();
    let (tier, fact) = match rest.split_once(char::is_whitespace) {
        Some(("project", fact)) => (MemoryTier::Project, fact.trim()),
        _ => (MemoryTier::User, rest),
    };
    if fact.is_empty() || fact == "project" {
        return Err(REMEMBER_USAGE.to_string());
    }
    Ok(RememberRequest {
        tier,
        fact: fact.to_string(),
    })
}

pub(crate) fn parse_memory_command(input: &str) -> Result<MemoryCommandRequest, String> {
    let parts = input.split_whitespace().collect::<Vec<_>>();
    if parts.first().is_none_or(|command| *command != "/memory") {
        return Err(MEMORY_USAGE.to_string());
    }
    match parts.as_slice() {
        ["/memory"] => Ok(MemoryCommandRequest::List),
        ["/memory", "forget", name] => Ok(MemoryCommandRequest::Forget {
            name: (*name).to_string(),
        }),
        ["/memory", name] if *name != "forget" => Ok(MemoryCommandRequest::View {
            name: (*name).to_string(),
        }),
        _ => Err(MEMORY_USAGE.to_string()),
    }
}

/// The `/memory` listing: entries grouped user tier then project tier, with
/// both store paths in the footer so a gitignored `.bonsai/` is at least
/// visible instead of silently not shipping.
pub(crate) fn format_memory_list(
    entries: &[MemoryEntry],
    user_dir: &Path,
    project_dir: &Path,
) -> String {
    let mut lines = Vec::new();
    if entries.is_empty() {
        lines.push(
            "No memory entries saved. Pin one with /remember <fact>; the agent captures durable facts automatically."
                .to_string(),
        );
    }
    for tier in [MemoryTier::User, MemoryTier::Project] {
        let mut tier_entries = entries
            .iter()
            .filter(|entry| entry.tier == tier)
            .collect::<Vec<_>>();
        if tier_entries.is_empty() {
            continue;
        }
        tier_entries.sort_by(|a, b| a.name.cmp(&b.name));
        lines.push(format!("{} memory:", capitalize(tier.label())));
        for entry in tier_entries {
            let updated = if entry.updated.is_empty() {
                String::new()
            } else {
                format!(", updated {}", entry.updated)
            };
            lines.push(format!(
                "- {} — {} ({}{updated})",
                entry.name,
                entry.description,
                entry.entry_type.label(),
            ));
        }
    }
    lines.push(String::new());
    lines.push(format!(
        "Stores: {} · {}",
        user_dir.display(),
        project_dir.display()
    ));
    lines.join("\n")
}

/// The `/memory <name>` view: the file path plus the full rendered file. The
/// primary edit affordance is the manager modal's `e` (wizard edit mode); the
/// path remains the escape hatch for editing the file directly.
pub(crate) fn format_memory_view(entry: &MemoryEntry) -> String {
    format!("{}\n\n{}", entry.path.display(), render_memory_file(entry))
}

/// Error text for an unknown entry name, listing what exists.
pub(crate) fn unknown_memory_entry_message(name: &str, entries: &[MemoryEntry]) -> String {
    if entries.is_empty() {
        return format!("No memory entry named {name:?}; the store is empty.");
    }
    let names = entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!("No memory entry named {name:?}. Available: {names}")
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::memory::entry::MemoryEntryType;

    #[test]
    fn parses_remember_tiers_and_fact() {
        assert_eq!(
            parse_remember_command("/remember use tabs"),
            Ok(RememberRequest {
                tier: MemoryTier::User,
                fact: "use tabs".to_string(),
            })
        );
        assert_eq!(
            parse_remember_command("/remember project deploys go through CI"),
            Ok(RememberRequest {
                tier: MemoryTier::Project,
                fact: "deploys go through CI".to_string(),
            })
        );
        // A fact that merely starts with "project" still needs content after it.
        assert_eq!(
            parse_remember_command("/remember project"),
            Err(REMEMBER_USAGE.to_string())
        );
        assert_eq!(
            parse_remember_command("/remember"),
            Err(REMEMBER_USAGE.to_string())
        );
    }

    #[test]
    fn parses_memory_subcommands() {
        assert_eq!(
            parse_memory_command("/memory"),
            Ok(MemoryCommandRequest::List)
        );
        assert_eq!(
            parse_memory_command("/memory some-entry"),
            Ok(MemoryCommandRequest::View {
                name: "some-entry".to_string(),
            })
        );
        assert_eq!(
            parse_memory_command("/memory forget some-entry"),
            Ok(MemoryCommandRequest::Forget {
                name: "some-entry".to_string(),
            })
        );
        assert_eq!(
            parse_memory_command("/memory forget"),
            Err(MEMORY_USAGE.to_string())
        );
        assert_eq!(
            parse_memory_command("/memory forget a b"),
            Err(MEMORY_USAGE.to_string())
        );
    }

    fn entry(tier: MemoryTier, name: &str) -> MemoryEntry {
        MemoryEntry {
            tier,
            name: name.to_string(),
            description: format!("desc of {name}"),
            entry_type: MemoryEntryType::Preference,
            enabled: true,
            created: "2026-07-01".to_string(),
            updated: "2026-07-02".to_string(),
            body: "body\n".to_string(),
            content_hash: String::new(),
            path: PathBuf::from(format!("/m/{name}.md")),
        }
    }

    #[test]
    fn list_groups_by_tier_with_store_paths_footer() {
        let entries = vec![
            entry(MemoryTier::Project, "proj-fact"),
            entry(MemoryTier::User, "user-fact"),
        ];
        let listing = format_memory_list(
            &entries,
            Path::new("/home/u/.bonsai/memory"),
            Path::new("/repo/.bonsai/memory"),
        );
        let user_heading = listing.find("User memory:").unwrap();
        let project_heading = listing.find("Project memory:").unwrap();
        assert!(user_heading < project_heading);
        assert!(
            listing.contains("- user-fact — desc of user-fact (preference, updated 2026-07-02)")
        );
        assert!(listing.ends_with("Stores: /home/u/.bonsai/memory · /repo/.bonsai/memory"));
    }

    #[test]
    fn empty_list_still_prints_stores() {
        let listing =
            format_memory_list(&[], Path::new("/h/memory"), Path::new("/p/.bonsai/memory"));
        assert!(listing.contains("No memory entries saved"));
        assert!(listing.contains("Stores: /h/memory · /p/.bonsai/memory"));
    }

    #[test]
    fn view_prints_path_and_rendered_file() {
        let viewed = format_memory_view(&entry(MemoryTier::User, "user-fact"));
        assert!(viewed.starts_with("/m/user-fact.md\n\n---\n"));
        assert!(viewed.contains("name: user-fact"));
    }

    #[test]
    fn unknown_entry_lists_available_names() {
        let entries = vec![entry(MemoryTier::User, "known")];
        let message = unknown_memory_entry_message("nope", &entries);
        assert!(message.contains("\"nope\""));
        assert!(message.contains("known"));
        assert!(unknown_memory_entry_message("nope", &[]).contains("store is empty"));
    }
}
