-- Support the periodic garbage-collection sweep of orphaned staged uploads
-- (`gc::sweep`): DELETE FROM comment_media WHERE comment_id IS NULL AND
-- created_at < <now - 24h>. A composite (comment_id, created_at) index lets the
-- sweep seek directly to the staged (NULL comment_id) rows ordered by age instead
-- of scanning the whole table each hour. The existing single-column
-- idx_comment_media_comment still serves the link-time lookups.
CREATE INDEX IF NOT EXISTS idx_comment_media_gc ON comment_media(comment_id, created_at);
