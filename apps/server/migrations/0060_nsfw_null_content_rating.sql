-- Data fix: clear NSFW over-flagging on the Suwayomi-only tail migration 0053 could
-- not reach.
--
-- 0053 reset `is_nsfw` on works MangaDex rates safe/suggestive, and that predicate now
-- matches 0 rows. But it keys on `work.content_rating`, and a residual 1,956 works
-- carry `is_nsfw = 1` with `content_rating IS NULL` (measured 2026-07-26) — so 0053's
-- WHERE clause structurally cannot see them. Every one of them is Suwayomi-only (0 have
-- a `mangadex` `source_series` and 0 have a `mangadex` `work_external_id`), which is
-- exactly why the column is NULL: only the direct-MangaDex ingest path populates it.
-- They were therefore flagged by source-level taint alone — `source_extension.is_nsfw`
-- OR'd onto the work by `mark_work_nsfw` / `rederive_suwayomi_nsfw` — and every
-- MangaDex Suwayomi source is flagged adult (61 per-language source rows, all
-- `is_nsfw = 1`), because MangaDex hosts adult content somewhere in its catalogue.
-- This is the same false-positive class as 0053: mainstream titles hidden from every
-- anonymous surface.
--
-- RECOVERING THE MISSING RATING. The MangaDex Tachiyomi extension emits the title's
-- MangaDex content rating as a genre string ("Content rating: Suggestive"), and omits
-- it entirely when the rating is `safe`. `suwayomi_series.genre` stores that array
-- verbatim, so the authoritative rating IS present for these works — just in the wrong
-- column. Validated against ground truth on the 8,420 works that carry BOTH an
-- `all.mangadex` Suwayomi series AND a MangaDex-anchored `content_rating`:
--
--     content_rating   genre tag       n
--     safe             (no tag)     5,888     suggestive   suggestive   2,524
--     safe             suggestive       2     suggestive   (no tag)         4
--     erotica          erotica          1     erotica      suggestive       1
--
-- i.e. "no tag" => safe at 5,888/5,892 (99.93%) and "suggestive tag" => suggestive at
-- 2,524/2,528 (99.84%); the 6 disagreements all sit WITHIN the safe/suggestive band
-- that this migration treats identically, so the effective error rate on the SFW/NSFW
-- decision is 2/8,420 (the two erotica rows, both excluded below by the genre guard).
--
-- SCOPE — 1,831 of the 1,956 (verified by COUNT on live data 2026-07-26):
--   1,348 rated safe (no rating tag)   e.g. Bocchi the Rock!, Teenage Mercenary, Shy
--     483 rated suggestive             e.g. One-Punch Man, My Dress-Up Darling, Nagatoro
-- Deliberately NOT touched, leaving them NSFW:
--     106 with `is_nsfw_override = 1`  admin-marked; omegascans, genuinely adult
--      14 whose Suwayomi genres hit the `genre_is_nsfw` keyword list
--       1 tagged "Content rating: Erotica" (caught by the same keyword guard)
--       4 with no MangaDex-extension source at all — no rating to recover, so no
--         evidence either way; left flagged rather than guessed.
--
-- WHY IT ALSO WRITES `content_rating`. Clearing `is_nsfw` alone does not hold: the
-- guards in `catalog::mark_work_nsfw`, `merge_works` and the `rederiveSuwayomiNsfw`
-- mutation all suppress re-flagging via `content_rating IN ('safe','suggestive')`, so
-- with the column still NULL the next Suwayomi add — or one admin re-derive — would
-- re-flag all 1,831 immediately. Writing the recovered rating is what makes the fix
-- durable, and it is the *same* value the MangaDex API would have supplied. It is not
-- a permanent invention either: if such a work later gains a MangaDex anchor,
-- `catalog::upsert_work`'s `content_rating = excluded.content_rating` overwrites it
-- with the authoritative value. Nothing outside those NSFW guards reads the column
-- (it is not exposed in GraphQL), so the blast radius is exactly the flag it fixes.
--
-- SAFETY. `is_nsfw_override` is never written and works that have one are excluded
-- outright — the gate reads COALESCE(is_nsfw_override, is_nsfw), so an admin decision
-- still wins. erotica/pornographic-rated works are untouched (their `content_rating`
-- is non-NULL, so they fail the WHERE). This cannot undo 0053, which only ever set
-- `is_nsfw = 0`. `updated_at` is deliberately left alone, matching 0053.
--
-- COST. `SCAN work` (no index covers `is_nsfw`; the 112,765-row scan is the floor) with
-- the three EXISTS reached by index/PK seek — confirmed by EXPLAIN QUERY PLAN, which
-- shows `idx_source_series_work`, the `source_extension` PK autoindex, and
-- `suwayomi_series` INTEGER PRIMARY KEY, and evaluates them only for the ~1,956 rows
-- that survive the cheap column filter. Measured on a full production snapshot replaying
-- the whole cold-start migration sequence 0056 -> 0061: 0.065 s and 0.135 s over two
-- trials. The `work` scan is effectively free here because 0059 UPDATEs 109,205 `work`
-- rows immediately beforehand, so the table is already resident; run in isolation
-- against a dropped page cache it costs 5-6 s, but that ordering cannot occur at boot.
--
-- FTS/feed: `work_fts` indexes only MangaDex-anchored works, and none of these 1,831
-- are — so it needs no rebuild here. `feed_updates` copies `is_nsfw` and is rebuilt at
-- boot by `refresh_feed_updates`, picking the corrected values up automatically.
UPDATE work
SET is_nsfw = 0,
    content_rating = CASE
        WHEN EXISTS (
            SELECT 1
            FROM source_series ss
            JOIN source_extension se ON se.source_id = ss.source_id
            JOIN suwayomi_series s ON s.id = CAST(ss.source_key AS INTEGER)
            WHERE ss.work_id = work.id
              AND ss.source_type = 'suwayomi'
              AND se.pkg_name = 'eu.kanade.tachiyomi.extension.all.mangadex'
              AND s.genre LIKE '%Content rating: Suggestive%'
        ) THEN 'suggestive'
        ELSE 'safe'
    END
