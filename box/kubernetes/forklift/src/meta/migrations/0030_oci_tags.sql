-- OCI tags: mutable pointers from (repository, OCI name, tag) to a manifest
-- digest. Manifests and blobs are immutable digest-addressed artifact rows;
-- tags are the only mutable object in the OCI format, so they live here rather
-- than in the artifacts table (an artifact path is a stable identity, a tag is
-- a moving one).
CREATE TABLE oci_tags (
    repo_id         INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    tag             TEXT NOT NULL,
    manifest_digest TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    PRIMARY KEY (repo_id, name, tag)
);

-- The prune pass and manifest DELETE both resolve "which tags point at this
-- digest", so that lookup is indexed.
CREATE INDEX idx_oci_tags_manifest ON oci_tags(repo_id, name, manifest_digest);
