-- Backfill progress marker for MangaDex metadata enrichment (H1).
--
-- `backfillMangadexMetadata` previously re-selected works that have no
-- `work_description` row on every call — but a work whose upstream MangaDex
-- record carries no localized description NEVER gets a row, so the cursor stuck
-- and the backfill never drained. This column records that a work's metadata was
-- ATTEMPTED (set by every enrichment upsert), independently of whether any
-- description was found, so the backfill advances past description-less works.
-- NULL = never attempted (pre-S2 works) → eligible for backfill.
ALTER TABLE work ADD COLUMN metadata_synced_at TEXT;
