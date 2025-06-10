-- Make like-milestone notifications idempotent per (comment, milestone) so concurrent
-- likes can't double-send one — a partial UNIQUE index the milestone insert conflicts
-- on (INSERT OR IGNORE). Also backs `like_milestone_exists`'s comment_id lookup, which
-- previously had no supporting index.
CREATE UNIQUE INDEX IF NOT EXISTS idx_notifications_like_milestone
    ON notifications (comment_id, count)
    WHERE kind = 'like_milestone';
