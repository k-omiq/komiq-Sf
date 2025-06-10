-- Session expiry (A1). Sessions previously had no TTL: a captured token stayed
-- valid forever. Add an absolute `expires_at`; `user_for_token` now requires
-- `expires_at > now`. New sessions get their expiry set in `new_session`
-- (SESSION_TTL_SECS, default 30d), formatted as a fixed-width, lexically
-- sortable `YYYY-MM-DDTHH:MM:SSZ` so the string comparison agrees with time.
ALTER TABLE sessions ADD COLUMN expires_at TEXT;

-- Backfill existing sessions to created_at + 30 days, in the same canonical
-- format used at runtime. strftime returns NULL for any unparseable timestamp;
-- a NULL expires_at fails the `> now` check, so such sessions simply require a
-- fresh login — a safe default for a security fix.
UPDATE sessions
   SET expires_at = strftime('%Y-%m-%dT%H:%M:%SZ', created_at, '+30 days')
 WHERE expires_at IS NULL;

CREATE INDEX idx_sessions_expires ON sessions(expires_at);
