-- Store user avatars as BLOBs in SQLite rather than on the VM data volume.
--
-- Rationale: the SQLite DB is already Litestream-replicated to R2 (continuous
-- backup + restore-on-boot) and is the single source of truth. Keeping avatars
-- here inherits that backup for free AND lets any server replica serve any
-- avatar (a local /data file could only be served by the replica that stored
-- it). Avatars are small (≤70 KB, one per user), so the DB-size cost is modest.
--
-- The processed lossless-WebP bytes live here; `users.avatar_url` still holds the
-- public path (`/avatars/<user_id>.webp?v=<version>`) the reader renders, and the
-- `/avatars/{file}` route reads the bytes from this table.
CREATE TABLE user_avatars (
    user_id    TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    webp       BLOB NOT NULL,
    version    INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);
