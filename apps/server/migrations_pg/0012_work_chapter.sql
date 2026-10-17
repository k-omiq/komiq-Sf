-- M9 W3a: the work's own chapter list.
-- One entry per chapter of a work; each source copy links to one entry, once.
-- No data moves here: the backfill drain (catalog::work_chapter) links existing copies.
CREATE TABLE "work_chapter" (
    "id" text NOT NULL,
    "work_id" text NOT NULL,
    -- Numbered: the spine key (round(number * 100)). Unnumbered: 'x:' plus the copy's
    -- stable source identity. NULL only for admin-made entries with no matching rule.
    "match_key" text NULL,
    "sort_key" numeric NOT NULL,
    "number" text NULL,
    "label" text NULL,
    "special" boolean NOT NULL DEFAULT false,
    "special_by" text NULL,
    "hidden" boolean NOT NULL DEFAULT false,
    "title_override" text NULL,
    "created_at" timestamptz NOT NULL,
    CONSTRAINT "work_chapter_pkey" PRIMARY KEY ("id"),
    CONSTRAINT "work_chapter_special_by_ck" CHECK (special_by IS NULL OR special_by IN ('matcher', 'admin')),
    CONSTRAINT "work_chapter_work_fk" FOREIGN KEY ("work_id") REFERENCES "work" ("id") ON DELETE CASCADE
);
CREATE INDEX "work_chapter_work_sort_idx" ON "work_chapter" ("work_id", "sort_key");
CREATE UNIQUE INDEX "work_chapter_match_key_uq" ON "work_chapter" ("work_id", "match_key") WHERE "match_key" IS NOT NULL;

CREATE TABLE "work_chapter_link" (
    "chapter_id" text NOT NULL,
    "work_chapter_id" text NOT NULL,
    "linked_by" text NOT NULL,
    "linked_at" timestamptz NOT NULL,
    CONSTRAINT "work_chapter_link_pkey" PRIMARY KEY ("chapter_id"),
    CONSTRAINT "work_chapter_link_by_ck" CHECK (linked_by IN ('matcher', 'admin')),
    CONSTRAINT "work_chapter_link_chapter_fk" FOREIGN KEY ("chapter_id") REFERENCES "chapter" ("id") ON DELETE CASCADE,
    CONSTRAINT "work_chapter_link_entry_fk" FOREIGN KEY ("work_chapter_id") REFERENCES "work_chapter" ("id") ON DELETE CASCADE
);
CREATE INDEX "work_chapter_link_entry_idx" ON "work_chapter_link" ("work_chapter_id");

-- `ready`: every copy of the work is linked. Only the backfill drain sets it true.
CREATE TABLE "work_chapter_state" (
    "work_id" text NOT NULL,
    "ready" boolean NOT NULL DEFAULT false,
    "list_rev" bigint NOT NULL DEFAULT 0,
    "updated_at" timestamptz NOT NULL,
    CONSTRAINT "work_chapter_state_pkey" PRIMARY KEY ("work_id"),
    CONSTRAINT "work_chapter_state_work_fk" FOREIGN KEY ("work_id") REFERENCES "work" ("id") ON DELETE CASCADE
);
