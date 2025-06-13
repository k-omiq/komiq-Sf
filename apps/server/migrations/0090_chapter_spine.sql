-- Phase B — one canonical chapter spine for every source.
--
-- WHY THIS EXISTS
--
-- `chapter` holds 877,824 MangaDex rows and **zero** Suwayomi rows. The Suwayomi half of
-- the catalogue lives in `suwayomi_chapter`, keyed by Suwayomi's own manga id, with no
-- link to `source_series` and therefore none to `work`. Every downstream query that wants
-- "the chapters of this work" is consequently a two-branch UNION written by hand
-- (`catalog::work_source_chapters`), and every query that wants "the newest chapter of
-- this work across all its sources" — which is what /updates is — cannot be written at
-- all. Phase C's `release_event` ledger needs exactly that query, so the spine comes first.
--
-- This migration is ADDITIVE AND INERT. It adds two columns and two indexes; it moves no
-- data. Both drains that populate it (`catalog::spine`) run in the background, are
-- resumable, and are self-limiting.
--
-- THE TWO COLUMNS
--
-- `chapter_key` is the grouping identity that folds the same chapter across sources into
-- one row. It is `round(number * 100)` as text, which is NOT a new invention: it is
-- exactly what `chapter_override.chapter_key` has been since migration 0032 and what
-- `graphql::chapter_key` computes, and admin chapter-hiding matches on it. Using any other
-- scale would silently stop hiding hidden chapters. `chapter_label::ChapterLabel::key` is
-- the one implementation now, and unnumbered chapters (oneshots, "Extra") get their own
-- `x:<external_id>` namespace so a work's three oneshots stay three rows instead of
-- colliding onto chapter 0.
--
-- `label` is what to PRINT for the chapter — "45", "10.5", "Oneshot" — as decided by the
-- one labelling rule. It is stored, not derived on read, because the consumers that need
-- it most are SQL: the release ledger's seed and the feed writers. `feed_series_updates`
-- currently builds its own label with `printf('%g', lc.chapter_number)`, which is a FOURTH
-- labelling rule sitting next to the three `chapter_label` replaced — it prints
-- `Ch. 100000000` for the TEST upload, `Ch. -1` for Suwayomi's oneshot sentinel, and
-- nothing usable for a chapter whose number is a word. Storing the label is what lets that
-- become a column read instead.
--
-- `label` and `chapter_key` are written by the same `chapter_display` call, in the same
-- statement, and there is exactly one function that writes either. So they cannot disagree
-- with each other; the only staleness either can have is against a *newer version of the
-- rule*, which is what the key drain re-runs.
--
-- `scanlator` is carried because `work_source_chapters` already returns it for Suwayomi
-- rows and the unified query has to keep returning it. MangaDex leaves it NULL — the
-- mirror models translation groups as a relationship, not a string.
--
-- NULLABLE, DELIBERATELY. `chapter_key` stays NULL on all 877,824 existing rows. A boot
-- migration must not rewrite a table that size, and the partial index below turns "not yet
-- keyed" into the backfill's own work-list: it holds only the rows still to do and shrinks
-- to nothing as the drain runs, at which point it costs nothing to maintain. Same shape as
-- migration 0073's `idx_chapter_needs_readable_at`.
ALTER TABLE chapter ADD COLUMN chapter_key TEXT;
ALTER TABLE chapter ADD COLUMN label TEXT;
ALTER TABLE chapter ADD COLUMN scanlator TEXT;

-- Ranking a source by how far ahead it reads — `MAX(CAST(chapter_key AS INTEGER))` over
-- one source_series — is the query behind F12's source picker (Phase C1b). It is bounded
-- by a single series' chapter count, so this composite is what keeps it an index scan
-- rather than a heap read per chapter.
CREATE INDEX IF NOT EXISTS idx_chapter_key
    ON chapter (source_series_id, chapter_key);

-- The key drain's work-list. Partial, so once it drains this index is empty and free.
CREATE INDEX IF NOT EXISTS idx_chapter_needs_key
    ON chapter (id)
    WHERE chapter_key IS NULL;
