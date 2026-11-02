-- SME-354 B13: when the storage-grace pass first tried to remove this
-- quiet stop's claims. Stamped before the removal runs and cleared with
-- the mark, so a retry of a removal given up on after it deleted the
-- claims (which then finds nothing to delete) still sends the stop's one
-- notice. NULL: no attempt yet for the current mark.
ALTER TABLE sandbox_pods ADD COLUMN storage_removal_started_at TIMESTAMP NULL;
