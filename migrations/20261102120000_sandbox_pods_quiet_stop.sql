-- SME-345: when smelt stops a pod quietly (idle, or a sub-agent's
-- finish), the stop stamps the row, and the storage-grace pass removes
-- the conversation's two claims that many days later. NULL means the
-- row's end wasn't a quiet stop — a user's Stop, the model's
-- `terminate_pod`, a lost pod — or it ended before this column existed,
-- so its storage is never swept automatically. Added nullable with no
-- default: rows the upgrade finds already written keep NULL.
ALTER TABLE sandbox_pods ADD COLUMN quietly_stopped_at TIMESTAMP NULL;
