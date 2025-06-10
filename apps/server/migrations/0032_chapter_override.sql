-- Admin chapter overrides for the series-detail editor. Chapters are read-only
-- projections of upstream sources (Suwayomi/MangaDex), so "delete" and "rename" are
-- modelled as a NON-DESTRUCTIVE override layer keyed by canonical work + chapter
-- number: hiding suppresses a chapter from readers WITHOUT deleting the cached row,
-- so a re-scan can't resurrect it and the action is reversible.
--
-- chapter_key is the aggregate bucket key `round(number * 100)` (as text) that
-- group_aggregated_chapters() uses, so an override matches a chapter number across
-- every source that provides it (e.g. hiding a spam/duplicate ch. 10.5 everywhere).
CREATE TABLE chapter_override (
    work_id        TEXT NOT NULL REFERENCES work(id) ON DELETE CASCADE,
    chapter_key    TEXT NOT NULL,               -- round(number*100) as text
    hidden         INTEGER NOT NULL DEFAULT 0,  -- 1 => suppressed from readers
    title_override TEXT,                         -- NULL => use the source title
    updated_at     TEXT NOT NULL,
    PRIMARY KEY (work_id, chapter_key)
);

CREATE INDEX idx_chapter_override_work ON chapter_override(work_id);
