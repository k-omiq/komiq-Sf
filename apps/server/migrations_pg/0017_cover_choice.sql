-- Cover choice: a source ranking for automatic covers, and the gallery choice behind an
-- admin-chosen cover. A table and a nullable column only; no data moves here.

-- The cover group's source ranking (admin Covers page). `source_id` is `mangadex` or a
-- Suwayomi source id. Ranked sources come first, by position; unranked ones keep the
-- default order after them (MangaDex first, then the oldest mapping). No rows: the
-- default order alone, as before.
CREATE TABLE "cover_source_rank" (
    "source_id" text NOT NULL,
    "position" integer NOT NULL,
    CONSTRAINT "cover_source_rank_pkey" PRIMARY KEY ("source_id"),
    CONSTRAINT "cover_source_rank_position_key" UNIQUE ("position")
);

-- The gallery cover an admin picked (`md:<file name>` or `src:<source_series id>`), or
-- NULL for an uploaded file. Either way the row is the work's admin cover.
ALTER TABLE "cover_upload" ADD COLUMN "choice" text;
