-- When a webhook last accepted a message for the receiver, shown in the receiver list.
ALTER TABLE notification_receivers ADD COLUMN last_sent_at TEXT;
