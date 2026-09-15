-- Per-source scan rate limits an admin sets from the console (Settings › Sources).
--
-- The same cap `SCAN_SOURCE_RATE_LIMITS` sets from the server environment: at most
-- `requests` HTTP requests to one source per rolling `window_secs`. Until now that
-- variable was the only way in, so protecting one more site meant editing the
-- container environment and restarting the server.
--
-- A row here wins over the variable for the same source: the variable is the server's
-- default, and removing the row restores it (`scanner::merge_source_rate_limits`). The
-- scan supervisor applies a change live, without a restart.
CREATE TABLE source_rate_limit (
    source_id   TEXT PRIMARY KEY,   -- Suwayomi source id (matches source_series.source_id)
    requests    INTEGER NOT NULL CHECK (requests > 0),
    window_secs INTEGER NOT NULL CHECK (window_secs > 0),
    updated_at  TEXT NOT NULL
);
