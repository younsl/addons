-- Forklift coverage: how much of the organisation's source actually builds
-- through this forklift, measured by walking a GitLab instance and reading each
-- project's CI and package-manager configuration.
--
-- Four tables, because the four things have different lifetimes: settings are
-- edited rarely and by hand, the result is replaced wholesale by each scan, the
-- history grows one row per scan, and the opt-outs outlive every scan.

-- Runtime settings, edited by an administrator in the console. One row.
--
-- The GitLab base URL and access token are deliberately absent: they come from
-- the environment (a Kubernetes Secret), so the credential never lands in this
-- database, in the snapshots synchronised to object storage, or in a settings
-- API response. The forklift host is absent for a different reason: the server
-- knows what it is called, so it is derived from FORKLIFT_EXTERNAL_URL.
CREATE TABLE coverage_settings (
    id                      INTEGER PRIMARY KEY CHECK (id = 1),
    -- External domain a project's configuration must reference to count as
    -- wired. Empty falls back to the host of FORKLIFT_EXTERNAL_URL, which is
    -- what forklift already knows itself by, so a normal deployment leaves it
    -- unset.
    forklift_host           TEXT    NOT NULL DEFAULT '',
    -- JSON array of GitLab topics that opt a repository out of the measurement
    -- from the repository side. What is in scope at all is decided by the access
    -- token, so there is no group or path list here.
    exclude_topics          TEXT    NOT NULL DEFAULT '[]',
    scan_cron               TEXT    NOT NULL DEFAULT '0 10 * * 1-5',
    timezone                TEXT    NOT NULL DEFAULT 'UTC',
    auto_scan_enabled       INTEGER NOT NULL DEFAULT 1,
    -- Notification receiver the scheduled report is sent to. Empty disables it.
    receiver                TEXT    NOT NULL DEFAULT '',
    skip_when_full_coverage INTEGER NOT NULL DEFAULT 0,
    -- How deep the crawl looks. There is deliberately no request-rate column:
    -- the safe rate depends on the instance and the hour, so it is discovered
    -- at run time rather than written down here.
    max_branches            INTEGER NOT NULL DEFAULT 10,
    since_days              INTEGER NOT NULL DEFAULT 180,
    use_search              INTEGER NOT NULL DEFAULT 0,
    updated_by              TEXT    NOT NULL DEFAULT '',
    updated_at              TEXT    NOT NULL
);

-- The last completed scan, so a restart shows the previous picture instead of
-- an empty page. One row: results only ever change on a scheduled or manual
-- scan, so a single JSON payload keeps the read path to one query.
CREATE TABLE coverage_results (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    payload     TEXT    NOT NULL,
    scanned_at  TEXT    NOT NULL,
    duration_ms INTEGER NOT NULL DEFAULT 0
);

-- One row per completed scan, for the coverage trend. Kept as plain counts
-- rather than a payload because the chart reads the whole retention window at
-- once. Rows past that window are pruned on each scan.
CREATE TABLE coverage_history (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    target      INTEGER NOT NULL,
    applied     INTEGER NOT NULL,
    partial     INTEGER NOT NULL,
    not_applied INTEGER NOT NULL,
    skipped     INTEGER NOT NULL,
    percent     INTEGER NOT NULL,
    scanned_at  TEXT    NOT NULL
);

-- The trend chart reads a window ending now, and retention deletes a window
-- ending in the past; both are range scans over this column.
CREATE INDEX idx_coverage_history_scanned_at ON coverage_history(scanned_at);

-- Projects muted from the console, so they stop counting towards coverage.
--
-- Kept apart from the GitLab topic so the two sources stay independent: the
-- repository owns its topics, and whoever runs the console owns this table. A
-- muted project keeps its verdict, which is what makes muting reversible
-- without waiting for another scan.
CREATE TABLE coverage_muted (
    project_path TEXT PRIMARY KEY,
    muted_by     TEXT NOT NULL DEFAULT '',
    muted_at     TEXT NOT NULL
);
