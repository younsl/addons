-- Acknowledged security insight checks, after Trusted Advisor. An
-- acknowledgement takes a failing check out of the score and the alarms until
-- it expires or the check flags more items than it did when acknowledged
-- (flagged), so acknowledging today's state never hides tomorrow's.
CREATE TABLE insight_acknowledgements (
    check_id        TEXT PRIMARY KEY,
    note            TEXT NOT NULL,
    flagged         INTEGER NOT NULL,
    acknowledged_by TEXT NOT NULL,
    acknowledged_at TEXT NOT NULL,
    expires_at      TEXT NOT NULL
);
