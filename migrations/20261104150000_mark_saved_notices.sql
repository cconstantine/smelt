-- SME-357: every notice the server saves into a transcript starts with
-- U+2063 (INVISIBLE SEPARATOR, api::chat::NOTICE_MARK), and the readers
-- (system_notice, display_text, compaction's is_typed) tell one from the
-- same words a user typed by that mark alone. This marks the notices
-- already saved, per family, on the 20261103135400_mark_failure_notices
-- pattern: a notice is one text block, its saved opening is 24 characters
-- long, and an already-marked row stops matching, so every statement is
-- idempotent. A prefix marks every matching unmarked user row, including
-- the rare one a user typed before the fix: those rows already rendered
-- and quoted as notices (the readers matched their words), so marking
-- changes nothing for them, and no wording tells the two apart.
UPDATE messages SET content = '[{"type":"text","text":"' || U&'\2063' || substr(content, 25) WHERE role = 'user' AND content LIKE '[{"type":"text","text":"The user stopped this turn before it finished.%';
UPDATE messages SET content = '[{"type":"text","text":"' || U&'\2063' || substr(content, 25) WHERE role = 'user' AND content LIKE '[{"type":"text","text":"Your last reply was cut off: it reached its limit of %';
UPDATE messages SET content = '[{"type":"text","text":"' || U&'\2063' || substr(content, 25) WHERE role = 'user' AND content LIKE '[{"type":"text","text":"Your last reply was stopped by the model provider''s safety system%';
UPDATE messages SET content = '[{"type":"text","text":"' || U&'\2063' || substr(content, 25) WHERE role = 'user' AND content LIKE '[{"type":"text","text":"Your last reply had no text and no tool call.%';
UPDATE messages SET content = '[{"type":"text","text":"' || U&'\2063' || substr(content, 25) WHERE role = 'user' AND content LIKE '[{"type":"text","text":"Your last reply wrote a tool call as text instead of making it%';
UPDATE messages SET content = '[{"type":"text","text":"' || U&'\2063' || substr(content, 25) WHERE role = 'user' AND content LIKE '[{"type":"text","text":"Terminal command %';
UPDATE messages SET content = '[{"type":"text","text":"' || U&'\2063' || substr(content, 25) WHERE role = 'user' AND content LIKE '[{"type":"text","text":"Sandbox pod %';
UPDATE messages SET content = '[{"type":"text","text":"' || U&'\2063' || substr(content, 25) WHERE role = 'user' AND content LIKE '[{"type":"text","text":"Docker in sandbox pod %';
UPDATE messages SET content = '[{"type":"text","text":"' || U&'\2063' || substr(content, 25) WHERE role = 'user' AND content LIKE '[{"type":"text","text":"The user stopped sandbox pod %';
UPDATE messages SET content = '[{"type":"text","text":"' || U&'\2063' || substr(content, 25) WHERE role = 'user' AND content LIKE '[{"type":"text","text":"The user removed this conversation''s sandbox storage%';
UPDATE messages SET content = '[{"type":"text","text":"' || U&'\2063' || substr(content, 25) WHERE role = 'user' AND content LIKE '[{"type":"text","text":"Language server %';
UPDATE messages SET content = '[{"type":"text","text":"' || U&'\2063' || substr(content, 25) WHERE role = 'user' AND content LIKE '[{"type":"text","text":"The user now trusts %';
UPDATE messages SET content = '[{"type":"text","text":"' || U&'\2063' || substr(content, 25) WHERE role = 'user' AND content LIKE '[{"type":"text","text":"The user chose not to trust %';
UPDATE messages SET content = '[{"type":"text","text":"' || U&'\2063' || substr(content, 25) WHERE role = 'user' AND content LIKE '[{"type":"text","text":"<task-notification %';
UPDATE messages SET content = '[{"type":"text","text":"' || U&'\2063' || substr(content, 25) WHERE role = 'user' AND content LIKE '[{"type":"text","text":"<task-output %';
UPDATE messages SET content = '[{"type":"text","text":"' || U&'\2063' || substr(content, 25) WHERE role = 'user' AND content LIKE '[{"type":"text","text":"Notice: %';
