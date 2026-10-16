ALTER TABLE engine_generation ADD COLUMN reason text;
ALTER TABLE engine_generation ADD COLUMN since timestamptz NOT NULL DEFAULT now();
ALTER TABLE engine_generation ADD COLUMN unfenced boolean NOT NULL DEFAULT false;
-- A slow first marker write can be retried without treating it as marker loss.
ALTER TABLE engine_generation ADD COLUMN marker_written boolean NOT NULL DEFAULT false;
CREATE UNIQUE INDEX engine_generation_current ON engine_generation ((state <> 'detached')) WHERE state <> 'detached';

-- Created only on an explicit generation change. A rollback cannot preserve the
-- queued-write fence, so a populated table prevents reverse export.
CREATE TABLE "engine_numeric_state" (
    "singleton" boolean NOT NULL DEFAULT true,
    "epoch" bigint NOT NULL,
    "changed_at" timestamptz NOT NULL,
    CONSTRAINT "engine_numeric_state_pkey" PRIMARY KEY ("singleton"),
    CONSTRAINT "engine_numeric_state_ck_0" CHECK (singleton),
    CONSTRAINT "engine_numeric_state_ck_1" CHECK (epoch > 0)
);
CREATE TABLE "suwayomi_progress_detached" (
    "generation" uuid NOT NULL,
    "user_id" uuid NOT NULL,
    "chapter_id" text NOT NULL,
    "series_id" text NOT NULL,
    "last_page_read" bigint NOT NULL,
    "read" boolean NOT NULL,
    "updated_at" timestamptz NOT NULL,
    CONSTRAINT "suwayomi_progress_detached_pkey" PRIMARY KEY ("generation", "user_id", "chapter_id")
);
ALTER TABLE "suwayomi_progress_detached" ADD CONSTRAINT "suwayomi_progress_detached_fk_0" FOREIGN KEY ("generation") REFERENCES "engine_generation" ("id") ON DELETE NO ACTION ON UPDATE NO ACTION;
ALTER TABLE "suwayomi_progress_detached" ADD CONSTRAINT "suwayomi_progress_detached_fk_1" FOREIGN KEY ("user_id") REFERENCES "users" ("id") ON DELETE CASCADE ON UPDATE NO ACTION;
