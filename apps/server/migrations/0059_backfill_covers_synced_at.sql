-- Backfill `work.covers_synced_at` for works that already have cover data.
--
-- Migration 0021 added the column with no backfill, and the only writer is the
-- enrichment path (`catalog::mark_covers_synced`), which is off by default
-- (`METADATA_BACKFILL` unset). Verified against production: of 109,241
-- mangadex-anchored works, `metadata_synced_at` is non-NULL for ALL of them but
-- `covers_synced_at` is NULL for ALL of them. So `works_needing_enrichment`'s
-- predicate — `metadata_synced_at IS NULL OR covers_synced_at IS NULL` — reduces
-- to TRUE and selects 100% of the catalogue. Turning the drainer on would issue
-- ~109k MangaDex round-trips to re-enrich already-enriched works, purely to set
-- this column.
--
-- SEMANTIC NOTE (deliberate): 0021 defines the column as "the FULL cover set was
-- fetched from /cover", whereas the catalogue sweep stores only the PRIMARY cover.
-- This backfill therefore asserts something slightly weaker than the original
-- intent — "this work has cover data" rather than "every per-volume/per-locale
-- cover was fetched". That is the right trade here: the reader and admin render
-- `work.cover_file_name` (the primary), the full `work_cover` set is not surfaced
-- anywhere today, and the alternative is 109k API calls to re-derive a timestamp.
-- A work that genuinely needs its full cover set can still be re-enriched by
-- clearing this column for it.
--
-- Rows touched, re-measured read-only against production 2026-07-26: 109,205 of
-- 112,765 works (an earlier reading of 109,200 has since drifted — the catalogue
-- is live, so treat this as approximate). The two candidate predicates — a
-- `work_cover` row, or a non-NULL `cover_file_name` — select exactly the same
-- 109,205 works; both are kept so the migration stays correct if they diverge.
-- `covers_synced_at` is non-NULL for 0 works beforehand, so the guard below is
-- what makes the re-run free rather than what limits the first run.
--
-- `COALESCE(updated_at, created_at)`: verified that `updated_at` is non-NULL for
-- all 109,205 matched rows, so the fallback never actually fires today. It is
-- kept because `work.updated_at` is nullable in the schema.
--
-- Runtime, measured on a byte-for-byte copy of the production DB with the
-- production pragmas (mmap 1.5 GiB, cache_size -16000) and the page cache
-- explicitly evicted first: 5.05 s cold for the 109,205-row UPDATE, then 0.08 s
-- on a second run (idempotency confirmed: the row count does not change and the
-- `covers_synced_at IS NULL` guard matches nothing). Migrations run before the
-- listener binds, so this delays the FIRST boot after deploy by ~5 s and no boot
-- after that. That is a one-time cost paid against ~109k avoided MangaDex
-- requests; it is not chunked because a partial application would leave the
-- enrichment predicate in exactly the mixed state this is fixing.
--
-- `updated_at` is used as the stamp: it is the closest available "when was this
-- work's data, including its cover, last known good".
UPDATE work
SET covers_synced_at = COALESCE(updated_at, created_at)
WHERE covers_synced_at IS NULL
  AND (
        cover_file_name IS NOT NULL
     OR EXISTS (SELECT 1 FROM work_cover wc WHERE wc.work_id = work.id)
  );
