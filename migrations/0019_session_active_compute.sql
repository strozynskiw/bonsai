-- Issue #146 enforcement half: the default session active-time budget consumes
-- model, active tool, and child-active time, not the whole wall clock. That
-- reading has to survive a resume, so it gets its own durable counter rather
-- than reusing `active_run_ms`: the heartbeat union behind that column is what
-- the explicit `wall_clock` policy consumes, and its open-segment and recovery
-- invariants only hold for the union clock.
--
-- `checkpoint_task_run_timing` adds each checkpoint's own compute delta here in
-- the same transaction that records it on the task run, so a resumed session
-- enforces its budget against what it has already spent instead of from zero.
ALTER TABLE sessions ADD COLUMN active_compute_ms INTEGER NOT NULL DEFAULT 0
  CHECK (active_compute_ms >= 0);
