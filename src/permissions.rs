use std::sync::{Arc, Mutex};

use anyhow::Result;
use glob::Pattern;

use crate::storage::{PermissionScope, RuleKind, Storage, StoredPermissionRule};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Allow,
    Deny,
    Ask,
}

crate::impl_db_enum!(Permission {
    Allow => "allow",
    Deny => "deny",
    Ask => "ask",
} else Ask);

/// Where a rule came from. Drives both priority (the order `check` walks) and
/// `/permissions` display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleSource {
    /// In-memory "always for session" rule; lost on restart.
    Session,
    /// Persisted, scoped to the current project.
    Project,
    /// Persisted, applies to every project.
    Global,
}

impl RuleSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Project => "project",
            Self::Global => "global",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PermissionMatchSource {
    HardDeny,
    Session,
    Project,
    Global,
    EnvConfig,
    BuiltInDefault,
    Fallback,
    Unavailable,
}

impl PermissionMatchSource {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::HardDeny => "hard-deny",
            Self::Session => "session",
            Self::Project => "project",
            Self::Global => "global",
            Self::EnvConfig => "env-config",
            Self::BuiltInDefault => "built-in-default",
            Self::Fallback => "fallback",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PermissionMatch {
    pub(crate) permission: Permission,
    pub(crate) source: PermissionMatchSource,
}

#[derive(Debug, Clone)]
struct CompiledRule {
    pattern: Pattern,
    permission: Permission,
}

/// A user-supplied rule (session or persisted), kept with its raw pattern and
/// db id so `/permissions` can list and remove it.
#[derive(Debug, Clone)]
struct UserRule {
    pattern: Pattern,
    raw: String,
    permission: Permission,
    source: RuleSource,
    id: Option<i64>,
}

/// A flattened, display-ready view of one user rule for `/permissions`.
#[derive(Debug, Clone)]
pub struct RuleView {
    pub source: RuleSource,
    pub pattern: String,
    pub permission: Permission,
    /// Database id for persisted rules; `None` for session rules.
    pub id: Option<i64>,
}

/// Evaluates a command against rules in priority order:
///
/// 1. `deny_floor` — built-in hard denies (`rm -rf /`, `sudo *`, `… | sh`).
///    A user rule cannot re-enable these; the always-ask floor extends it.
/// 2. user rules — session first (newest wins), then persisted project, then
///    persisted global. This is what lets "always for project: `git push *`"
///    override the soft default `Ask`.
/// 3. `default_rules` — built-in allow/ask defaults.
/// 4. `default_permission` (`Ask`) when nothing matches.
pub struct PermissionService {
    deny_floor: Vec<CompiledRule>,
    session_rules: Vec<UserRule>,
    persisted_rules: Vec<UserRule>,
    default_rules: Vec<CompiledRule>,
    default_permission: Permission,
}

impl PermissionService {
    pub fn new() -> Self {
        Self {
            deny_floor: deny_floor_rules(),
            session_rules: Vec::new(),
            persisted_rules: Vec::new(),
            default_rules: default_rules(),
            default_permission: Permission::Ask,
        }
    }

    /// A service with no built-in rules: empty deny floor, no defaults, default
    /// `Ask`. Used for permission kinds whose patterns are not shell commands —
    /// e.g. WebFetch domains, which must never be matched against bash's
    /// command-shaped deny floor (`sudo *`, `rm -rf /`) or allow defaults.
    pub fn empty() -> Self {
        Self {
            deny_floor: Vec::new(),
            session_rules: Vec::new(),
            persisted_rules: Vec::new(),
            default_rules: Vec::new(),
            default_permission: Permission::Ask,
        }
    }

    pub fn check(&self, command: &str) -> Permission {
        self.check_match(command).permission
    }

    pub(crate) fn check_match(&self, command: &str) -> PermissionMatch {
        for rule in &self.deny_floor {
            if rule.pattern.matches(command) {
                return PermissionMatch {
                    permission: rule.permission,
                    source: PermissionMatchSource::HardDeny,
                };
            }
        }
        for rule in self.session_rules.iter().chain(&self.persisted_rules) {
            if rule.pattern.matches(command) {
                return PermissionMatch {
                    permission: rule.permission,
                    source: match rule.source {
                        RuleSource::Session => PermissionMatchSource::Session,
                        RuleSource::Project => PermissionMatchSource::Project,
                        RuleSource::Global => PermissionMatchSource::Global,
                    },
                };
            }
        }
        for rule in &self.default_rules {
            if rule.pattern.matches(command) {
                return PermissionMatch {
                    permission: rule.permission,
                    source: PermissionMatchSource::BuiltInDefault,
                };
            }
        }
        PermissionMatch {
            permission: self.default_permission,
            source: PermissionMatchSource::Fallback,
        }
    }

