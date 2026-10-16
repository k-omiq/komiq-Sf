CREATE TABLE "media_object" (
    "sha256" bytea NOT NULL,
    "byte_length" bigint NOT NULL,
    "mime" text NOT NULL,
    "width" integer NULL,
    "height" integer NULL,
    "first_seen" timestamptz NOT NULL,
    CONSTRAINT "media_object_pkey" PRIMARY KEY ("sha256")
);
CREATE TABLE "media_copy" (
    "id" text NOT NULL,
    "source_id" text NOT NULL,
    "manga_key" text NOT NULL,
    "chapter_key" text NOT NULL,
    "work_id" text NULL,
    "created_at" timestamptz NOT NULL,
    CONSTRAINT "media_copy_pkey" PRIMARY KEY ("id"),
    CONSTRAINT "media_copy_identity_unique" UNIQUE ("source_id", "manga_key", "chapter_key")
);
CREATE TABLE "media_manifest" (
    "copy_id" text NOT NULL,
    "revision" integer NOT NULL,
    "page_count" integer NOT NULL,
    "policy_epoch" bigint NOT NULL,
    "created_at" timestamptz NOT NULL,
    "superseded_at" timestamptz NULL,
    "stale" boolean NOT NULL DEFAULT false,
    CONSTRAINT "media_manifest_pkey" PRIMARY KEY ("copy_id", "revision")
);
ALTER TABLE "media_manifest" ADD CONSTRAINT "media_manifest_fk_0" FOREIGN KEY ("copy_id") REFERENCES "media_copy" ("id");
CREATE TABLE "media_manifest_page" (
    "copy_id" text NOT NULL,
    "revision" integer NOT NULL,
    "idx" integer NOT NULL,
    "sha256" bytea NOT NULL,
    CONSTRAINT "media_manifest_page_pkey" PRIMARY KEY ("copy_id", "revision", "idx")
);
ALTER TABLE "media_manifest_page" ADD CONSTRAINT "media_manifest_page_fk_0" FOREIGN KEY ("sha256") REFERENCES "media_object" ("sha256");
-- One-column keys only, like every planned foreign key (`pg_import::verifier::constraints`).
-- A page row belongs to its revision by (copy_id, revision); `store::prune` deletes both together.
ALTER TABLE "media_manifest_page" ADD CONSTRAINT "media_manifest_page_fk_1" FOREIGN KEY ("copy_id") REFERENCES "media_copy" ("id");
CREATE INDEX idx_media_manifest_page_sha256 ON media_manifest_page (sha256);
