-- Session identity may exist before a task starts (peer liveness, onboarding,
-- authorization). Never infer task success from those process-only rows.
ALTER TABLE sessions ADD COLUMN kind TEXT NOT NULL DEFAULT 'legacy_unclassified'
  CHECK (kind IN ('task', 'lifecycle_only', 'provider_probe', 'legacy_unclassified'));
ALTER TABLE sessions ADD COLUMN lifecycle_diagnostic TEXT;

-- Classify only positive historical evidence. Ambiguous rows remain available
-- in the diagnostic view; migration never deletes user data.
UPDATE sessions SET kind = 'task'
WHERE prompt_token_count > 0 OR completion_token_count > 0
   OR EXISTS (SELECT 1 FROM messages WHERE session_id = sessions.id AND role = 'user')
   OR EXISTS (SELECT 1 FROM tool_calls WHERE session_id = sessions.id)
   OR EXISTS (SELECT 1 FROM usage_turns WHERE session_id = sessions.id)
   OR EXISTS (SELECT 1 FROM task_runs WHERE session_id = sessions.id
              AND goal_id NOT LIKE 'legacy-session:%');

CREATE INDEX idx_sessions_project_kind_updated
  ON sessions(project_id, kind, updated_at_ms DESC);

-- Task evidence is forbidden until the identity is explicitly promoted. Keep
-- this at the database boundary so standalone writers cannot bypass it.
CREATE TRIGGER task_runs_require_task_session BEFORE INSERT ON task_runs
WHEN (SELECT kind FROM sessions WHERE id = NEW.session_id) != 'task'
BEGIN SELECT RAISE(ABORT, 'task evidence requires a task session'); END;
CREATE TRIGGER verification_requires_task_session BEFORE INSERT ON verification_runs
WHEN (SELECT kind FROM sessions WHERE id = NEW.session_id) != 'task'
BEGIN SELECT RAISE(ABORT, 'verification evidence requires a task session'); END;
CREATE TRIGGER self_review_requires_task_session BEFORE INSERT ON self_review_runs
WHEN (SELECT kind FROM sessions WHERE id = NEW.session_id) != 'task'
BEGIN SELECT RAISE(ABORT, 'review evidence requires a task session'); END;
CREATE TRIGGER todos_require_task_session BEFORE INSERT ON todos
WHEN (SELECT kind FROM sessions WHERE id = NEW.session_id) != 'task'
BEGIN SELECT RAISE(ABORT, 'todo evidence requires a task session'); END;
CREATE TRIGGER episodes_require_task_session BEFORE INSERT ON episodes
WHEN (SELECT kind FROM sessions WHERE id = NEW.session_id) != 'task'
BEGIN SELECT RAISE(ABORT, 'episode evidence requires a task session'); END;