    /// Evaluate several command strings and return the most restrictive
    /// permission across them: `Deny` if any is denied, otherwise `Ask` if any
    /// needs confirmation, otherwise `Allow`. An empty list yields `Allow`.
    ///
    /// This is how a single shell invocation that runs several commands (a
    /// pipeline, an `&&`/`||`/`;` chain, or a command substitution) is
    /// evaluated: each segment is checked on its own so a dangerous one can't
    /// ride in behind an allowed leading program.
    #[cfg(test)]
    pub fn check_all(&self, commands: &[String]) -> Permission {
        self.check_all_match(commands).permission
    }

    pub(crate) fn check_all_match(&self, commands: &[String]) -> PermissionMatch {
        let mut result = PermissionMatch {
            permission: Permission::Allow,
            source: PermissionMatchSource::Fallback,
        };
        for command in commands {
            let matched = self.check_match(command);
            match matched.permission {
                Permission::Deny => return matched,
                Permission::Ask => result = matched,
                Permission::Allow if result.permission == Permission::Allow => result = matched,
                Permission::Allow => {}
            }
        }
        result
    }

    /// Add an in-memory, session-scoped rule. Sits just below the deny floor and
    /// above persisted and default rules; newest wins.
    pub fn add_session_rule(
        &mut self,
        pattern: &str,
        permission: Permission,
    ) -> Result<(), glob::PatternError> {
        let compiled = Pattern::new(pattern)?;
        // Refresh a remembered decision instead of accumulating duplicate
        // entries after repeated "allow matching" prompts.
        self.session_rules.retain(|rule| rule.raw != pattern);
        self.session_rules.insert(
            0,
            UserRule {
                pattern: compiled,
                raw: pattern.to_string(),
                permission,
                source: RuleSource::Session,
                id: None,
            },
        );
        Ok(())
    }

    /// Drop the in-memory session rule whose raw pattern is `pattern`. Returns
    /// whether a rule matched. Session rules have no database id, so this is the
    /// only way to remove one — `delete_permission_rule` is id-keyed and never
    /// touches them.
    pub fn remove_session_rule(&mut self, pattern: &str) -> bool {
        let before = self.session_rules.len();
        self.session_rules.retain(|rule| rule.raw != pattern);
        self.session_rules.len() != before
    }

    /// Replace the persisted-rule set (project + global) from storage rows,
    /// preserving session rules. Rows arrive project-first then newest-first, so
    /// they keep that order here. Invalid glob patterns are skipped.
    pub fn load_persisted(&mut self, rules: &[StoredPermissionRule]) {
        self.persisted_rules = rules
            .iter()
            .filter_map(|row| {
                let pattern = Pattern::new(&row.pattern).ok()?;
                Some(UserRule {
                    pattern,
                    raw: row.pattern.clone(),
                    permission: row.decision,
                    source: match row.scope {
                        PermissionScope::Project => RuleSource::Project,
                        PermissionScope::Global => RuleSource::Global,
                    },
                    id: Some(row.id),
                })
            })
            .collect();
    }

    /// User rules (session + persisted) in priority order, for `/permissions`.
    pub fn user_rules(&self) -> Vec<RuleView> {
        self.session_rules
            .iter()
            .chain(&self.persisted_rules)
            .map(|rule| RuleView {
                source: rule.source,
                pattern: rule.raw.clone(),
                permission: rule.permission,
                id: rule.id,
            })
            .collect()
    }

    pub fn deny_floor_len(&self) -> usize {
        self.deny_floor.len()
    }

