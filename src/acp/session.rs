//! Session lifecycle for one ACP connection, delegated to `server::Controller`.
//!
//! Editors multiplex threads over one agent process (Zed opens a new thread on
//! the same connection), so each ACP session gets its own controller — one
//! loaded session per controller, exactly as `bonsai serve` runs it. Idle
//! sessions beyond [`MAX_LOADED`] are drained, least recently used first.
//!
//! Workspace trust, sandbox, budgets and the approval level come from bonsai's
//! own config via `server::prepare_runtime`; the client contributes only a
//! working directory, honored when it is the launch directory or an
//! already-trusted workspace.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use agent_client_protocol::v1::schema::{McpServer, SessionId as AcpSessionId};
use agent_client_protocol::{Error, ErrorCode};
use tokio::sync::Mutex;

use crate::resource::command::CommandRegistry;
use crate::server::controller::{ClaimError, Controller};
use crate::storage::{SessionId, SessionRunSelection, Storage};
use crate::tool::ApprovalLevel;
use crate::workspace_trust::{WorkspaceTrust, WorkspaceTrustGate};

/// Longest a load waits for a dead owner's heartbeat to go stale: the 15 s
/// liveness threshold plus one 3 s heartbeat interval and slack.
const CLAIM_FENCE_WAIT: Duration = Duration::from_secs(20);

/// Loaded sessions kept per connection before idle ones are drained.
const MAX_LOADED: usize = 8;

/// Everything a turn needs, cloned out so no lock is held while it runs.
#[derive(Clone)]
pub(crate) struct Handle {
    pub(crate) controller: Arc<Controller>,
    pub(crate) id: SessionId,
    pub(crate) acp_id: AcpSessionId,
    pub(crate) root: PathBuf,
    /// The client's own spelling of the root (symlinks unresolved), used for
    /// every path sent back so editors can match their open buffers.
    pub(crate) display_root: PathBuf,
    pub(crate) cancelled: Arc<AtomicBool>,
    /// Models of every authorized connection, as the TUI's `/model` lists them.
    pub(crate) models: Arc<Vec<ModelGroup>>,
    pub(crate) selection: Arc<std::sync::Mutex<SessionRunSelection>>,
    /// The live approval level every tool of this session reads.
    pub(crate) autonomy: crate::yolo::YoloMode,
    /// Highest level the client may select: the level the operator saved in
    /// the TUI. Yolo is reachable only when the operator already chose it.
    pub(crate) autonomy_ceiling: ApprovalLevel,
    /// Custom slash commands for this workspace, loaded within its trust
    /// boundary and advertised as `available_commands_update`.
    pub(crate) commands: Arc<CommandRegistry>,
}

/// One authorized connection and the models it offers.
#[derive(Debug, Clone)]
pub(crate) struct ModelGroup {
    pub(crate) provider_id: String,
    pub(crate) label: String,
    pub(crate) models: Vec<String>,
    reasoning: crate::provider::ReasoningSelection,
}

