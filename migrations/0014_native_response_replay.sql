-- Native protocol payloads use response UUID provenance outside visible text
-- and mutable UI row IDs. Save this sidecar with the context snapshot.
CREATE TABLE native_response_replay (
    session_id INTEGER PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    raw_json TEXT NOT NULL
);