    pub fn default_rules_len(&self) -> usize {
        self.default_rules.len()
    }
}

impl Default for PermissionService {
    fn default() -> Self {
        Self::new()
    }
}

fn compile(rules: &[(&str, Permission)]) -> Vec<CompiledRule> {
    rules
        .iter()
        .filter_map(|(pattern, permission)| {
            Pattern::new(pattern).ok().map(|pattern| CompiledRule {
                pattern,
                permission: *permission,
            })
        })
        .collect()
}

/// Built-in hard denies. These are the non-overridable floor: a user "allow"
/// rule is checked *after* this list, so it can never re-enable them.
fn deny_floor_rules() -> Vec<CompiledRule> {
    compile(&[
        ("rm -rf /", Permission::Deny),
        ("rm -rf ~/*", Permission::Deny),
        ("rm -rf ~", Permission::Deny),
        ("sudo *", Permission::Deny),
        ("chmod 777 *", Permission::Deny),
        ("dd *", Permission::Deny),
        ("mkfs *", Permission::Deny),
        ("curl * | *sh", Permission::Deny),
        ("wget * | *sh", Permission::Deny),
        // …and the same with the spaces around the pipe removed.
        ("curl *|*sh", Permission::Deny),
        ("wget *|*sh", Permission::Deny),
    ])
}

/// Built-in allow/ask defaults, checked after user rules so they can be
/// overridden by an explicit "always for project/session" allow.
fn default_rules() -> Vec<CompiledRule> {
    compile(&[
        // Allow safe read-only commands
        ("ls", Permission::Allow),
        ("ls *", Permission::Allow),
        ("pwd", Permission::Allow),
        ("echo *", Permission::Allow),
        ("date", Permission::Allow),
        ("whoami", Permission::Allow),
        ("id", Permission::Allow),
        ("uname *", Permission::Allow),
        ("hostname", Permission::Allow),
        ("df *", Permission::Allow),
        ("du *", Permission::Allow),
        ("free *", Permission::Allow),
        ("uptime", Permission::Allow),
        ("ps *", Permission::Allow),
        ("top *", Permission::Allow),
        ("which *", Permission::Allow),
        ("whereis *", Permission::Allow),
        // `env`/`printenv` are intentionally NOT allowlisted: they dump
        // environment variables (API keys, tokens) into model context and
        // persistence. Without an allow rule they fall to the default `Ask` and,
        // classified `High` in risk.rs, prompt at `balanced` and below.
        // Allow safe git commands
        ("git status", Permission::Allow),
        ("git status *", Permission::Allow),
        ("git log", Permission::Allow),
        ("git log *", Permission::Allow),
        ("git diff", Permission::Allow),
        ("git diff *", Permission::Allow),
        ("git show *", Permission::Allow),
        ("git branch", Permission::Allow),
        // `git branch <name>` creates a branch; only the no-argument listing
        // form is intrinsically read-only.
        ("git tag", Permission::Allow),
        // `git tag <name>` and `git remote add/remove` mutate repository
        // configuration, so they intentionally fall through to `Ask`.
        ("git remote", Permission::Allow),
        ("git ls-files", Permission::Allow),
        ("git ls-files *", Permission::Allow),
        ("git rev-parse *", Permission::Allow),
        ("git describe *", Permission::Allow),
        ("git blame *", Permission::Allow),
        ("git shortlog *", Permission::Allow),
        // Allow safe language commands
        ("go version", Permission::Allow),
        ("go help *", Permission::Allow),
        ("go list *", Permission::Allow),
        ("go env *", Permission::Allow),
        ("go doc *", Permission::Allow),
        // Build, test, formatter, and project-script commands can execute
        // repository-controlled code or mutate the checkout. They stay `Ask`
        // here and are only auto-approved when the active autonomy threshold
        // independently permits their classified risk.
        ("cargo --version", Permission::Allow),
        ("npm --version", Permission::Allow),
        ("node --version", Permission::Allow),
        ("python --version", Permission::Allow),
        ("python3 --version", Permission::Allow),
        ("rustc --version", Permission::Allow),
        ("rustup show", Permission::Allow),
        // Ask for confirmation on potentially destructive commands
        ("rm *", Permission::Ask),
        ("git push *", Permission::Ask),
        ("git commit *", Permission::Ask),
        ("git reset *", Permission::Ask),
        ("git checkout *", Permission::Ask),
        ("git merge *", Permission::Ask),
        ("git rebase *", Permission::Ask),
        ("npm install *", Permission::Ask),
        ("yarn install *", Permission::Ask),
        ("cargo build *", Permission::Ask),
        ("cargo run *", Permission::Ask),
        ("make *", Permission::Ask),
        ("docker *", Permission::Ask),
        ("docker-compose *", Permission::Ask),
    ])
}

#[derive(Clone)]
struct PermissionPersistence {
    storage: Storage,
    project_id: i64,
    /// Which rule namespace this manager reads and writes. Keeps a bash manager
    /// and a domain manager backed by the same table from seeing each other's
    /// rows.
    kind: RuleKind,
}

/// Runtime coordinator over the in-memory [`PermissionService`] and the
/// persisted `permission_rules` table. Enforcement reads stay synchronous;
/// persistence is async. Cheap to clone (it shares the service handle + storage
/// pool), so the same manager backs the bash tool and the `/permissions`
/// command and they observe each other's changes.
#[derive(Clone)]
pub struct PermissionManager {
    service: Arc<Mutex<PermissionService>>,
    persistence: Option<PermissionPersistence>,
}

impl PermissionManager {
    /// Build a persistent manager for bash-command rules, loading the project's
    /// saved rules into the in-memory service.
    pub async fn load(storage: Storage, project_id: i64) -> Result<Self> {
        Self::load_kind(
            storage,
            project_id,
            RuleKind::Bash,
            PermissionService::new(),
        )
        .await
    }

