-- Index EVERY sourced work in `work_fts`, not just the MangaDex-anchored ones.
--
-- THE BUG. Migration 0052 built the index with
--     AND EXISTS (SELECT 1 FROM source_series ss
--                 WHERE ss.work_id = w.id AND ss.source_type = 'mangadex')
-- and justified it as "those are exactly the works `canonicalSeries` will serve (it rejects
-- a non-anchored backfilled shell), so an FTS hit always resolves to a real page". That
-- premise was true in 0052 and is FALSE now: migration 0064 introduced `reader_id` — a
-- `w_…` for a MangaDex-anchored work, its numeric Suwayomi series id otherwise — precisely
-- so a non-anchored work HAS a page that can serve it, and 0069 extended that rule to the
-- whole Browse catalogue. So Suwayomi-only works have been browsable but not searchable.
--
-- Measured on production before this migration: `work` = 115,530 rows, `work_fts` = 113,706,
-- and `SELECT count(DISTINCT work_id) FROM source_series GROUP BY source_type` = 113,706
-- mangadex / 12,761 suwayomi. The index was EXACTLY the MangaDex set, and the 1,824-row
-- difference is exactly the works reachable only through Suwayomi.
--
-- THAT 1,824 IS A POINT-IN-TIME FIGURE AND IT GROWS. The Suwayomi scanner mints new
-- non-anchored works continuously (~90 works entered `work` during the hour this migration
-- was written), so a later count will be higher and is not evidence of a second bug. The
-- backlog is ~2/3 series genuinely absent from MangaDex and ~1/3 duplicates of an anchored
-- work under an English rather than romanized title ("Ace of Diamond" vs "Diamond no Ace")
-- — the latter are a DEDUP problem, not a search one, and merging them is what the admin
-- merge tool is for. Indexing both is still correct: an unmerged duplicate that a user can
-- find beats one they cannot.
--
-- Reported symptom: the
-- reader's "My Brother is a Vicious Dog" (w_dba52b1f209c4d0a9fcc580bf7a0783a, sources =
-- 'suwayomi') and "My Brother Is A Mad Dog" (w_e7c71624a002427fb23310d3387dcdec, three
-- Suwayomi sources consolidated) return zero hits for any query, at any spelling, because
-- neither has a row to match against.
--
-- THE NEW PREDICATE is 0069's exclusion rule verbatim — a source to open and a title to
-- label the card with — so Browse and search now agree on which works exist. `w.id` is not
-- used as the navigation target here: `search_works_fts` returns work ids and the resolver
-- hydrates them through `browse_catalogue`, which is where `reader_id` lives.
--
-- Kept in sync afterwards by `catalog::refresh_work_fts`, whose INSERT this mirrors
-- statement-for-statement (same hand-maintained convention as 0052/0068/0069). That
-- function now runs at the end of the `refresh_feed_updates` chain, so it refreshes on the
-- same cadence as `browse_catalogue` rather than on its own.
DELETE FROM work_fts;

INSERT INTO work_fts (work_id, chapters, title, aliases)
SELECT w.id,
       COALESCE(cc.n, 0),
       COALESCE(w.title_override, w.primary_title, ''),
       COALESCE(
           (SELECT group_concat(a.raw_title, ' ')
            FROM work_alias a WHERE a.work_id = w.id),
           '')
FROM work w
LEFT JOIN (
    SELECT ss.work_id, COUNT(*) AS n
    FROM chapter ch
    JOIN source_series ss ON ss.id = ch.source_series_id
    GROUP BY ss.work_id
) cc ON cc.work_id = w.id
WHERE COALESCE(w.title_override, w.primary_title, '') <> ''
  AND EXISTS (SELECT 1 FROM source_series ss WHERE ss.work_id = w.id);
