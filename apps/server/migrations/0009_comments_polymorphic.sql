-- Generalize comments from chapter-only to a polymorphic target so one comment
-- system serves per-chapter threads AND series-level discussion (and later canonical
-- works). Rebuild the table (SQLite can't add NOT NULL + drop a column in place),
-- backfilling existing rows as chapter comments. Nothing references `comments`, so the
-- drop/rename is safe.
CREATE TABLE comments_new (
    id          TEXT PRIMARY KEY,
    target_type TEXT NOT NULL,                        -- 'chapter' | 'series'
    target_id   TEXT NOT NULL,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    body        TEXT NOT NULL,
    has_spoiler INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL
);

INSERT INTO comments_new (id, target_type, target_id, user_id, body, has_spoiler, created_at)
    SELECT id, 'chapter', chapter_id, user_id, body, has_spoiler, created_at FROM comments;

DROP TABLE comments;
ALTER TABLE comments_new RENAME TO comments;

CREATE INDEX idx_comments_target ON comments(target_type, target_id);
