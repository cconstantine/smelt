-- SME-354 B19: a failure notice was told from the user's own text by its
-- words alone ("The last turn failed before it finished: "), so a message
-- the user typed starting with them showed as a notice. New notices start
-- with U+2063 (INVISIBLE SEPARATOR, api::chat::NOTICE_MARK), which
-- send_message drops from what the user sends; this marks the ones already
-- saved, so they still show as notices. A notice is one text block, its
-- message saved by serde_json as `[{"type":"text","text":"…"}]`; the mark
-- goes in after that block's opening, which is 24 characters long. A
-- message already marked no longer matches, so a second run changes nothing.
UPDATE messages
SET content = '[{"type":"text","text":"' || U&'\2063' || substr(content, 25)
WHERE role = 'user'
  AND content LIKE '[{"type":"text","text":"The last turn failed before it finished: %';
