-- Article-insensitive duplicate key for `work_alias`.
--
-- Sites disagree on leading articles: one publishes "The Beginning After the End",
-- another "Beginning After End". `normalized_title` keeps the articles (search and the
-- fuzzy block depend on its exact tokens), so those two never met in the duplicate
-- lookup. `match_key` is `normalized_title` with the word tokens `the` / `a` / `an`
-- removed, EXCEPT in last position: a trailing "a" is a series marker ("Heavy Object A",
-- "Ultraman A"), not an article, and keeping the last token also means the key can
-- never be empty.
--
-- VIRTUAL, so there is nothing to backfill and nothing that can drift from
-- `normalized_title`; only the index below is materialised. Each `replace` runs twice
-- because SQLite's replace does not rescan what it just produced ("the the x").
--
-- Must stay in step with `catalog::normalize::match_key`, which is the definition the
-- merge gate trusts. A disagreement is safe by construction: the gate recomputes the key
-- in Rust and refuses a pair whose titles do not actually produce it.
ALTER TABLE work_alias ADD COLUMN match_key TEXT GENERATED ALWAYS AS (
    trim(replace(replace(replace(replace(replace(replace(
        ' ' || normalized_title,
        ' the ', ' '), ' the ', ' '),
        ' an ', ' '), ' an ', ' '),
        ' a ', ' '), ' a ', ' '))
) VIRTUAL;

-- Covers both the keyset walk of the consolidation sweep (GROUP BY match_key, COUNT
-- DISTINCT work_id) and the exact-key lookup at ingest.
CREATE INDEX idx_work_alias_match_key ON work_alias(match_key, work_id);
