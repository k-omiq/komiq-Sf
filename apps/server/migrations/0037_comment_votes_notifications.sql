-- Comment likes/dislikes + the inbound notification feed.

-- One vote per (comment, user); `value` is +1 (like) or -1 (dislike). Re-voting
-- upserts the row; clearing a vote deletes it. Distinct from `reviews` (series
-- ratings) — this is per-comment reaction. ON DELETE CASCADE drops a comment's votes
-- with the comment (and a user's votes with the user).
CREATE TABLE IF NOT EXISTS comment_votes (
    comment_id TEXT NOT NULL REFERENCES comments(id) ON DELETE CASCADE,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    value      INTEGER NOT NULL,            -- +1 like, -1 dislike
    created_at TEXT NOT NULL,
    PRIMARY KEY (comment_id, user_id)
);

-- Tally likes/dislikes for a comment, and find a viewer's own vote, by comment.
CREATE INDEX IF NOT EXISTS idx_comment_votes_comment ON comment_votes (comment_id);

-- Inbound notifications: something ANOTHER user did to the recipient's content.
-- Distinct from `user_activity` (the recipient's OWN outbound action feed). Kinds:
--   'reply'          — someone replied to the recipient's comment (per event).
--   'like_milestone' — the recipient's comment crossed a like-count milestone.
-- `read_at` NULL means unread. `actor_id` is the triggering user (the replier); it is
-- NULL for an aggregate milestone. `comment_id` is the recipient's OWN comment the
-- event is about; `target_type`/`target_id` carry its thread so the client can deep-link.
CREATE TABLE IF NOT EXISTS notifications (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,     -- recipient
    kind        TEXT NOT NULL,
    actor_id    TEXT REFERENCES users(id) ON DELETE SET NULL,
    comment_id  TEXT REFERENCES comments(id) ON DELETE CASCADE,
    target_type TEXT,
    target_id   TEXT,
    count       INTEGER,                                                  -- milestone value ('like_milestone')
    created_at  TEXT NOT NULL,
    read_at     TEXT
);

-- Newest-first per recipient (the bell dropdown) and the unread-count query.
CREATE INDEX IF NOT EXISTS idx_notifications_user ON notifications (user_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_notifications_unread ON notifications (user_id, read_at);