    /// Build a persistent manager for WebFetch domain rules. Uses an
    /// [`PermissionService::empty`] service so domains are never matched against
    /// bash's deny floor / allow defaults.
    pub async fn load_domains(storage: Storage, project_id: i64) -> Result<Self> {
        Self::load_kind(
            storage,
            project_id,
            RuleKind::Domain,
            PermissionService::empty(),
        )
        .await
    }

    /// Build a persistent manager for extension-tool rules. Uses an
    /// [`PermissionService::empty`] service, like domains: dotted extension
    /// ids are not shell commands and must never be matched against bash's
    /// deny floor / allow defaults.
    pub async fn load_mcp(storage: Storage, project_id: i64) -> Result<Self> {
        Self::load_kind(
            storage,
            project_id,
            RuleKind::Mcp,
            PermissionService::empty(),
        )
        .await
    }

    /// Build a persistent manager for hook trust rules. Uses an
    /// [`PermissionService::empty`] service, like mcp/domains: a
    /// `hook.<name>:<hash>` pattern is not a shell command and must never be
    /// matched against bash's deny floor / allow defaults.
    pub async fn load_hooks(storage: Storage, project_id: i64) -> Result<Self> {
        Self::load_kind(
            storage,
            project_id,
            RuleKind::Hook,
            PermissionService::empty(),
        )
        .await
    }

    /// Build a persistent manager for workspace-trust decisions. This
    /// is a separate namespace from commands, domains, extensions, and hooks:
    /// one trust decision must never authorize an unrelated side effect.
    pub async fn load_workspace_trust(storage: Storage, project_id: i64) -> Result<Self> {
        Self::load_kind(
            storage,
            project_id,
            RuleKind::WorkspaceTrust,
            PermissionService::empty(),
        )
        .await
    }

    async fn load_kind(
        storage: Storage,
        project_id: i64,
        kind: RuleKind,
        mut service: PermissionService,
    ) -> Result<Self> {
        let rows = storage
            .permission_rules_for_project(project_id, kind)
            .await?;
        service.load_persisted(&rows);
        Ok(Self {
            service: Arc::new(Mutex::new(service)),
            persistence: Some(PermissionPersistence {
                storage,
                project_id,
                kind,
            }),
        })
    }

    /// A bash manager with no database behind it: session rules work and "always
    /// for project" degrades to a session rule. Used in tests and noninteractive
    /// runs.
    pub fn memory_only() -> Self {
        Self {
            service: Arc::new(Mutex::new(PermissionService::new())),
            persistence: None,
        }
    }

    /// A domain manager with no database behind it (see [`Self::memory_only`]).
    /// Used in tests and evals, where every fetch to a fresh domain must prompt
    /// or fail rather than silently reuse a persisted allow.
    pub fn memory_only_domains() -> Self {
        Self {
            service: Arc::new(Mutex::new(PermissionService::empty())),
            persistence: None,
        }
    }

    /// An extension-tool manager with no database behind it for MCP tests.
    #[cfg(test)]
    pub(crate) fn memory_only_mcp() -> Self {
        Self {
            service: Arc::new(Mutex::new(PermissionService::empty())),
            persistence: None,
        }
    }

    /// A hook-trust manager with no database behind it for hook tests.
    #[cfg(test)]
    pub(crate) fn memory_only_hooks() -> Self {
        Self {
            service: Arc::new(Mutex::new(PermissionService::empty())),
            persistence: None,
        }
    }

    #[cfg(test)]
    pub fn check_all(&self, commands: &[String]) -> Permission {
        // Fail closed: a poisoned lock means a panic happened mid-mutation, so
        // deny rather than fall through to `Ask` (which a high autonomy level
        // would auto-approve).
        self.service
            .lock()
            .map(|svc| svc.check_all(commands))
            .unwrap_or(Permission::Deny)
    }

    pub(crate) fn check_all_detailed(&self, commands: &[String]) -> PermissionMatch {
        self.service
            .lock()
            .map(|svc| svc.check_all_match(commands))
            .unwrap_or(PermissionMatch {
                permission: Permission::Deny,
                source: PermissionMatchSource::Unavailable,
            })
    }

