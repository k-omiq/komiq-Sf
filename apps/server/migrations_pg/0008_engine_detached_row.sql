-- Numeric library, review, view and admin rows that a new engine generation could
-- not move to a work. Kept, not shown. A populated table prevents reverse export.
CREATE TABLE "engine_detached_row" (
    "generation" uuid NOT NULL,
    "source_table" text NOT NULL,
    "row_key" text NOT NULL,
    "user_id" uuid,
    "data" jsonb NOT NULL,
    CONSTRAINT "engine_detached_row_pkey" PRIMARY KEY ("generation", "source_table", "row_key")
);
ALTER TABLE "engine_detached_row" ADD CONSTRAINT "engine_detached_row_fk_0" FOREIGN KEY ("generation") REFERENCES "engine_generation" ("id") ON DELETE NO ACTION ON UPDATE NO ACTION;
ALTER TABLE "engine_detached_row" ADD CONSTRAINT "engine_detached_row_fk_1" FOREIGN KEY ("user_id") REFERENCES "users" ("id") ON DELETE CASCADE ON UPDATE NO ACTION;
