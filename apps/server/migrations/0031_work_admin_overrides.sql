-- Admin-editable metadata overrides for the series-detail editor. Source-derived
-- work fields (title/description/NSFW) stay immutable; these override columns layer
-- on top at read time (same convention as `series_admin` for scan/status). A NULL
-- override column means "no override — use the derived/source value".
ALTER TABLE work ADD COLUMN title_override       TEXT;
ALTER TABLE work ADD COLUMN description_override TEXT;
ALTER TABLE work ADD COLUMN is_nsfw_override     INTEGER;  -- NULL => derive; 0/1 => forced
-- content_type_override lives in 0030.

-- Admin-curated tag/genre set for a work. When a work has ANY row here, this IS its
-- genre list (a full replace, edited as a whole in the console). When it has none,
-- the reader shape falls back to the genres derived from its linked Suwayomi source.
CREATE TABLE work_tag (
    work_id TEXT NOT NULL REFERENCES work(id) ON DELETE CASCADE,
    tag     TEXT NOT NULL,
    ord     INTEGER NOT NULL DEFAULT 0,       -- preserves the admin's ordering
    PRIMARY KEY (work_id, tag)
);

CREATE INDEX idx_work_tag_work ON work_tag(work_id);
