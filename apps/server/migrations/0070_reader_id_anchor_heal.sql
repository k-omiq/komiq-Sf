-- Heal `reader_id` for MangaDex-anchored works that a scanner-first feed insert pinned to
-- the numeric Suwayomi id.
--
-- THE BUG. A work that HAS a MangaDex anchor (a `source_series` of type `mangadex`) but
-- whose mirror carries no dated chapter — a licensing TAKEDOWN strips chapters from the
-- spine, which correlates with POPULAR titles (Boku no Hero Academia, Solo Leveling …) —
-- gets its `feed_series_updates` row from the SCANNER/Suwayomi half of the rebuild, which
-- used to write `reader_id = <numeric Suwayomi id>`. So its Browse + Updates cards navigate
-- to `/series/<numeric>` (the Suwayomi page: raw `api.komiq.cc` cover, no source picker,
-- reads over the slow origin proxy) instead of its canonical `/series/w_…` page (cached
-- cover, source picker, MangaDex@Home reads). 293 such rows measured on production
-- 2026-07-27.
--
-- THE FIX has two halves. The runtime rebuild now derives `reader_id` from the anchor in
-- both `catalog::refresh_feed_series_updates` and `scanner::upsert_feed_series_update` (and
-- `browse_catalogue_select` prefers the anchor), so new rows are correct. That rebuild runs
-- ~20 s into boot and once per catalogue-sync cycle; this migration heals the EXISTING rows
-- immediately, so the very first post-deploy request is already correct rather than serving
-- the old destination until the rebuild lands.
--
-- BOTH tables, so a Browse card and the same work's Updates card agree (the invariant
-- migration 0069 documents). For a MangaDex-anchored work the canonical id IS `work_id`
-- (the `w_…` primary key), so the corrected value is simply `work_id`. Rows already on
-- `work_id`, and non-anchored Suwayomi-only works, are left untouched.

UPDATE feed_series_updates SET reader_id = work_id
 WHERE reader_id <> work_id
   AND EXISTS (SELECT 1 FROM source_series ss
                WHERE ss.work_id = feed_series_updates.work_id
                  AND ss.source_type = 'mangadex');

UPDATE browse_catalogue SET reader_id = work_id
 WHERE reader_id <> work_id
   AND EXISTS (SELECT 1 FROM source_series ss
                WHERE ss.work_id = browse_catalogue.work_id
                  AND ss.source_type = 'mangadex');
