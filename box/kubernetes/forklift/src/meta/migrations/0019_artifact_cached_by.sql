-- Records the principal who first caused an artifact to be cached/uploaded, so
-- the artifact browser can show who fetched it. Empty for anonymous or for rows
-- created before this migration.
ALTER TABLE artifacts ADD COLUMN cached_by TEXT NOT NULL DEFAULT '';
