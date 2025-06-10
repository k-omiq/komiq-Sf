-- Per-user reading state for numeric Suwayomi-sourced series.
--
-- Until now, library membership was per-user (`user_library`, migration 0024) but
-- reading progress for numeric Suwayomi series was still GLOBAL: `setProgress` wrote
-- back to Suwayomi and `chapters()`/`libraryProgress` read the shared
-- `suwayomi_chapter.is_read`/`last_page_read` cache — so every user saw the same
-- read/unread state and resume points. Canonical (`w_`) works already had per-user
-- state via `canonical_progress` (migration 0014); this table gives numeric series
-- the identical round-trip. Suwayomi is now a content source only — we no longer
-- push user progress back to it.
--
-- Keyed on the numeric Suwayomi chapter id (stored as TEXT to match the opaque
-- `chapterId` the client sends). `series_id` is the numeric Suwayomi manga id (as
-- TEXT), carried for per-series aggregation in `libraryProgress`. Both are FK-less:
-- like `canonical_progress`, the referenced ids aren't rows in a stable owned table
-- (they live in the refreshable `suwayomi_chapter`/`suwayomi_series` cache).
--
-- Existing global progress in `suwayomi_chapter`/Suwayomi is NOT migrated to any
-- user: we start fresh. It was shared state that never belonged to one account, so
-- attributing it would be wrong; everyone simply begins unread on numeric series.
-- `mergeWorks` needs no repoint here — its ids are `w_` work ids, whereas these keys
-- are numeric Suwayomi ids that a merge never rewrites.
CREATE TABLE suwayomi_progress (
    user_id        TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    chapter_id     TEXT NOT NULL,   -- numeric Suwayomi chapter id (not an owned FK)
    series_id      TEXT NOT NULL,   -- numeric Suwayomi manga id, for per-series aggregation
    last_page_read INTEGER NOT NULL DEFAULT 0,
    read           INTEGER NOT NULL DEFAULT 0,
    updated_at     TEXT NOT NULL,
    PRIMARY KEY (user_id, chapter_id)
);
CREATE INDEX idx_suwayomi_progress_series ON suwayomi_progress(user_id, series_id);
