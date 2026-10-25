CREATE TABLE server_tokens (
    id TEXT PRIMARY KEY,
    digest TEXT NOT NULL UNIQUE,
    workspace TEXT NOT NULL,
    sessions_json TEXT NOT NULL,
    scopes_json TEXT NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    revoked INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE server_audit (
    id INTEGER PRIMARY KEY,
    at_ms INTEGER NOT NULL,
    token_id TEXT,
    session_id INTEGER,
    run_id TEXT,
    request_id TEXT NOT NULL,
    action TEXT NOT NULL,
    decision TEXT NOT NULL
);
CREATE TABLE server_mutations (
    workspace TEXT NOT NULL,
    operation TEXT NOT NULL,
    session_id INTEGER NOT NULL,
    request_id TEXT NOT NULL,
    digest TEXT NOT NULL,
    result_json TEXT,
    PRIMARY KEY (workspace, operation, session_id, request_id)
);
CREATE TABLE server_artifacts (
    id TEXT PRIMARY KEY,
    workspace TEXT NOT NULL,
    session_id INTEGER NOT NULL REFERENCES sessions(id),
    size INTEGER NOT NULL,
    digest TEXT NOT NULL,
    name TEXT NOT NULL
);
