-- Browse "all series" (series_cache::search_catalogue) filters NSFW with a
-- correlated subquery that looks up `source_series.source_key = <suwayomi id>`
-- constrained to `source_type = 'suwayomi'`. The existing indexes could not serve it:
-- UNIQUE(source_type, source_id, source_key) can't seek on source_key without
-- source_id, and idx_source_series_work is on work_id. So the subquery scanned every
-- suwayomi source_series row for each of ~13k catalogue rows (O(n^2)), making the
-- empty-query catalogue search take ~60s+ and the Browse page effectively unusable.
--
-- A composite (source_type, source_key) index lets that subquery seek directly,
-- cutting the whole catalogue search to sub-second. Cheap to build (~13k rows) and
-- additive — no data change.
CREATE INDEX IF NOT EXISTS idx_source_series_type_key
    ON source_series(source_type, source_key);
