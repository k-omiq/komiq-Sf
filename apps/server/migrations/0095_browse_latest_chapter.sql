-- Browse's chapter figure becomes TWO figures: "12 ch · Ch. 151".
--
-- WHY THIS IS A SERVER MIGRATION AND NOT A READER EDIT
--
-- Browse cards print `browse_catalogue.en_chapter_count` — a COUNT of chapters we know
-- about. The owner asked for the newest chapter's NUMBER alongside it, and there was no
-- latest-chapter-number column anywhere on the Browse path: `browse_catalogue` (0069)
-- copies eleven columns from `feed_series_updates` and `latest_chapter` was not one of
-- them. The reader cannot invent the number from the count, and the whole point of the F4
-- work is that it must never TRY — a count rendered under a "Ch." label is the exact bug
-- (`Ch. 412` on a series whose newest chapter is 10.5) this overhaul exists to kill.
--
-- WHAT THE COLUMN HOLDS. A LABEL, not a number: the string
-- `feed_series_updates.latest_chapter` already carries, which after Phase C2's ledger
-- projection is `release_event.label` for the newest event — "151", "10.5", "Oneshot",
-- occasionally a scanlator's word. It is TEXT for that reason and must not be parsed back
-- into a float; `chapter_label::ChapterLabel` is the one place that vocabulary is defined
-- and the reader's `chapterChip()` is the one place it is formatted.
--
-- NULLABLE, AND THE NULL IS THE INTERESTING CASE. Three cohorts have no value:
--   1. The ~67,000 works with no `feed_series_updates` row at all (0069's whole reason to
--      exist) — no dated chapter, so no newest chapter to name.
--   2. Works whose feed row predates the ledger projection.
--   3. Oneshot-only works whose label is a word rather than a numeral.
-- Cohorts 1 and 2 must render as the count ALONE ("12 ch"), never as "Ch. undefined" and
-- never as the count wearing a "Ch." label. That fallback lives in the reader
-- (`browse/+page.svelte`), and it is the reason this column is nullable instead of
-- defaulted: a DEFAULT would make "we do not know" indistinguishable from a real answer.
--
-- NO INDEX. This is display payload — never a filter key, never a sort key. Browse's four
-- ordering indices (0069) stay exactly as they are; adding this to them would widen every
-- one of them to serve a column no query ever seeks on.
--
-- ALTER + UPDATE, not a table rewrite. `ADD COLUMN` with no DEFAULT is metadata-only in
-- SQLite (no row rewrite), and the backfill below touches only the ~65,000 rows that have
-- a feed row to copy from. `catalog::refresh_browse_catalogue` overwrites all of it on its
-- first pass; this backfill exists for the same reason 0069's does — so Browse is correct
-- on the first request after boot rather than for the ~20 s until that pass lands.
ALTER TABLE browse_catalogue ADD COLUMN latest_chapter TEXT;

-- Copied VERBATIM from the feed row, including its NULLs, exactly like the other ten
-- shared columns. A Browse card must not disagree with the same work's Updates card, and
-- re-deriving here would be a second implementation of a rule (the ledger projection) that
-- already has one.
UPDATE browse_catalogue SET latest_chapter = f.latest_chapter
    FROM feed_series_updates f
   WHERE f.work_id = browse_catalogue.work_id;
