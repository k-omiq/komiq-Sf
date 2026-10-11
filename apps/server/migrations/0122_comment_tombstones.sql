-- Preserve replies when a comment's author or a moderator removes its content.
ALTER TABLE comments ADD COLUMN deleted_by TEXT NULL
    CHECK (deleted_by IN ('author', 'moderator'));
