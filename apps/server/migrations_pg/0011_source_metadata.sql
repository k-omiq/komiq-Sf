-- Work metadata trust (M9 W2). A row bars one source from giving works one group of
-- metadata. `source_id` is `mangadex` or a Suwayomi source id; no row means allowed.
-- The source id that supplied the published work cover master; NULL for uploads,
-- legacy copies and covers published before this column.
ALTER TABLE work ADD COLUMN cover_source text;
CREATE INDEX work_cover_source_idx ON work (cover_source, id) WHERE cover_source IS NOT NULL;
-- A revoked source master must never fall back to, or be restored from, SQLite.
-- Kept after a refill: the legacy art can no longer be trusted for this work.
CREATE TABLE work_cover_revocation (
    work_id text PRIMARY KEY REFERENCES work(id) ON DELETE CASCADE
);
CREATE TABLE "source_metadata_bar" (
    "source_id" text NOT NULL,
    "field_group" text NOT NULL,
    "created_at" timestamptz NOT NULL,
    CONSTRAINT "source_metadata_bar_pkey" PRIMARY KEY ("source_id", "field_group"),
    CONSTRAINT "source_metadata_bar_ck_0" CHECK (field_group IN ('title', 'description', 'credits', 'tags', 'cover', 'status'))
);
-- What each source mapping last said about its work, one row per value. The work is
-- `source_series.work_id`, so the rows follow a mapping through merge and split and
-- go away with it. `catalog::work_metadata` projects them onto the work's own
-- columns and child tables. Slot '' with value '' records a source that was observed
-- and gave nothing for the group.
CREATE TABLE "work_metadata_contribution" (
    "source_series_id" text NOT NULL,
    "field_group" text NOT NULL,
    "slot" text NOT NULL,
    "lang" text NOT NULL DEFAULT '',
    "value" text NOT NULL,
    "value_key" text GENERATED ALWAYS AS (md5(value)) STORED,
    "ord" integer NOT NULL DEFAULT 0,
    "detail" jsonb NULL,
    "attribution" text NOT NULL,
    "observed_at" timestamptz NOT NULL,
    CONSTRAINT "work_metadata_contribution_pkey" PRIMARY KEY ("source_series_id", "field_group", "slot", "lang", "value_key"),
    CONSTRAINT "work_metadata_contribution_ck_0" CHECK (field_group IN ('title', 'description', 'credits', 'tags', 'cover', 'status')),
    CONSTRAINT "work_metadata_contribution_ck_1" CHECK (attribution IN ('observed', 'inferred', 'unknown'))
);
ALTER TABLE "work_metadata_contribution" ADD CONSTRAINT "work_metadata_contribution_fk_0" FOREIGN KEY ("source_series_id") REFERENCES "source_series" ("id") ON DELETE CASCADE ON UPDATE NO ACTION;
