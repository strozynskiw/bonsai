//! Shared durable session lifecycle, independent of a display or transport.
use anyhow::{Context, Result, bail};

use crate::agent::Agent;
use crate::protocol::TranscriptItem;
use crate::storage::{SessionId, SessionRunSelection, SessionSnapshot, Storage};

/// Claim an existing session without starting execution.
pub(crate) async fn claim(
    storage: &Storage,
    root: &std::path::Path,
    id: SessionId,
    selection: &SessionRunSelection,
) -> Result<()> {
    match storage
        .claim_session_for_resume_with_selection(root, id, selection)
        .await?
    {
        crate::storage::ResumeSessionOutcome::Resumed => Ok(()),
        crate::storage::ResumeSessionOutcome::Live => {
            bail!("Session #{id} is live in another bonsai process.")
        }
        crate::storage::ResumeSessionOutcome::NotFound => bail!("No session #{id}."),
        crate::storage::ResumeSessionOutcome::DifferentProject => {
            bail!("Session #{id} belongs to a different project.")
        }
    }
}

/// Restore shared engine state without running the model or a goal.
pub(crate) async fn restore(
    agent: &mut Agent,
    storage: &Storage,
    id: SessionId,
    todos: &crate::todo::SharedTodoStore,
    plan: &crate::plan::SharedPlanStore,
    snapshot: Option<&SessionSnapshot>,
) -> Result<()> {
    let key = storage.conversation_cache_key(id).await?;
    agent.set_conversation_cache_key(&key);
    if let Some(snapshot) = snapshot {
        todos.lock().await.set_todos(snapshot.todos.clone());
        *plan.lock().await = snapshot.plan.clone();
        crate::session_persist::restore_agent_state(agent, snapshot).await;
    }
    Ok(())
}

/// Persist the complete transcript and cumulative engine ledger before publishing completion.
pub(crate) async fn persist(
    storage: &Storage,
    id: SessionId,
    agent: &Agent,
    todos: &crate::todo::SharedTodoStore,
    plan: &crate::plan::SharedPlanStore,
    transcript: &[TranscriptItem],
) -> Result<()> {
    let state = crate::session_persist::AgentStateSnapshot::capture(agent);
    let todos = todos.lock().await.todos().to_vec();
    let plan = plan.lock().await.clone();
    let snapshot = crate::session_persist::SessionSnapshotData {
        transcript,
        plan: &plan,
        todos: &todos,
        agent: Some(&state),
        fallback_usage: None,
        ui_peer_delivery_receipts: &[],
        agent_peer_delivery_receipts: state.peer_delivery_receipts(),
    };
    crate::session_persist::SessionSnapshotWriter::new(storage, id)
        .persist(snapshot, Default::default())
        .await
        .context("Failed to persist session snapshot")?;
    Ok(())
}
