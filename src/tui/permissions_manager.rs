//! `/permissions` manager state: the row model listing every editable
//! permission rule (bash command + web-domain, session + persisted) with its
//! scope and decision, plus the filter helper shared by the keymap, reducer,
//! and render. The prompt's allow/never options are how rules are *added*; this
//! manager is how they are inspected, searched, and removed.

use crate::permissions::{Permission, RuleSource, RuleView};

/// Which permission namespace a manager row belongs to. Determines which
/// manager owns the rule for removal and cache refresh — bash rules live in the
/// command namespace, domain rules in the WebFetch/WebSearch one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuleLane {
    /// A bash command rule (the default `/permissions` namespace).
    Bash,
    /// A WebFetch/WebSearch domain rule.
    Domain,
}

impl RuleLane {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Bash => "command",
            Self::Domain => "domain",
        }
    }
}

/// One editable rule in the manager list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PermissionRuleRow {
    pub lane: RuleLane,
    pub source: RuleSource,
    pub pattern: String,
    pub permission: Permission,
    /// Database id for persisted rules; `None` for session rules, which are
    /// removed from memory by pattern rather than deleted from storage.
    pub id: Option<i64>,
}

impl PermissionRuleRow {
    /// Case-insensitive substring match on the pattern, plus the lane, scope,
    /// and decision labels — so `/domain`, `/session`, or `/deny` all narrow the
    /// list the way a user would expect.
    pub(crate) fn matches_filter(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        query.is_empty()
            || self.pattern.to_lowercase().contains(&query)
            || self.lane.label().contains(&query)
            || self.source.label().contains(&query)
            || self.permission.as_db_str().contains(&query)
    }
}

/// Merge the two managers' rules into one manager list: bash command rules
/// first (in `user_rules` priority order), then domain rules, each preserving
/// its own order. `cursor` indexes the filtered projection of this list.
pub(crate) fn permission_manager_rows(
    bash_rules: &[RuleView],
    domain_rules: &[RuleView],
) -> Vec<PermissionRuleRow> {
    let bash = bash_rules.iter().map(|rule| row(RuleLane::Bash, rule));
    let domain = domain_rules.iter().map(|rule| row(RuleLane::Domain, rule));
    bash.chain(domain).collect()
}

fn row(lane: RuleLane, view: &RuleView) -> PermissionRuleRow {
    PermissionRuleRow {
        lane,
        source: view.source,
        pattern: view.pattern.clone(),
        permission: view.permission,
        id: view.id,
    }
}

/// The rows narrowed to `filter`, original order preserved. Shared by the
/// keymap/reducer/render and the delete resolver so `cursor` — which indexes the
/// *filtered* view — always maps back to the same underlying row.
pub(crate) fn permission_manager_filtered<'a>(
    rows: &'a [PermissionRuleRow],
    filter: &str,
) -> Vec<&'a PermissionRuleRow> {
    rows.iter()
        .filter(|row| row.matches_filter(filter))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(
        source: RuleSource,
        pattern: &str,
        permission: Permission,
        id: Option<i64>,
    ) -> RuleView {
        RuleView {
            source,
            pattern: pattern.to_string(),
            permission,
            id,
        }
    }

    #[test]
    fn rows_place_bash_before_domain_preserving_order() {
        let bash = vec![
            view(RuleSource::Session, "make *", Permission::Allow, None),
            view(RuleSource::Project, "rm *", Permission::Deny, Some(3)),
        ];
        let domain = vec![view(
            RuleSource::Project,
            "example.com",
            Permission::Allow,
            Some(8),
        )];

        let rows = permission_manager_rows(&bash, &domain);

        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].lane, RuleLane::Bash);
        assert_eq!(rows[0].pattern, "make *");
        assert_eq!(rows[1].lane, RuleLane::Bash);
        assert_eq!(rows[2].lane, RuleLane::Domain);
        assert_eq!(rows[2].pattern, "example.com");
    }

    #[test]
    fn filter_matches_pattern_lane_scope_and_decision() {
        let rows = permission_manager_rows(
            &[
                view(RuleSource::Session, "make *", Permission::Allow, None),
                view(RuleSource::Project, "rm -rf *", Permission::Deny, Some(1)),
            ],
            &[view(
                RuleSource::Project,
                "example.com",
                Permission::Allow,
                Some(2),
            )],
        );

        assert_eq!(permission_manager_filtered(&rows, "").len(), 3);
        // Pattern substring.
        assert_eq!(permission_manager_filtered(&rows, "make").len(), 1);
        // Lane label.
        assert_eq!(permission_manager_filtered(&rows, "domain").len(), 1);
        // Decision label — only the deny rule.
        let deny = permission_manager_filtered(&rows, "deny");
        assert_eq!(deny.len(), 1);
        assert_eq!(deny[0].pattern, "rm -rf *");
        // Scope label — session rule only.
        assert_eq!(permission_manager_filtered(&rows, "session").len(), 1);
        assert!(permission_manager_filtered(&rows, "nomatch").is_empty());
    }
}
