-- M9 gate fixes: keep admin entry merges merged, and make revisioned progress saves
-- idempotent. Tables and a nullable column only; no data moves here.

-- A match_key an admin merged away (`mergeWorkChapters`) keeps pointing at the entry
-- that took its copies, so the matcher links a new or returning copy with that key
-- there instead of recreating the dropped entry. Numbered and `x:` keys alike. A key
-- has an alias only while the work has no entry of its own with that match_key.
CREATE TABLE "work_chapter_alias" (
    "work_id" text NOT NULL,
    "match_key" text NOT NULL,
    "work_chapter_id" text NOT NULL,
    "created_at" timestamptz NOT NULL,
    CONSTRAINT "work_chapter_alias_pkey" PRIMARY KEY ("work_id", "match_key"),
    CONSTRAINT "work_chapter_alias_work_fk" FOREIGN KEY ("work_id") REFERENCES "work" ("id") ON DELETE CASCADE,
    CONSTRAINT "work_chapter_alias_entry_fk" FOREIGN KEY ("work_chapter_id") REFERENCES "work_chapter" ("id") ON DELETE CASCADE
);
CREATE INDEX "work_chapter_alias_entry_idx" ON "work_chapter_alias" ("work_chapter_id");

-- The client's id of the last revisioned save applied for this reader and work. A retry
-- of that save (its answer was lost) is answered APPLIED with the current revision
-- instead of being refused as stale. NULL after any other change.
ALTER TABLE "work_progress" ADD COLUMN "last_op_id" text NULL;
ALTER TABLE "work_progress" ADD CONSTRAINT "work_progress_last_op_id_len_ck"
    CHECK (last_op_id IS NULL OR length(last_op_id) BETWEEN 1 AND 64);
