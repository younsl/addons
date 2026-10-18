-- One row per check per analyzer run, so a check page can chart how many items
-- it flagged and how its status changed. Pruned with the report trend rows.
CREATE TABLE insight_check_history (
    check_id     TEXT NOT NULL,
    generated_at TEXT NOT NULL,
    status       TEXT NOT NULL,
    severity     TEXT NOT NULL DEFAULT '',
    flagged      INTEGER NOT NULL,
    PRIMARY KEY (check_id, generated_at)
);

CREATE INDEX idx_insight_check_history_generated ON insight_check_history(generated_at);
