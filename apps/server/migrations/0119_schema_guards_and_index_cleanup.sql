-- Reader/index inventory and before/after EXPLAIN QUERY PLAN were checked on an
-- ANALYZEd 1.97 GB database copy. Browse now reads browse_catalogue; updatesFeed
-- orders announced_at with migration 0099's four indexes. None of these indexes
-- is named by a live INDEXED BY hint. Keep idx_source_series_type_created.
DROP INDEX IF EXISTS idx_fsu_order;
DROP INDEX IF EXISTS idx_fsu_type_order;
DROP INDEX IF EXISTS idx_fsu_all_order;
DROP INDEX IF EXISTS idx_fsu_type_all_order;
DROP INDEX IF EXISTS idx_fsu_browse_chapters;
DROP INDEX IF EXISTS idx_fsu_browse_type_chapters;
DROP INDEX IF EXISTS idx_fsu_browse_all_chapters;
DROP INDEX IF EXISTS idx_fsu_browse_type_all_chapters;
DROP INDEX IF EXISTS idx_fsu_browse_order;
DROP INDEX IF EXISTS idx_fsu_browse_all_order;

-- These keys are left prefixes of existing longer indexes or UNIQUE keys.
DROP INDEX IF EXISTS idx_work_tag_work;
DROP INDEX IF EXISTS idx_chapter_override_work;
DROP INDEX IF EXISTS idx_comment_votes_comment;
DROP INDEX IF EXISTS idx_reviews_series;
DROP INDEX IF EXISTS idx_source_series_type_key;
-- recently_added orders COALESCE(created_at, updated_at), which this cannot serve.
DROP INDEX IF EXISTS idx_suwayomi_series_created_at;

-- Child-side foreign-key probes otherwise scan the complete child table. The
-- partial like-milestone index cannot serve notifications of every kind.
CREATE INDEX IF NOT EXISTS idx_merge_candidate_work
    ON merge_candidate(candidate_work_id);
CREATE INDEX IF NOT EXISTS idx_merge_candidate_source
    ON merge_candidate(source_series_id);
CREATE INDEX IF NOT EXISTS idx_reviews_user ON reviews(user_id);
CREATE INDEX IF NOT EXISTS idx_notifications_comment ON notifications(comment_id);

-- Preserve any legacy rows that were written after migration 0024 copied the
-- superseded table, without overwriting current library preferences.
INSERT INTO user_library(user_id, series_id, created_at)
SELECT user_id, work_id, created_at FROM canonical_library WHERE true
ON CONFLICT(user_id, series_id) DO NOTHING;
DROP TABLE canonical_library;

-- Like migration 0103's username guards, these reject future case collisions
-- without deleting, merging or renaming pre-existing accounts.
CREATE TRIGGER users_email_nocase_insert
BEFORE INSERT ON users
WHEN EXISTS (SELECT 1 FROM users WHERE email = NEW.email COLLATE NOCASE)
BEGIN
    SELECT RAISE(ABORT, 'UNIQUE constraint failed: users.email');
END;

CREATE TRIGGER users_email_nocase_update
BEFORE UPDATE OF email ON users
WHEN NEW.email != OLD.email
 AND EXISTS (SELECT 1 FROM users WHERE email = NEW.email COLLATE NOCASE AND id != OLD.id)
BEGIN
    SELECT RAISE(ABORT, 'UNIQUE constraint failed: users.email');
END;

-- The range-only due query never visits NULL. Repair legacy NULLs, then enforce
-- the invariant on every new row and explicit schedule update, without a rebuild.
UPDATE series_scan_state SET next_scan_at = '1970-01-01T00:00:00+00:00'
WHERE next_scan_at IS NULL;

CREATE TRIGGER series_scan_state_next_scan_insert
BEFORE INSERT ON series_scan_state
WHEN NEW.next_scan_at IS NULL
BEGIN
    SELECT RAISE(ABORT, 'NOT NULL constraint failed: series_scan_state.next_scan_at');
END;

CREATE TRIGGER series_scan_state_next_scan_update
BEFORE UPDATE OF next_scan_at ON series_scan_state
WHEN NEW.next_scan_at IS NULL
BEGIN
    SELECT RAISE(ABORT, 'NOT NULL constraint failed: series_scan_state.next_scan_at');
END;

-- Migration 0026 copied epoch-second in_library_at values alongside ISO dates.
-- Touch only digit-only epoch seconds/milliseconds in this one column; leave
-- already ISO-formatted timestamps and unrelated columns byte-for-byte intact.
UPDATE suwayomi_series
   SET created_at = strftime('%Y-%m-%dT%H:%M:%fZ',
       CASE length(created_at)
           WHEN 10 THEN CAST(created_at AS REAL)
           WHEN 13 THEN CAST(created_at AS REAL) / 1000.0
       END, 'unixepoch')
 WHERE length(created_at) IN (10, 13)
   AND created_at NOT GLOB '*[^0-9]*'
   AND strftime('%s', CASE length(created_at)
           WHEN 10 THEN CAST(created_at AS REAL)
           WHEN 13 THEN CAST(created_at AS REAL) / 1000.0
       END, 'unixepoch') IS NOT NULL;

ANALYZE sqlite_schema;
