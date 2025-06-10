-- Per-user reading status + favourite flag for library series.
--
-- `status` is the shelf the viewer explicitly filed a series under —
-- 'reading' | 'completed' | 'onhold' | 'plan'. NULL means "auto": the client
-- derives the shelf from read progress (completed when fully read, plan when
-- untouched, otherwise reading), preserving the pre-existing behaviour for every
-- row that hasn't been filed by hand. An explicit value always wins.
--
-- `is_favorite` is a separate axis from status: a series can be favourited on any
-- shelf. It backs the "favourite" button on the series page and the Favorites
-- view on the profile/library.
ALTER TABLE user_library ADD COLUMN status TEXT;
ALTER TABLE user_library ADD COLUMN is_favorite INTEGER NOT NULL DEFAULT 0;
