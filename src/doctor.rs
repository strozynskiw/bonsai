//! Release diagnostics shared by `bonsai doctor` and `/doctor`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;

use crate::config::Config;
use crate::mcp::McpReachabilityReport;
use crate::model_catalog::ModelCatalog;
use crate::provider::ProviderRegistry;
use crate::release::{ReleaseIdentity, ReleaseStatus};
use crate::sandbox::CommandSandbox;
use crate::session::SessionStore;
use crate::storage::Storage;

const REPORT_SCHEMA_VERSION: u32 = 1;
const SANDBOX_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const SANDBOX_PROBE_SCRIPT: &str = ": > \"$BONSAI_DOCTOR_PROBE\"";

/// Requested human- or machine-readable diagnostic output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum DoctorOutputFormat {
    #[default]
    Text,
    Json,
}

/// Whether diagnostics may contact configured remote services.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum DoctorNetworkMode {
    #[default]
    Offline,
    Online,
}

/// Options shared by the CLI and slash-command surfaces.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DoctorRequest {
    pub(crate) format: DoctorOutputFormat,
    pub(crate) network: DoctorNetworkMode,
}

impl DoctorRequest {
    pub(crate) fn parse_slash_args(args: &str) -> Result<Self, &'static str> {
        let mut request = Self::default();
        let mut saw_json = false;
        let mut saw_online = false;
        for arg in args.split_whitespace() {
            match arg {
                "json" if !saw_json => {
                    saw_json = true;
                    request.format = DoctorOutputFormat::Json;
                }
                "online" if !saw_online => {
                    saw_online = true;
                    request.network = DoctorNetworkMode::Online;
                }
                _ => return Err("Usage: /doctor [json] [online]"),
            }
        }
        Ok(request)
    }
}

/// Severity of one release diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DoctorStatus {
    Pass,
    Warning,
    Fail,
}

impl DoctorStatus {
    const fn marker(self) -> &'static str {
        match self {
            Self::Pass => "ok",
            Self::Warning => "warn",
            Self::Fail => "fail",
        }
    }
}

/// A stable, secret-redacted diagnostic result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DoctorCheck {
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) status: DoctorStatus,
    pub(crate) summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next_action: Option<String>,
}

impl DoctorCheck {
    fn pass(id: &'static str, label: &'static str, summary: impl Into<String>) -> Self {
        Self::new(id, label, DoctorStatus::Pass, summary, None::<String>)
    }

    fn warning(
        id: &'static str,
        label: &'static str,
        summary: impl Into<String>,
        next_action: impl Into<String>,
    ) -> Self {
        Self::new(id, label, DoctorStatus::Warning, summary, Some(next_action))
    }

    fn fail(
        id: &'static str,
        label: &'static str,
        summary: impl Into<String>,
        next_action: impl Into<String>,
    ) -> Self {
        Self::new(id, label, DoctorStatus::Fail, summary, Some(next_action))
    }

    fn new(
        id: &'static str,
        label: &'static str,
        status: DoctorStatus,
        summary: impl Into<String>,
        next_action: Option<impl Into<String>>,
    ) -> Self {
        let summary = summary.into();
        let next_action = next_action.map(Into::into);
        Self {
            id,
            label,
            status,
            summary: crate::redact::redact(&summary).into_owned(),
            next_action: next_action.map(|action| crate::redact::redact(&action).into_owned()),
        }
    }
}

/// Aggregate counts included in the JSON support form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct DoctorSummary {
    pub(crate) passed: usize,
    pub(crate) warnings: usize,
    pub(crate) failed: usize,
}

/// Versioned diagnostic report with no credential values or local paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DoctorReport {
    pub(crate) schema_version: u32,
    pub(crate) bonsai_version: &'static str,
    pub(crate) os: &'static str,
    pub(crate) arch: &'static str,
    pub(crate) summary: DoctorSummary,
    pub(crate) checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    fn new(checks: Vec<DoctorCheck>) -> Self {
        let summary = DoctorSummary {
            passed: checks
                .iter()
                .filter(|check| check.status == DoctorStatus::Pass)
                .count(),
            warnings: checks
                .iter()
                .filter(|check| check.status == DoctorStatus::Warning)
                .count(),
            failed: checks
                .iter()
                .filter(|check| check.status == DoctorStatus::Fail)
                .count(),
        };
        Self {
            schema_version: REPORT_SCHEMA_VERSION,
            bonsai_version: env!("CARGO_PKG_VERSION"),
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            summary,
            checks,
        }
    }

    pub(crate) fn has_failures(&self) -> bool {
        self.summary.failed > 0
    }

    pub(crate) fn render(&self, format: DoctorOutputFormat) -> anyhow::Result<String> {
        match format {
            DoctorOutputFormat::Text => Ok(self.render_text()),
            DoctorOutputFormat::Json => {
                serde_json::to_string_pretty(self).map_err(anyhow::Error::from)
            }
        }
    }

    fn render_text(&self) -> String {
        let mut lines = vec![format!(
            "Bonsai doctor: {} passed, {} warnings, {} failed",
            self.summary.passed, self.summary.warnings, self.summary.failed
        )];
        for check in &self.checks {
            lines.push(format!(
                "[{}] {} — {}",
                check.status.marker(),
                check.label,
                check.summary
            ));
            if let Some(action) = &check.next_action {
                lines.push(format!("       next: {action}"));
            }
        }
        lines.join("\n")
    }
}

