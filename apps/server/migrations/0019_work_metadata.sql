-- MangaDex metadata enrichment (S2, CATALOGUE.md §3/§4).
--
-- MangaDex is the canonical spine: even where it mirrors no chapters (licensed
-- titles) it carries the richest metadata — multi-language descriptions and full
-- author/artist credit lists. Alt titles already live per-language in
-- `work_alias` (raw_title + lang); these two tables complete the picture. The
-- singular `work.description` / `work.author` / `work.artist` columns remain as
-- the English-preferred "primary" values the reader shape reads today.

-- Every localized description of a work, keyed by language.
CREATE TABLE work_description (
    work_id     TEXT NOT NULL REFERENCES work(id) ON DELETE CASCADE,
    lang        TEXT NOT NULL,
    description TEXT NOT NULL,
    PRIMARY KEY (work_id, lang)
);

-- Full credit list (a work can have several authors AND several artists; the
-- singular work.author/artist columns keep only the first of each).
CREATE TABLE work_credit (
    work_id TEXT NOT NULL REFERENCES work(id) ON DELETE CASCADE,
    role    TEXT NOT NULL,   -- author | artist
    name    TEXT NOT NULL,
    PRIMARY KEY (work_id, role, name)
);

CREATE INDEX idx_work_credit_work ON work_credit(work_id);
