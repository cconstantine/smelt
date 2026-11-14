-- SME-521: whether the user's Stop ended this pod (true), or something
-- else did (false). NULL: still live, or ended before this column
-- existed, when a Stop and a terminate_pod looked the same. The
-- storage-grace rule reads it to pick the grace (docs/plans/sme-521.md).
ALTER TABLE sandbox_pods ADD COLUMN stopped_by_user BOOLEAN NULL;
