-- Materialized "canonical updates" feed.
--
-- `canonicalUpdates` computed a GROUP BY over the whole `chapter` table (804k rows,
-- two temp B-trees) on EVERY request — measured 3.5-4.0s, and it ran on every view of
-- both `/` and `/updates`. This table holds the already-grouped result (one row per
-- work, its newest RELEASED English chapter), so the resolver becomes an indexed
-- range-scan of ~20 rows.
--
-- It is a cache: every column is derivable from `chapter` + `work` + `source_series`,
-- so it is excluded from nothing that matters and can be rebuilt at any time. Refreshed
-- by `refresh_feed_updates` at boot and after each MangaDex catalogue sync — which is
-- exactly when the underlying canonical chapters change, so no separate timer is
-- needed. (Caveat: with CATALOGUE_SYNC off the feed only refreshes at boot; on the
-- production single-VPS deploy sync is on, so it stays fresh within the sync interval.)
-- Rebuilding rather than maintaining per-insert avoids instrumenting every
-- chapter-write path, at the cost of at most one sync-interval of staleness — fine for
-- a "what's new" surface.
CREATE TABLE feed_updates (
    work_id              TEXT PRIMARY KEY REFERENCES work(id) ON DELETE CASCADE,
    mangadex_id          TEXT,
    title                TEXT NOT NULL,
    is_nsfw              INTEGER NOT NULL DEFAULT 0,
    cover_url            TEXT,
    latest_chapter       TEXT,       -- chapter number of the newest released chapter
    latest_chapter_title TEXT,
    latest_at            TEXT NOT NULL  -- real publish time, always <= the refresh's "now"
);

-- The feed's only read pattern: newest-first, gated by NSFW. `is_nsfw` leads so the
-- anonymous (is_nsfw = 0) slice is a covered prefix scan.
CREATE INDEX idx_feed_updates_order ON feed_updates(is_nsfw, latest_at DESC, work_id DESC);
