-- SME-123 DR1: titles came from the first 60 characters of the first
-- message, so an agent's brief left "You are the **Ticket Driver for
-- SME-345** (Linear, …" in the sidebar. New titles are the first bold
-- phrase (db::title_from_text); this tidies the ones already saved: a
-- closed bold phrase becomes the title, and a cut-off one loses its marks.
UPDATE conversations
SET title = CASE
    WHEN title ~ '\*\*[^*]*[^*\s][^*]*\*\*' THEN btrim(substring(title from '\*\*([^*]*[^*\s][^*]*)\*\*'))
    ELSE btrim(regexp_replace(replace(title, '**', ''), '\s+', ' ', 'g'))
END
WHERE title LIKE '%**%';