    /// Evaluate a single value (e.g. a WebFetch host) against the rules. Fails
    /// closed to `Deny` on a poisoned lock, like [`Self::check_all`].
    pub fn check_one(&self, value: &str) -> Permission {
        self.service
            .lock()
            .map(|svc| svc.check(value))
            .unwrap_or(Permission::Deny)
    }

    pub(crate) fn check_one_detailed(&self, value: &str) -> PermissionMatch {
        self.service
            .lock()
            .map(|svc| svc.check_match(value))
            .unwrap_or(PermissionMatch {
                permission: Permission::Deny,
                source: PermissionMatchSource::Unavailable,
            })
    }

    /// "Always for session" — in-memory only, lost on restart.
    pub fn allow_for_session(&self, pattern: &str) {
        self.add_session_rule(pattern, Permission::Allow);
    }

    /// Add an arbitrary session rule. The lock is held only for the in-memory
    /// insert, never across an await.
    ///
    /// `pub(crate)`, not `pub`: the sanctioned public entry point is
    /// [`Self::allow_for_session`] (and `allow_for_project`). This arbitrary-
    /// permission variant exists for `allow_for_session` and for tests seeding a
    /// `Deny`; keeping it crate-private stops callers from minting rules that
    /// sidestep the kind-namespaced boundary the managers enforce.
    pub(crate) fn add_session_rule(&self, pattern: &str, permission: Permission) {
        if let Ok(mut svc) = self.service.lock()
            && let Err(err) = svc.add_session_rule(pattern, permission)
        {
            tracing::warn!(pattern, %err, "invalid permission pattern; rule skipped");
        }
    }

    /// Remove the in-memory session rule with this exact pattern, returning
    /// whether one matched. The `/permissions` manager uses this to delete a
    /// session rule, which — having no database id — cannot go through
    /// [`Self::remove`]. Persisted rules are untouched.
    pub fn remove_session_rule(&self, pattern: &str) -> bool {
        match self.service.lock() {
            Ok(mut svc) => svc.remove_session_rule(pattern),
            Err(_) => {
                tracing::warn!(
                    pattern,
                    "permission service lock poisoned; session rule not removed"
                );
                false
            }
        }
    }

    /// "Always for project" — persist (scope `Project`) and apply immediately.
    /// Without a database it falls back to a session rule so the run isn't blocked.
    pub async fn allow_for_project(&self, pattern: &str) -> Result<()> {
        self.set_for_project(pattern, Permission::Allow).await
    }

    /// Persist an explicit project decision in this manager's namespace. Kept
    /// crate-visible because ordinary callers should only mint allows through
    /// the narrow public API; workspace trust additionally needs a remembered
    /// deny so an explicitly rejected repository does not prompt again.
    pub(crate) async fn set_for_project(
        &self,
        pattern: &str,
        permission: Permission,
    ) -> Result<()> {
        let Some(persistence) = self.persistence.clone() else {
            self.add_session_rule(pattern, permission);
            return Ok(());
        };
        persistence
            .storage
            .upsert_permission_rule(
                Some(persistence.project_id),
                pattern,
                permission,
                PermissionScope::Project,
                persistence.kind,
            )
            .await?;
        self.reload(&persistence).await
    }

    /// "Never for project" — persist (scope `Project`) a deny for `pattern` and
    /// apply it immediately, so a matching request is refused without
    /// re-prompting. Used by workspace trust ("keep restricted" is remembered
    /// rather than reprompting at every launch of the same repository) and by
    /// the approval prompts' "Never" option. Without a database it falls back to
    /// a session rule so the current run still honors the decision.
    pub(crate) async fn deny_for_project(&self, pattern: &str) -> Result<()> {
        self.set_for_project(pattern, Permission::Deny).await
    }

    /// User rules (session + persisted) in priority order, for `/permissions`.
    ///
    /// On a poisoned lock this returns an empty list — the display can't fail
    /// closed the way [`Self::check_one`]/[`Self::check_all`] do — but it logs a
    /// warning first so the empty `/permissions` output is not silently
    /// mistaken for "no rules" when the in-memory state is actually inconsistent.
    pub fn user_rules(&self) -> Vec<RuleView> {
        match self.service.lock() {
            Ok(svc) => svc.user_rules(),
            Err(_) => {
                tracing::warn!(
                    "permission service lock poisoned; /permissions rule list is incomplete"
                );
                Vec::new()
            }
        }
    }

