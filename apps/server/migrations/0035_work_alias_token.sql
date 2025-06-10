-- H9: word-token inverted index over work aliases, replacing the leading-wildcard
-- `work_alias.normalized_title LIKE '%token%'` full-scan used by the dedup fuzzy
-- block. `%token%` is correct (a distinctive mid-title word like "slime" still
-- matches) but can't use an index, so every dedup block token full-scanned
-- `work_alias`. This table stores one row per (work, whole word token), so the block
-- becomes an index-usable exact-token lookup while preserving word-level recall (the
-- fuzzy block already keys on whole normalized words, never substrings).
CREATE TABLE work_alias_token (
    work_id TEXT NOT NULL REFERENCES work(id) ON DELETE CASCADE,
    token   TEXT NOT NULL,
    UNIQUE (work_id, token)
);

-- Lookup index (exact token -> work ids). The composite covers the DISTINCT work_id
-- projection so the block query is served from the index alone.
CREATE INDEX idx_work_alias_token_token ON work_alias_token(token);
CREATE INDEX idx_work_alias_token_token_work ON work_alias_token(token, work_id);

-- Backfill: tokenize every existing `work_alias.normalized_title` into its
-- whitespace-separated word tokens. normalized_title is already lowercased, folded
-- to single-space-separated alphanumeric words, so a recursive split on ' ' yields
-- exactly the same tokens the fuzzy block feeds the lookup. Tokens shorter than 2
-- bytes are dropped (the lookup ignores them anyway). DISTINCT + the UNIQUE key make
-- the backfill idempotent.
WITH RECURSIVE split(work_id, token, rest) AS (
    SELECT work_id, '', normalized_title || ' '
    FROM work_alias
    WHERE normalized_title <> ''
    UNION ALL
    SELECT work_id,
           substr(rest, 1, instr(rest, ' ') - 1),
           substr(rest, instr(rest, ' ') + 1)
    FROM split
    WHERE rest <> ''
)
INSERT OR IGNORE INTO work_alias_token (work_id, token)
SELECT DISTINCT work_id, token
FROM split
WHERE token <> '' AND length(CAST(token AS BLOB)) >= 2;
