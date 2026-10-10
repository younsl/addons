-- Marks a user as a Robot Account: a token-only service identity that cannot log
-- in interactively (no password or OIDC session) but whose personal access
-- tokens still authenticate for package operations. 0 = normal user, 1 = robot.
ALTER TABLE users ADD COLUMN robot INTEGER NOT NULL DEFAULT 0;
