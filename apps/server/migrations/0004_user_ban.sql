-- Moderation: let an admin suspend (ban) a user account. A banned user cannot
-- authenticate — login is refused and their existing session tokens stop
-- resolving (see auth::user_for_token) — but their identity and content are
-- preserved. Deleting content (e.g. a comment) is a separate admin action.

ALTER TABLE users ADD COLUMN is_banned INTEGER NOT NULL DEFAULT 0;
