-- Security insight settings, edited from the console. A single row: absent
-- until somebody saves, when the defaults (and FORKLIFT_INSIGHTS_INTERVAL for
-- the analysis interval) apply.
CREATE TABLE insight_settings (
    id                   INTEGER PRIMARY KEY CHECK (id = 1),
    auto_analyze_enabled INTEGER NOT NULL DEFAULT 1,
    interval_minutes     INTEGER NOT NULL DEFAULT 60,
    report_enabled       INTEGER NOT NULL DEFAULT 0,
    -- Notification receiver the scheduled alarm is sent to, by name.
    receiver             TEXT    NOT NULL DEFAULT '',
    report_cron          TEXT    NOT NULL DEFAULT '0 10 * * 1-5',
    timezone             TEXT    NOT NULL DEFAULT 'UTC',
    -- The least severe open finding that makes the scheduled alarm go out.
    min_severity         TEXT    NOT NULL DEFAULT 'high',
    skip_when_unchanged  INTEGER NOT NULL DEFAULT 0,
    -- JSON object of check id to flagged count at the last scheduled alarm, so
    -- skip_when_unchanged can tell whether anything new has turned up.
    last_report_state    TEXT    NOT NULL DEFAULT '{}',
    updated_by           TEXT    NOT NULL DEFAULT '',
    updated_at           TEXT    NOT NULL DEFAULT ''
);
