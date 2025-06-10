-- AD-5: full-text search over the canonical catalogue.
--
-- Before this, a text `search(query:"…")` abandoned the 112k-work catalogue and
-- fanned the query out live to up to 24 installed source extensions (8s timeout
-- each) — slow, nondeterministic (results depended on which sources answered),
-- ranked only by exact-title equality, and it even WROTE new rows into the
-- catalogue as a side effect. The data to answer the query locally already
-- existed (`work.primary_title` + `work_alias.raw_title`); nothing queried it.
--
-- This adds an FTS5 index over that text so a text query becomes an indexed,
-- ranked DB read that returns canonical `w_` works (which, unlike the numeric
-- Suwayomi ids the old path yielded, carry a translator/source picker).
--
-- DEVIATION FROM AD-5's literal `content=''`: a contentless FTS5 table exposes only
-- an integer rowid, but `work.id` is TEXT (`w_<hex>`). Rather than maintain a
-- separate rowid→work_id side table, this is a standalone FTS5 table carrying the
-- id (and a `chapters` popularity signal) in UNINDEXED columns — retrievable
-- directly from a MATCH, and still compact (it stores only titles + aliases).
-- `unicode61 remove_diacritics 2` folds accents so "re:zero"/"berserk" match
-- regardless of diacritics.
--
-- RANKING (applied in `catalog::search_works_fts`): an exact/prefix full-title tier
-- FIRST (so typing a real title surfaces the main work, not a short spin-off that
-- bm25's term-frequency bias would otherwise float up), then `chapters` DESC as a
-- notability proxy, then bm25. `chapters` is a stand-in until MangaDex `follows`
-- ratings are ingested (AD-6) — swap it for `follows` then.
CREATE VIRTUAL TABLE work_fts USING fts5(
    work_id UNINDEXED,
    chapters UNINDEXED,   -- mirrored chapter count across the work's sources (popularity proxy)
    title,
    aliases,
    tokenize = 'unicode61 remove_diacritics 2'
);

-- Backfill from current data. Only MangaDex-anchored works with a non-empty title
-- are indexed: those are exactly the works `canonicalSeries` will serve (it rejects
-- a non-anchored backfilled shell), so an FTS hit always resolves to a real page.
-- The chapter count is grouped once (not a per-work correlated subquery) via a
-- derived join. Kept in sync afterwards by `catalog::refresh_work_fts` (boot + each
-- catalogue sync), mirroring how `feed_updates` (0051) is maintained.
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
  AND EXISTS (
      SELECT 1 FROM source_series ss
      WHERE ss.work_id = w.id AND ss.source_type = 'mangadex'
  );
