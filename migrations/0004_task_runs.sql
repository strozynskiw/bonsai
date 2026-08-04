-- A durable chat session describes process lifecycle. Task runs describe whether
-- a concrete user goal was achieved and intentionally survive session resumes.
CREATE TABLE task_runs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  episode_seq INTEGER CHECK (episode_seq IS NULL OR episode_seq > 0),
  goal_id TEXT NOT NULL CHECK (trim(goal_id) <> '' AND length(goal_id) <= 128),
  goal TEXT NOT NULL CHECK (trim(goal) <> '' AND length(goal) <= 4096),
  outcome TEXT CHECK (
    outcome IS NULL OR outcome IN (
      'succeeded', 'blocked', 'failed', 'cancelled', 'superseded', 'unknown'
    )
  ),
  terminal_reason_code TEXT CHECK (
    terminal_reason_code IS NULL OR terminal_reason_code IN (
      'goal_superseded',
      'user_cancelled',
      'budget_exhausted',
      'provider_failure',
      'execution_failure',
      'verification_failure',
      'process_interrupted',
      'session_ended'
    )
  ),
  terminal_reason_detail TEXT CHECK (
    terminal_reason_detail IS NULL OR length(terminal_reason_detail) <= 1024
  ),
  started_at_ms INTEGER NOT NULL CHECK (started_at_ms >= 0),
  ended_at_ms INTEGER,
  CHECK (ended_at_ms IS NULL OR ended_at_ms >= started_at_ms),
  CHECK (
    (outcome IS NULL
      AND terminal_reason_code IS NULL
      AND terminal_reason_detail IS NULL
      AND ended_at_ms IS NULL)
    OR
    (outcome IN ('succeeded', 'unknown')
      AND terminal_reason_code IS NULL
      AND terminal_reason_detail IS NULL
      AND ended_at_ms IS NOT NULL)
    OR
    (outcome IN ('blocked', 'failed', 'cancelled', 'superseded')
      AND trim(terminal_reason_code) <> ''
      AND trim(terminal_reason_detail) <> ''
      AND ended_at_ms IS NOT NULL)
  )
);

CREATE UNIQUE INDEX idx_task_runs_one_active_per_session
  ON task_runs(session_id) WHERE outcome IS NULL;
CREATE INDEX idx_task_runs_session_started
  ON task_runs(session_id, started_at_ms DESC, id DESC);
CREATE INDEX idx_task_runs_outcome ON task_runs(outcome);
CREATE INDEX idx_task_runs_goal_id ON task_runs(goal_id);

-- Historical session lifecycle is not evidence of task success. Seed one
-- explicitly unclassified record per existing session so analytics and resume
-- surfaces never reinterpret `sessions.status` as a task outcome.
INSERT INTO task_runs (
  session_id,
  goal_id,
  goal,
  outcome,
  started_at_ms,
  ended_at_ms
)
SELECT
  id,
  'legacy-session:' || id,
  substr(
    COALESCE(NULLIF(trim(summary), ''), NULLIF(trim(name), ''), 'Historical session'),
    1,
    4096
  ),
  'unknown',
  started_at_ms,
  MAX(started_at_ms, COALESCE(ended_at_ms, updated_at_ms))
FROM sessions;
