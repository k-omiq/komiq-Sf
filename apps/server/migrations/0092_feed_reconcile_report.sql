-- Phase C3 — the feed drift report.
--
-- WHAT CHANGES, AND WHY IT NEEDS A TABLE
--
-- `refresh_feed_updates` and its chain (`feed_series_updates` → `browse_catalogue` →
-- `work_fts`) has been THE mechanism keeping /updates correct: rebuild everything, from
-- scratch, at boot and once per `CATALOGUE_SYNC_INTERVAL_SECS`. That is ~20-25 s of held
-- write lock against a 15 s `busy_timeout`, which is why the plan forbids tightening the
-- chapter cycle before incremental maintenance exists — 6 h → 15 min would fire that chain
-- 96×/day instead of 4× and push the scanner's writes into SQLITE_BUSY.
--
-- Phase C2 gave both halves incremental writers, so the chain is demoted: it now runs
-- DAILY, and its job is to *report* that the incremental writers are keeping up rather than
-- to be the thing that keeps them honest.
--
-- "Report" needs somewhere to report TO. A log line alone is not enough: the question this
-- answers — "is the incremental path actually correct, or has it been quietly drifting for
-- a week?" — is asked days later, by a human looking at the admin console, not by whoever
-- happened to be tailing logs at 04:00. One row, overwritten each run, so it cannot grow.
--
-- READ THIS THE RIGHT WAY ROUND. `drifted > 0` does NOT mean the feed was broken — the
-- reconciler has already corrected it by the time the row is written. It means the
-- incremental writers MISSED something, and the number is how much. Sustained non-zero
-- drift is the signal that a write path has a hole in it; a one-off is usually a restart
-- landing between an incremental write and its projection.
CREATE TABLE IF NOT EXISTS feed_reconcile_report (
    -- Single row, always id 1.
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    ran_at       TEXT    NOT NULL,
    -- How long the whole chain held its locks. Tracked because this is the cost that
    -- justified demoting it, and a regression here is what would make the 15-minute chapter
    -- cycle unsafe again.
    duration_ms  INTEGER NOT NULL,
    -- Feed rows before the rebuild, and how many the rebuild CHANGED.
    rows_before  INTEGER NOT NULL,
    rows_after   INTEGER NOT NULL,
    drifted      INTEGER NOT NULL,
    -- A few concrete examples of what drifted, so the number is actionable rather than
    -- merely alarming. Capped in the writer; never a full dump.
    sample       TEXT
);
