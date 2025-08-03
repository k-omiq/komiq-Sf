-- Enforce case-insensitive identity uniqueness for future writes without
-- merging, deleting or renaming pre-existing accounts with case collisions.
-- SQLite serializes writes, so this guard closes the application pre-check race.
CREATE TRIGGER users_username_nocase_insert
BEFORE INSERT ON users
WHEN EXISTS (SELECT 1 FROM users WHERE username = NEW.username COLLATE NOCASE)
BEGIN
    SELECT RAISE(ABORT, 'UNIQUE constraint failed: users.username');
END;

CREATE TRIGGER users_username_nocase_update
BEFORE UPDATE OF username ON users
WHEN NEW.username != OLD.username
 AND EXISTS (SELECT 1 FROM users WHERE username = NEW.username COLLATE NOCASE AND id != OLD.id)
BEGIN
    SELECT RAISE(ABORT, 'UNIQUE constraint failed: users.username');
END;
