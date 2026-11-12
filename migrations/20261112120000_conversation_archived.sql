-- SME-480: a top-level conversation can be archived, so the sidebar lists
-- only the ones still in use. NULL = in use; set only on rows with
-- `parent_id IS NULL` (an agent follows its root, which the tree reads).
-- No index: it's read per row of the full-table tree query.
ALTER TABLE conversations ADD COLUMN archived_at TIMESTAMP NULL;