/// Live inputs used by the shared diagnostic engine.
#[derive(Debug)]
pub(crate) struct DoctorContext<'a> {
    pub(crate) project_root: &'a Path,
    pub(crate) storage: Option<&'a Storage>,
    pub(crate) catalog: Option<&'a ModelCatalog>,
    pub(crate) catalog_loaded_cleanly: bool,
    pub(crate) config: &'a Config,
    pub(crate) session_store: Option<&'a SessionStore>,
    pub(crate) registry: Option<&'a ProviderRegistry>,
    pub(crate) sandbox: &'a CommandSandbox,
    pub(crate) network_mode: DoctorNetworkMode,
    pub(crate) mcp_reachability: Option<&'a McpReachabilityReport>,
    pub(crate) release_status: Option<&'a ReleaseStatus>,
}

/// Run deterministic checks, adding remote probes only when explicitly requested.
pub(crate) async fn collect(context: DoctorContext<'_>) -> DoctorReport {
    let mut checks = Vec::with_capacity(13);
    checks.push(database_check(context.storage).await);
    checks.push(catalog_check(
        context.catalog,
        context.catalog_loaded_cleanly,
    ));
    checks.push(config_check(context.config));
    checks.push(provider_check(context.session_store, context.registry));
    if context.network_mode == DoctorNetworkMode::Online {
        checks.push(provider_reachability_check(context.session_store, context.registry).await);
    }
    checks.push(sandbox_check(context.sandbox, context.project_root).await);
    checks.push(executable_check(
        "git",
        "Git",
        "git",
        "Install Git and ensure `git` is available on PATH.",
    ));
    checks.push(executable_check(
        "github_cli",
        "GitHub CLI",
        "gh",
        "Install GitHub CLI (`gh`) or use Git remotes without PR automation.",
    ));
    checks.push(language_server_check(context.project_root));
    checks.push(mcp_check(context.config));
    if context.network_mode == DoctorNetworkMode::Online {
        checks.push(mcp_reachability_check(
            context.config,
            context.mcp_reachability,
        ));
    }
    checks.push(terminal_check());
    checks.push(release_check(context.network_mode, context.release_status));
    DoctorReport::new(checks)
}

/// Build a standalone report for the `bonsai doctor` CLI entry point.
pub(crate) async fn collect_standalone(
    project_root: &Path,
    network_mode: DoctorNetworkMode,
) -> DoctorReport {
    let discovered_paths = crate::storage::BonsaiPaths::discover().ok();
    let storage = Storage::open().await.ok();
    let home_dir = storage
        .as_ref()
        .map(|storage| storage.home_dir().to_path_buf())
        .or_else(|| discovered_paths.map(|paths| paths.home_dir().to_path_buf()))
        .unwrap_or_else(|| PathBuf::from(".bonsai"));
    let config = crate::config::load(project_root, &home_dir);

    let catalog_result =
        crate::model_catalog::load_catalog_from_home_and_project(&home_dir, Some(project_root));
    let catalog_loaded_cleanly = catalog_result.is_ok();
    let catalog = catalog_result
        .ok()
        .or_else(|| ModelCatalog::load_builtin().ok());
    let registry = catalog.as_ref().map(ProviderRegistry::from_catalog);
    let session_store = match (&storage, &catalog) {
        (Some(storage), Some(catalog)) => {
            SessionStore::load_with_storage_and_catalog(storage, Some(catalog))
                .await
                .ok()
        }
        _ => None,
    };
    let sandbox = CommandSandbox::from_config(
        crate::sandbox::detect_backend(),
        project_root,
        &config.sandbox,
    );
    let credential_persistence = match &storage {
        Some(storage) => storage
            .credential_persistence()
            .await
            .ok()
            .flatten()
            .unwrap_or_default(),
        None => crate::session::CredentialPersistence::default(),
    };
    let (mcp_reachability, release_status) = if network_mode == DoctorNetworkMode::Online {
        let mcp = crate::mcp::probe_configured_reachability(
            &config,
            &sandbox,
            project_root,
            crate::session::CredentialStore::with_home(&home_dir),
            credential_persistence,
        );
        let release = crate::release::check();
        let (mcp, release) = tokio::join!(mcp, release);
        (Some(mcp), Some(release))
    } else {
        (None, None)
    };
    let report = collect(DoctorContext {
        project_root,
        storage: storage.as_ref(),
        catalog: catalog.as_ref(),
        catalog_loaded_cleanly,
        config: &config,
        session_store: session_store.as_ref(),
        registry: registry.as_ref(),
        sandbox: &sandbox,
        network_mode,
        mcp_reachability: mcp_reachability.as_ref(),
        release_status: release_status.as_ref(),
    })
    .await;

    drop(session_store);
    if let Some(storage) = &storage {
        storage.close().await;
    }
    report
}

