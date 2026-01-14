-- Chapters already labelled "No Group" predate the uploader fallback. Re-offer
-- only those MangaDex rows to the existing paced, resumable attribution backfill.
-- Named groups and non-MangaDex sources retain their existing attribution.
UPDATE chapter
SET scanlator = NULL
WHERE scanlator = 'No Group'
  AND EXISTS (
    SELECT 1 FROM source_series ss
    WHERE ss.id = chapter.source_series_id AND ss.source_type = 'mangadex'
  );
