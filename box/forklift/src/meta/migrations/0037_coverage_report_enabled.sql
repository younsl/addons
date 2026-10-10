-- Whether the scheduled coverage report is sent at all, kept apart from the
-- receiver it is sent to.
--
-- The two answer different questions, the same way the scan schedule keeps its
-- switch apart from its cron expression: turning the report off should not make
-- an operator re-pick the receiver to turn it back on. Defaults to on, which
-- preserves the behaviour of every row written before this column existed,
-- where having a receiver was what enabled the report.
ALTER TABLE coverage_settings ADD COLUMN report_enabled INTEGER NOT NULL DEFAULT 1;
