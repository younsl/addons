-- Correlate the logical paths and aggregate metadata produced by one upload.
ALTER TABLE audit_logs ADD COLUMN request_id TEXT NOT NULL DEFAULT '';
ALTER TABLE audit_logs ADD COLUMN detail_json TEXT NOT NULL DEFAULT '';
CREATE INDEX idx_audit_request ON audit_logs(request_id) WHERE request_id != '';
