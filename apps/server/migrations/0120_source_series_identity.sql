-- A Suwayomi manga id is global across its sources. Migration 0005's natural
-- key allowed a placeholder source_id='' and a known source_id to map it twice.
-- Keep one mapping deterministically, preferring the engine's recorded source,
-- then a MangaDex-anchored work, then a known source and titled work. Retain the
-- selected mapping's work/id: this migration never deletes a work or user data.
CREATE TEMP TABLE _ss_identity_map (
    old_id TEXT PRIMARY KEY,
    keep_id TEXT NOT NULL
);
INSERT INTO _ss_identity_map(old_id, keep_id)
SELECT ss.id,
       first_value(ss.id) OVER (
           PARTITION BY ss.source_key
           ORDER BY
               COALESCE(ss.source_id = sw.source_id, 0) DESC,
               EXISTS(SELECT 1 FROM source_series md
                       WHERE md.work_id = ss.work_id AND md.source_type = 'mangadex') DESC,
               (NULLIF(ss.source_id, '') IS NOT NULL) DESC,
               (NULLIF(COALESCE(w.title_override, w.primary_title), '') IS NOT NULL) DESC,
               ss.created_at, ss.id
       )
  FROM source_series ss
  JOIN work w ON w.id = ss.work_id
  LEFT JOIN suwayomi_series sw ON sw.id = CAST(ss.source_key AS INTEGER)
 WHERE ss.source_type = 'suwayomi';
CREATE INDEX _ss_identity_map_keep ON _ss_identity_map(keep_id);

-- source_id was NOT NULL, so replacing the table is necessary to normalize
-- unknown to SQL NULL. Its chapter/merge_candidate children CASCADE on deletion.
-- Preserve them first, empty them, rebuild the parent, then restore them, all in
-- sqlx's enclosing migration transaction with foreign_keys ON. Do not disable
-- foreign keys or ALTER/rename child tables: their own schema/indexes stay intact.
-- A child whose source mapping is already gone (removed while foreign keys were
-- not enforced) belongs to no work and cannot be restored under foreign_keys ON:
-- leave it out. sqlite_0120_preflight.sql lists those rows beforehand.
CREATE TEMP TABLE _ss_identity_source AS SELECT * FROM source_series;
CREATE INDEX _ss_identity_source_id ON _ss_identity_source(id);
CREATE TEMP TABLE _ss_identity_chapter AS
SELECT c.* FROM chapter c
 WHERE EXISTS (SELECT 1 FROM source_series ss WHERE ss.id = c.source_series_id);
CREATE TEMP TABLE _ss_identity_candidate AS
SELECT c.* FROM merge_candidate c
 WHERE EXISTS (SELECT 1 FROM source_series ss WHERE ss.id = c.source_series_id);
DELETE FROM chapter;
DELETE FROM merge_candidate;
DROP TABLE source_series;

CREATE TABLE source_series (
    id          TEXT PRIMARY KEY,
    work_id     TEXT NOT NULL REFERENCES work(id) ON DELETE CASCADE,
    source_type TEXT NOT NULL,
    source_id   TEXT DEFAULT NULL,
    source_key  TEXT NOT NULL,
    source_url  TEXT,
    is_nsfw     INTEGER NOT NULL DEFAULT 0,
    last_seen   TEXT,
    created_at  TEXT NOT NULL,
    UNIQUE (source_type, source_id, source_key)
);

INSERT INTO source_series
    (id, work_id, source_type, source_id, source_key, source_url,
     is_nsfw, last_seen, created_at)
