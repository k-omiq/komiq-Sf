-- Per-user library: the series a user has explicitly added ("add to library").
--
-- Distinct from `suwayomi_series.in_library`, which marks catalogued content the
-- scanner tracks (an operator/catalogue concept, shared across everyone) — THIS is
-- the user-facing "Your Library". Before this table the `library` query returned
-- the whole catalogued set and the `mark` mutation toggled Suwayomi's global
-- in-library flag, so every user saw the same ~571-series "library" and an
-- anonymous visitor saw one too. Membership is now per-user and requires auth.
--
-- `series_id` is an opaque text id — a numeric Suwayomi series id or a `w_`
-- canonical work id — matching whatever `mark(seriesId:)` was called with.
-- Supersedes the canonical-only `canonical_library` (folded in below).
CREATE TABLE user_library (
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    series_id  TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (user_id, series_id)
);
CREATE INDEX idx_user_library_user ON user_library(user_id, created_at);

-- Fold any existing canonical library rows into the unified table.
INSERT OR IGNORE INTO user_library (user_id, series_id, created_at)
    SELECT user_id, work_id, created_at FROM canonical_library;
