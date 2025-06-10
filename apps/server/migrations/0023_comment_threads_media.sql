-- Threaded replies + one static WebP image per comment for the unified comment
-- engine (the reader's series `Discussion` and per-chapter threads now share this
-- one engine; the old bespoke "Reviews" comment list is retired to a pure rating).

-- Self-referential parent for arbitrary-depth reply trees. NULL = a top-level
-- (root) comment. ON DELETE CASCADE removes a whole subtree when a parent is
-- deleted — but `delete_comment` also removes the subtree explicitly, so
-- moderation still works on connections that haven't enabled the foreign_keys
-- pragma (e.g. the in-memory test pool). Adding a REFERENCES column via
-- ALTER TABLE is allowed because its default is NULL.
ALTER TABLE comments ADD COLUMN parent_id TEXT REFERENCES comments(id) ON DELETE CASCADE;

CREATE INDEX idx_comments_parent ON comments(parent_id);

-- One optional image per comment, stored as a budgeted lossless-WebP BLOB in
-- SQLite (Litestream-replicated like `user_avatars`, so any replica can serve it).
-- A row is created at upload time with `comment_id` NULL (staged, owned by the
-- uploader) and linked to its comment when the comment is posted. Unlinked rows
-- are orphaned drafts and can be garbage-collected by age.
CREATE TABLE comment_media (
    id          TEXT PRIMARY KEY,                                    -- media id == URL slug
    comment_id  TEXT REFERENCES comments(id) ON DELETE CASCADE,      -- NULL until linked
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    webp        BLOB NOT NULL,
    width       INTEGER NOT NULL,
    height      INTEGER NOT NULL,
    created_at  TEXT NOT NULL
);

CREATE INDEX idx_comment_media_comment ON comment_media(comment_id);
CREATE INDEX idx_comment_media_user ON comment_media(user_id);
