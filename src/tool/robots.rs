//! Minimal robots.txt support for WebFetch (RFC 9309 subset).
//!
//! Etiquette, not security: a missing, unreachable, or unparsable robots.txt
//! allows the fetch. Group selection prefers a `bonsai` user-agent group and
//! falls back to `*`; within the group the longest matching rule wins and a
//! tie goes to `Allow`. `*` matches any run of characters and a trailing `$`
//! anchors the end of the path.

/// The product token bonsai matches against `User-agent:` lines.
pub(crate) const AGENT_TOKEN: &str = "bonsai";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    allow: bool,
    pattern: String,
}

/// Parsed rules for one origin, already narrowed to bonsai's group.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Robots {
    rules: Vec<Rule>,
}

impl Robots {
    /// Everything allowed (no file, 4xx, unreachable, unparsable).
    pub(crate) fn allow_all() -> Self {
        Self::default()
    }

    pub(crate) fn parse(text: &str) -> Self {
        // Collect groups: consecutive User-agent lines open a group, rule lines
        // attach to the most recent group.
        let mut groups: Vec<(Vec<String>, Vec<Rule>)> = Vec::new();
        let mut last_was_agent = false;
        for raw in text.lines() {
            let line = raw.split('#').next().unwrap_or_default().trim();
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            match key.as_str() {
                "user-agent" => {
                    if !last_was_agent || groups.is_empty() {
                        groups.push((Vec::new(), Vec::new()));
                    }
                    if let Some((agents, _)) = groups.last_mut() {
                        agents.push(value.to_ascii_lowercase());
                    }
                    last_was_agent = true;
                }
                "allow" | "disallow" => {
                    last_was_agent = false;
                    let Some((_, rules)) = groups.last_mut() else {
                        continue;
                    };
                    // An empty Disallow means "allow everything"; no rule needed.
                    if value.is_empty() {
                        continue;
                    }
                    rules.push(Rule {
                        allow: key == "allow",
                        pattern: value.to_string(),
                    });
                }
                _ => last_was_agent = false,
            }
        }
        let names_bonsai =
            |agent: &str| agent != "*" && (agent == AGENT_TOKEN || agent.starts_with("bonsai/"));
        let specific = groups
            .iter()
            .any(|(agents, _)| agents.iter().any(|agent| names_bonsai(agent)));
        let rules = groups
            .into_iter()
            .filter(|(agents, _)| {
                agents.iter().any(|agent| {
                    if specific {
                        names_bonsai(agent)
                    } else {
                        agent == "*"
                    }
                })
            })
            .flat_map(|(_, rules)| rules)
            .collect();
        Self { rules }
    }

    /// Whether `path` (path + query, starting with `/`) may be fetched.
    pub(crate) fn allows(&self, path: &str) -> bool {
        if path == "/robots.txt" {
            return true;
        }
        let mut best: Option<(usize, bool)> = None;
        for rule in &self.rules {
            if matches(&rule.pattern, path) {
                let length = rule.pattern.len();
                best = match best {
                    Some((best_len, best_allow))
                        if best_len > length || (best_len == length && best_allow) =>
                    {
                        Some((best_len, best_allow))
                    }
                    _ => Some((length, rule.allow)),
                };
            }
        }
        best.is_none_or(|(_, allow)| allow)
    }
}

/// Prefix match with `*` wildcards and an optional trailing `$` anchor.
fn matches(pattern: &str, path: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(stripped) => (stripped, true),
        None => (pattern, false),
    };
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = path.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    for (index, part) in parts.iter().enumerate() {
        let is_last = index + 1 == parts.len();
        if is_last && anchored {
            return rest.ends_with(part);
        }
        match rest.find(part) {
            Some(found) => rest = &rest[found + part.len()..],
            None => return false,
        }
    }
    !anchored || rest.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_group_applies_with_longest_match_and_allow_ties() {
        let robots = Robots::parse(
            "User-agent: *\nDisallow: /private\nAllow: /private/public\nDisallow: /tmp/\n",
        );
        assert!(robots.allows("/"));
        assert!(!robots.allows("/private"));
        assert!(!robots.allows("/private/secret"));
        assert!(robots.allows("/private/public/page"));
        assert!(!robots.allows("/tmp/x"));
        assert!(robots.allows("/tmpfile"));
        assert!(robots.allows("/robots.txt"));
        let tie = Robots::parse("User-agent: *\nDisallow: /a\nAllow: /a\n");
        assert!(tie.allows("/a"));
    }

    #[test]
    fn a_bonsai_group_overrides_the_star_group() {
        let robots =
            Robots::parse("User-agent: *\nDisallow: /\n\nUser-agent: Bonsai\nDisallow: /admin\n");
        assert!(robots.allows("/docs"));
        assert!(!robots.allows("/admin/panel"));
        let others = Robots::parse("User-agent: googlebot\nDisallow: /\n");
        assert!(others.allows("/anything"), "other bots' rules don't apply");
    }

    #[test]
    fn wildcards_anchors_comments_and_empty_disallow() {
        let robots = Robots::parse(
            "# comment\nUser-agent: *  # all\nDisallow: /*.pdf$\nDisallow: /search*q=\nDisallow:\n",
        );
        assert!(!robots.allows("/files/report.pdf"));
        assert!(robots.allows("/files/report.pdf?download=1"));
        assert!(!robots.allows("/search?q=rust"));
        assert!(robots.allows("/search"));
        assert!(Robots::parse("User-agent: *\nDisallow:\n").allows("/x"));
        assert!(Robots::allow_all().allows("/x"));
        assert!(Robots::parse("garbage\n\u{0}").allows("/x"));
    }

    #[test]
    fn grouped_user_agents_share_rules() {
        let robots = Robots::parse("User-agent: foo\nUser-agent: *\nDisallow: /x\n");
        assert!(!robots.allows("/x"));
    }
}
