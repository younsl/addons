-- Content-addressed cache for format-aware group metadata aggregation.
CREATE TABLE group_metadata_cache (
    group_repo_id   INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    path            TEXT NOT NULL,
    representation  TEXT NOT NULL,
    blob_sha256     TEXT NOT NULL REFERENCES blobs(sha256),
    size            INTEGER NOT NULL CHECK(size >= 0),
    sources_json    TEXT NOT NULL,
    config_revision TEXT NOT NULL,
    expires_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    PRIMARY KEY(group_repo_id, path, representation)
);
CREATE INDEX idx_group_metadata_expiry ON group_metadata_cache(expires_at);
