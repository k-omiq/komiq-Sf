-- Keep the baseline history intact; existing PostgreSQL installs upgrade here.
ALTER TABLE comments ADD COLUMN deleted_by TEXT NULL
    CHECK (deleted_by IN ('author', 'moderator'));
