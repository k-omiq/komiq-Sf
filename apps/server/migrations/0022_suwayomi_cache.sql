-- Persisted Suwayomi source-series metadata + chapter lists (S1).
--
-- Root cause of slow home/updates/series loads: every reader request LIVE-FETCHED
-- from Suwayomi (which itself fetches the upstream source over the network) —
-- there was no DB cache for Suwayomi-sourced series (only the MangaDex `work`
-- mirror existed, unused by the Suwayomi reader path). These tables cache the raw
-- Suwayomi metadata so subsequent loads read from SQLite; the scanner + ingest
-- refresh them, so sources are only hit on scan/refresh, not per request.
--
-- Cover posture (memory): we store the cover REFERENCE (thumbnail_url) only and
-- serve it through the existing Worker proxy — NO object-storage / blob cover cache.
CREATE TABLE suwayomi_series (
    id              INTEGER PRIMARY KEY,   -- Suwayomi manga id
    title           TEXT NOT NULL,
    thumbnail_url   TEXT,                  -- cover REFERENCE (proxied by the Worker), never bytes
    author          TEXT,
    artist          TEXT,
    description     TEXT,
    genre           TEXT,                  -- JSON array of genre/tag strings
    status          TEXT NOT NULL,
    in_library      INTEGER NOT NULL DEFAULT 0,
    in_library_at   TEXT,
    last_fetched_at TEXT,
    source_id       TEXT NOT NULL,
    lang            TEXT,
    chapter_count   INTEGER NOT NULL DEFAULT 0,
    updated_at      TEXT NOT NULL
);

CREATE INDEX idx_suwayomi_series_library ON suwayomi_series(in_library);

CREATE TABLE suwayomi_chapter (
    id             INTEGER PRIMARY KEY,    -- Suwayomi chapter id
    manga_id       INTEGER NOT NULL,
    name           TEXT NOT NULL,
    chapter_number REAL NOT NULL,
    scanlator      TEXT,
    upload_date    TEXT,
    is_read        INTEGER NOT NULL DEFAULT 0,
    is_bookmarked  INTEGER NOT NULL DEFAULT 0,
    is_downloaded  INTEGER NOT NULL DEFAULT 0,
    last_page_read INTEGER NOT NULL DEFAULT 0,
    page_count     INTEGER NOT NULL DEFAULT 0,
    updated_at     TEXT NOT NULL
);

CREATE INDEX idx_suwayomi_chapter_manga ON suwayomi_chapter(manga_id);
