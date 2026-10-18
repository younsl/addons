-- Security insight reports. The analyzer writes one row per run. summary_json
-- (score and per-severity counts) is kept for the trend, while report_json, the
-- full finding list, is kept only on the newest row: older rows are blanked on
-- insert so hourly runs do not grow the database by a report each.
CREATE TABLE security_insight_reports (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    generated_at TEXT NOT NULL,
    score        INTEGER NOT NULL,
    summary_json TEXT NOT NULL,
    report_json  TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_security_insight_reports_generated ON security_insight_reports(generated_at);
