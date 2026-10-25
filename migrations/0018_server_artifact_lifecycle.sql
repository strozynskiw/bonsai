-- Preserve existing artifact registrations while allowing session deletion.
CREATE TABLE server_artifacts_next (
    id TEXT PRIMARY KEY,
    workspace TEXT NOT NULL,
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    size INTEGER NOT NULL,
    digest TEXT NOT NULL,
    name TEXT NOT NULL
);
INSERT INTO server_artifacts_next SELECT id, workspace, session_id, size, digest, name FROM server_artifacts;
DROP TABLE server_artifacts;
ALTER TABLE server_artifacts_next RENAME TO server_artifacts;
