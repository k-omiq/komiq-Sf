-- 0097 — back-fill the read-catch-up for progress recorded before it existed.
--
-- `set_progress` now marks every earlier chapter read when a reader finishes one
-- (`catch_up_canonical` / `catch_up_suwayomi` in graphql/mod.rs), because readers arrive
-- mid-series: their first chapter here is 178 and the 177 they read elsewhere were being
-- counted as unread forever. That fix only applies to reads made from now on, so this
-- migration applies the same rule once to what is already stored.
--
-- The rule is identical to the runtime one, and so are the guards:
--   * only chapters strictly BELOW the highest one the user has marked read;
--   * only numerically-labelled chapters (`GLOB '[0-9]*'`) — a bare CAST reads every
--     non-numeric label as 0.0, and 0 is a real chapter, so the two can't be told apart;
--   * `WHERE read = 0` on the conflict, so an already-read chapter keeps its stored
--     `updated_at` and doesn't get shoved to the top of the "Recently read" sort;
--   * `last_page_read` written on INSERT only, so a chapter someone genuinely stopped
--     partway through keeps that position.
--
-- Scope at the time of writing: 4 users, 203 read rows. This only ever adds read rows or
-- flips read 0 -> 1; it never unmarks anything and never deletes.

-- Canonical (MangaDex-anchored) works.
INSERT INTO canonical_progress
    (user_id, chapter_id, work_id, last_page_read, read, updated_at)
SELECT hi.user_id,
       c.external_id,
       hi.work_id,
       0,
       1,
       hi.updated_at
  FROM (SELECT cp.user_id,
               cp.work_id,
               MAX(CAST(c2.number AS REAL)) AS top,
               MAX(cp.updated_at)           AS updated_at
          FROM canonical_progress cp
          JOIN chapter c2 ON c2.external_id = cp.chapter_id
         WHERE cp.read = 1
           AND cp.work_id <> ''
           AND c2.number GLOB '[0-9]*'
         GROUP BY cp.user_id, cp.work_id) hi
  JOIN source_series ss ON ss.work_id = hi.work_id AND ss.source_type = 'mangadex'
  JOIN chapter c ON c.source_series_id = ss.id
 WHERE c.lang = 'en'
   AND c.number GLOB '[0-9]*'
   AND CAST(c.number AS REAL) < hi.top
    ON CONFLICT(user_id, chapter_id) DO UPDATE SET
       read = 1, updated_at = excluded.updated_at
     WHERE canonical_progress.read = 0;

-- Suwayomi-anchored series. `chapter_number` is already REAL here, so the label guard is
-- just IS NOT NULL.
INSERT INTO suwayomi_progress
    (user_id, chapter_id, series_id, last_page_read, read, updated_at)
SELECT hi.user_id,
       CAST(sc.id AS TEXT),
       hi.series_id,
       0,
       1,
       hi.updated_at
  FROM (SELECT sp.user_id,
               sp.series_id,
               MAX(sc2.chapter_number) AS top,
               MAX(sp.updated_at)      AS updated_at
          FROM suwayomi_progress sp
          JOIN suwayomi_chapter sc2 ON CAST(sc2.id AS TEXT) = sp.chapter_id
         WHERE sp.read = 1
           AND sp.series_id <> ''
           AND sc2.chapter_number IS NOT NULL
         GROUP BY sp.user_id, sp.series_id) hi
  JOIN suwayomi_chapter sc ON CAST(sc.manga_id AS TEXT) = hi.series_id
 WHERE sc.chapter_number IS NOT NULL
   AND sc.chapter_number < hi.top
    ON CONFLICT(user_id, chapter_id) DO UPDATE SET
       read = 1, updated_at = excluded.updated_at
     WHERE suwayomi_progress.read = 0;

-- Re-derive `en_chapter_count` under the new best-sourced rule (Suwayomi count now RAISES
-- the MangaDex-English one instead of only filling a zero — see `fill_en_chapter_count`).
-- Without this the 495 affected works keep printing their stale stub count until whatever
-- next touches their feed row rewrites it.
UPDATE feed_series_updates
   SET en_chapter_count = sw.n
  FROM (SELECT ss.work_id AS work_id, MAX(sy.chapter_count) AS n
          FROM source_series ss
          JOIN suwayomi_series sy ON sy.id = CAST(ss.source_key AS INTEGER)
         WHERE ss.source_type = 'suwayomi'
         GROUP BY ss.work_id) AS sw
 WHERE sw.work_id = feed_series_updates.work_id
   AND sw.n > feed_series_updates.en_chapter_count;
