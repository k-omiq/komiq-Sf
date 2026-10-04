-- READ ONLY: run on a consistent COPY, never the original/live database.
SELECT source_type, source_id IS NULL AS is_null, source_id = '' AS is_empty,
       COUNT(*) AS mappings
  FROM source_series
 GROUP BY source_type, is_null, is_empty;

SELECT source_key, COUNT(*) AS mappings, COUNT(DISTINCT work_id) AS works,
       group_concat(id) AS source_series_ids,
       group_concat(work_id) AS work_ids,
       group_concat(source_id) AS source_ids
  FROM source_series
 WHERE source_type = 'suwayomi'
 GROUP BY source_key
HAVING COUNT(*) > 1
 ORDER BY source_key;

SELECT email COLLATE NOCASE AS email, COUNT(*) AS accounts
  FROM users GROUP BY email COLLATE NOCASE HAVING COUNT(*) > 1;

SELECT COUNT(*) AS unreachable_scan_rows
  FROM series_scan_state WHERE next_scan_at IS NULL;

SELECT CASE
           WHEN created_at IS NULL THEN 'NULL'
           WHEN created_at != '' AND created_at NOT GLOB '*[^0-9]*'
               THEN 'epoch-' || length(created_at)
           WHEN strftime('%s', created_at) IS NOT NULL THEN 'ISO'
           ELSE 'other'
       END AS format, COUNT(*) AS rows
  FROM suwayomi_series GROUP BY format;
