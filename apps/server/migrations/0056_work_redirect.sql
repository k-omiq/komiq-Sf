-- Permanent forwarding addresses for merged-away works.
--
-- `merge_works` physically DELETES the losing `work` row after folding it into the
-- survivor, so every reference minted while that id was live — a user's bookmark, a
-- cached reader URL, a notification `target_id`, an external/shared link — resolves to
-- nothing forever after. Production logs show this as recurring
-- `error=No such work path=[Field("canonicalSeries")]`.
--
-- This table is the redirect map: `old_id` (the deleted work) -> `new_id` (the
-- survivor). `merge_works` writes one row per merge and, in the same transaction,
-- rewrites any existing row whose `new_id` is the work being merged away, so a chain
-- A->B->C always collapses to a SINGLE hop (A->C, B->C) and resolvers never need to
-- walk it. `old_id` therefore never appears as a `new_id`.
--
-- Deliberately NO foreign key on either column: `old_id` names a row that no longer
-- exists (an FK would be unsatisfiable), and an FK on `new_id` would cascade-delete
-- redirects if the survivor is itself later purged — a stale redirect is harmless (the
-- resolver falls through to its normal not-found), a lost one is not.
--
-- Cheap + idempotent by construction: creating an empty table and its index touches no
-- existing row, so this does not lock the 112k-work catalogue on deploy.
--
-- The CHECK is load-bearing, not decoration. A self-redirect (`old_id = new_id`) would
-- name a work that the merge is about to DELETE as its own survivor: `redirect_work_id`
-- would hand a resolver the dead id straight back and it would 404 exactly as if no
-- redirect existed — a silent, invisible failure. The only statement that could mint one
-- is the chain-collapse `UPDATE ... SET new_id = <target> WHERE new_id = <source>` firing
-- on a pre-existing row whose `old_id` is already the target, which requires the target
-- to have been merged away earlier while still existing now (impossible: ids are fresh
-- UUIDs and `merge_works_ex` verifies both works exist). The constraint turns that
-- "impossible" into a hard, LOUD failure: the violation aborts the merge transaction, so
-- nothing is deleted and no bad redirect is written — the safe outcome for an operation
-- with no undo.
CREATE TABLE IF NOT EXISTS work_redirect (
    old_id     TEXT PRIMARY KEY,   -- the merged-away (deleted) `w_` id
    new_id     TEXT NOT NULL,      -- the surviving `w_` id it now resolves to
    created_at TEXT NOT NULL,
    CHECK (old_id <> new_id)
);

-- Supports the chain-collapse rewrite (`UPDATE ... WHERE new_id = <loser>`) that runs
-- on every merge, and any future "what folded into this work" admin lookup.
CREATE INDEX IF NOT EXISTS idx_work_redirect_new ON work_redirect (new_id);