SELECT ss.id, ss.work_id, ss.source_type, NULLIF(ss.source_id, ''), ss.source_key,
       COALESCE(ss.source_url,
           (SELECT b.source_url FROM _ss_identity_source b
              JOIN _ss_identity_map bm ON bm.old_id = b.id
             WHERE bm.keep_id = ss.id AND b.source_url IS NOT NULL
             ORDER BY b.id LIMIT 1)),
       COALESCE((SELECT MAX(b.is_nsfw) FROM _ss_identity_source b
                  JOIN _ss_identity_map bm ON bm.old_id = b.id
                 WHERE bm.keep_id = ss.id), ss.is_nsfw),
       COALESCE((SELECT MAX(b.last_seen) FROM _ss_identity_source b
                  JOIN _ss_identity_map bm ON bm.old_id = b.id
                 WHERE bm.keep_id = ss.id), ss.last_seen),
       COALESCE((SELECT MIN(b.created_at) FROM _ss_identity_source b
                  JOIN _ss_identity_map bm ON bm.old_id = b.id
                 WHERE bm.keep_id = ss.id), ss.created_at)
  FROM _ss_identity_source ss
  LEFT JOIN _ss_identity_map m ON m.old_id = ss.id
 WHERE m.keep_id IS NULL OR m.old_id = m.keep_id;

CREATE INDEX idx_source_series_work ON source_series(work_id);
CREATE INDEX idx_source_series_type_created ON source_series(source_type, created_at);
CREATE INDEX idx_source_series_suwayomi_nsfw
    ON source_series(source_type, source_key, work_id);
CREATE INDEX idx_source_series_suwayomi_key
    ON source_series(source_key, source_id) WHERE source_type = 'suwayomi';
CREATE UNIQUE INDEX uq_source_series_suwayomi_key
    ON source_series(source_key) WHERE source_type = 'suwayomi';
-- SQL UNIQUE permits multiple NULLs. Preserve the old natural-key uniqueness
-- for an unknown source as well, after converting its previous '' sentinel.
CREATE UNIQUE INDEX uq_source_series_unknown_key
    ON source_series(source_type, source_key) WHERE source_id IS NULL;

-- Restore the kept mapping's chapters first. Repoint distinct chapters from
-- duplicate mappings; an external_id already on the keeper retains its existing
-- chapter row and id, filling only its missing metadata from duplicates.
-- Reader/progress chapter ids are external_id, unchanged here.
INSERT INTO chapter
    (id, source_series_id, external_id, number, volume, lang, title,
     published_at, created_at, readable_at, external_url, chapter_key, label, scanlator)
SELECT c.id, c.source_series_id, c.external_id, c.number, c.volume, c.lang, c.title,
       c.published_at, c.created_at, c.readable_at, c.external_url,
       c.chapter_key, c.label, c.scanlator
  FROM _ss_identity_chapter c
 WHERE NOT EXISTS(SELECT 1 FROM _ss_identity_map m
                   WHERE m.old_id = c.source_series_id AND m.old_id != m.keep_id);

INSERT INTO chapter
    (id, source_series_id, external_id, number, volume, lang, title,
     published_at, created_at, readable_at, external_url, chapter_key, label, scanlator)
SELECT c.id, m.keep_id, c.external_id, c.number, c.volume, c.lang, c.title,
       c.published_at, c.created_at, c.readable_at, c.external_url,
       c.chapter_key, c.label, c.scanlator
  FROM _ss_identity_chapter c
  JOIN _ss_identity_map m ON m.old_id = c.source_series_id
 WHERE m.old_id != m.keep_id
 ORDER BY c.created_at, c.id
ON CONFLICT(source_series_id, external_id) DO UPDATE SET
    number = COALESCE(chapter.number, excluded.number),
    volume = COALESCE(chapter.volume, excluded.volume),
    lang = COALESCE(chapter.lang, excluded.lang),
    title = COALESCE(chapter.title, excluded.title),
    published_at = COALESCE(chapter.published_at, excluded.published_at),
    created_at = MIN(chapter.created_at, excluded.created_at),
    readable_at = COALESCE(chapter.readable_at, excluded.readable_at),
    external_url = COALESCE(chapter.external_url, excluded.external_url),
    chapter_key = COALESCE(chapter.chapter_key, excluded.chapter_key),
    label = COALESCE(chapter.label, excluded.label),
    scanlator = COALESCE(chapter.scanlator, excluded.scanlator);

INSERT INTO merge_candidate
    (id, source_series_id, candidate_work_id, score, method, status, created_at, resolved_at)
