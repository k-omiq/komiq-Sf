-- Genres for canonical works: MangaDex tags become a SOURCED half of `work_tag`.
--
-- WHY. `work_tag` was admin-curation-only, and it is EMPTY in production — 0 rows across
-- 115,531 works. So every canonical surface that wants a genre falls through to
-- `catalog::work_effective_genres`' second branch, the distinct genres of the work's
-- linked Suwayomi source series, which exists only for the 13,847 works that HAVE such a
-- link. The other ~101k MangaDex-only works carry no genre at all. That is why Browse
-- could never offer a catalogue-wide genre filter: outside the Suwayomi subset there was
-- nothing to filter on, and `search`'s FTS path documents the consequence directly
-- ("canonical genre coverage is sparse until MangaDex tags are ingested").
--
-- MangaDex ships these tags in the SAME `/manga` payload the catalogue sweep already
-- fetches (`attributes.tags[]`, groups `genre`/`theme`/`format`/`content`) — the
-- deserializer simply had no field for them. Ingesting them costs no extra upstream
-- request and no new sweep.
--
-- WHY A `source` COLUMN, rather than just letting ingest write into the table. The
-- documented precedence is "admin-curated `work_tag` wins outright over source genres".
-- If ingest wrote unmarked rows, that curated early-return would fire for every work
-- with upstream tags, and a human's deliberate edit would be indistinguishable from
-- ~700k rows of upstream noise — the curation feature would silently stop working. The
-- column keeps the two halves separable, so curation still wins outright AND ingest can
-- refresh its own rows on every re-sync without ever touching a human's.
--
-- DEFAULT 'admin' is the correct backfill for the rows already there: the admin curation
-- mutation is the only writer this table has ever had.
ALTER TABLE work_tag ADD COLUMN source TEXT NOT NULL DEFAULT 'admin';

-- The hot read is "this work's tags, one half at a time" — `work_effective_genres` asks
-- for the curated half, then the upstream half, and the ingest refresh deletes exactly
-- its own half (`WHERE work_id = ? AND source = 'mangadex'`). `idx_work_tag_work`
-- (work_id) can serve those only by fetching the work's whole tag list and filtering;
-- this lets all three seek straight to the half they want.
CREATE INDEX idx_work_tag_work_source ON work_tag(work_id, source);

-- The reverse direction, for the genre FACET list and the genre filter, which scan BY
-- tag rather than by work. At 0 rows that was free; after the backfill this table holds
-- roughly 700k rows (~6 tags × 113k works), so a `COUNT(*) ... GROUP BY tag` facet query
-- must not be a full table scan. Covering (tag, work_id) so the facet count and the
-- filter's work-id lookup are both index-only.
CREATE INDEX idx_work_tag_tag ON work_tag(tag, work_id);
