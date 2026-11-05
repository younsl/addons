-- Who last downloaded an artifact, alongside last_accessed_at. Populated by the
-- serving path's throttled touch, so like the timestamp it is minute-grained.
-- Empty for anonymous pulls and for artifacts never served since this column
-- was added.
ALTER TABLE artifacts ADD COLUMN last_accessed_by TEXT NOT NULL DEFAULT '';
