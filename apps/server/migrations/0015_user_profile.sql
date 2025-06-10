-- Editable profile fields + a real per-user activity log (profile overhaul).
--
-- `display_name` / `bio` are user-editable (see `updateProfile`); both nullable,
-- so an account that never edits its profile shows `username` + a derived bio.
-- Avatars live on the VM's data volume (see `AVATAR_DIR`); only `avatar_url`
-- (a `/avatars/<id>.webp` path) is stored here, set by the upload endpoint.
ALTER TABLE users ADD COLUMN display_name TEXT;
ALTER TABLE users ADD COLUMN bio TEXT;

-- Real activity stream: one row per user-visible action (review / comment /
-- library add), written by the corresponding mutation. Replaces the reader's
-- previous library-snapshot approximation with timestamped events.
CREATE TABLE user_activity (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL,          -- 'review' | 'comment' | 'library_add'
    target_type TEXT,                   -- 'series' | 'chapter' (nullable)
    target_id   TEXT,                   -- the series/chapter id the action was on
    created_at  TEXT NOT NULL
);

-- Newest-first reads per user (the profile feed).
CREATE INDEX idx_user_activity_user ON user_activity (user_id, created_at DESC);
