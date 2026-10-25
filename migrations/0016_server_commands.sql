-- Durable admission: accepted commands are never automatically re-executed.
CREATE TABLE server_commands (
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    request_id TEXT NOT NULL,
    prompt TEXT NOT NULL,
    run_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('accepted', 'finished', 'interrupted')),
    PRIMARY KEY (session_id, request_id)
);
