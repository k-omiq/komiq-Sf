-- Phase F step 1 — a DURABLE home for every `all.mangadex` row's MangaDex UUID.
--
-- WHY A NEW TABLE, and not one of the three obvious existing homes:
--
--   * `source_series.source_url` is NULL on all 10,479 all.mangadex rows today and IS
--     the schema's designated place for a source-relative URL — so Phase F writes it
--     too (it is what makes the mapping visible in the admin/DB without a join). But it
--     CANNOT be the durable home: it lives ON THE ROW Phase F exists to delete. The
--     moment step 4's delete runs, every UUID stored there disappears with it, and with
--     it the only evidence of what was deleted and why it was safe.
--
--   * `work_external_id (provider = 'mangadex')` is PRIMARY KEY (provider, external_id)
--     — GLOBALLY unique, not per-work. Measured on the 2026-07-31 13:35 snapshot: all
--     496 anchorless UUIDs and all 54 mismatch UUIDs are ALREADY registered there,
--     owned by a DIFFERENT work. Writing the all.mangadex row's UUID there is therefore
--     not merely wrong, it is impossible without stealing the id from its current owner
--     (or being silently swallowed by an `OR IGNORE`).
--
--   * `suwayomi_series` has no url column at all (§4.9), and adding one would make
--     every non-MangaDex source carry a column only one extension can fill.
--
-- So: a small, standalone, append-mostly ledger keyed by `source_series.id`, holding
-- the UUID plus the disposition Phase F reached for that row. 10,479 rows.
--
-- NO FOREIGN KEY, DELIBERATELY — same reasoning `release_event.first_source_series_id`
-- is documented with (§9's "non-cascading" mitigation). A `REFERENCES source_series(id)`
-- would either CASCADE (destroying the audit trail at exactly the moment it becomes
-- evidence) or BLOCK the delete outright. This table's whole job is to OUTLIVE the row
-- it describes, so it must not be referentially bound to it.

CREATE TABLE IF NOT EXISTS all_mangadex_uuid (
    -- `source_series.id` of the all.mangadex row. Not an FK; see above.
    source_series_id TEXT PRIMARY KEY,
    -- The work the row sat on when the UUID was resolved (unresolved through
    -- `work_redirect` — the reader of this table resolves, so the record stays a
    -- verbatim account of what was true at write time).
    work_id          TEXT NOT NULL,
    -- Suwayomi manga id (== `source_series.source_key` for this row) — the key the UUID
    -- was read against, so a re-resolve can be diffed against this one.
    suwayomi_key     TEXT NOT NULL,
    -- The MangaDex UUID parsed out of Suwayomi's `MangaType.url` (`/manga/<uuid>`),
    -- stored LOWERCASE. Lowercase because every `source_series.source_key` of
    -- `source_type = 'mangadex'` is lowercase (verified: 0 of 113,889 are not), which is
    -- what lets the redundancy gate compare with `=` and use `idx_source_series_type_key`
    -- instead of a `lower()` full scan.
    mangadex_uuid    TEXT NOT NULL,
    resolved_at      TEXT NOT NULL,

    -- What Phase F did with the row, NULL until it acts:
    --   'merged'    — step 2 folded this row's anchorless work into the work that already
    --                 owned its UUID; the row itself survives, on the survivor.
    --   'split'     — step 4 re-pointed a UUID-mismatch row onto the work its UUID
    --                 actually identifies (`prev_work_id` records where it came from, so
    --                 the move is reversible with one UPDATE).
    --   'deleted'   — the row was proven redundant at the UUID level and removed.
    disposition      TEXT,
    disposed_at      TEXT,
    -- The work this row sat on BEFORE a 'split'/'merge' moved it. This is the undo.
    prev_work_id     TEXT
);

-- The gate joins UUID -> the direct MangaDex anchor that owns it; this index makes the
-- reverse lookup (which all.mangadex row claims this UUID) equally cheap, and is what
-- keeps a full classification pass over 10,479 rows index-driven on both sides.
CREATE INDEX IF NOT EXISTS idx_all_mangadex_uuid_uuid
    ON all_mangadex_uuid (mangadex_uuid);
CREATE INDEX IF NOT EXISTS idx_all_mangadex_uuid_disposition
    ON all_mangadex_uuid (disposition);
