-- Whether the repo's last clone attempt created its directory (SME-32 code
-- review 3). A retry of a failed clone clears the directory only then: a
-- cut-off or half-finished attempt's leftovers go, and a directory that
-- held work before the clone is never touched.
ALTER TABLE conversation_repos ADD COLUMN dir_created BOOLEAN NOT NULL DEFAULT false;