    /// `(deny_floor, defaults)` built-in rule counts, for the `/permissions` summary.
    /// Reports `(0, 0)` on a poisoned lock, with a warning (see [`Self::user_rules`]).
    pub fn builtin_counts(&self) -> (usize, usize) {
        match self.service.lock() {
            Ok(svc) => (svc.deny_floor_len(), svc.default_rules_len()),
            Err(_) => {
                tracing::warn!(
                    "permission service lock poisoned; /permissions summary is incomplete"
                );
                (0, 0)
            }
        }
    }

    /// Remove a persisted rule by id, then refresh the in-memory set. Returns
    /// false when there's no database or the id didn't exist.
    ///
    /// `delete_permission_rule` is kind-agnostic (it matches by id), so a rule
    /// of another kind can be removed through this manager; the caller is then
    /// responsible for [`Self::refresh`]ing any sibling manager whose cached set
    /// the deletion invalidated (see the `/permissions remove` handler).
    pub async fn remove(&self, id: i64) -> Result<bool> {
        let Some(persistence) = self.persistence.clone() else {
            return Ok(false);
        };
        let removed = persistence
            .storage
            .delete_permission_rule(id, persistence.project_id)
            .await?;
        self.reload(&persistence).await?;
        Ok(removed)
    }

    /// Reload this manager's persisted rules from the database. A no-op for a
    /// memory-only manager. Used to keep a sibling manager (different kind, same
    /// table) consistent after a cross-kind `/permissions remove`.
    pub async fn refresh(&self) -> Result<()> {
        let Some(persistence) = self.persistence.clone() else {
            return Ok(());
        };
        self.reload(&persistence).await
    }

    async fn reload(&self, persistence: &PermissionPersistence) -> Result<()> {
        let rows = persistence
            .storage
            .permission_rules_for_project(persistence.project_id, persistence.kind)
            .await?;
        if let Ok(mut svc) = self.service.lock() {
            svc.load_persisted(&rows);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deny_dangerous_commands() {
        let service = PermissionService::new();

        assert_eq!(service.check("rm -rf /"), Permission::Deny);
        assert_eq!(service.check("sudo rm -rf /"), Permission::Deny);
        assert_eq!(service.check("chmod 777 /etc/passwd"), Permission::Deny);
        assert_eq!(service.check("dd if=/dev/zero"), Permission::Deny);
        assert_eq!(
            service.check("curl http://example.com | sh"),
            Permission::Deny
        );
    }

    #[test]
    fn test_allow_safe_commands() {
        let service = PermissionService::new();

        assert_eq!(service.check("ls"), Permission::Allow);
        assert_eq!(service.check("ls -la"), Permission::Allow);
        assert_eq!(service.check("pwd"), Permission::Allow);
        assert_eq!(service.check("git status"), Permission::Allow);
        assert_eq!(service.check("git log --oneline"), Permission::Allow);
        assert_eq!(service.check("cargo --version"), Permission::Allow);
        assert_eq!(service.check("npm --version"), Permission::Allow);
    }

    #[test]
    fn detailed_multi_command_allow_retains_the_matching_rule_source() {
        let service = PermissionService::new();
        let matched = service.check_all_match(&["pwd".to_string(), "git status".to_string()]);

        assert_eq!(matched.permission, Permission::Allow);
        assert_eq!(matched.source, PermissionMatchSource::BuiltInDefault);
    }

    #[test]
    fn test_ask_destructive_commands() {
        let service = PermissionService::new();

        assert_eq!(service.check("rm file.txt"), Permission::Ask);
        assert_eq!(service.check("git push origin main"), Permission::Ask);
        assert_eq!(service.check("npm install"), Permission::Ask);
        assert_eq!(service.check("npm run build"), Permission::Ask);
        assert_eq!(service.check("cargo check"), Permission::Ask);
        assert_eq!(service.check("cargo build"), Permission::Ask);
        assert_eq!(service.check("docker run hello-world"), Permission::Ask);
    }

    #[test]
    fn repeated_session_rule_replaces_the_previous_decision() {
        let mut service = PermissionService::empty();

        service
            .add_session_rule("example.com", Permission::Allow)
            .unwrap();
        service
            .add_session_rule("example.com", Permission::Deny)
            .unwrap();

        assert_eq!(service.check("example.com"), Permission::Deny);
        assert_eq!(service.user_rules().len(), 1);
    }

    #[test]
    fn session_rule_overrides_a_soft_default() {
        let mut service = PermissionService::new();
        assert_eq!(service.check("git push origin main"), Permission::Ask);

        service
            .add_session_rule("git push *", Permission::Allow)
            .unwrap();
        // User rules are checked before built-in defaults.
        assert_eq!(service.check("git push origin main"), Permission::Allow);
    }

    #[test]
    fn deny_floor_is_not_overridable_by_a_user_allow() {
        let mut service = PermissionService::new();
        service
            .add_session_rule("rm -rf /", Permission::Allow)
            .unwrap();
        service
            .add_session_rule("sudo *", Permission::Allow)
            .unwrap();

        // The hard-deny floor is checked first, so a user "allow" can't re-enable it.
        assert_eq!(service.check("rm -rf /"), Permission::Deny);
        assert_eq!(service.check("sudo apt install"), Permission::Deny);
    }

    #[test]
    fn newest_session_rule_wins() {
        let mut service = PermissionService::new();
        service
            .add_session_rule("make *", Permission::Allow)
            .unwrap();
        service
            .add_session_rule("make *", Permission::Deny)
            .unwrap();
        assert_eq!(service.check("make all"), Permission::Deny);
    }

    #[test]
    fn persisted_rules_apply_and_session_outranks_them() {
        let mut service = PermissionService::new();
        service.load_persisted(&[StoredPermissionRule {
            id: 7,
            pattern: "docker *".to_string(),
            decision: Permission::Allow,
            scope: PermissionScope::Project,
        }]);
        assert_eq!(service.check("docker ps"), Permission::Allow);

        // A session rule is checked before persisted rules.
        service
            .add_session_rule("docker *", Permission::Deny)
            .unwrap();
        assert_eq!(service.check("docker ps"), Permission::Deny);

        let listed = service.user_rules();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].source, RuleSource::Session);
        assert_eq!(listed[1].source, RuleSource::Project);
        assert_eq!(listed[1].id, Some(7));
    }

