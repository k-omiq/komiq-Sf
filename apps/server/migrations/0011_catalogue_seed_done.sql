-- Track whether a sync job's initial full `createdAt` seed has completed (M3/M6).
-- While seed_done = 0, `last_synced_at` is a PROVISIONAL createdAt resume point
-- persisted as the seed slides its window, so an interrupted seed resumes near
-- where it stopped instead of restarting from createdAt=0 (M6). Once the seed
-- finishes, seed_done flips to 1 and `last_synced_at` becomes the incremental
-- `updatedAtSince` cursor. The chapters job is gated on the catalogue job's
-- seed_done so old chapters aren't permanently skipped (M3).
ALTER TABLE catalogue_sync_state ADD COLUMN seed_done INTEGER NOT NULL DEFAULT 0;

-- Existing rows were written by the old cursor logic only after a full cycle
-- completed, so they represent a finished seed → incremental from here on.
UPDATE catalogue_sync_state SET seed_done = 1;
