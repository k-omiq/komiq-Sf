-- Earlier repairs/recompression changed work.cover_cached_version but left the
-- materialized card URLs behind. Publish the existing stored cover's current
-- version so browsers stop reusing damaged immutable thumbnails. No image or
-- reader data is deleted; works without a stored cover retain their fallback.
UPDATE feed_updates
SET cover_url = (SELECT '/covers/' || w.id || '.webp?v=' || w.cover_cached_version
                 FROM work w WHERE w.id = feed_updates.work_id)
WHERE EXISTS (SELECT 1 FROM work w WHERE w.id = feed_updates.work_id
              AND w.cover_cached_version IS NOT NULL)
  AND cover_url IS NOT (SELECT '/covers/' || w.id || '.webp?v=' || w.cover_cached_version
                       FROM work w WHERE w.id = feed_updates.work_id);

UPDATE feed_series_updates
SET cover_url = (SELECT '/covers/' || w.id || '.webp?v=' || w.cover_cached_version
                 FROM work w WHERE w.id = feed_series_updates.work_id)
WHERE EXISTS (SELECT 1 FROM work w WHERE w.id = feed_series_updates.work_id
              AND w.cover_cached_version IS NOT NULL)
  AND cover_url IS NOT (SELECT '/covers/' || w.id || '.webp?v=' || w.cover_cached_version
                       FROM work w WHERE w.id = feed_series_updates.work_id);

UPDATE browse_catalogue
SET cover_url = (SELECT '/covers/' || w.id || '.webp?v=' || w.cover_cached_version
                 FROM work w WHERE w.id = browse_catalogue.work_id)
WHERE EXISTS (SELECT 1 FROM work w WHERE w.id = browse_catalogue.work_id
              AND w.cover_cached_version IS NOT NULL)
  AND cover_url IS NOT (SELECT '/covers/' || w.id || '.webp?v=' || w.cover_cached_version
                       FROM work w WHERE w.id = browse_catalogue.work_id);