impl Handle {
    pub(crate) fn current_selection(&self) -> SessionRunSelection {
        self.selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

pub(crate) struct Sessions {
    launch_root: PathBuf,
    storage: Storage,
    /// Most recently used last.
    loaded: Mutex<Vec<Handle>>,
}

impl Sessions {
    pub(crate) fn new(launch_root: PathBuf, storage: Storage) -> Self {
        Self {
            launch_root,
            storage,
            loaded: Mutex::new(Vec::new()),
        }
    }

    /// `session/new`: durable create + claim, never a model call.
    pub(crate) async fn create(
        &self,
        cwd: &Path,
        mcp_servers: &[McpServer],
    ) -> Result<Handle, Error> {
        reject_client_mcp(mcp_servers)?;
        let root = self.resolve_root(cwd).await?;
        let prepared = self.controller(&root).await?;
        let id = prepared
            .controller
            .create(&prepared.selection)
            .await
            .map_err(internal)?;
        self.claim(prepared, root, cwd, id).await
    }

    /// `session/load`: reuse the session if this connection already holds
    /// it, otherwise claim it from storage (same workspace only).
    pub(crate) async fn load(
        &self,
        acp_id: &AcpSessionId,
        cwd: &Path,
        mcp_servers: &[McpServer],
    ) -> Result<Handle, Error> {
        reject_client_mcp(mcp_servers)?;
        let id = parse_id(acp_id)?;
        let root = self.resolve_root(cwd).await?;
        if let Ok(handle) = self.handle(acp_id).await {
            if handle.root != root {
                return Err(invalid("session belongs to a different workspace"));
            }
            return Ok(handle);
        }
        let prepared = self.controller(&root).await?;
        self.claim(prepared, root, cwd, id).await
    }

    pub(crate) async fn handle(&self, acp_id: &AcpSessionId) -> Result<Handle, Error> {
        let id = parse_id(acp_id)?;
        let mut loaded = self.loaded.lock().await;
        let index = loaded
            .iter()
            .position(|handle| handle.id == id)
            .ok_or_else(|| not_found(acp_id))?;
        let handle = loaded.remove(index);
        loaded.push(handle.clone());
        Ok(handle)
    }

    /// Drain every session on disconnect; ownership is retained on a failed drain.
    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        let loaded = std::mem::take(&mut *self.loaded.lock().await);
        let results = futures::future::join_all(
            loaded
                .iter()
                .map(|handle| handle.controller.shutdown(Duration::from_secs(30))),
        )
        .await;
        results.into_iter().collect()
    }

    /// A fresh controller built from bonsai's own config for `root`.
    async fn controller(&self, root: &Path) -> Result<Prepared, Error> {
        let (factory, selection) = crate::server::prepare_runtime(root, &self.storage)
            .await
            .map_err(internal)?;
        let models = model_groups(&factory);
        let autonomy = factory.autonomy.clone();
        // Project commands are repository-authored prompts: inert until the
        // workspace is trusted, the same rule the runtime applies to skills.
        let commands = Arc::new(CommandRegistry::load(
            root,
            matches!(factory.trust.state(), WorkspaceTrust::Trusted),
        ));
        let saved = self
            .storage
            .approval_level()
            .await
            .map_err(internal)?
            .unwrap_or_default();
        let controller =
            Controller::new(root, self.storage.clone(), Arc::new(factory)).map_err(internal)?;
        Ok(Prepared {
            controller: Arc::new(controller),
            selection,
            models,
            autonomy,
            autonomy_ceiling: saved,
            commands,
        })
    }

    async fn claim(
        &self,
        prepared: Prepared,
        root: PathBuf,
        cwd: &Path,
        id: SessionId,
    ) -> Result<Handle, Error> {
        let Prepared {
            controller,
            selection,
            models,
            autonomy,
            autonomy_ceiling,
            commands,
        } = prepared;
        // An owner that died without draining (an editor killed on quit)
        // leaves a fresh heartbeat; the session becomes claimable once it
        // goes stale. Wait that fence out instead of failing the reopen.
        let deadline = tokio::time::Instant::now() + CLAIM_FENCE_WAIT;
        loop {
            match controller.resume(id, &selection).await {
                Ok(()) => break,
                Err(error)
                    if error.downcast_ref::<ClaimError>() == Some(&ClaimError::Live)
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(error) => {
                    return Err(Error::new(
                        ErrorCode::RESOURCE_NOT_FOUND,
                        format!(
                            "Session {} cannot be loaded: {}",
                            id.as_i64(),
                            crate::redact::redact(&format!("{error:#}"))
                        ),
                    ));
                }
            }
        }
        let handle = Handle {
            controller,
            id,
            acp_id: AcpSessionId::new(id.as_i64().to_string()),
            root,
            display_root: cwd.to_path_buf(),
            cancelled: Arc::new(AtomicBool::new(false)),
            models: Arc::new(models),
            selection: Arc::new(std::sync::Mutex::new(selection)),
            autonomy,
            autonomy_ceiling,
            commands,
        };
        let mut loaded = self.loaded.lock().await;
        loaded.push(handle.clone());
        self.evict(&mut loaded).await;
        Ok(handle)
    }

    /// `session/list`: this workspace's sessions, most recent first, as the
    /// TUI's resume list shows them. A client `cwd` follows the same trust
    /// rule as `session/new`; no cwd means the launch directory.
    pub(crate) async fn list(
        &self,
        cwd: Option<&Path>,
    ) -> Result<Vec<agent_client_protocol::v1::schema::SessionInfo>, Error> {
        let (root, display) = match cwd {
            Some(cwd) => (self.resolve_root(cwd).await?, cwd.to_path_buf()),
            None => (self.launch_root.clone(), self.launch_root.clone()),
        };
        let sessions = self
            .storage
            .recent_sessions_for_project(&root, LIST_LIMIT)
            .await
            .map_err(internal)?;
        Ok(sessions
            .into_iter()
            .map(|summary| {
                agent_client_protocol::v1::schema::SessionInfo::new(
                    AcpSessionId::new(summary.id.as_i64().to_string()),
                    display.clone(),
                )
                .title(session_title(&summary))
                .updated_at(rfc3339_utc(summary.updated_at_ms))
            })
            .collect())
    }

    /// `session/set_config_option` for `model`: only a model of an
    /// authorized connection, only while idle. The session is released and
    /// claimed again with the new selection, the same path `resume` takes.
    pub(crate) async fn set_model(
        &self,
        acp_id: &AcpSessionId,
        value: &str,
    ) -> Result<Handle, Error> {
        let handle = self.handle(acp_id).await?;
        let (provider_id, model) = value
            .split_once(MODEL_VALUE_SEPARATOR)
            .ok_or_else(|| invalid("unknown model"))?;
        let group = handle
            .models
            .iter()
            .find(|g| g.provider_id == provider_id && g.models.iter().any(|m| m == model))
            .ok_or_else(|| invalid("model is not offered by an authorized connection"))?;
        let current = handle.current_selection();
        if current.provider_id == provider_id && current.model == model {
            return Ok(handle);
        }
        let next = SessionRunSelection {
            provider_id: provider_id.into(),
            model: model.into(),
            reasoning: group.reasoning,
        };
        handle.controller.release_idle().await.map_err(|_| {
            Error::new(
                ErrorCode::INVALID_REQUEST,
                "Wait for the current prompt to finish before switching models",
            )
        })?;
        if let Err(error) = handle.controller.resume(handle.id, &next).await {
            // Put the session back on its previous model rather than leave it unloaded.
            let restored = handle.controller.resume(handle.id, &current).await;
            if restored.is_err() {
                self.loaded.lock().await.retain(|h| h.id != handle.id);
            }
            return Err(internal(error));
        }
        *handle
            .selection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next;
        Ok(handle)
    }

    /// `session/set_config_option` for `autonomy`: any level from `ask` up
    /// to the operator ceiling, applied live to the session's tools.
    pub(crate) async fn set_autonomy(
        &self,
        acp_id: &AcpSessionId,
        value: &str,
    ) -> Result<Handle, Error> {
        let handle = self.handle(acp_id).await?;
        let level = ApprovalLevel::parse(value).ok_or_else(|| invalid("unknown autonomy level"))?;
        if level > handle.autonomy_ceiling {
            return Err(invalid(
                "autonomy above the level saved in bonsai cannot be selected from the editor",
            ));
        }
        handle.autonomy.set_level(level);
        Ok(handle)
    }

    /// Drain least recently used *idle* sessions above the cap. Busy ones are
    /// never cancelled to make room.
    async fn evict(&self, loaded: &mut Vec<Handle>) {
        let mut index = 0;
        while loaded.len() > MAX_LOADED && index + 1 < loaded.len() {
            if loaded[index].controller.release_idle().await.is_ok() {
                let evicted = loaded.remove(index);
                if let Err(error) = evicted.controller.shutdown(Duration::from_secs(5)).await {
                    tracing::warn!(%error, "failed to drain an evicted ACP session");
                }
            } else {
                index += 1;
            }
        }
    }

    /// The launch directory is the operator's choice (the editor spawned us
    /// there). Any other directory must already be a trusted workspace:
    /// a client cannot point bonsai at a fresh repository and inherit trust.
    async fn resolve_root(&self, cwd: &Path) -> Result<PathBuf, Error> {
        if !cwd.is_absolute() {
            return Err(invalid("cwd must be an absolute path"));
        }
        let root = cwd
            .canonicalize()
            .map_err(|_| invalid("cwd does not exist"))?;
        if !root.is_dir() {
            return Err(invalid("cwd is not a directory"));
        }
        if root == self.launch_root {
            return Ok(root);
        }
        let project = self.storage.ensure_project(&root).await.map_err(internal)?;
        let trust = WorkspaceTrustGate::load(self.storage.clone(), project)
            .await
            .map_err(internal)?;
        if trust.state() == WorkspaceTrust::Trusted {
            Ok(root)
        } else {
            Err(invalid(
                "cwd is not a trusted bonsai workspace; open it with `bonsai` once and trust it, \
                 or start `bonsai acp` from that directory",
            ))
        }
    }
}

/// Sessions returned by `session/list` (one page; no cursor is issued).
const LIST_LIMIT: i64 = 200;

/// Summary, else the latest task's prompt, else a numbered placeholder.
fn session_title(summary: &crate::storage::SessionSummary) -> String {
    let text = if summary.summary.trim().is_empty() {
        summary
            .latest_task
            .as_ref()
            .map(|task| task.goal.as_str())
            .unwrap_or_default()
    } else {
        summary.summary.as_str()
    };
    let line = text.lines().map(str::trim).find(|l| !l.is_empty());
    match line {
        Some(line) if line.chars().count() > 80 => {
            format!("{}…", line.chars().take(80).collect::<String>())
        }
        Some(line) => line.to_string(),
        None => format!("Session {}", summary.id.as_i64()),
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` from Unix milliseconds (proleptic Gregorian).
fn rfc3339_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// A runtime built from bonsai's config, before a session is claimed on it.
struct Prepared {
    controller: Arc<Controller>,
    selection: SessionRunSelection,
    models: Vec<ModelGroup>,
    autonomy: crate::yolo::YoloMode,
    autonomy_ceiling: ApprovalLevel,
    commands: Arc<CommandRegistry>,
}

/// Separates connection id and model id in a config value; model ids may
/// themselves contain `/`.
pub(crate) const MODEL_VALUE_SEPARATOR: &str = "::";

/// Models of authorized connections, in registry order — the TUI `/model` list.
fn model_groups(runtime: &crate::server::controller::SessionRuntime) -> Vec<ModelGroup> {
    let mut seen = std::collections::HashSet::new();
    runtime
        .registry
        .all()
        .iter()
        // A connection id registered twice (built-in + override) is one choice.
        .filter(|factory| seen.insert(factory.metadata().id.to_string()))
        .filter_map(|factory| {
            let metadata = factory.metadata();
            let provider_id = metadata.id.as_ref();
            let provider = runtime.session.providers.get(provider_id)?;
            if !factory.is_authorized(provider) {
                return None;
            }
            // Canonical catalog ids, as the TUI shows and saves them (sessions
            // store `deepseek/deepseek-flash`, not the wire name).
            let mut seen_models = std::collections::HashSet::new();
            let models: Vec<String> = crate::model_catalog::available_model_ids_for_provider(
                Some(&runtime.catalog),
                provider_id,
                metadata,
                &provider.model,
            )
            .into_iter()
            .map(|model| {
                crate::model_resolution::resolved_model_for_provider_model(
                    Some(&runtime.catalog),
                    provider_id,
                    &model,
                )
                .map_or(model, |resolved| resolved.model_id.to_string())
            })
            .filter(|model| seen_models.insert(model.clone()))
            .collect();
            (!models.is_empty()).then(|| ModelGroup {
                provider_id: provider_id.to_string(),
                label: metadata.display_name.to_string(),
                models,
                reasoning: provider.reasoning,
            })
        })
        .collect()
}

/// Client-supplied MCP servers would run client-chosen processes under
/// bonsai's authority. Refused until there is an explicit trust story.
fn reject_client_mcp(servers: &[McpServer]) -> Result<(), Error> {
    if servers.is_empty() {
        return Ok(());
    }
    Err(invalid(
        "bonsai does not start client-supplied MCP servers; configure MCP servers in bonsai \
         (see docs/mcp.md) and remove them from the editor's agent settings",
    ))
}

fn parse_id(acp_id: &AcpSessionId) -> Result<SessionId, Error> {
    acp_id
        .0
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .map(SessionId::from_raw)
        .ok_or_else(|| not_found(acp_id))
}

fn not_found(acp_id: &AcpSessionId) -> Error {
    Error::new(
        ErrorCode::RESOURCE_NOT_FOUND,
        format!("Unknown session {}", acp_id.0),
    )
}

pub(crate) fn invalid(message: &str) -> Error {
    Error::new(ErrorCode::INVALID_PARAMS, message)
}

pub(crate) fn internal(error: anyhow::Error) -> Error {
    Error::new(
        ErrorCode::INTERNAL_ERROR,
        crate::redact::redact(&format!("{error:#}")).into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::rfc3339_utc;

    #[test]
    fn formats_unix_milliseconds_as_rfc3339_utc() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_utc(1_794_934_681_498), "2026-11-17T16:58:01Z");
    }
}
