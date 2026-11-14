-- Per-check muting: an operator can waive one half of the wiring instead of
-- taking the whole project out of the measurement.
--
-- The column carries the muted checks as a JSON array of "ci" / "registry". An
-- array holding both is the whole project muted, which is what every row
-- written before this migration meant, so the default backfills them that way.
ALTER TABLE coverage_muted ADD COLUMN scopes TEXT NOT NULL DEFAULT '["ci","registry"]';
