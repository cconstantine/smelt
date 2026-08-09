-- messages.content used to hold plain text; application code now stores
-- Vec<ContentBlock> serialized as JSON (see Message::blocks()). Backfill
-- existing rows into the equivalent single-Text-block JSON shape so old and
-- new rows parse identically. Runs once per database (sqlx tracks applied
-- migrations), so there's no risk of double-wrapping already-migrated rows.
UPDATE messages
SET content = json_build_array(json_build_object('type', 'text', 'text', content))::text;
