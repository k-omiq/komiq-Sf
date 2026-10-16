-- Source-native identities can be rebuilt from the engine after a SQLite rollback.
CREATE TABLE "engine_generation" (
    "id" uuid NOT NULL,
    "first_seen" timestamptz NOT NULL,
    "last_seen" timestamptz NOT NULL,
    "state" text NOT NULL,
    CONSTRAINT "engine_generation_pkey" PRIMARY KEY ("id"),
    CONSTRAINT "engine_generation_ck_1" CHECK (state IN ('active', 'quarantined', 'detached'))
);
CREATE TABLE "engine_manga_key" (
    "generation" uuid NOT NULL,
    "manga_id" bigint NOT NULL,
    "source_id" text NOT NULL,
    "manga_key" text NOT NULL,
    "web_url" text NULL,
    "title" text NULL,
    "updated_at" timestamptz NOT NULL,
    -- Cache revision covered by a successful full-list backfill, including absences.
    "backfilled_at" timestamptz NULL,
    CONSTRAINT "engine_manga_key_pkey" PRIMARY KEY ("generation", "manga_id")
);
ALTER TABLE "engine_manga_key" ADD CONSTRAINT "engine_manga_key_fk_0" FOREIGN KEY ("generation") REFERENCES "engine_generation" ("id") ON DELETE NO ACTION ON UPDATE NO ACTION;
CREATE TABLE "engine_chapter_key" (
    "generation" uuid NOT NULL,
    "chapter_id" bigint NOT NULL,
    "manga_id" bigint NOT NULL,
    "source_order" integer NOT NULL,
    "chapter_key" text NOT NULL,
    "web_url" text NULL,
    "updated_at" timestamptz NOT NULL,
    CONSTRAINT "engine_chapter_key_pkey" PRIMARY KEY ("generation", "chapter_id"),
    CONSTRAINT "engine_chapter_key_source_unique" UNIQUE ("generation", "manga_id", "chapter_key")
);
ALTER TABLE "engine_chapter_key" ADD CONSTRAINT "engine_chapter_key_fk_0" FOREIGN KEY ("generation") REFERENCES "engine_generation" ("id") ON DELETE NO ACTION ON UPDATE NO ACTION;
