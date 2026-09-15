-- An admin's "this whole source is adult" decision (`markSourceNsfw(sourceId, true)`).
--
-- Until now that mutation only wrote `work.is_nsfw_override` on the works the source
-- had ALREADY been ingested into, and remembered nothing: a series added from the same
-- source a minute later was judged by the engine's flag and its own genres alone, so it
-- came in as safe. A row here is that memory. The ingest path reads it for every new
-- series (`graphql::resolve_source_nsfw`) and pins the work adult.
--
-- Deliberately NOT a column on `source_extension`: that table is rewritten from the
-- engine by `scanner::record_source_extensions` (which would overwrite the admin's
-- answer with the extension's own flag), and it has no row at all for a source whose
-- extension carries no repo. `markSourceNsfw(sourceId, false)` deletes the row.
CREATE TABLE source_nsfw_pin (
    source_id  TEXT PRIMARY KEY,   -- Suwayomi source id (matches source_series.source_id)
    created_at TEXT NOT NULL
);
