-- One row per explicit untick of one chapter copy (MangaDex uuid or numeric Suwayomi id).
-- At read time it covers every copy with the same chapter_key in the same work;
-- M9's per-work unticked list takes these rows over.
CREATE TABLE "progress_untick" (
    "user_id" uuid NOT NULL,
    "chapter_id" text NOT NULL,
    "created_at" timestamptz NOT NULL,
    CONSTRAINT "progress_untick_pkey" PRIMARY KEY ("user_id", "chapter_id")
);
ALTER TABLE "progress_untick" ADD CONSTRAINT "progress_untick_fk_0" FOREIGN KEY ("user_id") REFERENCES "users" ("id") ON DELETE CASCADE ON UPDATE NO ACTION;
