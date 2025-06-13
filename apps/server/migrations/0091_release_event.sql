-- Phase C1 — the release ledger. First-source-wins becomes a PRIMARY KEY.
--
-- THE REQUIREMENT, VERBATIM
--
--   "For series with multiple sources, if x source updates chapter y first, it will be
--    registered in the updates page and if more sources update the chapters later, they
--    won't hit the updates page since another extension updated it earlier."
--
-- Today the feed does the exact opposite. `catalog::refresh_feed_series_updates` merges a
-- work's release time with
--     released_at = MAX(feed_series_updates.released_at, excluded.released_at)
-- so when a SECOND source mirrors a chapter a first source already reported, the card's
-- clock moves FORWARD and it jumps back to the top of /updates. The same chapter is
-- announced twice, and the second announcement outranks fresh ones.
--
-- That is not a query bug that can be patched in place: `feed_series_updates` is keyed by
-- WORK, so it has nowhere to record which chapters have already been announced. The rule
-- is per CHAPTER. This table is that missing key.
--
-- With `PRIMARY KEY (work_id, chapter_key)` and `INSERT OR IGNORE`, the second source's
-- write is a no-op at the storage layer. There is no comparison to get backwards, no
-- ordering to defeat, and no query that can regress it.
--
-- THREE THINGS THAT LOOK LIKE DETAILS AND ARE NOT
--
-- 1. `first_source_series_id` HAS NO FOREIGN KEY, DELIBERATELY. `chapter.source_series_id`
--    is `ON DELETE CASCADE` and `db.rs` sets `PRAGMA foreign_keys = ON`, so a real
--    reference here would mean Phase F's deletion of 10,422 `all.mangadex` `source_series`
--    rows silently erased the first-seen history of every chapter those rows happened to
--    announce first. The column is a nullable, non-enforcing note about who won the race —
--    losing the source must not lose the fact.
--
-- 2. `work_id` DOES cascade, and that is correct: a work is the ledger's actual parent. But
--    a merge must not USE that cascade — `catalog::merge_release_events` moves the losing
--    work's rows onto the survivor, keeping the EARLIEST `first_seen_at` per key, before
--    the `DELETE FROM work` runs. Without it every dedup merge would re-announce the
--    merged-in work's entire back catalogue.
--
-- 3. `first_seen_at` IS EPOCH MILLIS, INTEGER. Not TEXT. The two halves of the catalogue
--    store timestamps in incompatible text encodings (ISO-8601 vs 13-digit millis) and
--    SQLite compares TEXT under BINARY collation, where every '2…' sorts above every '1…'
--    — migration 0064 exists because of that. An INTEGER cannot have the problem.
--
-- SEEDED FROM min(released_at), NEVER FROM now(). This is the single worst deployment
-- hazard in the plan: seeding with the current time would stamp ~1.3 M back-catalogue
-- chapters as having been released this instant and dump the entire history onto page 1 of
-- /updates. The seed lives in `catalog::ledger`, runs in the background AFTER the Phase B
-- spine drains (it cannot run here — the Suwayomi half of the spine does not exist yet at
-- migration time), takes its time from `COALESCE(readable_at, published_at)`, and asserts
-- `MAX(first_seen_at) <= now` before it is allowed to finish.
CREATE TABLE IF NOT EXISTS release_event (
    work_id                TEXT    NOT NULL REFERENCES work(id) ON DELETE CASCADE,
    chapter_key            TEXT    NOT NULL,
    -- Epoch MILLIS. When this chapter was first readable anywhere, from the source that
    -- had it first — not when we noticed, and not when a later source mirrored it.
    first_seen_at          INTEGER NOT NULL,
    -- Who had it first. Nullable and NON-ENFORCING; see note 1 above.
    first_source_series_id TEXT,
    -- What to call it, from the winning source, via the one labelling rule.
    label                  TEXT    NOT NULL,
    PRIMARY KEY (work_id, chapter_key)
) WITHOUT ROWID;

-- WITHOUT ROWID because the primary key IS the identity: every read is either "this work's
-- events" or "the newest events", nothing holds a rowid reference, and at ~1.3 M rows of
-- ~60 bytes the saved second b-tree is worth having.

-- /updates in one index: newest release first. The feed's whole job is this ordering, and
-- it is what lets the incremental writer ask "what is new since X" without touching the
-- 877k-row chapter table.
CREATE INDEX IF NOT EXISTS idx_release_event_recent
    ON release_event (first_seen_at DESC);
