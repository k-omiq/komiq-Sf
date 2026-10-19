-- Queued "add picked": the admin's picked series become items of the source's ingest
-- job, and the background runner adds them one by one inside the source's request
-- budget (ingest.rs). Before this, "add picked" was one HTTP request that added every
-- series in turn; on a rate-limited source each one waits for the budget, and the
-- request was cut at the proxy's 100 s limit after one or two series.
--
-- `kind` says what the job does besides draining its items: `all` walks the source's
-- listings first ("add all from source"), `picked` only drains its items. Picks made
-- while either kind runs join that job.
ALTER TABLE "source_ingest_job"
    ADD COLUMN "kind" text NOT NULL DEFAULT 'all',
    ADD COLUMN "items_queued" bigint NOT NULL DEFAULT 0,
    ADD CONSTRAINT "source_ingest_job_kind_check" CHECK ("kind" IN ('all', 'picked'));

-- One row per queued series. `finished_at` is NULL while the item waits; the outcome
-- columns are then either the dedup result or `error`. `position` keeps pick order.
CREATE TABLE "source_ingest_item" (
    "job_id" text NOT NULL REFERENCES "source_ingest_job" ("id") ON DELETE CASCADE,
    "manga_id" bigint NOT NULL,
    "position" bigint GENERATED ALWAYS AS IDENTITY,
    "decision" text,
    "work_id" text,
    "matched_work_id" text,
    "score" double precision,
    "method" text,
    "source_series_id" text,
    "error" text,
    "finished_at" timestamptz,
    CONSTRAINT "source_ingest_item_pkey" PRIMARY KEY ("job_id", "manga_id")
);

CREATE INDEX "idx_source_ingest_item_pending"
    ON "source_ingest_item" ("job_id", "position") WHERE "finished_at" IS NULL;
