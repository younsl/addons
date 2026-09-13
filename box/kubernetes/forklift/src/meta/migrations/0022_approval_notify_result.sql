-- Records the outcome of the approval-request alarm delivery, shown on the
-- approval review detail page: when the alarm was actually sent (RFC3339, empty
-- until delivered), the delivery result ("delivered" or "failed"), and how long
-- the send took in milliseconds (0 until delivered). The receivers targeted are
-- already in notified_receivers (0020).
ALTER TABLE package_approvals ADD COLUMN notified_at TEXT NOT NULL DEFAULT '';
ALTER TABLE package_approvals ADD COLUMN notify_result TEXT NOT NULL DEFAULT '';
ALTER TABLE package_approvals ADD COLUMN notify_duration_ms INTEGER NOT NULL DEFAULT 0;
