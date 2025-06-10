-- Canonical catalogue layer (the "one entry per series" pivot; see CATALOGUE.md).
-- The backend moves from pure live-federation to a hybrid: MangaDex metadata +
-- chapter lists are mirrored here and every source-series resolves to ONE canonical
-- `work`. This migration is ADDITIVE — it does not touch the existing series_id
-- columns on reviews/series_admin/series_scan_state, so the running server is
-- unaffected. Later milestones migrate reads onto work_id.

-- The one canonical entry per logical series. MangaDex is the spine; works with no
-- MangaDex anchor are first-class too. Fields are nullable where a backfilled or
-- not-yet-enriched work has no value yet (title/description arrive from sync).
CREATE TABLE work (
    id                TEXT PRIMARY KEY,
    primary_title     TEXT,                          -- NULL until enriched (falls back to live source)
    primary_lang      TEXT,
    description       TEXT,
    year              INTEGER,
    original_language TEXT,
    status            TEXT,                           -- ONGOING/COMPLETED/HIATUS/CANCELLED/UNKNOWN
    demographic       TEXT,                           -- shounen/shoujo/seinen/josei/NULL
    content_rating    TEXT,                           -- safe/suggestive/erotica/pornographic
    is_nsfw           INTEGER NOT NULL DEFAULT 0,     -- source nsfw flag OR contentRating in {erotica,pornographic}
    author            TEXT,                           -- denormalised primary author (display + matching)
    artist            TEXT,
    cover_phash       TEXT,                           -- perceptual hash (hex) for cover-based matching
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL
);

-- Alias index built from MangaDex altTitles (+ the primary title). normalized_title
-- is the matcher's lookup key (romanised, lowercased, punctuation/season-suffix stripped).
CREATE TABLE work_alias (
    id               TEXT PRIMARY KEY,
    work_id          TEXT NOT NULL REFERENCES work(id) ON DELETE CASCADE,
    normalized_title TEXT NOT NULL,
    raw_title        TEXT NOT NULL,
    lang             TEXT,
    UNIQUE (work_id, normalized_title, lang)
);

CREATE INDEX idx_work_alias_norm ON work_alias(normalized_title);
CREATE INDEX idx_work_alias_work ON work_alias(work_id);

-- External catalogue IDs (AniList/MAL/MangaUpdates/Kitsu/AnimePlanet/MangaDex) — the
-- highest-precision match key. UNIQUE(provider, external_id) enforces global identity.
CREATE TABLE work_external_id (
    work_id     TEXT NOT NULL REFERENCES work(id) ON DELETE CASCADE,
    provider    TEXT NOT NULL,                        -- al/mal/mu/kt/ap/mangadex
    external_id TEXT NOT NULL,
    PRIMARY KEY (provider, external_id)
);

CREATE INDEX idx_work_external_work ON work_external_id(work_id);

-- A concrete series on a concrete source, resolved to a canonical work (many-to-one).
-- The Suwayomi manga id (previously the sole series identity) now lives in source_key.
CREATE TABLE source_series (
    id          TEXT PRIMARY KEY,
    work_id     TEXT NOT NULL REFERENCES work(id) ON DELETE CASCADE,
    source_type TEXT NOT NULL,                        -- mangadex | suwayomi
    source_id   TEXT NOT NULL DEFAULT '',             -- extension/source id ('' if unknown, e.g. backfill)
    source_key  TEXT NOT NULL,                        -- manga id/slug within the source
    source_url  TEXT,
    is_nsfw     INTEGER NOT NULL DEFAULT 0,
    last_seen   TEXT,
    created_at  TEXT NOT NULL,
    UNIQUE (source_type, source_id, source_key)
);

CREATE INDEX idx_source_series_work ON source_series(work_id);

-- Mirrored chapter list, per source_series. Powers stored update-checking without a
-- live round-trip. external_id is the chapter id within the source (e.g. MangaDex uuid).
CREATE TABLE chapter (
    id               TEXT PRIMARY KEY,
    source_series_id TEXT NOT NULL REFERENCES source_series(id) ON DELETE CASCADE,
    external_id      TEXT NOT NULL,
    number           TEXT,                            -- string: chapters can be "10.5"
    volume           TEXT,
    lang             TEXT,
    title            TEXT,
    published_at     TEXT,
    created_at       TEXT NOT NULL,
    UNIQUE (source_series_id, external_id)
);

CREATE INDEX idx_chapter_source_series ON chapter(source_series_id);

-- Mid-confidence dedup matches awaiting manual admin review (confirm/reject/new).
CREATE TABLE merge_candidate (
    id                TEXT PRIMARY KEY,
    source_series_id  TEXT NOT NULL REFERENCES source_series(id) ON DELETE CASCADE,
    candidate_work_id TEXT NOT NULL REFERENCES work(id) ON DELETE CASCADE,
    score             REAL NOT NULL,
    method            TEXT NOT NULL,                  -- external_id/title_exact/fuzzy/description/cover
    status            TEXT NOT NULL DEFAULT 'pending',-- pending/confirmed/rejected
    created_at        TEXT NOT NULL,
    resolved_at       TEXT
);

CREATE INDEX idx_merge_candidate_status ON merge_candidate(status);

-- Backfill: seed a placeholder canonical work + a Suwayomi source_series for every
-- series id already referenced by the social/admin/scan tables, so the canonical layer
-- is populated from day one. Ids are derived deterministically ('w_'/'ss_' + series_id)
-- so the two inserts correlate without a UUID function. Titles stay NULL (still served
-- live) until MangaDex sync enriches them. source_id is '' (unknown at backfill).
INSERT OR IGNORE INTO work (id, is_nsfw, created_at, updated_at)
SELECT DISTINCT 'w_' || series_id, 0,
       strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
FROM (
    SELECT series_id FROM series_admin
    UNION SELECT series_id FROM series_scan_state
    UNION SELECT series_id FROM reviews
)
WHERE series_id IS NOT NULL AND series_id <> '';

INSERT OR IGNORE INTO source_series (id, work_id, source_type, source_id, source_key, created_at)
SELECT DISTINCT 'ss_' || series_id, 'w_' || series_id, 'suwayomi', '', series_id,
       strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
FROM (
    SELECT series_id FROM series_admin
    UNION SELECT series_id FROM series_scan_state
    UNION SELECT series_id FROM reviews
)
WHERE series_id IS NOT NULL AND series_id <> '';
