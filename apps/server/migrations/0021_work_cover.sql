-- Multi-cover storage (F2).
--
-- `work.cover_file_name` keeps the single PRIMARY cover (unchanged — the reader's
-- current cover rendering reads it). This table holds the FULL cover set a
-- MangaDex work carries: per-volume, per-locale art. The main catalogue sweep
-- stores just the primary here (from the manga's expanded cover_art relationship,
-- no extra request); the enrichment/backfill path fetches the complete set from
-- MangaDex's `/cover` endpoint and replaces the rows.
CREATE TABLE work_cover (
    work_id        TEXT NOT NULL REFERENCES work(id) ON DELETE CASCADE,
    cover_file_name TEXT NOT NULL,       -- the covers/{mangadex_id}/{fileName} leaf
    lang           TEXT,                  -- cover locale (e.g. "ja", "en"); NULL if absent
    volume         TEXT,                  -- volume label (e.g. "1", "12.5"); NULL for a standalone cover
    is_primary     INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (work_id, cover_file_name)
);

CREATE INDEX idx_work_cover_work ON work_cover(work_id);

-- Marks that a work's FULL cover set was fetched from `/cover` (F2). The sweep
-- stores only the primary and leaves this NULL, so the enrichment/backfill path
-- (which fetches the complete set) selects `covers_synced_at IS NULL` and drains
-- without re-processing already-fetched works.
ALTER TABLE work ADD COLUMN covers_synced_at TEXT;
