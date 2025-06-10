-- Komika-native tables. Catalog/chapters/pages/library/progress are NOT stored
-- here — they are federated live from Suwayomi. This DB owns identity + social.

CREATE TABLE users (
    id            TEXT PRIMARY KEY,
    username      TEXT NOT NULL UNIQUE,
    email         TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    avatar_url    TEXT,
    is_admin      INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT NOT NULL
);

CREATE TABLE sessions (
    token      TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL
);

CREATE INDEX idx_sessions_user ON sessions(user_id);

-- One review per (user, series); posting again updates it (upsert).
CREATE TABLE reviews (
    id          TEXT PRIMARY KEY,
    series_id   TEXT NOT NULL,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    score       INTEGER NOT NULL,
    body        TEXT NOT NULL,
    has_spoiler INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    UNIQUE (series_id, user_id)
);

CREATE INDEX idx_reviews_series ON reviews(series_id);

CREATE TABLE comments (
    id          TEXT PRIMARY KEY,
    chapter_id  TEXT NOT NULL,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    body        TEXT NOT NULL,
    has_spoiler INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL
);

CREATE INDEX idx_comments_chapter ON comments(chapter_id);
