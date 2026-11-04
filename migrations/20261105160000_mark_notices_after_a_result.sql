-- SME-386 A6: 20261104150000_mark_saved_notices marked a notice only when
-- it was a row's first block. A notice can also follow a question's
-- result in one row, [tool_result, text(notice)]: a notice delivered as a
-- wake turn's message (`turn::notify::deliver_notice`) while an answered
-- question waited is saved after that answer
-- (`db::create_message_taking_question`). This marks the notice blocks
-- anywhere in a user row, per family, with the same families and the same
-- trade-off as that migration.
--
-- Text, not jsonb: a row is the compact JSON serde wrote, and a jsonb
-- round trip would respace it, which `db::agents::messages`'s
-- `LIKE '%"type":"tool_result"%'` relies on not happening. A text block
-- starts with exactly `{"type":"text","text":"`, and inside a JSON string
-- every quote is escaped, so that start can't match within another
-- block's string (a tool result quoting the words). A marked block stops
-- matching, so every statement is idempotent, and rows the first
-- migration marked are unchanged.
UPDATE messages SET content = replace(content, '{"type":"text","text":"The user stopped this turn before it finished.', '{"type":"text","text":"' || U&'\2063' || 'The user stopped this turn before it finished.') WHERE role = 'user' AND content LIKE '%{"type":"text","text":"The user stopped this turn before it finished.%';
UPDATE messages SET content = replace(content, '{"type":"text","text":"Your last reply was cut off: it reached its limit of ', '{"type":"text","text":"' || U&'\2063' || 'Your last reply was cut off: it reached its limit of ') WHERE role = 'user' AND content LIKE '%{"type":"text","text":"Your last reply was cut off: it reached its limit of %';
UPDATE messages SET content = replace(content, '{"type":"text","text":"Your last reply was stopped by the model provider''s safety system', '{"type":"text","text":"' || U&'\2063' || 'Your last reply was stopped by the model provider''s safety system') WHERE role = 'user' AND content LIKE '%{"type":"text","text":"Your last reply was stopped by the model provider''s safety system%';
UPDATE messages SET content = replace(content, '{"type":"text","text":"Your last reply had no text and no tool call.', '{"type":"text","text":"' || U&'\2063' || 'Your last reply had no text and no tool call.') WHERE role = 'user' AND content LIKE '%{"type":"text","text":"Your last reply had no text and no tool call.%';
UPDATE messages SET content = replace(content, '{"type":"text","text":"Your last reply wrote a tool call as text instead of making it', '{"type":"text","text":"' || U&'\2063' || 'Your last reply wrote a tool call as text instead of making it') WHERE role = 'user' AND content LIKE '%{"type":"text","text":"Your last reply wrote a tool call as text instead of making it%';
UPDATE messages SET content = replace(content, '{"type":"text","text":"Terminal command ', '{"type":"text","text":"' || U&'\2063' || 'Terminal command ') WHERE role = 'user' AND content LIKE '%{"type":"text","text":"Terminal command %';
UPDATE messages SET content = replace(content, '{"type":"text","text":"Sandbox pod ', '{"type":"text","text":"' || U&'\2063' || 'Sandbox pod ') WHERE role = 'user' AND content LIKE '%{"type":"text","text":"Sandbox pod %';
UPDATE messages SET content = replace(content, '{"type":"text","text":"Docker in sandbox pod ', '{"type":"text","text":"' || U&'\2063' || 'Docker in sandbox pod ') WHERE role = 'user' AND content LIKE '%{"type":"text","text":"Docker in sandbox pod %';
UPDATE messages SET content = replace(content, '{"type":"text","text":"The user stopped sandbox pod ', '{"type":"text","text":"' || U&'\2063' || 'The user stopped sandbox pod ') WHERE role = 'user' AND content LIKE '%{"type":"text","text":"The user stopped sandbox pod %';
UPDATE messages SET content = replace(content, '{"type":"text","text":"The user removed this conversation''s sandbox storage', '{"type":"text","text":"' || U&'\2063' || 'The user removed this conversation''s sandbox storage') WHERE role = 'user' AND content LIKE '%{"type":"text","text":"The user removed this conversation''s sandbox storage%';
UPDATE messages SET content = replace(content, '{"type":"text","text":"Language server ', '{"type":"text","text":"' || U&'\2063' || 'Language server ') WHERE role = 'user' AND content LIKE '%{"type":"text","text":"Language server %';
UPDATE messages SET content = replace(content, '{"type":"text","text":"The user now trusts ', '{"type":"text","text":"' || U&'\2063' || 'The user now trusts ') WHERE role = 'user' AND content LIKE '%{"type":"text","text":"The user now trusts %';
UPDATE messages SET content = replace(content, '{"type":"text","text":"The user chose not to trust ', '{"type":"text","text":"' || U&'\2063' || 'The user chose not to trust ') WHERE role = 'user' AND content LIKE '%{"type":"text","text":"The user chose not to trust %';
UPDATE messages SET content = replace(content, '{"type":"text","text":"<task-notification ', '{"type":"text","text":"' || U&'\2063' || '<task-notification ') WHERE role = 'user' AND content LIKE '%{"type":"text","text":"<task-notification %';
UPDATE messages SET content = replace(content, '{"type":"text","text":"<task-output ', '{"type":"text","text":"' || U&'\2063' || '<task-output ') WHERE role = 'user' AND content LIKE '%{"type":"text","text":"<task-output %';
UPDATE messages SET content = replace(content, '{"type":"text","text":"Notice: ', '{"type":"text","text":"' || U&'\2063' || 'Notice: ') WHERE role = 'user' AND content LIKE '%{"type":"text","text":"Notice: %';
