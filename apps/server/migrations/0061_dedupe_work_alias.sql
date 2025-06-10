-- Cleanup: collapse the `work_alias` rows the table's UNIQUE key failed to constrain,
-- and drop review candidates that resurrect an already-rejected pair.
--
-- `work_alias` declares UNIQUE(work_id, normalized_title, lang), but 12,981 of its
-- 425,175 rows have `lang IS NULL` and NULLs are distinct under SQLite UNIQUE — so a
-- repeat insert of the very same alias was never deduped. Only two writers exist
-- (`catalog::insert_aliases` and the alias fold in `merge_works`) and both use
-- `INSERT OR IGNORE`, i.e. both INTENDED the constraint to swallow the repeat; with a
-- NULL lang it silently didn't. Every read site already uses DISTINCT
-- (`catalog/mod.rs:128`, `:528`), so this was never wrong answers — it is bloat plus
-- duplicated text in the `work_fts` `aliases` column, which slightly skews bm25.
--
-- WHAT IS *NOT* TOUCHED, and why the fix is much narrower than "15,503 duplicate
-- (work_id, normalized_title) groups". Most of those groups are legitimate and the
-- declared constraint permits them ON PURPOSE: they are per-language alt-titles that
-- happen to normalize to the same key — ('Crayon Shin Chan', en) vs
-- ('Crayon Shin-chan', ja-ro), or one title recorded in 19 languages. 13,637 of the
-- 14,040 exact-text-duplicate groups differ only by a KNOWN `lang`. Deleting those
-- would discard real per-language records that MangaDex ingest supplies (and would
-- simply re-create on the next metadata sync, since `mangadex::to_work_input` binds a
-- real lang for every alias). Likewise the 117 NULL-lang groups whose `raw_title`
-- genuinely differs are left intact — `normalize_title` strips parentheticals, so
-- 'One Piece' and 'One Piece (Official Colored)' collapse to one normalized key while
-- being distinct alt-titles; picking a "winner" there would lose real text from the
-- `SELECT DISTINCT raw_title` alt-title list. Only rows that are redundant in EVERY
-- column anything reads are removed.
--
-- STEP 1 — 404 rows (verified on live data 2026-07-26). Two or more NULL-lang rows for
-- the same work with the SAME normalized_title AND the SAME raw_title: byte-identical
-- aliases apart from the surrogate `id`. Nothing distinguishes them, so the survivor
-- rule is simply the lowest `id` — arbitrary by construction, not a quality judgement.
--
-- STEP 2 — 8,763 rows. A NULL-lang row whose (work_id, normalized_title, raw_title) is
-- ALSO present with a real `lang`. NULL here means "language unknown", so the
-- lang-bearing row strictly dominates: same text, more information. These arise when a
-- Suwayomi series (`graphql/mod.rs:7710` binds `lang: None`) is linked to a work whose
-- MangaDex titles already contain that exact string. Keeping the lang-bearing row is
-- the whole survivor decision — it is never ambiguous.
--
-- Total 9,166 rows deleted, 425,175 -> 416,009 (2.2%).
--
-- `work_alias_token` needs NO maintenance: every deleted row shares its
-- (work_id, normalized_title) with a surviving row of the same work, so the token
-- inverted index (keyed on work_id + token, derived from normalized_title) stays exactly
-- correct. This is why both steps require normalized_title equality, not just raw_title.
--
-- `work_fts` DOES go stale (its `aliases` column is `group_concat(raw_title)`), but it
-- needs no forced rebuild here: `main.rs:1085` runs `catalog::refresh_work_fts` on every
-- boot, ~20 s after the listener binds — i.e. on this very startup, off the critical
-- path. Rebuilding inside the migration would move that ~5-7 s onto boot downtime for a
-- correction that is already scheduled seconds later, and the interim staleness is only
-- a duplicated word in a search-text blob (search still matches; only bm25 weight
-- differs, exactly as it does today).
--
-- RECURRENCE. The partial unique index below is the real constraint the table always
-- meant to have for the NULL-lang case, and SQLite supports it without the table
-- rebuild a true UNIQUE(...) would need (a 425k-row rebuild is not worth the boot cost).
-- It is keyed on raw_title too, so it blocks the exact-repeat insert of Step 1 while
-- still permitting the distinct-text variants described above. Safe against both
-- writers because both are `INSERT OR IGNORE`: a conflicting insert is now silently
-- skipped, which is precisely the intended behaviour — verified on a snapshot that a
-- repeat `INSERT OR IGNORE` adds 0 rows and raises nothing, that a DIFFERENT raw_title
-- under the same normalized_title is still accepted, and that a plain INSERT of an exact
-- repeat now raises. Measured 0.080 s to build over the 3,815 NULL-lang rows that remain
-- after the two deletes.
-- Step 2's class canNOT be expressed as an index (it spans a NULL and a non-NULL lang),
-- so it can slowly re-accumulate; preventing it needs a one-line code change in
-- `catalog::insert_aliases` (skip a NULL-lang insert when a lang-bearing row with the
-- same work_id/normalized_title/raw_title exists), which is out of scope for a migration.
--
-- MERGE_CANDIDATE. `merge_candidate` has no UNIQUE on (source_series_id,
-- candidate_work_id) and holds 5 duplicate pairs. Inspecting them, they are not an
-- insert race: in all 5 an admin REJECTED the pair, and the dedup scanner later
-- re-proposed the identical pair — 4 of them now sitting `pending` in the review queue,
-- overriding a decision a human already made. Those 4 are deleted, restoring the
-- rejection. The 5th group is rejected+rejected: two audit records, not queue noise, so
-- it is left alone on the same principle as 0054 (a resolved row is the audit trail of a
-- past decision and is never purged). NO unique index is added here: unlike
-- `work_alias`, both writers (`catalog::insert_merge_candidate`, and the dedup path)
-- use a PLAIN `INSERT`, so a UNIQUE constraint would turn a duplicate enqueue into a
-- hard error propagated up the ingest path rather than a silent no-op. Adding it needs
-- `INSERT OR IGNORE` in `catalog::insert_merge_candidate` first; that pairing belongs in
-- a code change, not here.
--
-- COST. Each DELETE scans `work_alias` once and seeks the correlated EXISTS through
-- `sqlite_autoindex_work_alias_2` (UNIQUE(work_id, normalized_title, lang) — note 0058
-- drops the now-redundant `idx_work_alias_work`, whose leading column that autoindex
-- already provides). Measured on a full production snapshot replaying the whole
-- cold-start migration sequence 0056 -> 0061: 1.33 s and 2.29 s over two trials for this
-- entire file (both deletes + the index + the merge_candidate delete). Re-run cost is
-- 0.04 s. Idempotent: a second run deletes 0 rows and `IF NOT EXISTS` skips the index —
-- verified, along with `integrity_check` = ok and an empty `foreign_key_check`.

