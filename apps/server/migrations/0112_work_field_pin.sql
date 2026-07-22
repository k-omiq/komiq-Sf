-- Admin edits to a work's source-owned columns (author, artist, year, original language,
-- status, demographic, content rating).
--
-- These columns are written by several sync paths (the MangaDex catalogue upsert, the
-- metadata backfill, Suwayomi enrichment), all of which would silently undo an admin's
-- correction on their next pass. A pin records the admin's value; the triggers below put
-- it back whenever anything else writes the column, and remember what the source wanted
-- (`source_value`) so removing the pin restores the current source value.
--
-- The value lives in the `work` column itself, so every reader — Browse, search, the
-- duplicate matcher, the content-type derivation — sees the edit without a COALESCE.
CREATE TABLE work_field_pin (
    work_id      TEXT NOT NULL REFERENCES work(id) ON DELETE CASCADE,
    field        TEXT NOT NULL,   -- a `work` column name from the list above
    value        TEXT,            -- the pinned value; NULL pins the field empty
    source_value TEXT,            -- the last value a source wrote
    pinned_at    TEXT NOT NULL,
    PRIMARY KEY (work_id, field)
) WITHOUT ROWID;

CREATE TRIGGER work_pin_author
AFTER UPDATE OF author ON work
WHEN EXISTS (SELECT 1 FROM work_field_pin
             WHERE work_id = NEW.id AND field = 'author' AND value IS NOT NEW.author)
BEGIN
    UPDATE work_field_pin SET source_value = NEW.author
     WHERE work_id = NEW.id AND field = 'author';
    UPDATE work SET author = (SELECT value FROM work_field_pin
                           WHERE work_id = NEW.id AND field = 'author')
     WHERE id = NEW.id;
END;

CREATE TRIGGER work_pin_artist
AFTER UPDATE OF artist ON work
WHEN EXISTS (SELECT 1 FROM work_field_pin
             WHERE work_id = NEW.id AND field = 'artist' AND value IS NOT NEW.artist)
BEGIN
    UPDATE work_field_pin SET source_value = NEW.artist
     WHERE work_id = NEW.id AND field = 'artist';
    UPDATE work SET artist = (SELECT value FROM work_field_pin
                           WHERE work_id = NEW.id AND field = 'artist')
     WHERE id = NEW.id;
END;

CREATE TRIGGER work_pin_year
AFTER UPDATE OF year ON work
WHEN EXISTS (SELECT 1 FROM work_field_pin
             WHERE work_id = NEW.id AND field = 'year' AND CAST(value AS INTEGER) IS NOT NEW.year)
BEGIN
    UPDATE work_field_pin SET source_value = NEW.year
     WHERE work_id = NEW.id AND field = 'year';
    UPDATE work SET year = (SELECT CAST(value AS INTEGER) FROM work_field_pin
                           WHERE work_id = NEW.id AND field = 'year')
     WHERE id = NEW.id;
END;

CREATE TRIGGER work_pin_original_language
AFTER UPDATE OF original_language ON work
WHEN EXISTS (SELECT 1 FROM work_field_pin
             WHERE work_id = NEW.id AND field = 'original_language' AND value IS NOT NEW.original_language)
BEGIN
    UPDATE work_field_pin SET source_value = NEW.original_language
     WHERE work_id = NEW.id AND field = 'original_language';
    UPDATE work SET original_language = (SELECT value FROM work_field_pin
                           WHERE work_id = NEW.id AND field = 'original_language')
     WHERE id = NEW.id;
END;

CREATE TRIGGER work_pin_status
AFTER UPDATE OF status ON work
WHEN EXISTS (SELECT 1 FROM work_field_pin
             WHERE work_id = NEW.id AND field = 'status' AND value IS NOT NEW.status)
BEGIN
    UPDATE work_field_pin SET source_value = NEW.status
     WHERE work_id = NEW.id AND field = 'status';
    UPDATE work SET status = (SELECT value FROM work_field_pin
                           WHERE work_id = NEW.id AND field = 'status')
     WHERE id = NEW.id;
END;

CREATE TRIGGER work_pin_demographic
AFTER UPDATE OF demographic ON work
WHEN EXISTS (SELECT 1 FROM work_field_pin
             WHERE work_id = NEW.id AND field = 'demographic' AND value IS NOT NEW.demographic)
BEGIN
    UPDATE work_field_pin SET source_value = NEW.demographic
     WHERE work_id = NEW.id AND field = 'demographic';
    UPDATE work SET demographic = (SELECT value FROM work_field_pin
                           WHERE work_id = NEW.id AND field = 'demographic')
     WHERE id = NEW.id;
END;

CREATE TRIGGER work_pin_content_rating
AFTER UPDATE OF content_rating ON work
WHEN EXISTS (SELECT 1 FROM work_field_pin
             WHERE work_id = NEW.id AND field = 'content_rating' AND value IS NOT NEW.content_rating)
BEGIN
    UPDATE work_field_pin SET source_value = NEW.content_rating
     WHERE work_id = NEW.id AND field = 'content_rating';
    UPDATE work SET content_rating = (SELECT value FROM work_field_pin
                           WHERE work_id = NEW.id AND field = 'content_rating')
     WHERE id = NEW.id;
END;
