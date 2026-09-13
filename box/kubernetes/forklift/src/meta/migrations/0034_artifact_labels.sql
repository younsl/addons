-- Per-artifact labels: short operator tags on one stored path, added by an
-- administrator or by the principal who uploaded (or first cached) the artifact.
-- Deliberately format-agnostic: the identity is (repository, path), the same
-- identity every format already stores, so a maven jar, a raw file and an OCI
-- manifest all carry labels the same way.
--
-- The foreign key follows artifacts(repo_id, path), which is unique, so removing
-- an artifact by any route (delete, force delete, purge, idle reaper, LRU
-- eviction) takes its labels with it and no label can ever name a path that is
-- no longer stored.
CREATE TABLE artifact_labels (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id    INTEGER NOT NULL,
    path       TEXT NOT NULL,
    label      TEXT NOT NULL,
    created_by TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    UNIQUE (repo_id, path, label),
    FOREIGN KEY (repo_id, path) REFERENCES artifacts(repo_id, path) ON DELETE CASCADE
);

-- The sidebar's global search matches label text across every repository, so
-- the lookup by label alone is indexed rather than scanning the table.
CREATE INDEX idx_artifact_labels_label ON artifact_labels(label);
