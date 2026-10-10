-- OCI blob upload sessions. A push is a multi-request protocol (POST open,
-- PATCH append, PUT finalize) and the requests of one session may land on
-- different replicas behind a Service, so the session offset is persisted here
-- and the bytes accumulate in a file under the shared data directory rather
-- than in any one process's memory.
CREATE TABLE oci_upload_sessions (
    id         TEXT PRIMARY KEY,          -- opaque hex id, also the temp file name
    repo_id    INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    offset     INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
