//! Shared substrate for project and global on-disk resources: skills and
//! custom subagents. It owns discovery, [`frontmatter`] parsing, activation,
//! atomic repository updates, and the byte-stable [`render_index_section`]
//! used in the cacheable prompt prefix.

pub(crate) mod activation;
pub(crate) mod agent;
pub(crate) mod builtin;
pub(crate) mod discovery;
#[cfg(test)]
mod drift_tests;
pub(crate) mod frontmatter;
pub(crate) mod repository;
pub(crate) mod skill;

/// Cap on a resource body (skill or subagent instructions) folded into the
/// prompt. Shared so both kinds truncate identically.
pub(crate) const MAX_RESOURCE_BODY_BYTES: usize = 16 * 1024;

/// Truncate a resource body to [`MAX_RESOURCE_BODY_BYTES`] on a char boundary,
/// reporting whether anything was cut.
pub(crate) fn cap_body(body: String) -> (String, bool) {
    if body.len() <= MAX_RESOURCE_BODY_BYTES {
        return (body, false);
    }
    let mut end = MAX_RESOURCE_BODY_BYTES;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    (body[..end].to_string(), true)
}

/// Render a byte-stable, sorted index section for the cacheable prompt prefix.
///
/// Each entry renders as `- <name> — <description>`, sorted by name. At most
/// `max` entries are listed; any remainder collapses into a single
/// `- …and N more` line — never silent truncation. Returns an empty string when
/// there are no entries, so a project with no resources contributes nothing to
/// the prefix and its prompt cache stays intact.
pub(crate) fn render_index_section(
    heading: &str,
    entries: &[(String, String)],
    max: usize,
) -> String {
    if entries.is_empty() {
        return String::new();
    }

    let mut sorted: Vec<&(String, String)> = entries.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    let mut lines = Vec::with_capacity(sorted.len().min(max) + 2);
    lines.push(heading.to_string());

    let shown = sorted.len().min(max);
    for (name, description) in &sorted[..shown] {
        // Keep one entry per line: a multi-line description (e.g. a YAML block
        // scalar) would otherwise break the index's line-per-resource shape.
        let description = description.replace(['\n', '\r'], " ");
        lines.push(format!("- {name} — {description}"));
    }
    let remaining = sorted.len() - shown;
    if remaining > 0 {
        lines.push(format!("- …and {remaining} more"));
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(names: &[&str]) -> Vec<(String, String)> {
        names
            .iter()
            .map(|n| (n.to_string(), format!("desc of {n}")))
            .collect()
    }

    #[test]
    fn empty_entries_render_to_empty_string() {
        assert_eq!(render_index_section("## Skills", &[], 10), "");
    }

    #[test]
    fn entries_render_sorted_with_em_dash() {
        let rendered = render_index_section("## Skills", &entries(&["zebra", "alpha"]), 10);
        assert_eq!(
            rendered,
            "## Skills\n- alpha — desc of alpha\n- zebra — desc of zebra"
        );
    }

    #[test]
    fn overflow_collapses_into_explicit_more_line() {
        let rendered = render_index_section("## Skills", &entries(&["a", "b", "c", "d"]), 2);
        assert_eq!(
            rendered,
            "## Skills\n- a — desc of a\n- b — desc of b\n- …and 2 more"
        );
    }
}
