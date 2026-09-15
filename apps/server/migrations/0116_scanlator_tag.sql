-- An admin's "this scanlator uses AI" decision (`setScanlatorAi`), one row per tagged name.
--
-- A scanlator has no table of its own: it is the free-text `chapter.scanlator` credit of
-- a chapter copy (MangaDex group names joined with ' & ', an uploader name, or whatever a
-- source reports). So the tag is keyed by that name, case-folded and trimmed
-- (`scanlators::name_key`), and it applies to every chapter credited to the name, on every
-- source, including chapters that arrive later.
--
--   ai_level   'ai'       the translation is machine made
--              'partial'  AI is part of the work (for example machine translation that a
--                         person then edits)
--
-- A name with no row is not tagged. `setScanlatorAi(name, null)` deletes the row. A joint
-- credit ("A & B") with no row of its own takes the strongest tag of its members, on read
-- (`scanlators::effective_level`); nothing is stored for it.
--
-- `name` keeps the spelling the admin tagged, for display where no chapter carries the
-- name any more.
CREATE TABLE scanlator_tag (
    name_key   TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    ai_level   TEXT NOT NULL CHECK (ai_level IN ('ai', 'partial')),
    updated_at TEXT NOT NULL
);
