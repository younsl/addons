-- Covering indexes for the two whole-table aggregates the console's repository
-- list runs on every request.
--
-- Both were table scans: the per-repository inventory reads repo_id and size for
-- every artifact, and the scan-coverage ratio reads repo_id, version and path for
-- every versioned artifact. With these indexes SQLite answers both from the index
-- alone, so the cost follows the index rather than the row width (metadata_json,
-- in particular, is never touched).
--
-- idx_artifacts_repo(repo_id) from the initial schema is now redundant: it is a
-- prefix of idx_artifacts_repo_size, which serves the same lookups.
CREATE INDEX idx_artifacts_repo_size ON artifacts(repo_id, size);
CREATE INDEX idx_artifacts_scan_targets ON artifacts(repo_id, version, path);
DROP INDEX idx_artifacts_repo;
