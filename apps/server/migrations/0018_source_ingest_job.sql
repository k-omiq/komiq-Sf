-- "Add all from source" bulk-ingest jobs (EXT-4 / S1).
--
-- One row per background ingest run over a Suwayomi source's full catalogue.
-- The row IS the control plane: the runner flushes progress here after every
-- page, `cancelSourceIngest` flips `state` to 'cancelled' (the runner observes
-- it between items), and a `running` row found at startup means the process
-- died mid-run — it is marked 'failed' so the source isn't blocked forever.
CREATE TABLE source_ingest_job (
    id                TEXT PRIMARY KEY,             -- ing_<uuid>
    source_id         TEXT NOT NULL,                -- Suwayomi source id being ingested
    state             TEXT NOT NULL,                -- running | completed | cancelled | failed
    pages_done        INTEGER NOT NULL DEFAULT 0,
    items_seen        INTEGER NOT NULL DEFAULT 0,
    succeeded         INTEGER NOT NULL DEFAULT 0,
    failed            INTEGER NOT NULL DEFAULT 0,
    new_works         INTEGER NOT NULL DEFAULT 0,   -- dedup decision counters
    auto_merged       INTEGER NOT NULL DEFAULT 0,
    queued_for_review INTEGER NOT NULL DEFAULT 0,
    already_existing  INTEGER NOT NULL DEFAULT 0,
    error             TEXT,                         -- terminal failure message
    started_at        TEXT NOT NULL,
    finished_at       TEXT
);

-- At most ONE running job per source; a concurrent second start loses the
-- INSERT race on this index instead of double-ingesting.
CREATE UNIQUE INDEX uq_source_ingest_running
    ON source_ingest_job(source_id) WHERE state = 'running';
