-- Component-aware publication ownership for atomic UI/API artifact uploads.
-- Existing artifacts remain unmanaged (publication_id NULL).
CREATE TABLE artifact_publications (
    id                TEXT PRIMARY KEY,
    repo_id           INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    format            TEXT NOT NULL,
    package_name      TEXT NOT NULL,
    version           TEXT NOT NULL,
    coordinate        TEXT NOT NULL,
    upload_id         TEXT NOT NULL,
    created_by        TEXT NOT NULL,
    created_by_source TEXT NOT NULL,
    yanked            INTEGER NOT NULL DEFAULT 0 CHECK(yanked IN (0, 1)),
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL,
    UNIQUE(repo_id, format, package_name, version)
);

ALTER TABLE artifacts ADD COLUMN publication_id TEXT
    REFERENCES artifact_publications(id) ON DELETE SET NULL;
ALTER TABLE artifacts ADD COLUMN artifact_role TEXT NOT NULL DEFAULT 'primary';
CREATE INDEX idx_artifacts_publication ON artifacts(publication_id);

CREATE TABLE artifact_publication_tombstones (
    repo_id      INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    format       TEXT NOT NULL,
    package_name TEXT NOT NULL,
    version      TEXT NOT NULL,
    asset_key    TEXT NOT NULL,
    deleted_at   TEXT NOT NULL,
    deleted_by   TEXT NOT NULL,
    PRIMARY KEY(repo_id, format, package_name, version, asset_key)
);
