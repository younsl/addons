-- Site-wide announcement shown on the main page (Jenkins system-message
-- style). A single row holds the Markdown source; an empty body means no
-- announcement is displayed.
CREATE TABLE announcement (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    body       TEXT NOT NULL,
    updated_by TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
