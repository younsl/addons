-- When the announcement stops showing. Empty keeps it up until it is cleared.
ALTER TABLE announcement ADD COLUMN expires_at TEXT NOT NULL DEFAULT '';
