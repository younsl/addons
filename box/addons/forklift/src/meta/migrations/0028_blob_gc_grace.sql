-- Grace period for blob garbage collection.
--
-- Blob deletion is irreversible, but in the s3 backend the metadata database is
-- replicated asynchronously (see internal/objstore): a failover or restart can
-- move it backwards by up to one sync interval. Deleting bytes the moment
-- ref_count reaches zero therefore lets a rollback resurrect an artifact row
-- whose bytes are already gone -- a dangling reference that cannot be repaired.
--
-- unreferenced_since records when a blob last became unreferenced so the sweeper
-- can wait out a grace period that comfortably exceeds the replication lag.
-- It is cleared whenever the blob is referenced again.
ALTER TABLE blobs ADD COLUMN unreferenced_since TEXT;

-- Backfill existing unreferenced rows with the migration time rather than NULL,
-- so upgrading does not hand the sweeper a batch that is instantly past its
-- grace period.
UPDATE blobs
   SET unreferenced_since = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
 WHERE ref_count <= 0;

CREATE INDEX idx_blobs_unreferenced ON blobs(unreferenced_since) WHERE ref_count <= 0;
