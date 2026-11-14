-- Cover the artifact page's per-path rolling download counts.
CREATE INDEX idx_audit_artifact_downloads ON audit_logs(repo_name, path, created_at)
WHERE event = 'download' AND method = 'GET' AND status IN (200, 206);
