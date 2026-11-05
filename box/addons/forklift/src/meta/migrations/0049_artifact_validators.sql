-- Upstream cache validators of proxied documents, bound to the blob they validate and the upstream that issued them.
CREATE TABLE artifact_validators (
    repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    blob_sha256 TEXT NOT NULL,
    upstream_url TEXT NOT NULL,
    etag TEXT NOT NULL DEFAULT '',
    last_modified TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (repo_id, path)
);
