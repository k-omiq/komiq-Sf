-- Cover bytes live in content-addressed files under KOMIKA_COVERS_DIR; this is their index.
-- One row per asset and kind: asset_key is 'w:<work id>', 's:<manga id>' or 'icon:<package>',
-- kind is 'master', 'w192' or 'w320'. A derivative row names the master it was made from, so a
-- stale derivative can never match a newer master. version is the work cover version (work
-- masters only). The row and the work's cover pointer change in one transaction.
CREATE TABLE "cover_object" (
    "asset_key" text NOT NULL,
    "kind" text NOT NULL,
    "sha256" bytea NOT NULL,
    "byte_length" integer NOT NULL,
    "version" bigint,
    "master_sha256" bytea,
    "updated_at" timestamptz NOT NULL,
    -- Work rows follow their work: a merge or delete drops them in the same transaction.
    "work_id" text GENERATED ALWAYS AS (
        CASE WHEN "asset_key" LIKE 'w:%' THEN substr("asset_key", 3) END
    ) STORED,
    CONSTRAINT "cover_object_pkey" PRIMARY KEY ("asset_key", "kind"),
    CONSTRAINT "cover_object_kind_check" CHECK ("kind" IN ('master', 'w192', 'w320')),
    CONSTRAINT "cover_object_sha256_check" CHECK (length("sha256") = 32),
    CONSTRAINT "cover_object_master_check" CHECK (
        ("kind" = 'master') = ("master_sha256" IS NULL)
    ),
    CONSTRAINT "cover_object_work_fk" FOREIGN KEY ("work_id") REFERENCES "work" ("id") ON DELETE CASCADE
);
CREATE INDEX "cover_object_sha256_idx" ON "cover_object" ("sha256");
CREATE INDEX "cover_object_work_id_idx" ON "cover_object" ("work_id") WHERE "work_id" IS NOT NULL;

-- An admin-uploaded work cover (the processed master). Cover files are a cache and are not
-- backed up; this row is, so an uploaded cover survives the loss of the files. While a row
-- exists its sha256 equals the work's current master.
CREATE TABLE "cover_upload" (
    "work_id" text NOT NULL,
    "bytes" bytea NOT NULL,
    "sha256" bytea NOT NULL,
    "created_at" timestamptz NOT NULL,
    CONSTRAINT "cover_upload_pkey" PRIMARY KEY ("work_id"),
    CONSTRAINT "cover_upload_sha256_check" CHECK (length("sha256") = 32),
    CONSTRAINT "cover_upload_work_fk" FOREIGN KEY ("work_id") REFERENCES "work" ("id") ON DELETE CASCADE
);
