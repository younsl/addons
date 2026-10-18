-- One row per alarm delivery attempt, scheduled or manual, so the Security and
-- Coverage settings pages can chart how their alarms went out. Kept 90 days.
CREATE TABLE alarm_deliveries (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    feature   TEXT NOT NULL,
    sent_at   TEXT NOT NULL,
    receiver  TEXT NOT NULL,
    trigger   TEXT NOT NULL,
    ok        INTEGER NOT NULL,
    error     TEXT NOT NULL DEFAULT '',
    items     INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_alarm_deliveries_feature_sent ON alarm_deliveries(feature, sent_at);
