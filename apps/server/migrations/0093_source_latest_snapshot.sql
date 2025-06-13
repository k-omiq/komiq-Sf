-- Phase E5 — LATEST-diff discovery.
--
-- The measured fact (§7 Phase E5, re-derived honestly in §8e): ~98.8% of every live
-- `fetchMangaAndChapters` mutation the scanner issues discovers NOTHING. Polling every
-- series on a timer is the wrong model. A source's LATEST listing is already ordered by
-- the source itself, newest chapter first — a push signal we throw away every day.
--
-- E5 reads that signal. Every ~15 min it fetches page 1 of each subscribed source's
-- LATEST and diffs the ordered manga-id list against the previous snapshot. Any series
-- that ENTERED the page or MOVED UP has new content upstream, so it is enqueued for one
-- targeted scan (reusing E2's `trigger_due_now`); everything else is left alone.
--
-- This table is that previous snapshot — one row per source, the last-seen page-1 order.
-- It must survive process restarts (a lost snapshot re-baselines and triggers nothing for
-- one tick, which is correct-but-slow, not wrong), so it lives in the DB rather than in
-- memory. `ordered_ids` is a JSON array of Suwayomi manga ids, newest-first, capped to a
-- bounded window at write time (see `discovery::SNAPSHOT_WINDOW`).
--
-- Keyed by `source_id` (the Suwayomi numeric source id, same value carried on
-- `source_series.source_id`), not by pkg — a multi-language extension exposes one source
-- per language and only the English one is walked.
CREATE TABLE IF NOT EXISTS source_latest_snapshot (
    source_id   TEXT PRIMARY KEY,
    ordered_ids TEXT NOT NULL,
    captured_at TEXT NOT NULL
);
