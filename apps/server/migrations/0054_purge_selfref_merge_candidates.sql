-- Purge self-referential dedup review candidates.
--
-- A `merge_candidate` proposes folding a source series into a candidate work. When a
-- past merge folded work A into work B, A's source series were repointed to B — but
-- any pending candidate that pointed at B (with a source series that just moved to B)
-- was left behind, now self-referential: its source series and its candidate work are
-- the SAME work. Merging a source into the work it already belongs to is a no-op, so
-- these rows are dead weight. Measured 2026-07-23: ~570 of ~1,600 candidates (and 505
-- of ~1,530 pending) were self-referential, burying real duplicates behind the review
-- queue's (now removed) 200-row cap.
--
-- `merge_works` now deletes these at merge time and `merge_queue` filters them at read
-- time; this clears the historical backlog. Only self-referential rows are removed —
-- genuine cross-work candidates are untouched.
-- Only pending rows are purged: a confirmed/rejected self-ref is the audit record of
-- a past merge, not review-queue noise.
DELETE FROM merge_candidate
WHERE id IN (
    SELECT mc.id
    FROM merge_candidate mc
    JOIN source_series ss ON ss.id = mc.source_series_id
    WHERE mc.candidate_work_id = ss.work_id AND mc.status = 'pending'
);
