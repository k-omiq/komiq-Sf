-- Progress belongs to entries; copy rows still retain their own resume positions.
CREATE TABLE "work_progress" (
    "user_id" uuid NOT NULL,
    "work_id" text NOT NULL,
    "through_entry_id" text NULL,
    "revision" bigint NOT NULL DEFAULT 0,
    "updated_at" timestamptz NOT NULL,
    CONSTRAINT "work_progress_pkey" PRIMARY KEY ("user_id", "work_id"),
    CONSTRAINT "work_progress_user_fk" FOREIGN KEY ("user_id") REFERENCES "users" ("id") ON DELETE CASCADE,
    CONSTRAINT "work_progress_work_fk" FOREIGN KEY ("work_id") REFERENCES "work" ("id") ON DELETE CASCADE,
    CONSTRAINT "work_progress_through_fk" FOREIGN KEY ("through_entry_id") REFERENCES "work_chapter" ("id") ON DELETE SET NULL
);
CREATE INDEX "work_progress_work_idx" ON "work_progress" ("work_id");

CREATE TABLE "work_chapter_mark" (
    "user_id" uuid NOT NULL,
    "work_chapter_id" text NOT NULL,
    "read" boolean NOT NULL,
    "created_at" timestamptz NOT NULL,
    CONSTRAINT "work_chapter_mark_pkey" PRIMARY KEY ("user_id", "work_chapter_id"),
    CONSTRAINT "work_chapter_mark_user_fk" FOREIGN KEY ("user_id") REFERENCES "users" ("id") ON DELETE CASCADE,
    CONSTRAINT "work_chapter_mark_entry_fk" FOREIGN KEY ("work_chapter_id") REFERENCES "work_chapter" ("id") ON DELETE CASCADE
);
CREATE INDEX "work_chapter_mark_entry_idx" ON "work_chapter_mark" ("work_chapter_id");
