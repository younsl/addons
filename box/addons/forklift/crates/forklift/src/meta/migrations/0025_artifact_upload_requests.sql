-- Idempotency state and temporary blob leases for streaming publication.
CREATE TABLE artifact_upload_requests (
    idempotency_key  TEXT NOT NULL,
    repo_id          INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    principal_name   TEXT NOT NULL,
    principal_source TEXT NOT NULL,
    upload_id        TEXT NOT NULL,
    state            TEXT NOT NULL CHECK(state IN ('receiving','conflict','committed','failed')),
    plan_json        TEXT NOT NULL DEFAULT '',
    result_json      TEXT NOT NULL DEFAULT '',
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL,
    expires_at       TEXT NOT NULL,
    PRIMARY KEY(repo_id, principal_source, principal_name, idempotency_key)
);
CREATE UNIQUE INDEX idx_artifact_upload_id ON artifact_upload_requests(upload_id);
CREATE INDEX idx_artifact_upload_expiry ON artifact_upload_requests(state, expires_at);

CREATE TABLE artifact_upload_staged_blobs (
    upload_id TEXT NOT NULL REFERENCES artifact_upload_requests(upload_id) ON DELETE CASCADE,
    sha256    TEXT NOT NULL,
    size      INTEGER NOT NULL CHECK(size >= 0),
    PRIMARY KEY(upload_id, sha256)
);
