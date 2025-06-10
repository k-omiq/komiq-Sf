-- Per-user NSFW visibility toggle (CATALOGUE.md §2). Default off: NSFW-flagged
-- works (source nsfw flag OR MangaDex contentRating in {erotica,pornographic}) are
-- hidden from discovery/search/updates unless the user opts in. Stored on the canonical
-- `work` / `source_series` as is_nsfw; this column is the viewer's preference.
ALTER TABLE users ADD COLUMN show_nsfw INTEGER NOT NULL DEFAULT 0;
