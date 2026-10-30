-- SME-242: the credential chain bit. Nullable, and its only possible
-- member today is `git-ssh` — it is the chain credential, the one a
-- persona can narrow away and a parent passes down (Rule G); a named
-- secret's name never enters this column, that's Rule S reading the
-- persona snapshot. It stays a TEXT[] so a future chain-granted
-- built-in joins without a migration. NULL is only ever a pre-ticket
-- row: Rule G reads it fall-open — granted, as that conversation was
-- before the column existed (docs/plans/sme-242.md, D2 and D3).

ALTER TABLE agents ADD COLUMN credentials TEXT[];
