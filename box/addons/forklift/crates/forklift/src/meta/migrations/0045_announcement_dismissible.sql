-- Whether signed-in users may dismiss the announcement. Off keeps it in view
-- for everyone until it ends or is taken down.
ALTER TABLE announcement ADD COLUMN user_dismissible INTEGER NOT NULL DEFAULT 0;
