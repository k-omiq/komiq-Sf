-- Lookups by series or work that run inside guarded transactions. The merge and purge
-- units (catalog::merge_discovery) read "which users have rows on these works" from
-- each of these tables while they hold their locks. Every existing index on them leads
-- with user_id (or created_at), so each lookup was a full scan of the table.
-- Additive only: no column, constraint or existing index changes.
CREATE INDEX idx_user_library_series ON user_library(series_id);
CREATE INDEX idx_canonical_progress_work_only ON canonical_progress(work_id);
CREATE INDEX idx_suwayomi_progress_series_only ON suwayomi_progress(series_id);
CREATE INDEX idx_user_activity_target ON user_activity(target_type, target_id);
CREATE INDEX idx_notifications_target ON notifications(target_type, target_id);
