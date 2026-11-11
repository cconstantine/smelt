-- SME-455: an agent seat that owed its parent a report with nothing
-- running: set in the transaction that queues the seat its nudge (the
-- first plain report-less turn end of a spell), cleared when it reports
-- to its parent or the user engages it. The tree's mark and the
-- loop-guard that keeps the nudge to one per spell.
ALTER TABLE agents ADD COLUMN report_owed_at TIMESTAMP NULL;
