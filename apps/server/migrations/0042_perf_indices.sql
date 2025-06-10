-- Additive read-path indices (no data change). Three hot queries do full table
-- scans today because the columns they filter/sort/aggregate on are unindexed.
-- All partial/composite, all cheap to build, all safe under Litestream (they
-- replicate with the main DB like any other schema object).

-- 1. "Latest Updates" home feed (graphql: `updates`) orders every federated series
--    by `last_new_chapter_at DESC`. `series_scan_state` is PK-only (series_id), so
--    the query full-scans + filesorts the whole table on every home load. A partial
--    DESC index over the non-NULL rows lets the ORDER BY ... LIMIT stop early.
CREATE INDEX IF NOT EXISTS idx_scan_state_new_chapter
    ON series_scan_state(last_new_chapter_at DESC)
    WHERE last_new_chapter_at IS NOT NULL;

-- 2. Cover drainer (cover::pending_cover_count / crawl_uncached_covers) repeatedly
--    finds works whose cover isn't materialized yet via `cover_cached_version IS NULL`.
--    Unindexed => a full `work` scan each tick; the partial index shrinks to near-zero
--    as covers fill in, so the drain gets cheaper over its own lifetime.
CREATE INDEX IF NOT EXISTS idx_work_cover_pending
    ON work(cover_cached_version)
    WHERE cover_cached_version IS NULL;

-- 3. Canonical "updates" feed (graphql: `canonical_updates`) joins chapter->source_series,
--    filters `source_type='mangadex' AND lang='en'`, and takes
--    MAX(COALESCE(published_at, created_at)) per work group. The existing
--    idx_chapter_source_series (source_series_id only) can't serve the lang filter or the
--    per-group aggregate; this composite covers the join key, the lang filter, and both
--    aggregate columns so the MAX is found without touching the table.
CREATE INDEX IF NOT EXISTS idx_chapter_ss_lang_pubdate
    ON chapter(source_series_id, lang, published_at, created_at);
