CREATE TABLE session_goals (
    goal_id TEXT PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    is_current INTEGER NOT NULL CHECK (is_current IN (0, 1)),
    record_json TEXT NOT NULL
);
CREATE UNIQUE INDEX one_current_session_goal ON session_goals(session_id) WHERE is_current = 1;
