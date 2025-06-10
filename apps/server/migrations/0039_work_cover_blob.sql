-- Cache canonical-work cover images as WebP BLOBs in SQLite, served from the VPS
-- (`/covers/{work_id}.webp`), so the web reader loads covers from OUR OWN origin
-- instead of routing every cover through the Cloudflare image Worker.
--
-- Rationale: the DB is already Litestream-replicated to R2 (backup + restore-on-
-- boot), so BLOB-in-SQLite inherits that backup for free AND lets any replica
-- serve any cover (mirrors the `user_avatars` posture, 0016). Covers are bounded
-- WebP thumbnails (see `cover::MAX_COVER_BYTES`), so the DB-size cost stays modest
-- even across the whole catalogue.
--
-- `work.cover_cached_version` is the presence + cache-bust signal, and it rides on
-- the `work` row already loaded on every hot path, so resolvers build the VPS URL
-- with NO extra query (no N+1):
--   * NULL              → no cached blob; serve the Worker-proxied MangaDex URL.
--   * <integer version> → `/covers/{id}.webp?v=<version>` is available; the `?v`
--                         busts the browser/edge cache when the cover is re-saved.
CREATE TABLE work_cover_blob (
    work_id    TEXT PRIMARY KEY REFERENCES work(id) ON DELETE CASCADE,
    webp       BLOB NOT NULL,
    version    INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);

ALTER TABLE work ADD COLUMN cover_cached_version INTEGER;
