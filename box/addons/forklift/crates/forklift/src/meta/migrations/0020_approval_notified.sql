-- Records which notification receivers an approval-request alarm was dispatched
-- to for a quarantined package, so the approval queue can show the alarm targets.
-- Comma-separated receiver names (receiver names never contain commas); empty
-- when no receiver was configured or the row predates this migration.
ALTER TABLE package_approvals ADD COLUMN notified_receivers TEXT NOT NULL DEFAULT '';