WHERE work.is_nsfw = 1
  AND work.content_rating IS NULL
  AND work.is_nsfw_override IS NULL
  -- (1) a MangaDex-extension mirror exists whose genres carry no rating tag (=> safe)
  --     or the suggestive tag — the recovered authoritative rating.
  AND EXISTS (
      SELECT 1
      FROM source_series ss
      JOIN source_extension se ON se.source_id = ss.source_id
      JOIN suwayomi_series s ON s.id = CAST(ss.source_key AS INTEGER)
      WHERE ss.work_id = work.id
        AND ss.source_type = 'suwayomi'
        AND se.pkg_name = 'eu.kanade.tachiyomi.extension.all.mangadex'
        AND (s.genre NOT LIKE '%Content rating:%'
             OR s.genre LIKE '%Content rating: Suggestive%')
  )
  -- (2) no Suwayomi source of this work carries an adult genre. Mirrors
  --     `graphql::genre_is_nsfw`'s keyword list exactly (SQLite LIKE is
  --     case-insensitive for ASCII, matching its `to_ascii_lowercase` + `contains`).
  --     This also excludes a "Content rating: Erotica" tag, which contains 'erotica'.
  AND NOT EXISTS (
      SELECT 1
      FROM source_series ss
      JOIN suwayomi_series s ON s.id = CAST(ss.source_key AS INTEGER)
      WHERE ss.work_id = work.id
        AND ss.source_type = 'suwayomi'
        AND (s.genre LIKE '%hentai%' OR s.genre LIKE '%erotica%'
          OR s.genre LIKE '%smut%' OR s.genre LIKE '%pornographic%'
          OR s.genre LIKE '%adult%' OR s.genre LIKE '%mature%'
          OR s.genre LIKE '%18+%' OR s.genre LIKE '%nsfw%'
          OR s.genre LIKE '%r18%' OR s.genre LIKE '%r-18%')
  )
  -- (3) no OTHER adult-flagged source contributes to this work. The MangaDex extension
  --     is excluded from this test precisely because its source-level flag is the taint
  --     being corrected; any other `is_nsfw` source (e.g. omegascans) still wins.
  AND NOT EXISTS (
      SELECT 1
      FROM source_series ss
      JOIN source_extension se ON se.source_id = ss.source_id
      WHERE ss.work_id = work.id
        AND se.is_nsfw = 1
        AND se.pkg_name <> 'eu.kanade.tachiyomi.extension.all.mangadex'
  );
