-- SME-123 DR1: titles came from the first 60 characters of the first
-- message, so an agent's brief left "You are the **Ticket Driver for
-- SME-345** (Linear, …" in the sidebar. New titles drop markdown marks,
-- and an agent's is its brief's first bold phrase (db::title_from_text);
-- this tidies the ones already saved the same way. Only an agent's title
-- becomes its bold phrase: the user's own keeps its words, marks dropped
-- (review 1 M2: "Fix the **login** page" isn't "login").
UPDATE conversations
SET title = CASE
    WHEN parent_id IS NOT NULL AND title ~ '\*\*[^*]*[^*\s][^*]*\*\*'
        THEN btrim(substring(title from '\*\*([^*]*[^*\s][^*]*)\*\*'))
    ELSE btrim(regexp_replace(replace(title, '**', ''), '\s+', ' ', 'g'))
END
WHERE title LIKE '%**%';
