-- Self-host setup (phase 1): instance settings + forced password change.
--
-- `instance_setting` holds the settings an admin can change at runtime (instance
-- name, sign-up switch, …). An env variable always wins over a row here, and a
-- missing row falls back to a built-in default; see `instance::Setting`. Keys
-- are a fixed allow-list in code, never free-form user input.
CREATE TABLE instance_setting (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Set when an admin creates an account or resets its password, cleared when
-- the user picks their own via `changePassword`. Exposed on the session user so
-- the clients can ask for a new password before anything else.
ALTER TABLE users ADD COLUMN must_change_password INTEGER NOT NULL DEFAULT 0;
