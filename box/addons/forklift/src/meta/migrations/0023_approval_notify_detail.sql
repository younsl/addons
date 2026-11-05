-- A concrete, human-readable detail for the approval alarm delivery outcome,
-- shown next to notify_result on the review page: the HTTP status on a reachable
-- webhook ("HTTP 200", "HTTP 500") or a short reason when the webhook could not
-- be reached ("no response from webhook"). Empty until delivered.
ALTER TABLE package_approvals ADD COLUMN notify_detail TEXT NOT NULL DEFAULT '';
