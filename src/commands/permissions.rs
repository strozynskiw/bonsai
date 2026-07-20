//! `/permissions` — list the active permission rules and remove persisted ones.
//! The prompt's "always for project / session" options are the primary way to
//! *add* rules; this command is for inspecting and pruning them.

use crate::permissions::RuleView;
use crate::storage::AuthorizationDecisionRecord;

pub(crate) const PERMISSIONS_USAGE: &str = "Usage: /permissions [list|remove <id>]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PermissionsCommandRequest {
    List,
    Remove(i64),
}

pub(crate) fn parse_permissions_command(
    input: &str,
) -> std::result::Result<PermissionsCommandRequest, String> {
    let args = super::command_args(input, "/permissions", PERMISSIONS_USAGE)?;
    match args.as_slice() {
        [] | ["list"] => Ok(PermissionsCommandRequest::List),
        ["remove", id] => id
            .parse::<i64>()
            .map(PermissionsCommandRequest::Remove)
            .map_err(|_| format!("Invalid rule id '{id}'. {PERMISSIONS_USAGE}")),
        _ => Err(PERMISSIONS_USAGE.to_string()),
    }
}

/// Render the active rules: a one-line built-in summary, the custom command
/// rules in the order `check` walks them (session, then persisted
/// project/global), and built-in web-tool domain rules as a separate section.
pub(crate) fn format_permission_rules(
    user_rules: &[RuleView],
    deny_floor: usize,
    defaults: usize,
    domain_rules: &[RuleView],
    decisions: &[AuthorizationDecisionRecord],
) -> String {
    let mut out = format!("Built-in: {deny_floor} hard-deny, {defaults} default rules.");
    if user_rules.is_empty() {
        out.push_str(
            "\nNo custom rules yet. Approve a command with \"always for project\" to add one.",
        );
    } else {
        out.push_str("\nCustom rules (priority order):");
        for rule in user_rules {
            append_rule_line(&mut out, rule);
        }
    }
    if domain_rules.is_empty() {
        out.push_str(
            "\nNo web domain rules yet. WebSearch and WebFetch ask before accessing a new domain.",
        );
    } else {
        out.push_str("\nWeb domains (WebSearch/WebFetch):");
        for rule in domain_rules {
            append_rule_line(&mut out, rule);
        }
    }
    out.push_str("\nRemove a persisted rule with /permissions remove <id>.");
    if decisions.is_empty() {
        out.push_str("\nNo authorization decisions recorded for this session yet.");
    } else {
        out.push_str("\nRecent authorization decisions (newest first):");
        for decision in decisions {
            out.push_str(&format!(
                "\n  #{} {} {} [{}; {}; {}; autonomy={}; {}] {} — {}",
                decision.id,
                decision.decision,
                decision.surface,
                decision.risk_tier,
                decision.effects.join(","),
                decision.rule_source,
                decision.autonomy_level,
                decision.sandbox_posture,
                decision.subject,
                decision.reason,
            ));
        }
    }
    out
}

fn append_rule_line(out: &mut String, rule: &RuleView) {
    let id = rule
        .id
        .map(|id| format!("#{id} "))
        .unwrap_or_else(|| "   ".to_string());
    out.push_str(&format!(
        "\n  {id}[{}] {} → {}",
        rule.source.label(),
        rule.pattern,
        rule.permission.as_db_str(),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::{Permission, RuleSource};

    #[test]
    fn parses_list_and_remove() {
        assert_eq!(
            parse_permissions_command("/permissions"),
            Ok(PermissionsCommandRequest::List)
        );
        assert_eq!(
            parse_permissions_command("/permissions list"),
            Ok(PermissionsCommandRequest::List)
        );
        assert_eq!(
            parse_permissions_command("/permissions remove 7"),
            Ok(PermissionsCommandRequest::Remove(7))
        );
        assert!(parse_permissions_command("/permissions remove nope").is_err());
        assert!(parse_permissions_command("/permissions bogus").is_err());
    }

    #[test]
    fn formats_empty_and_populated() {
        let empty = format_permission_rules(&[], 9, 70, &[], &[]);
        assert!(empty.contains("No custom rules"));
        assert!(empty.contains("No web domain rules"));

        let rules = vec![
            RuleView {
                source: RuleSource::Session,
                pattern: "make *".to_string(),
                permission: Permission::Allow,
                id: None,
            },
            RuleView {
                source: RuleSource::Project,
                pattern: "git push *".to_string(),
                permission: Permission::Allow,
                id: Some(3),
            },
        ];
        let domains = vec![RuleView {
            source: RuleSource::Project,
            pattern: "example.com".to_string(),
            permission: Permission::Allow,
            id: Some(8),
        }];
        let text = format_permission_rules(&rules, 9, 70, &domains, &[]);
        assert!(text.contains("9 hard-deny, 70 default"));
        assert!(text.contains("[session] make * → allow"));
        assert!(text.contains("#3 [project] git push * → allow"));
        assert!(text.contains("Web domains (WebSearch/WebFetch):"));
        assert!(text.contains("#8 [project] example.com → allow"));
    }
}
