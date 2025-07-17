-- Re-key Browse's "Latest added" ordering from `announced_at` to `created_at`.
--
-- WHY. `announced_at` is a CHAPTER clock: `catalog::project_feed_from_ledger` writes it
-- as MAX(COALESCE(announce_at, first_seen_at)) over a work's release events, so every
-- new chapter discovered on an already-watched source bumps the work's feed row to ~now
-- (see ANNOUNCE_AT_SQL in catalog/ledger.rs). That is the honest order for /updates —
-- "what is new" — which is exactly what migration 0099 built it for. But Browse's
-- LATEST_ADDED sort reused it (0101), so "Latest added" rendered "most recent new
-- chapter of an existing work": the page filled with actively-updating series — which
-- overlap TRENDING, because views follow new chapters — while a work genuinely added
-- yesterday but not updating sat pages deep. Measured symptom: page 1 of Latest added
-- matched the Updates feed's head, not the catalogue's intake order.
--
-- `created_at` is the WORK clock: `work.created_at`, verbatim, when the work entered
-- our catalogue — the same field `Series.createdAt` resolves from and the reader's
-- `addedAt` contract already documents (source.ts: "when the series entered the
-- catalogue"). It is NOT NULL on every row (0069), so no NULLS-last handling is needed.
--
-- ORDERING ON TEXT ISO-8601 IS SAFE HERE, WITH ONE CAVEAT. Every production writer
-- stamps `work.created_at` either as chrono `to_rfc3339()` ("2026-07-26T13:46:51.123+00:00",
-- UTC, "+00:00" suffix) or as this schema's own strftime("%Y-%m-%dT%H:%M:%SZ") ("…Z"
-- suffix, 0005's backfill). Under BINARY collation, date-time digits decide every
-- comparison before the suffix is reached, so DESC is chronological. The two suffix
-- encodings ('Z' vs '+00:00') compare 'Z' > '+', which can only mis-order two rows
-- whose timestamps are EQUAL TO THE WHOLE SECOND — a tie the mandatory `work_id DESC`
-- tiebreaker keeps total and deterministic. A space-separated "YYYY-MM-DD HH:MM:SS"
-- encoding would break same-day comparisons (' ' < 'T'); no production writer emits
-- that (the only ones are test fixtures, which bind their own literals).
--
-- THE 0101 INDICES ARE DEAD WEIGHT THE MOMENT THE SORT MOVES. Nothing else orders by
-- `browse_catalogue.announced_at` (the /updates feed reads `feed_series_updates`, a
-- different table), so the four idx_bc_*announced indices below would cost index
-- maintenance on every rebuild and incremental mirror for zero reads. Dropped. The
-- `announced_at` COLUMN stays: it is a verbatim copy of the feed row (the two writers
-- share one mutable-column list), it is cheap to keep writing, and dropping a column
-- from a 115k-row table buys nothing that matters.

DROP INDEX IF EXISTS idx_bc_announced;
DROP INDEX IF EXISTS idx_bc_type_announced;
DROP INDEX IF EXISTS idx_bc_all_announced;
DROP INDEX IF EXISTS idx_bc_type_all_announced;

-- The same four planner shapes as every other Browse ordering: {NSFW hidden, NSFW
-- shown} x {all formats, one format}. Payload columns keep the common filters
-- index-only. `created_at` is already populated NOT NULL on every row (0069), so there
-- is no backfill — the indices can be built directly.
CREATE INDEX IF NOT EXISTS idx_bc_added
    ON browse_catalogue(is_nsfw, created_at DESC, work_id DESC,
                        status, content_rating, en_chapter_count);
CREATE INDEX IF NOT EXISTS idx_bc_type_added
    ON browse_catalogue(is_nsfw, comic_type, created_at DESC, work_id DESC,
                        status, content_rating, en_chapter_count);
CREATE INDEX IF NOT EXISTS idx_bc_all_added
    ON browse_catalogue(created_at DESC, work_id DESC,
                        status, content_rating, en_chapter_count);
CREATE INDEX IF NOT EXISTS idx_bc_type_all_added
    ON browse_catalogue(comic_type, created_at DESC, work_id DESC,
                        status, content_rating, en_chapter_count);

ANALYZE browse_catalogue;
