-- One-shot subscriptions for process-local background work. Process handles are
-- deliberately absent: a restart must never bind a persisted wait to a reused
-- bg-N or pty-N handle.
CREATE TABLE background_wake_subscriptions (
  id INTEGER PRIMARY KEY,
  requester_session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  requester_generation INTEGER NOT NULL,
  -- The in-memory runtime that owns the process handles and local wake channel.
  owner_runtime_id TEXT NOT NULL,
  operation_key TEXT NOT NULL,
  target_kind TEXT NOT NULL CHECK (target_kind IN ('background_task', 'terminal')),
  target_id TEXT NOT NULL,
  -- Immutable per-registry-record token; protects an old pending wait when
  -- a restarted runtime allocates the same bg-N/pty-N identifier.
  target_incarnation TEXT NOT NULL,
  observed_version INTEGER NOT NULL,
  output_threshold INTEGER,
  deadline_at_ms INTEGER,
  created_at_ms INTEGER NOT NULL,
  fired_at_ms INTEGER,
  wake_reason TEXT,
  wake_version INTEGER,
  wake_output TEXT,
  wake_output_truncated INTEGER NOT NULL DEFAULT 0,
  CHECK (output_threshold IS NULL OR output_threshold >= 0),
  CHECK ((fired_at_ms IS NULL AND wake_reason IS NULL AND wake_version IS NULL)
      OR (fired_at_ms IS NOT NULL AND wake_reason IS NOT NULL AND wake_version IS NOT NULL)),
  UNIQUE (requester_session_id, operation_key)
);

CREATE INDEX idx_background_wake_subscriptions_pending_target
  ON background_wake_subscriptions(target_kind, target_id)
  WHERE fired_at_ms IS NULL;
CREATE INDEX idx_background_wake_subscriptions_pending_requester
  ON background_wake_subscriptions(requester_session_id)
  WHERE fired_at_ms IS NULL;
CREATE INDEX idx_background_wake_subscriptions_pending_runtime
  ON background_wake_subscriptions(owner_runtime_id)
  WHERE fired_at_ms IS NULL;

