-- Reader-filed reports: the catalogue's only inbound channel for the things only a
-- reader can see (Support → Report an issue).
--
-- WHY THIS EXISTS. Every existing queue in this schema is machine-generated:
-- `merge_candidate` is the matcher's own uncertainty, `work_cover_issue` is the cover
-- pipeline's own failures, `source_scan_outage` is the scanner's. None of them can
-- observe the four failure modes that are only visible from the reader's side of the
-- API:
--
--   wrong_merge     Two different series were folded into one canonical work, so the
--                   chapter list interleaves two unrelated stories. The matcher scored
--                   this as a CONFIDENT match, which is exactly why it is not in
--                   `merge_candidate` — the queue holds what it was unsure about.
--   needs_merge     The inverse: the same series exists twice under two ids because no
--                   corroborating signal crossed the matcher's threshold. Invisible to
--                   us, obvious to someone reading both.
--   missing_work    A series that is not in the catalogue at all. There is no row to
--                   attach a flag to, hence the free-form `subject_title` column.
--   comment_abuse   A person, not a record. `banUser`/`deleteComment` already exist but
--                   nothing routes a reader's complaint to an admin who can use them.
--
-- ANONYMOUS BY DESIGN. `user_id` is nullable and there is deliberately NO ip column.
-- Most catalogue-accuracy reports come from readers who never sign in, and dropping
-- those loses the majority of the signal; throttling is handled in-process by the
-- resolver's `RateLimiter` (keyed by request ip, never persisted) so the promise the
-- About page makes — that we retain nothing about who read what — stays true. A
-- signed-in reporter is recorded, because "who reported this" is genuinely useful when
-- triaging repeat abuse claims.
--
-- WHY THE TARGET COLUMNS ARE DENORMALIZED TEXT, NOT FOREIGN KEYS.
--   • `subject_id` / `subject_id_secondary` hold the id the READER was looking at,
--     which may be a numeric Suwayomi series id, a `w_` canonical work id, or (for a
--     merge report) one of each. There is no single table to point at, and the whole
--     point of a wrong-merge report is that the id mapping is wrong — an FK would
--     reject exactly the broken states we want reported.
--   • `comment_id` has no FK because `deleteComment` HARD-deletes comment rows
--     (recursive subtree delete, no tombstone). An `ON DELETE CASCADE` would erase the
--     report the moment an admin acted on it, destroying the audit trail; `ON DELETE
--     RESTRICT` would block the deletion the report asked for. So the reported author's
--     username and the body excerpt are copied in at submit time and the report
--     survives the moderation it triggered.
--
-- STATUS is the `merge_candidate` vocabulary ('open' → 'resolved' | 'rejected') rather
-- than a boolean, because "we looked and this is not a bug" is a different, and much
-- more common, outcome than "fixed" and the difference is what stops an admin
-- re-triaging the same false report every week.
--
-- (Version 0080, not 0073: the 007x block is left clear for migrations in flight on
-- other branches. sqlx applies by version order and tolerates gaps.)

CREATE TABLE IF NOT EXISTS report (
    id                   TEXT PRIMARY KEY,
    -- 'wrong_merge' | 'needs_merge' | 'missing_work' | 'comment_abuse' | 'other'
    kind                 TEXT NOT NULL,
    -- 'open' | 'resolved' | 'rejected'
    status               TEXT NOT NULL DEFAULT 'open',

    -- Reporter. NULL user_id = anonymous (see above).
    user_id              TEXT REFERENCES users(id) ON DELETE SET NULL,

    -- What it is about. Which of these are set depends on `kind`; the resolver
    -- validates that pairing, the schema stays permissive so a new kind does not need a
    -- migration.
    subject_id           TEXT,
    subject_id_secondary TEXT,
    subject_title        TEXT,
    comment_id           TEXT,
    reported_username    TEXT,
    comment_excerpt      TEXT,
    source_url           TEXT,

    -- The reader's own words. Required: a report with no description is not actionable.
    detail               TEXT NOT NULL,

    -- Triage.
    admin_note           TEXT,
    resolved_at          TEXT,
    resolved_by          TEXT REFERENCES users(id) ON DELETE SET NULL,

    created_at           TEXT NOT NULL
);

-- The admin queue's only two orderings: everything open, newest first (the default
-- view), and one kind at a time (the filter). Both are served by this pair.
CREATE INDEX IF NOT EXISTS idx_report_status_created ON report (status, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_report_kind_created ON report (kind, created_at DESC);
-- "Has this series already been reported?" — asked per row while triaging merges.
CREATE INDEX IF NOT EXISTS idx_report_subject ON report (subject_id) WHERE subject_id IS NOT NULL;
