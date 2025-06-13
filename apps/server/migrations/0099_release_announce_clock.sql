-- Bug (3) — the updates feed sorts by the SOURCE's release date, never by when we could
-- first show the chapter to anyone.
--
-- THE MEASURED SYMPTOM. Chapter 112.7 of `w_7d39e733…` carries a source release time of
-- 2026-07-28T05:07:25. Our ingest first wrote the row at 2026-08-09T15:43:07 — twelve and
-- a half days later, because the source held it behind early access until then.
-- `release_event.first_seen_at` is `COALESCE(chapter.readable_at, chapter.published_at)`
-- (see `catalog::ledger::RELEASED_AT_SQL`), so the ledger stamped 07-28, the feed's
-- `released_at` became 1785215245000, and the card entered /updates already twelve days
-- deep. It never got its spotlight. On a 2026-08-11 snapshot, of the 7,785 chapters we
-- discovered in the previous 7 days on series we were ALREADY tracking before the chapter
-- released, 2,168 (28%) were discovered more than 2.4 h after their claimed release, 1,108
-- more than 6 h, and 453 more than a full day.
--
-- `first_seen_at` IS NOT A DISCOVERY CLOCK DESPITE ITS NAME. It is the upstream release
-- clock, and it has to stay that way: `ledger::assert_no_future_events` is the deploy guard
-- that proves nothing in the table is dated from `now()`, and the feed prints this value as
-- the card's "· 4h" label (`apps/reader/src/lib/data/source.ts:331`). So this migration adds
-- a SECOND clock rather than redefining the first.
--
-- TWO COLUMNS, NOT ONE. `feed_series_updates.released_at` is simultaneously the sort key and
-- the visible label. Overwriting it with a discovery time would make a chapter released
-- twelve days ago render as "now" — the exact self-contradiction `graphql::updates` refuses
-- in its own ordering comment ("a visible ordering that contradicts its own visible labels is
-- a bug however you justify the key"). `announced_at` is the sort key; `released_at` keeps
-- describing the chapter.
--
-- THE CLOCK LIVES ON `release_event`, NOT ON `chapter`. `chapter.created_at` looks like the
-- obvious home — it is INSERT-only, never touched by `CHAPTER_UPSERT_SQL`'s DO UPDATE, so it
-- survives re-scans. It does not survive a RESEED, and the table has been reseeded: on the
-- 2026-08-11 snapshot the MINIMUM `created_at` across all 1,157,468 rows is
-- 2026-07-20T14:40:43, i.e. the column currently records when migration 0067's reseed ran and
-- nothing else. `release_event` is `INSERT OR IGNORE` + `merge_release_events`-preserving, so
-- a first writer wins permanently and no reseed can move it. That property is the whole
-- reason the ledger exists (0091), and it is what makes it the only safe place for a clock
-- that must never be recomputed.
--
-- NULLABLE ON `release_event`, AND THERE IS NO BACKFILL THERE. DELIBERATELY.
--
-- There is no honest value for the 1,031,373 existing events. We have no record of when we
-- first saw those chapters — `chapter.created_at` is the reseed clock (above), so a backfill
-- from it would stamp a million rows with a date we invented. Readers of THIS column
-- therefore spell it `COALESCE(announce_at, first_seen_at)`
-- (`catalog::project_feed_from_ledger`), which is the IDENTITY on every existing row: the new
-- rule applies only to events recorded from here on. Same shape as 0073 and 0090.
--
-- NOTE THE ASYMMETRY WITH `feed_series_updates.announced_at` BELOW, WHICH *IS* WRITTEN HERE.
-- The two columns are not the same kind of thing. `announce_at` is EVIDENCE — a fact about
-- one chapter that we either recorded or did not, and inventing one is the failure mode this
-- whole migration is built to avoid. `announced_at` is a materialized SORT KEY, derived from
-- evidence we already hold, and leaving it NULL does not make the feed honest — it makes the
-- feed unordered. So: never fabricate the evidence, always materialize the derivation.
ALTER TABLE release_event ADD COLUMN announce_at INTEGER;

-- THE WORK-LIST FOR THE PROJECTION, not an ordering index — nothing orders `release_event`
-- globally by this column.
--
-- `catalog::project_feed_from_ledger`'s announce pass only has to touch works that actually
-- HAVE an announce clock; for every other work the answer is `released_at`, which the floor
-- already supplies for free. Narrowing that pass to `work_id IN (SELECT … WHERE announce_at
-- IS NOT NULL)` took it from 1,867 ms to under 1 ms on a copy of production, which matters
-- because it runs inside the rebuild's `BEGIN IMMEDIATE` — a transaction already measured at
-- ~13 s against a 15 s `busy_timeout`, where the three-transaction split in
-- `refresh_feed_series_updates` exists specifically to preserve that margin. An unconditional
-- second aggregate over 1,031,473 rows would have spent a third of the remaining headroom to
-- recompute a value that, today, is identical for every row.
--
-- PARTIAL, so it holds only the rows that have a real announce clock: empty on the day this
-- lands, and growing only as chapters are genuinely discovered late. Same self-limiting shape
-- as 0073's `idx_chapter_needs_readable_at` and 0090's `idx_chapter_needs_key`, inverted —
-- those shrink to nothing as a backfill drains, this one grows from nothing as evidence
-- accumulates, and both cost nothing while they are empty.
CREATE INDEX IF NOT EXISTS idx_release_event_announced
    ON release_event (work_id)
    WHERE announce_at IS NOT NULL;

-- The feed's sort key. Epoch MILLIS, INTEGER, matching `released_at` — see 0091 on why a
-- timestamp in this schema is never TEXT.
ALTER TABLE feed_series_updates ADD COLUMN announced_at INTEGER;

-- AND POPULATED IN THE SAME MIGRATION, WHICH IS THE ONLY THING THAT MAKES THIS DEPLOYABLE.
--
-- `graphql::updates_feed` orders by the BARE `announced_at`. The live sequence on deploy is
-- migrate-then-serve: the process runs migrations, opens the listener, and only THEN spawns
-- the feed rebuild — on a timer, behind a sleep, in a `tokio::spawn` whose failure
-- `catalog::refresh_feed_updates`' caller logs and swallows. So every request served between
-- those two points reads whatever this migration left behind. Leaving the column NULL would
-- mean all 67,828 rows tie on NULL, SQLite breaks the tie on the index's next column, and
-- /updates serves an arbitrary `work_id`-ordered list of old series under the heading "what's
-- new" — for an unbounded window, and silently if the rebuild then fails.
--
-- Deriving the floor HERE removes that dependency entirely: the ordering is correct the
-- instant this file finishes, with no runtime pass required to make it so.
-- `catalog::default_announced_at` still exists and still runs, but it is now a maintainer of
-- an invariant this migration establishes, not the thing that establishes it.
--
-- `released_at` is `NOT NULL` on this table by contract (0064), so this leaves no NULLs
-- behind, and it is the IDENTITY: ordering by `announced_at` immediately after this statement
-- returns the same sequence as ordering by `released_at` did before it. Verified on a copy of
-- production — see the index comment below.
UPDATE feed_series_updates SET announced_at = released_at WHERE announced_at IS NULL;

-- ONE INDEX PER QUERY SHAPE, because `graphql::updates_feed` branches into four SQL shapes
-- rather than one parameterized one (`WHERE (? = 1 OR is_nsfw = 0)` is opaque to the planner,
-- so it sorts the whole table through a temp B-tree). These are 0064's four
-- `idx_fsu_*_order` indexes with `announced_at` in place of `released_at`; without all four,
-- switching the ORDER BY reintroduces exactly the temp B-tree 0064 exists to avoid.
--
-- They index the BARE column, not `COALESCE(announced_at, released_at)`, and the resolver
-- spells it bare to match. That is safe because the column is never NULL on a row the feed can
-- return, which is now true in all three states rather than eventually:
--
--   * at migration time, by the UPDATE above;
--   * after a wholesale rebuild, which `DELETE`s and re-`INSERT`s without the column, by
--     `catalog::default_announced_at` running UNGATED as the rebuild's last pass INSIDE the
--     same transaction — so no untorn generation is ever visible without it;
--   * after an incremental write, by the same function, scoped to one work.
--
-- The middle case is not hypothetical: `updates_feed_pages_are_disjoint_and_total_matches_a_-
-- full_walk` caught it during development, 25 rows with distinct release times coming back
-- shuffled. Only the ledger can supply a REAL announce clock and its projection is gated on
-- `ledger::is_complete`, so between any rebuild and a complete ledger this floor is the only
-- thing holding the ordering up.
--
-- Verified on a copy of production (67,828 feed rows) IMMEDIATELY after this file runs, with
-- no projection pass and every `announce_at` still NULL: `ORDER BY announced_at DESC,
-- work_id DESC` returns the identical row sequence to `ORDER BY released_at DESC,
-- work_id DESC` on all four query shapes, and EXPLAIN QUERY PLAN reads
-- `SEARCH … USING COVERING INDEX idx_fsu_announce_order` with no
-- `USE TEMP B-TREE FOR ORDER BY`.
CREATE INDEX IF NOT EXISTS idx_fsu_announce_order
    ON feed_series_updates (is_nsfw, announced_at DESC, work_id DESC);
CREATE INDEX IF NOT EXISTS idx_fsu_type_announce_order
    ON feed_series_updates (is_nsfw, comic_type, announced_at DESC, work_id DESC);
CREATE INDEX IF NOT EXISTS idx_fsu_all_announce_order
    ON feed_series_updates (announced_at DESC, work_id DESC);
CREATE INDEX IF NOT EXISTS idx_fsu_type_all_announce_order
    ON feed_series_updates (comic_type, announced_at DESC, work_id DESC);
