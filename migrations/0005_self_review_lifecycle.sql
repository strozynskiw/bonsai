ALTER TABLE self_review_runs ADD COLUMN tool_call_id TEXT;
ALTER TABLE self_review_runs ADD COLUMN status TEXT NOT NULL DEFAULT 'succeeded'
  CHECK (status IN ('running', 'succeeded', 'failed', 'timed_out', 'cancelled', 'parent_interrupted'));
ALTER TABLE self_review_runs ADD COLUMN result TEXT;

CREATE UNIQUE INDEX idx_self_review_runs_tool_call
  ON self_review_runs(session_id, tool_call_id);