    #[test]
    fn test_unknown_command_defaults_to_ask() {
        let service = PermissionService::new();

        assert_eq!(service.check("unknown-command"), Permission::Ask);
    }

    /// The core guarantee: "always for project" persists and survives a
    /// restart. Proven end-to-end through the manager + a real SQLite db.
    #[tokio::test]
    async fn manager_persists_project_rule_across_reload() {
        let ts = crate::storage::test_utils::TestStorage::new().await;
        let project_id = ts.storage.ensure_project(ts.project_path()).await.unwrap();
        let push = ["git push origin main".to_string()];

        // First "session": git push is Ask by default; approve it for the project.
        let manager = PermissionManager::load(ts.storage.clone(), project_id)
            .await
            .unwrap();
        assert_eq!(manager.check_all(&push), Permission::Ask);
        manager.allow_for_project("git push *").await.unwrap();
        assert_eq!(manager.check_all(&push), Permission::Allow);

        // Fresh "restart": a brand-new manager loads the rule from the database.
        let reloaded = PermissionManager::load(ts.storage.clone(), project_id)
            .await
            .unwrap();
        assert_eq!(reloaded.check_all(&push), Permission::Allow);

        // It lists as a persisted project rule, and removing it restores Ask.
        let rules = reloaded.user_rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].source, RuleSource::Project);
        let id = rules[0].id.expect("persisted rule has an id");
        assert!(reloaded.remove(id).await.unwrap());
        assert_eq!(reloaded.check_all(&push), Permission::Ask);
    }

    #[test]
    fn empty_service_has_no_deny_floor_or_defaults() {
        let service = PermissionService::empty();
        // Bash's built-in denies and allows are absent: everything is `Ask`
        // until a domain rule says otherwise.
        assert_eq!(service.check("rm -rf /"), Permission::Ask);
        assert_eq!(service.check("ls"), Permission::Ask);
        assert_eq!(service.check("example.com"), Permission::Ask);
        assert_eq!(service.deny_floor_len(), 0);
        assert_eq!(service.default_rules_len(), 0);
    }

    /// Domain rules persist and reload through their own `kind` namespace, and a
    /// bash manager over the same database never sees them (and vice versa).
    #[tokio::test]
    async fn domain_manager_persists_and_is_isolated_from_bash() {
        let ts = crate::storage::test_utils::TestStorage::new().await;
        let project_id = ts.storage.ensure_project(ts.project_path()).await.unwrap();

        let domains = PermissionManager::load_domains(ts.storage.clone(), project_id)
            .await
            .unwrap();
        assert_eq!(domains.check_one("example.com"), Permission::Ask);
        domains.allow_for_project("example.com").await.unwrap();
        assert_eq!(domains.check_one("example.com"), Permission::Allow);

        // A fresh domain manager reloads the rule; a bash manager does not see it.
        let reloaded = PermissionManager::load_domains(ts.storage.clone(), project_id)
            .await
            .unwrap();
        assert_eq!(reloaded.check_one("example.com"), Permission::Allow);
        let bash = PermissionManager::load(ts.storage.clone(), project_id)
            .await
            .unwrap();
        assert!(
            bash.user_rules().is_empty(),
            "bash manager sees no domain rules"
        );
        assert_eq!(
            bash.check_all(&["example.com".to_string()]),
            Permission::Ask,
            "a domain rule must not authorize a same-named bash command"
        );

        // Removing via one manager + refreshing the sibling keeps both consistent.
        let id = reloaded.user_rules()[0]
            .id
            .expect("persisted rule has an id");
        assert!(reloaded.remove(id).await.unwrap());
        reloaded.refresh().await.unwrap();
        assert_eq!(reloaded.check_one("example.com"), Permission::Ask);
    }

    /// Mcp rules persist and reload through their own `kind` namespace,
    /// isolated from bash and domain rules over the same table.
    #[tokio::test]
    async fn mcp_manager_persists_and_is_isolated_from_bash() {
        let ts = crate::storage::test_utils::TestStorage::new().await;
        let project_id = ts.storage.ensure_project(ts.project_path()).await.unwrap();

        let mcp = PermissionManager::load_mcp(ts.storage.clone(), project_id)
            .await
            .unwrap();
        assert_eq!(mcp.check_one("mcp.github.create_issue"), Permission::Ask);
        mcp.allow_for_project("mcp.github.*").await.unwrap();
        assert_eq!(mcp.check_one("mcp.github.create_issue"), Permission::Allow);

        let bash = PermissionManager::load(ts.storage.clone(), project_id)
            .await
            .unwrap();
        assert!(
            bash.user_rules().is_empty(),
            "bash manager sees no mcp rules"
        );
    }

    #[tokio::test]
    async fn memory_only_manager_degrades_project_to_session() {
        let manager = PermissionManager::memory_only();
        // No DB: "always for project" still takes effect this session.
        manager.allow_for_project("make *").await.unwrap();
        assert_eq!(
            manager.check_all(&["make all".to_string()]),
            Permission::Allow
        );
        assert!(manager.user_rules().iter().all(|r| r.id.is_none()));
    }

    #[test]
    fn remove_session_rule_drops_only_the_matching_pattern() {
        let manager = PermissionManager::memory_only();
        manager.add_session_rule("make *", Permission::Allow);
        manager.add_session_rule("rm -rf *", Permission::Deny);

        assert!(manager.remove_session_rule("make *"));
        // Gone: no longer allowed, falls back to the default ask.
        assert_eq!(
            manager.check_all(&["make all".to_string()]),
            Permission::Ask
        );
        // The sibling session rule is untouched.
        assert_eq!(
            manager.check_all(&["rm -rf x".to_string()]),
            Permission::Deny
        );
        // Removing a pattern that isn't a session rule reports no match.
        assert!(!manager.remove_session_rule("make *"));
    }

    #[tokio::test]
    async fn never_for_project_persists_a_matching_deny() {
        let manager = PermissionManager::memory_only();
        // The prompt's "Never" option: a default-allowed read-only command is
        // refused once a per-project deny is recorded for it.
        assert_eq!(
            manager.check_all(&["ls foo".to_string()]),
            Permission::Allow
        );
        manager.deny_for_project("ls *").await.unwrap();
        assert_eq!(manager.check_all(&["ls foo".to_string()]), Permission::Deny);
    }

    #[test]
    fn test_invalid_pattern_in_add_rule() {
        let mut service = PermissionService::new();
        let result = service.add_session_rule("[invalid", Permission::Allow);
        assert!(result.is_err());
    }

    #[test]
    fn check_all_returns_most_restrictive() {
        let service = PermissionService::new();

        // All allowed.
        assert_eq!(
            service.check_all(&["ls".to_string(), "pwd".to_string()]),
            Permission::Allow
        );
        // One segment needs confirmation -> Ask.
        assert_eq!(
            service.check_all(&["ls".to_string(), "rm file.txt".to_string()]),
            Permission::Ask
        );
        // One segment is denied -> Deny, even alongside allowed ones.
        assert_eq!(
            service.check_all(&[
                "echo ok".to_string(),
                "rm -rf ~".to_string(),
                "ls".to_string(),
            ]),
            Permission::Deny
        );
        // Empty list is permissive (caller always supplies at least one).
        assert_eq!(service.check_all(&[]), Permission::Allow);
    }
}
