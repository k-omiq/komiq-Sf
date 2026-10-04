-- An admin's "this whole source uses AI" decision (`setSourceAi`), one row per tagged source.
--
-- `scanlator_tag` (0116) tags a scanlator by the NAME a chapter credits. That reaches
-- MangaDex, where every chapter names its group, and almost nothing else: a site that
-- translates its own series is one scanlator, and its chapters carry no credit at all
-- (33,730 of the 36,480 Suwayomi chapters measured had none). There the SOURCE is the
-- scanlator, so the tag is keyed by the source.
--
-- The tag applies to every chapter copy read from the source, including the ones that
-- arrive later. A copy that also names a tagged group takes the stronger of the two
-- tags; nothing is stored per chapter.
--
--   ai_level   'ai'       the translation is machine made
--              'partial'  AI is part of the work
--
-- A source with no row is not tagged. `setSourceAi(sourceId, null)` deletes the row.
CREATE TABLE source_ai_tag (
    source_id  TEXT PRIMARY KEY,   -- Suwayomi source id (matches source_series.source_id)
    ai_level   TEXT NOT NULL CHECK (ai_level IN ('ai', 'partial')),
    updated_at TEXT NOT NULL
);
