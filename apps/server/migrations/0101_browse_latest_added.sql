-- Give Browse an explicit "Latest added" ordering. `released_at` answers when the
-- source says a chapter was published; `announced_at` answers when Komiq first made
-- that chapter available. A delayed source chapter therefore belongs at the top of
-- Latest added even when its release date is old.

ALTER TABLE browse_catalogue ADD COLUMN announced_at INTEGER;

-- Feed rows already carry the durable announce clock established by migration 0099.
-- Chapterless Browse rows have no feed row and remain NULL, which naturally sorts last
-- under DESC without fabricating a discovery time.
UPDATE browse_catalogue
   SET announced_at = (SELECT f.announced_at
                         FROM feed_series_updates f
                        WHERE f.work_id = browse_catalogue.work_id)
 WHERE EXISTS (SELECT 1 FROM feed_series_updates f
                WHERE f.work_id = browse_catalogue.work_id);

-- The same four planner shapes as the release-time ordering: NSFW hidden/shown crossed
-- with all formats/one format. Payload columns keep the common filters index-only.
CREATE INDEX IF NOT EXISTS idx_bc_announced
    ON browse_catalogue(is_nsfw, announced_at DESC, work_id DESC,
                        status, content_rating, en_chapter_count);
CREATE INDEX IF NOT EXISTS idx_bc_type_announced
    ON browse_catalogue(is_nsfw, comic_type, announced_at DESC, work_id DESC,
                        status, content_rating, en_chapter_count);
CREATE INDEX IF NOT EXISTS idx_bc_all_announced
    ON browse_catalogue(announced_at DESC, work_id DESC,
                        status, content_rating, en_chapter_count);
CREATE INDEX IF NOT EXISTS idx_bc_type_all_announced
    ON browse_catalogue(comic_type, announced_at DESC, work_id DESC,
                        status, content_rating, en_chapter_count);

ANALYZE browse_catalogue;
