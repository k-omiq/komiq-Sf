-- Preserve upstream taxonomy and a current tracker snapshot separately from
-- admin tags and historical cross-catalogue IDs used for identity matching.
ALTER TABLE work_tag ADD COLUMN tag_id TEXT;
ALTER TABLE work_tag ADD COLUMN tag_group TEXT;
ALTER TABLE work ADD COLUMN mangadex_details_synced_at TEXT;
CREATE TABLE work_tracker_link (
    work_id TEXT NOT NULL REFERENCES work(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    external_id TEXT NOT NULL,
    PRIMARY KEY (work_id, provider)
);
