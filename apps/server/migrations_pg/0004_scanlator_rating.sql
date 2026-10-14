-- A reader's rating of a scanlator, from 1 to 10. A scanlator is not a row anywhere:
-- scanlator_id is "site:<source id>" or "group:<name key>" (scanlators::ScanlatorId).
-- One score per reader and scanlator; deleting the account deletes its scores.
-- PostgreSQL only: no SQLite file holds this table, so the importer copies nothing into it.
CREATE TABLE "scanlator_rating" (
    "scanlator_id" text NOT NULL,
    "user_id" uuid NOT NULL,
    "score" integer NOT NULL,
    "created_at" timestamptz NOT NULL,
    "updated_at" timestamptz NOT NULL,
    CONSTRAINT "scanlator_rating_pkey" PRIMARY KEY ("scanlator_id", "user_id"),
    CONSTRAINT "scanlator_rating_ck_1" CHECK (score BETWEEN 1 AND 10)
);
ALTER TABLE "scanlator_rating" ADD CONSTRAINT "scanlator_rating_fk_0" FOREIGN KEY ("user_id") REFERENCES "users" ("id") ON DELETE CASCADE ON UPDATE NO ACTION;
-- "The scanlators I rated": the primary key leads with scanlator_id.
CREATE INDEX idx_scanlator_rating_user ON scanlator_rating(user_id);
