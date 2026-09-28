-- SME-51 B7: one live pod per conversation, as the database's own rule
-- rather than only a check before inserting. Any duplicates a past race
-- left are closed first, keeping each conversation's newest.
UPDATE sandbox_pods
   SET terminated_at = now()
 WHERE terminated_at IS NULL
   AND id NOT IN (
       SELECT max(id) FROM sandbox_pods WHERE terminated_at IS NULL GROUP BY conversation_id
   );

CREATE UNIQUE INDEX sandbox_pods_one_live_per_conversation
    ON sandbox_pods (conversation_id)
 WHERE terminated_at IS NULL;
