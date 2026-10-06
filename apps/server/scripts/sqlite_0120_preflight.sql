-- Read-only production preflight, BEFORE migration 0120:
-- sqlite3 -readonly -header -csv /path/to/komika.sqlite3 < sqlite_0120_preflight.sql
-- One read transaction keeps every result set on the same snapshot. query_only
-- also rejects writes if this file is accidentally extended with a write.
PRAGMA query_only = ON;
BEGIN;

-- Exact keeper ordering from 0120, including the engine's recorded source ID.
-- These source rows disappear; their works and user data do not disappear.
WITH identity_map AS (
    SELECT ss.id AS old_id,
           first_value(ss.id) OVER (
               PARTITION BY ss.source_key
               ORDER BY
                   COALESCE(ss.source_id = sw.source_id, 0) DESC,
                   EXISTS(SELECT 1 FROM source_series md
                           WHERE md.work_id = ss.work_id AND md.source_type = 'mangadex') DESC,
                   (NULLIF(ss.source_id, '') IS NOT NULL) DESC,
                   (NULLIF(COALESCE(w.title_override, w.primary_title), '') IS NOT NULL) DESC,
                   ss.created_at, ss.id
           ) AS keep_id
      FROM source_series ss
      JOIN work w ON w.id = ss.work_id
      LEFT JOIN suwayomi_series sw ON sw.id = CAST(ss.source_key AS INTEGER)
     WHERE ss.source_type = 'suwayomi'
)
SELECT count(*) OVER () AS collapsed_source_rows,
       ss.*, m.keep_id AS retained_source_series_id,
       keeper.work_id AS retained_work_id
  FROM identity_map m
  JOIN source_series ss ON ss.id = m.old_id
  JOIN source_series keeper ON keeper.id = m.keep_id
 WHERE m.old_id != m.keep_id
 ORDER BY ss.source_key, ss.id;

-- 0120 restores keeper chapters first, then losing chapters by created_at/id.
-- For each resulting (source,external_id), the first restored row keeps its ID.
-- Later rows disappear; their missing optional metadata and earlier created_at
-- can still be copied into the retained row. Distinct external IDs only move.
WITH identity_map AS (
    SELECT ss.id AS old_id,
           first_value(ss.id) OVER (
               PARTITION BY ss.source_key
               ORDER BY
                   COALESCE(ss.source_id = sw.source_id, 0) DESC,
                   EXISTS(SELECT 1 FROM source_series md
                           WHERE md.work_id = ss.work_id AND md.source_type = 'mangadex') DESC,
                   (NULLIF(ss.source_id, '') IS NOT NULL) DESC,
                   (NULLIF(COALESCE(w.title_override, w.primary_title), '') IS NOT NULL) DESC,
                   ss.created_at, ss.id
           ) AS keep_id
      FROM source_series ss
      JOIN work w ON w.id = ss.work_id
      LEFT JOIN suwayomi_series sw ON sw.id = CAST(ss.source_key AS INTEGER)
     WHERE ss.source_type = 'suwayomi'
), duplicate_ids AS (
    SELECT old_id, keep_id FROM identity_map
     WHERE keep_id IN (SELECT keep_id FROM identity_map WHERE old_id != keep_id)
), restored AS (
    SELECT c.id, COALESCE(m.keep_id, c.source_series_id) AS retained_source_series_id,
           row_number() OVER restore_order AS restore_rank,
           first_value(c.id) OVER restore_order AS retained_chapter_id
      FROM duplicate_ids m
      JOIN chapter c ON c.source_series_id = m.old_id
    WINDOW restore_order AS (
        PARTITION BY COALESCE(m.keep_id, c.source_series_id), c.external_id
        ORDER BY CASE WHEN m.old_id IS NULL OR m.old_id = m.keep_id THEN 0 ELSE 1 END,
                 c.created_at, c.id
    )
)
SELECT count(*) OVER () AS collapsed_chapter_rows,
       c.*, original.work_id AS original_work_id,
       r.retained_source_series_id, keeper.work_id AS retained_work_id,
       r.retained_chapter_id
  FROM restored r
  JOIN chapter c ON c.id = r.id
  JOIN source_series original ON original.id = c.source_series_id
  JOIN source_series keeper ON keeper.id = r.retained_source_series_id
 WHERE r.restore_rank > 1
 ORDER BY original.source_key, c.external_id, c.id;

-- Children of a source mapping that no longer exists: it was removed while
-- foreign keys were not enforced. The work they belonged to cannot be resolved,
-- and 0120 cannot restore them under foreign_keys ON. These rows disappear.
SELECT count(*) OVER () AS unreachable_chapter_rows, c.*
  FROM chapter c
 WHERE NOT EXISTS (SELECT 1 FROM source_series ss WHERE ss.id = c.source_series_id)
 ORDER BY c.source_series_id, c.id;

SELECT count(*) OVER () AS unreachable_candidate_rows, c.*
  FROM merge_candidate c
 WHERE NOT EXISTS (SELECT 1 FROM source_series ss WHERE ss.id = c.source_series_id)
 ORDER BY c.source_series_id, c.id;

ROLLBACK;
