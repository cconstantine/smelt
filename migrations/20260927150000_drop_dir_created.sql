-- Clones are staged in a directory of their own and moved into place only
-- when they succeed (SME-32 code review 4), so nothing needs to remember
-- whether an attempt created the checkout directory.
ALTER TABLE conversation_repos DROP COLUMN dir_created;