-- Step 1: byte-identical NULL-lang duplicates; keep the lowest id.
DELETE FROM work_alias
WHERE lang IS NULL
  AND EXISTS (
      SELECT 1 FROM work_alias b
      WHERE b.work_id = work_alias.work_id
        AND b.normalized_title = work_alias.normalized_title
        AND b.raw_title = work_alias.raw_title
        AND b.lang IS NULL
        AND b.id < work_alias.id
  );

-- Step 2: NULL-lang rows fully shadowed by an identical row that knows its language.
DELETE FROM work_alias
WHERE lang IS NULL
  AND EXISTS (
      SELECT 1 FROM work_alias b
      WHERE b.work_id = work_alias.work_id
        AND b.normalized_title = work_alias.normalized_title
        AND b.raw_title = work_alias.raw_title
        AND b.lang IS NOT NULL
  );

-- The constraint UNIQUE(work_id, normalized_title, lang) could not enforce, now that
-- the rows that violated it are gone.
CREATE UNIQUE INDEX IF NOT EXISTS idx_work_alias_nulllang_unique
    ON work_alias (work_id, normalized_title, raw_title)
    WHERE lang IS NULL;

-- Re-proposed candidates for a pair an admin already resolved: drop the pending copy,
-- keep the resolved audit row.
DELETE FROM merge_candidate
WHERE status = 'pending'
  AND EXISTS (
      SELECT 1 FROM merge_candidate o
      WHERE o.source_series_id = merge_candidate.source_series_id
        AND o.candidate_work_id = merge_candidate.candidate_work_id
        AND o.status <> 'pending'
        AND o.created_at < merge_candidate.created_at
  );
