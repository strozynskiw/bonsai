-- Tool cards and storage use canonical UUID IDs; protocols may need their
-- original correlators after compaction, provider rebuilds, and session resume.
CREATE TABLE tool_call_identities (
    session_id INTEGER PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    raw_json TEXT NOT NULL
);