async fn database_check(storage: Option<&Storage>) -> DoctorCheck {
    let Some(storage) = storage else {
        return DoctorCheck::fail(
            "database",
            "Database",
            "The session database could not be opened or migrated.",
            "Check free disk space and BONSAI_HOME permissions, follow https://github.com/strozynskiw/bonsai/blob/master/docs/compatibility.md, then rerun `bonsai doctor`.",
        );
    };
    match storage.health_check().await {
        Ok(()) => DoctorCheck::pass(
            "database",
            "Database",
            "Migrations, integrity check, and temporary write probe passed.",
        ),
        Err(_) => DoctorCheck::fail(
            "database",
            "Database",
            "The database failed its integrity or write probe.",
            format!(
                "Check free disk space and BONSAI_HOME permissions. If `sqlite3 bonsai.db 'PRAGMA quick_check'` also fails, stop Bonsai and restore a database from {} as described at https://github.com/strozynskiw/bonsai/blob/master/docs/compatibility.md.",
                storage.upgrade_backup_dir().display()
            ),
        ),
    }
}

fn catalog_check(catalog: Option<&ModelCatalog>, loaded_cleanly: bool) -> DoctorCheck {
    match catalog {
        Some(catalog) if loaded_cleanly && !catalog.connections().is_empty() => DoctorCheck::pass(
            "model_catalog",
            "Model catalog",
            format!(
                "Loaded {} provider connections.",
                catalog.connections().len()
            ),
        ),
        Some(_) => DoctorCheck::fail(
            "model_catalog",
            "Model catalog",
            "A user or project model-catalog file is invalid; built-ins were loaded as fallback.",
            "Run `/providers list`, then fix or remove the invalid provider/model TOML entry and rerun `/doctor`.",
        ),
        None => DoctorCheck::fail(
            "model_catalog",
            "Model catalog",
            "No valid built-in or user model catalog could be loaded.",
            "Reinstall bonsai, then rerun `bonsai doctor`.",
        ),
    }
}

fn config_check(config: &Config) -> DoctorCheck {
    if config.diagnostics.is_empty() {
        DoctorCheck::pass(
            "config",
            "Configuration",
            "Global and project configuration parsed without diagnostics.",
        )
    } else {
        DoctorCheck::warning(
            "config",
            "Configuration",
            format!(
                "{} configuration entr{} skipped or downgraded.",
                config.diagnostics.len(),
                if config.diagnostics.len() == 1 {
                    "y was"
                } else {
                    "ies were"
                }
            ),
            "Run `/config validate` and fix every reported field, then rerun `/doctor`.",
        )
    }
}

fn provider_check(
    session_store: Option<&SessionStore>,
    registry: Option<&ProviderRegistry>,
) -> DoctorCheck {
    let (Some(session_store), Some(registry)) = (session_store, registry) else {
        return DoctorCheck::fail(
            "provider_auth",
            "Provider",
            "Provider state could not be loaded.",
            "Fix the database and model-catalog failures above, then rerun `/doctor`.",
        );
    };
    let provider_id = session_store.current_kind_id();
    let Some(factory) = registry.lookup(provider_id) else {
        return DoctorCheck::fail(
            "provider_auth",
            "Provider",
            "The selected provider is not present in the active catalog.",
            "Run `/providers list`, then select an available provider with `/model`.",
        );
    };
    let Some(session) = session_store.providers.get(provider_id) else {
        return DoctorCheck::fail(
            "provider_auth",
            "Provider",
            "The selected provider has no session configuration.",
            format!("Run `/authorize {provider_id}`, then choose a model with `/model`."),
        );
    };
    if !factory.is_authorized(session) {
        return DoctorCheck::fail(
            "provider_auth",
            "Provider",
            format!(
                "{} is selected but not authorized.",
                factory.metadata().display_name
            ),
            format!("Run `/authorize {provider_id}` and complete the provider login."),
        );
    }
    if session.model.trim().is_empty() {
        return DoctorCheck::fail(
            "provider_auth",
            "Provider",
            format!(
                "{} is authorized but has no selected model.",
                factory.metadata().display_name
            ),
            "Run `/model` and select a model for the current provider.",
        );
    }
    DoctorCheck::pass(
        "provider_auth",
        "Provider",
        format!(
            "{} is authorized with a selected model.",
            factory.metadata().display_name
        ),
    )
}

