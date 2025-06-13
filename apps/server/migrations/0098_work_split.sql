-- 0098 — remember that an admin SPLIT two works apart, so nothing merges them back.
--
-- `splitSourceSeries` detaches source mappings off a work onto a freshly minted one. It
-- is the fix for a mis-merge: two different series folded together because they share a
-- title. That shared title is still on disk afterwards — the merge that caused the
-- problem copied the loser's alt-titles onto the survivor (`merge_works` folds
-- `work_alias`), and the detached source keeps calling itself what it always did.
--
-- Which is exactly what `consolidateExactDuplicates` hunts for. It walks `work_alias`
-- for a normalized title held by more than one work, and folds the pair when
-- `consolidate_gate` finds corroboration (year within 1, matching author key, or similar
-- cover pHash). A split whose two halves share a primary title and an author — the
-- ordinary shape of the mis-merge this feature exists to undo — clears that gate. Without
-- this table the next sweep would silently re-merge what the admin just separated, and
-- re-merging is DESTRUCTIVE: `merge_works` deletes the losing work row for good.
--
-- Worse, `consolidate` is not even the path that would get there first. A split-off work
-- is Suwayomi-only whenever the MangaDex anchor stayed behind, which is precisely the
-- predicate the post-ingest reconcile sweep selects on — and `dedup::resolve_ex` merges an
-- EXACT normalized-title hit with no corroboration required at all. So all three
-- automatic re-merge paths consult this table: `dedup::resolve_ex` (reconcile),
-- `consolidate_exact_duplicates_from` (alias-cluster sweep) and the auto-merge inside
-- `addSeriesAltTitle`, where typing the shared title on one half would otherwise drag the
-- other back in without anyone asking for a merge.
--
-- This is the only "these are not duplicates" memory in the schema; `merge_candidate`
-- cannot serve the purpose because a rejected candidate row is closed, not consulted, and
-- the sweep re-queues the pair from scratch on every pass.
--
-- KEYED BY THE UNORDERED PAIR. `consolidate_exact_duplicates_from` picks its survivor by
-- its own ordering (MangaDex-anchored first, then most sources, then lowest id), so which
-- of the two is the "loser" is not ours to predict and may flip as sources move. Rows are
-- written with `work_a < work_b` (lexicographic) and read the same way, so one row covers
-- the pair in both directions.
--
-- NO FOREIGN KEYS, deliberately. A blocked pair must outlive both works: if one side is
-- later merged away by a human who genuinely decided they ARE the same series, an
-- `ON DELETE CASCADE` would drop the row and re-arm the automated sweep for whatever
-- recycles that id. The rows are tiny and never swept.
CREATE TABLE IF NOT EXISTS work_split (
    work_a   TEXT NOT NULL,          -- the lexicographically SMALLER work id of the pair
    work_b   TEXT NOT NULL,          -- the larger
    split_at TEXT NOT NULL,          -- RFC 3339
    split_by TEXT,                   -- the admin's user id; NULL for a system-run split
    PRIMARY KEY (work_a, work_b)
) WITHOUT ROWID;
