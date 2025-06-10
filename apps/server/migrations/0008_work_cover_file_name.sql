-- Store the MangaDex cover fileName on `work` so cover URLs can be built for
-- canonical works (CATALOGUE.md §5–6). `work` already keeps cover_phash (for dedup)
-- but not the fileName, so the reader had no way to construct a cover URL for a
-- MangaDex-mirrored work. Cover URL =
--   https://uploads.mangadex.org/covers/{mangadexId}/{cover_file_name}  (+ .512.jpg thumb)
-- Additive + nullable: existing rows keep NULL until the next sync enriches them.
ALTER TABLE work ADD COLUMN cover_file_name TEXT;
