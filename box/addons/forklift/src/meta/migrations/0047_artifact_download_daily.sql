-- Per-day successful GET counts per artifact path, so the artifact page's
-- rolling download counts no longer need every download row kept in
-- audit_logs (nor the partial index over them).
CREATE TABLE artifact_download_daily (
    repo_name TEXT NOT NULL,
    path      TEXT NOT NULL,
    day       TEXT NOT NULL,
    count     INTEGER NOT NULL CHECK(count >= 0),
    PRIMARY KEY (repo_name, path, day)
) WITHOUT ROWID;

CREATE INDEX idx_artifact_download_daily_day ON artifact_download_daily(day);

-- Rows up to cutover_id predate live counting. The leader folds them into
-- artifact_download_daily in id-ordered chunks, resuming from last_id, then
-- drops idx_audit_artifact_downloads.
CREATE TABLE artifact_download_backfill (
    id         INTEGER PRIMARY KEY CHECK(id = 1),
    cutover_id INTEGER NOT NULL,
    last_id    INTEGER NOT NULL DEFAULT 0,
    done       INTEGER NOT NULL DEFAULT 0
);

INSERT INTO artifact_download_backfill(id, cutover_id)
SELECT 1, COALESCE(MAX(id), 0) FROM audit_logs;