SELECT c.id, COALESCE(m.keep_id, c.source_series_id), c.candidate_work_id,
       c.score, c.method, c.status, c.created_at, c.resolved_at
  FROM _ss_identity_candidate c
  LEFT JOIN _ss_identity_map m ON m.old_id = c.source_series_id;

-- Reparented back-catalogue chapters must carry their announcement history too.
-- Otherwise the ordinary ledger seed sees a missing keeper-work event and can
-- stamp an old chapter as available now. Copy only keys retained on the keeper;
-- unrelated chapters on a losing work and discarded chapter-key variants stay put.
-- Snapshot first so one copied event cannot become another copy's input when
-- duplicate source groups cross the same works. Preserve every original event.
CREATE TEMP TABLE _ss_identity_release_history AS
SELECT DISTINCT keeper.work_id, re.chapter_key, re.first_seen_at,
       re.first_source_series_id, re.label, re.announce_at
  FROM _ss_identity_chapter c
  JOIN _ss_identity_map m ON m.old_id = c.source_series_id
  JOIN _ss_identity_source origin ON origin.id = m.old_id
  JOIN _ss_identity_source keeper ON keeper.id = m.keep_id
  JOIN release_event re ON re.work_id = origin.work_id
                       AND re.chapter_key = c.chapter_key
 WHERE m.old_id != m.keep_id AND origin.work_id != keeper.work_id
   AND EXISTS (SELECT 1 FROM chapter kept
                WHERE kept.source_series_id = m.keep_id
                  AND kept.chapter_key = c.chapter_key);

INSERT INTO release_event
    (work_id, chapter_key, first_seen_at, first_source_series_id, label, announce_at)
SELECT work_id, chapter_key, first_seen_at, first_source_series_id, label, announce_at
  FROM _ss_identity_release_history WHERE true
 ORDER BY first_seen_at, COALESCE(first_source_series_id, ''), label, chapter_key
ON CONFLICT(work_id, chapter_key) DO UPDATE SET
    first_source_series_id = CASE
        WHEN excluded.first_seen_at < release_event.first_seen_at
        THEN excluded.first_source_series_id ELSE release_event.first_source_series_id END,
    label = CASE WHEN excluded.first_seen_at < release_event.first_seen_at
        THEN excluded.label ELSE release_event.label END,
    announce_at = MIN(COALESCE(release_event.announce_at, release_event.first_seen_at),
                      COALESCE(excluded.announce_at, excluded.first_seen_at)),
    first_seen_at = MIN(release_event.first_seen_at, excluded.first_seen_at);
DROP TABLE _ss_identity_release_history;

-- Non-enforcing historical pointers deliberately outlive their source row.
-- Do not remap source-id strings to the keeper: copied/earlier release provenance
-- retains its original source id. all_mangadex_uuid records stay unchanged, like
-- other source removals; no historical work is deleted or merged.

-- Accept legacy callers that still pass '' while storing exactly one unknown
-- representation. New writers bind NULLIF(source_id, '') themselves.
CREATE TRIGGER source_series_unknown_source_insert
AFTER INSERT ON source_series
WHEN NEW.source_id = ''
BEGIN
    UPDATE source_series SET source_id = NULL WHERE id = NEW.id;
END;
CREATE TRIGGER source_series_unknown_source_update
AFTER UPDATE OF source_id ON source_series
WHEN NEW.source_id = ''
BEGIN
    UPDATE source_series SET source_id = NULL WHERE id = NEW.id;
END;

CREATE TEMP TABLE _ss_identity_fk_guard (
    violations INTEGER CHECK (violations = 0)
);
INSERT INTO _ss_identity_fk_guard
SELECT COUNT(*) FROM pragma_foreign_key_check;
DROP TABLE _ss_identity_fk_guard;
DROP TABLE _ss_identity_candidate;
DROP TABLE _ss_identity_chapter;
DROP TABLE _ss_identity_source;
DROP TABLE _ss_identity_map;
ANALYZE source_series;
