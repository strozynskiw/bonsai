//! Plan-phase execution and advancement regressions.

use super::*;

pub(super) fn two_phase_plan() -> crate::plan::PlanDoc {
    let mut plan = crate::plan::PlanDoc::default();
    plan.edit().set_title("Phased");
    plan.edit().add_phase("Phase 1");
    plan.edit().add_phase("Phase 2");
    plan.edit().add_task_to_phase("Phase 1", "do a").unwrap();
    plan.edit().add_task_to_phase("Phase 2", "do b").unwrap();
    plan
}

pub(super) async fn drain_tasks(tasks: &mut TaskController) {
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if tasks.poll_finished().await.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
}

// Regression: phase advancement must fire from the per-frame driver, with the
// finished phase marked done, the next phase's todos seeded, and its run spawned.
#[tokio::test]
async fn maybe_advance_plan_phase_continues_to_next_phase() {
    use crate::tui::app::{PhaseAdvance, PlanExecution};

    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let plan = two_phase_plan();
    let plan_store = Arc::new(Mutex::new(plan.clone()));
    let todo_store = Arc::new(Mutex::new(crate::todo::TodoStore::new()));

    let mut app = app();
    app.plan = plan;
    app.plan_execution = Some(PlanExecution { phase_index: 0 });
    app.phase_advance = Some(PhaseAdvance::Continue);
    app.task_state = TaskState::Idle;
    let mut repo_map = empty_repo_map_injector();

    let advanced = maybe_advance_plan_phase(
        &mut app,
        &mut tasks,
        test_agent(Box::new(CompleteProvider)),
        Arc::new(NullSink),
        todo_store.clone(),
        plan_store.clone(),
        &mut repo_map,
    )
    .await;

    assert!(advanced, "a pending next phase should advance");
    assert_eq!(app.plan_execution, Some(PlanExecution { phase_index: 1 }));
    assert_eq!(app.task_state, TaskState::Running);
    assert!(tasks.is_busy(), "the next phase's run should be spawned");
    assert!(app.phase_advance.is_none(), "phase_advance is consumed");
    assert!(
        plan_store.lock().await.phases[0]
            .tasks
            .iter()
            .all(|t| t.done),
        "the finished phase is marked done"
    );
    let todos = todo_store.lock().await.todos().to_vec();
    assert_eq!(todos.len(), 1);
    assert_eq!(
        todos[0].content, "do b",
        "only the next phase's todos are seeded"
    );
    assert!(
        app.transcript.is_empty(),
        "phase progress is represented by the plan surface, not transcript rows"
    );

    drain_tasks(&mut tasks).await;
}

// Halt (error/interrupt) clears execution and spawns nothing; the phase is not
// marked done so /continue can resume from it.
#[tokio::test]
async fn maybe_advance_plan_phase_halts_without_advancing() {
    use crate::tui::app::{PhaseAdvance, PlanExecution};

    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let plan = two_phase_plan();
    let plan_store = Arc::new(Mutex::new(plan.clone()));

    let mut app = app();
    app.plan = plan;
    app.plan_execution = Some(PlanExecution { phase_index: 0 });
    app.phase_advance = Some(PhaseAdvance::Halt);
    app.task_state = TaskState::Idle;
    let mut repo_map = empty_repo_map_injector();

    let advanced = maybe_advance_plan_phase(
        &mut app,
        &mut tasks,
        test_agent(Box::new(CompleteProvider)),
        Arc::new(NullSink),
        Arc::new(Mutex::new(crate::todo::TodoStore::new())),
        plan_store.clone(),
        &mut repo_map,
    )
    .await;

    assert!(!advanced);
    assert!(app.plan_execution.is_none(), "halt clears execution state");
    assert!(!tasks.is_busy(), "halt spawns no run");
    assert!(
        plan_store.lock().await.phases[0]
            .tasks
            .iter()
            .all(|t| !t.done),
        "halt leaves the phase resumable"
    );
}

// After the last phase, execution completes cleanly with no further run.
#[tokio::test]
async fn maybe_advance_plan_phase_completes_after_final_phase() {
    use crate::tui::app::{PhaseAdvance, PlanExecution};

    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let mut plan = crate::plan::PlanDoc::default();
    plan.edit().add_phase("Only phase");
    plan.edit().add_task_to_phase("Only phase", "do a").unwrap();
    let plan_store = Arc::new(Mutex::new(plan.clone()));

    let mut app = app();
    app.plan = plan;
    app.plan_execution = Some(PlanExecution { phase_index: 0 });
    app.phase_advance = Some(PhaseAdvance::Continue);
    app.task_state = TaskState::Idle;
    let mut repo_map = empty_repo_map_injector();

    let advanced = maybe_advance_plan_phase(
        &mut app,
        &mut tasks,
        test_agent(Box::new(CompleteProvider)),
        Arc::new(NullSink),
        Arc::new(Mutex::new(crate::todo::TodoStore::new())),
        plan_store.clone(),
        &mut repo_map,
    )
    .await;

    assert!(!advanced, "no further phase to advance to");
    assert!(app.plan_execution.is_none(), "execution completes");
    assert!(!tasks.is_busy());
    assert!(
        plan_store.lock().await.phases[0]
            .tasks
            .iter()
            .all(|t| t.done)
    );
    assert!(
        app.transcript.is_empty(),
        "plan completion is represented by the canvas, not a transcript row"
    );
}

// Self-gating: while a run is still in flight the signal is left intact (not
// consumed), so the next idle frame can act on it. This is what makes the
// unconditional per-frame call safe against the reap-before-reduce ordering.
#[tokio::test]
async fn maybe_advance_plan_phase_preserves_signal_while_busy() {
    use crate::tui::app::{PhaseAdvance, PlanExecution};

    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let plan = two_phase_plan();
    let plan_store = Arc::new(Mutex::new(plan.clone()));

    // Occupy the controller so is_busy() is true.
    tasks
        .start_implement_plan(
            test_agent(Box::new(BlockingProvider)),
            plan.clone(),
            Arc::new(NullSink),
            Some(0),
        )
        .unwrap();

    let mut app = app();
    app.plan = plan;
    app.plan_execution = Some(PlanExecution { phase_index: 0 });
    app.phase_advance = Some(PhaseAdvance::Continue);
    app.task_state = TaskState::Idle;
    let mut repo_map = empty_repo_map_injector();

    let advanced = maybe_advance_plan_phase(
        &mut app,
        &mut tasks,
        test_agent(Box::new(CompleteProvider)),
        Arc::new(NullSink),
        Arc::new(Mutex::new(crate::todo::TodoStore::new())),
        plan_store,
        &mut repo_map,
    )
    .await;

    assert!(!advanced, "must not advance while a run is in flight");
    assert_eq!(
        app.phase_advance,
        Some(PhaseAdvance::Continue),
        "the signal is preserved, not consumed, so a later idle frame still advances"
    );

    tasks.cancel();
    drain_tasks(&mut tasks).await;
}
