-- Per-user reading state for canonical (MangaDex-mirrored, `w_`-prefixed) works.
-- Suwayomi (numeric-id) series keep their progress/library in Suwayomi; canonical
-- works have no per-user store, so the reader always resumed at chapter 1 and the
-- library toggle no-op'd (CR6). These two tables give canonical works the same
-- round-trip. Keyed on opaque text ids (MangaDex chapter uuid / `w_` work id),
-- mirroring `reviews`/`comments`: user-FK'd, but the mirrored ids are FK-less since
-- they aren't rows in a stable owned table.
CREATE TABLE canonical_progress (
    user_id        TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    chapter_id     TEXT NOT NULL,   -- MangaDex chapter uuid (not an owned FK)
    work_id        TEXT NOT NULL,   -- w_-prefixed work id, for per-series aggregation
    last_page_read INTEGER NOT NULL DEFAULT 0,
    read           INTEGER NOT NULL DEFAULT 0,
    updated_at     TEXT NOT NULL,
    PRIMARY KEY (user_id, chapter_id)
);
CREATE INDEX idx_canonical_progress_work ON canonical_progress(user_id, work_id);

CREATE TABLE canonical_library (
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    work_id    TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (user_id, work_id)
);