async fn provider_reachability_check(
    session_store: Option<&SessionStore>,
    registry: Option<&ProviderRegistry>,
) -> DoctorCheck {
    let (Some(session_store), Some(registry)) = (session_store, registry) else {
        return DoctorCheck::warning(
            "provider_reachability",
            "Provider reachability",
            "The online provider probe was skipped because provider state is unavailable.",
            "Fix the provider-state failures above, then rerun `/doctor online`.",
        );
    };
    let provider_id = session_store.current_kind_id();
    let (Some(factory), Some(session)) = (
        registry.lookup(provider_id),
        session_store.providers.get(provider_id),
    ) else {
        return DoctorCheck::warning(
            "provider_reachability",
            "Provider reachability",
            "The online provider probe was skipped because the selected provider is unavailable.",
            "Select an available provider with `/model`, authorize it, then rerun `/doctor online`.",
        );
    };
    if !factory.is_authorized(session) || session.model.trim().is_empty() {
        return DoctorCheck::warning(
            "provider_reachability",
            "Provider reachability",
            format!(
                "The online {} probe was skipped until authorization and model selection are complete.",
                factory.metadata().display_name
            ),
            format!(
                "Run `/authorize {provider_id}`, select a model with `/model`, then rerun `/doctor online`."
            ),
        );
    }

    match tokio::time::timeout(Duration::from_secs(10), factory.probe_reachability(session)).await {
        Ok(Ok(())) => DoctorCheck::pass(
            "provider_reachability",
            "Provider reachability",
            format!(
                "{} answered a read-only discovery request.",
                factory.metadata().display_name
            ),
        ),
        Ok(Err(_)) | Err(_) => DoctorCheck::fail(
            "provider_reachability",
            "Provider reachability",
            format!(
                "{} did not complete the online reachability probe.",
                factory.metadata().display_name
            ),
            format!(
                "Check network/provider status, rerun `/authorize {provider_id}`, then rerun `/doctor online`."
            ),
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SandboxProbeOutcome {
    Enforced { network_denied: bool },
    Unconfined,
    OutsideWriteAllowed,
    NoExternalTarget,
    ExecutionFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SandboxProbeRun {
    confined: bool,
    network_denied: bool,
    status_success: bool,
    wrote_target: bool,
}

async fn sandbox_check(sandbox: &CommandSandbox, project_root: &Path) -> DoctorCheck {
    let backend = sandbox.backend();
    if !backend.is_available() {
        return DoctorCheck::warning(
            "sandbox",
            "Sandbox",
            "No supported OS sandbox backend is available.",
            "Keep autonomy at ask or conservative; on Linux install Bubblewrap, then rerun `/doctor`.",
        );
    }
    if !sandbox.is_active() {
        return DoctorCheck::warning(
            "sandbox",
            "Sandbox",
            format!("The {} backend is available but disabled.", backend.label()),
            "Run `/sandbox on`, then rerun `/doctor`.",
        );
    }
    let outcome = probe_sandbox_enforcement(sandbox, project_root).await;
    sandbox_probe_check(backend.label(), sandbox.deny_network(), outcome)
}

async fn probe_sandbox_enforcement(
    sandbox: &CommandSandbox,
    project_root: &Path,
) -> SandboxProbeOutcome {
    let Some(private_temp) = sandbox.temp_dir() else {
        return SandboxProbeOutcome::ExecutionFailed;
    };
    let Ok(control_dir) = tempfile::Builder::new()
        .prefix("doctor-sandbox-control-")
        .tempdir_in(private_temp)
    else {
        return SandboxProbeOutcome::ExecutionFailed;
    };
    let control_target = control_dir.path().join("write-probe");
    let Some(control) = run_sandbox_probe(sandbox, project_root, &control_target).await else {
        return SandboxProbeOutcome::ExecutionFailed;
    };
    if !control.confined {
        return SandboxProbeOutcome::Unconfined;
    }
    if !control.status_success || !control.wrote_target {
        return SandboxProbeOutcome::ExecutionFailed;
    }

    let Some(external_dir) = external_probe_dir(sandbox, project_root) else {
        return SandboxProbeOutcome::NoExternalTarget;
    };
    let external_target = external_dir.path().join("write-probe");
    let Some(denied) = run_sandbox_probe(sandbox, project_root, &external_target).await else {
        return SandboxProbeOutcome::ExecutionFailed;
    };
    if !denied.confined {
        return SandboxProbeOutcome::Unconfined;
    }
    if denied.wrote_target {
        return SandboxProbeOutcome::OutsideWriteAllowed;
    }
    if denied.status_success {
        return SandboxProbeOutcome::ExecutionFailed;
    }

    SandboxProbeOutcome::Enforced {
        network_denied: control.network_denied && denied.network_denied,
    }
}

async fn run_sandbox_probe(
    sandbox: &CommandSandbox,
    project_root: &Path,
    target: &Path,
) -> Option<SandboxProbeRun> {
    let (mut command, decision) = sandbox.command("/bin/sh", SANDBOX_PROBE_SCRIPT, project_root);
    command
        .env("BONSAI_DOCTOR_PROBE", target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(SANDBOX_PROBE_TIMEOUT, command.status())
        .await
        .ok()?
        .ok()?;
    Some(SandboxProbeRun {
        confined: decision.confined,
        network_denied: decision.network_denied,
        status_success: status.success(),
        wrote_target: target.exists(),
    })
}

fn external_probe_dir(sandbox: &CommandSandbox, project_root: &Path) -> Option<tempfile::TempDir> {
    let writable_roots = sandbox.policy().writable_roots;
    if let Ok(candidate) = tempfile::Builder::new()
        .prefix("bonsai-doctor-denied-")
        .tempdir()
        && !is_within_writable_roots(candidate.path(), &writable_roots)
    {
        return Some(candidate);
    }

    let parent = project_root.parent()?;
    let candidate = tempfile::Builder::new()
        .prefix(".bonsai-doctor-denied-")
        .tempdir_in(parent)
        .ok()?;
    (!is_within_writable_roots(candidate.path(), &writable_roots)).then_some(candidate)
}

fn is_within_writable_roots(path: &Path, writable_roots: &[PathBuf]) -> bool {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    writable_roots
        .iter()
        .any(|root| canonical.starts_with(root))
}

fn sandbox_probe_check(
    backend_label: &str,
    network_deny_requested: bool,
    outcome: SandboxProbeOutcome,
) -> DoctorCheck {
    match outcome {
        SandboxProbeOutcome::Enforced {
            network_denied: false,
        } if network_deny_requested => DoctorCheck::warning(
            "sandbox",
            "Sandbox",
            format!(
                "{backend_label} blocked a write outside configured roots, but network isolation is unavailable."
            ),
            "Keep autonomy at ask on untrusted projects or use a host whose sandbox supports network isolation.",
        ),
        SandboxProbeOutcome::Enforced { network_denied } => DoctorCheck::pass(
            "sandbox",
            "Sandbox",
            format!(
                "{backend_label} allowed its control write and blocked a write outside configured roots; network is {}.",
                if network_denied {
                    "denied"
                } else {
                    "allowed by configuration"
                }
            ),
        ),
        SandboxProbeOutcome::Unconfined => DoctorCheck::fail(
            "sandbox",
            "Sandbox",
            format!(
                "The active {backend_label} backend launched the verification command without confinement."
            ),
            "Restart Bonsai and rerun `/doctor`; if it remains unconfined, keep autonomy at ask and report a sandbox bug.",
        ),
        SandboxProbeOutcome::OutsideWriteAllowed => DoctorCheck::fail(
            "sandbox",
            "Sandbox",
            format!(
                "The active {backend_label} backend allowed a write outside every configured writable root."
            ),
            "Keep autonomy at ask, restart Bonsai, and rerun `/doctor`; report a sandbox bug if the write remains allowed.",
        ),
        SandboxProbeOutcome::NoExternalTarget => DoctorCheck::fail(
            "sandbox",
            "Sandbox",
            "No temporary location outside the configured writable roots was available for an enforcement probe.",
            "Narrow `[sandbox].writable_roots` and BONSAI_SANDBOX_WRITABLE_ROOTS, ensure the system temp directory is writable, then rerun `/doctor`.",
        ),
        SandboxProbeOutcome::ExecutionFailed => DoctorCheck::fail(
            "sandbox",
            "Sandbox",
            format!(
                "The active {backend_label} backend could not complete its control and denied-write probes."
            ),
            "Verify `/bin/sh` and temporary-directory access, restart Bonsai, then rerun `/doctor`.",
        ),
    }
}

fn executable_check(
    id: &'static str,
    label: &'static str,
    executable: &str,
    next_action: &'static str,
) -> DoctorCheck {
    if executable_exists(executable) {
        DoctorCheck::pass(id, label, format!("`{executable}` is available on PATH."))
    } else {
        DoctorCheck::warning(
            id,
            label,
            format!("`{executable}` is not available on PATH."),
            next_action,
        )
    }
}

fn language_server_check(project_root: &Path) -> DoctorCheck {
    let registry = crate::lsp::LanguageServerRegistry::builtin();
    let Some(file) = registry.first_supported_file(project_root) else {
        return DoctorCheck::pass(
            "language_server",
            "Language server",
            "No Rust, TypeScript/JavaScript, Python, or Go source file was detected.",
        );
    };
    let Some(spec) = registry.spec_for_path(&file) else {
        return DoctorCheck::pass(
            "language_server",
            "Language server",
            "No managed language server applies to this workspace.",
        );
    };
    let command = spec.command();
    let command_label = if Path::new(&command).components().count() > 1 {
        "the configured executable".to_string()
    } else {
        format!("`{command}`")
    };
    if executable_exists(&command) {
        DoctorCheck::pass(
            "language_server",
            "Language server",
            format!("{} server {command_label} is available.", spec.display_name),
        )
    } else {
        DoctorCheck::warning(
            "language_server",
            "Language server",
            format!(
                "{} source was detected but {command_label} is unavailable.",
                spec.display_name
            ),
            spec.install_hint.clone(),
        )
    }
}

fn mcp_check(config: &Config) -> DoctorCheck {
    let enabled = config
        .mcp_servers
        .values()
        .filter(|(server, _)| server.enabled)
        .count();
    if config.mcp_servers.is_empty() {
        DoctorCheck::pass("mcp", "MCP", "No MCP servers are configured.")
    } else {
        DoctorCheck::pass(
            "mcp",
            "MCP",
            format!(
                "{} server{} configured; {enabled} enabled.",
                config.mcp_servers.len(),
                if config.mcp_servers.len() == 1 {
                    ""
                } else {
                    "s"
                }
            ),
        )
    }
}

fn mcp_reachability_check(config: &Config, report: Option<&McpReachabilityReport>) -> DoctorCheck {
    let enabled = config
        .mcp_servers
        .values()
        .filter(|(server, _)| server.enabled)
        .count();
    if enabled == 0 {
        return DoctorCheck::pass(
            "mcp_reachability",
            "MCP reachability",
            "No enabled MCP servers to probe.",
        );
    }
    let Some(report) = report else {
        return DoctorCheck::fail(
            "mcp_reachability",
            "MCP reachability",
            "Enabled MCP servers could not be probed from this runtime.",
            "Restart Bonsai, inspect `/mcp`, then rerun `/doctor online`.",
        );
    };
    if report.enabled != enabled {
        return DoctorCheck::fail(
            "mcp_reachability",
            "MCP reachability",
            "The live MCP runtime does not match the enabled server configuration.",
            "Restart Bonsai so MCP configuration is reloaded, then rerun `/doctor online`.",
        );
    }
    if report.reachable == report.enabled {
        return DoctorCheck::pass(
            "mcp_reachability",
            "MCP reachability",
            format!(
                "All {} enabled MCP server{} answered discovery requests.",
                report.enabled,
                if report.enabled == 1 { "" } else { "s" }
            ),
        );
    }

    DoctorCheck::fail(
        "mcp_reachability",
        "MCP reachability",
        format!(
            "{}/{} enabled MCP servers answered; failed: {}.",
            report.reachable,
            report.enabled,
            report.failed_servers.join(", ")
        ),
        "Inspect `/mcp`; allow network with `/sandbox net off` for HTTP servers, run `/mcp reload <server>` for each failed server, then rerun `/doctor online`.",
    )
}

fn terminal_check() -> DoctorCheck {
    match crate::terminal::probe_native_pty() {
        Ok(()) => DoctorCheck::pass(
            "terminal",
            "Terminal",
            "Native pseudo-terminal allocation succeeded.",
        ),
        Err(_) => DoctorCheck::warning(
            "terminal",
            "Terminal",
            "Native pseudo-terminal allocation failed.",
            "Use non-interactive bash commands and verify the host exposes a PTY device before rerunning `/doctor`.",
        ),
    }
}

fn release_check(
    network_mode: DoctorNetworkMode,
    release_status: Option<&ReleaseStatus>,
) -> DoctorCheck {
    if network_mode == DoctorNetworkMode::Offline {
        return match crate::release::identity() {
            ReleaseIdentity::Official { tag } => DoctorCheck::pass(
                "release",
                "Release",
                format!(
                    "Official release metadata for {tag} is embedded; online signature verification was not requested."
                ),
            ),
            ReleaseIdentity::Development => DoctorCheck::warning(
                "release",
                "Release",
                format!(
                    "Running development build {}; no official signing identity is embedded.",
                    env!("CARGO_PKG_VERSION")
                ),
                "Install a published Bonsai release before relying on release provenance.",
            ),
            ReleaseIdentity::Invalid => DoctorCheck::fail(
                "release",
                "Release",
                "The embedded release tag and verification key are incomplete or inconsistent.",
                "Reinstall Bonsai from a published release and rerun `bonsai doctor`.",
            ),
        };
    }

    match release_status {
        Some(ReleaseStatus::VerifiedCurrent { version }) => DoctorCheck::pass(
            "release",
            "Release",
            format!(
                "Bonsai {version} matches its signed release manifest and is the newest signed release."
            ),
        ),
        Some(ReleaseStatus::UpdateAvailable { current, latest }) => DoctorCheck::warning(
            "release",
            "Release",
            format!(
                "Bonsai {current} matches its signed manifest; signed release {latest} is available."
            ),
            "Install the newer release through the documented installer or package manager, then rerun `/doctor online`.",
        ),
        Some(ReleaseStatus::DevelopmentBuild) => DoctorCheck::warning(
            "release",
            "Release",
            format!(
                "Running development build {}; signed update lookup was skipped.",
                env!("CARGO_PKG_VERSION")
            ),
            "Install a published Bonsai release before relying on update status.",
        ),
        Some(ReleaseStatus::LookupFailed) => DoctorCheck::warning(
            "release",
            "Release",
            "The signed release lookup did not complete; no update claim was made.",
            "Check GitHub connectivity and rerun `/doctor online`.",
        ),
        Some(ReleaseStatus::VerificationFailed) => DoctorCheck::fail(
            "release",
            "Release",
            "The installed binary or fetched release metadata did not match a valid signed manifest.",
            "Do not update from the fetched assets; reinstall from the documented release channel and rerun `/doctor online`.",
        ),
        None => DoctorCheck::fail(
            "release",
            "Release",
            "Online release verification was requested but no verification result is available.",
            "Restart Bonsai and rerun `/doctor online`.",
        ),
    }
}

fn executable_exists(command: &str) -> bool {
    let command_path = Path::new(command);
    if command_path.components().count() > 1 {
        return is_executable_file(command_path);
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| {
        executable_candidates(&directory, command)
            .into_iter()
            .any(|candidate| is_executable_file(&candidate))
    })
}

fn executable_candidates(directory: &Path, command: &str) -> Vec<PathBuf> {
    let direct = directory.join(command);
    #[cfg(windows)]
    {
        let mut candidates = vec![direct.clone()];
        if direct.extension().is_none() {
            let extensions = std::env::var_os("PATHEXT")
                .map(|value| {
                    value
                        .to_string_lossy()
                        .split(';')
                        .filter(|extension| !extension.is_empty())
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| vec![".EXE".to_string(), ".CMD".to_string()]);
            candidates.extend(
                extensions
                    .into_iter()
                    .map(|extension| directory.join(format!("{command}{extension}"))),
            );
        }
        candidates
    }
    #[cfg(not(windows))]
    {
        vec![direct]
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_request_parser_accepts_options_in_either_order_and_is_strict() {
        assert_eq!(
            DoctorRequest::parse_slash_args(""),
            Ok(DoctorRequest::default())
        );
        assert_eq!(
            DoctorRequest::parse_slash_args("json online"),
            Ok(DoctorRequest {
                format: DoctorOutputFormat::Json,
                network: DoctorNetworkMode::Online,
            })
        );
        assert_eq!(
            DoctorRequest::parse_slash_args("online json"),
            Ok(DoctorRequest {
                format: DoctorOutputFormat::Json,
                network: DoctorNetworkMode::Online,
            })
        );
        assert_eq!(
            DoctorRequest::parse_slash_args("online online"),
            Err("Usage: /doctor [json] [online]")
        );
        assert_eq!(
            DoctorRequest::parse_slash_args("--json"),
            Err("Usage: /doctor [json] [online]")
        );
    }

    #[test]
    fn report_redacts_dynamic_text_and_gives_every_problem_an_action() {
        let secret = format!("sk-{}", "a".repeat(40));
        let report = DoctorReport::new(vec![
            DoctorCheck::pass("ok", "OK", "ready"),
            DoctorCheck::warning("warning", "Warning", secret, "fix it"),
            DoctorCheck::fail("fail", "Fail", "broken", "repair it"),
        ]);

        let json = report.render(DoctorOutputFormat::Json).unwrap();
        assert!(!json.contains("sk-"));
        assert!(json.contains("[REDACTED:OpenAI API key]"));
        assert_eq!(report.summary.passed, 1);
        assert_eq!(report.summary.warnings, 1);
        assert_eq!(report.summary.failed, 1);
        assert!(
            report
                .checks
                .iter()
                .filter(|check| check.status != DoctorStatus::Pass)
                .all(|check| check.next_action.is_some())
        );
    }

    #[test]
    fn malformed_layer_becomes_an_actionable_configuration_warning() {
        let mut config = Config::empty();
        config.diagnostics.push(crate::config::ConfigDiagnostic {
            source: crate::config::ConfigSource::Project,
            path: PathBuf::from("/workspace/.bonsai/config.toml"),
            scope: "<file>".to_string(),
            message: "failed to parse TOML: expected `]`".to_string(),
        });

        let check = config_check(&config);

        assert_eq!(check.status, DoctorStatus::Warning);
        assert!(check.summary.contains("1 configuration entry was skipped"));
        assert!(
            check
                .next_action
                .as_deref()
                .is_some_and(|action| action.contains("/config validate"))
        );
    }

    #[test]
    fn signed_release_results_map_to_actionable_doctor_states() {
        let current = release_check(
            DoctorNetworkMode::Online,
            Some(&ReleaseStatus::VerifiedCurrent {
                version: "1.0.0".to_string(),
            }),
        );
        assert_eq!(current.status, DoctorStatus::Pass);
        assert!(current.next_action.is_none());

        let update = release_check(
            DoctorNetworkMode::Online,
            Some(&ReleaseStatus::UpdateAvailable {
                current: "1.0.0".to_string(),
                latest: "1.0.1".to_string(),
            }),
        );
        assert_eq!(update.status, DoctorStatus::Warning);
        assert!(update.next_action.is_some());

        let invalid = release_check(
            DoctorNetworkMode::Online,
            Some(&ReleaseStatus::VerificationFailed),
        );
        assert_eq!(invalid.status, DoctorStatus::Fail);
        assert!(invalid.next_action.is_some());
    }

    #[test]
    fn sandbox_probe_outcomes_are_strict_and_actionable() {
        let enforced = sandbox_probe_check(
            "test",
            true,
            SandboxProbeOutcome::Enforced {
                network_denied: true,
            },
        );
        assert_eq!(enforced.status, DoctorStatus::Pass);
        assert!(enforced.next_action.is_none());

        let degraded = sandbox_probe_check(
            "test",
            true,
            SandboxProbeOutcome::Enforced {
                network_denied: false,
            },
        );
        assert_eq!(degraded.status, DoctorStatus::Warning);
        assert!(degraded.next_action.is_some());

        for outcome in [
            SandboxProbeOutcome::Unconfined,
            SandboxProbeOutcome::OutsideWriteAllowed,
            SandboxProbeOutcome::NoExternalTarget,
            SandboxProbeOutcome::ExecutionFailed,
        ] {
            let check = sandbox_probe_check("test", true, outcome);
            assert_eq!(check.status, DoctorStatus::Fail);
            assert!(check.next_action.is_some());
        }
    }

    #[test]
    fn sandbox_probe_target_must_be_outside_every_writable_root() {
        let temp = tempfile::TempDir::new().unwrap();
        let allowed = temp.path().join("allowed");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        assert!(is_within_writable_roots(
            &allowed.join("nested"),
            std::slice::from_ref(&allowed),
        ));
        assert!(!is_within_writable_roots(
            &outside,
            std::slice::from_ref(&allowed),
        ));
    }

    #[tokio::test]
    async fn storage_health_probe_does_not_leave_durable_state() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Storage::open_at(temp.path().join("bonsai.db"))
            .await
            .unwrap();

        storage.health_check().await.unwrap();
        storage.health_check().await.unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn executable_lookup_requires_execute_permission() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("tool");
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&path, permissions).unwrap();
        assert!(!is_executable_file(&path));

        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();
        assert!(is_executable_file(&path));
    }
}
