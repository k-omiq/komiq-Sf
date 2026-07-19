-- Per-source request health: WHY upstream requests fail, by hour.
--
-- `series_scan_state` (0046/0072) records THAT a scan failed and whether Suwayomi
-- answered from its cache, but never the reason, and it only sees the scanner. The
-- failures a reader actually hits — a chapter whose page list 404s, a page image the
-- source's CDN refuses — were logged and forgotten. So "is this extension blocked, or
-- did the site move, or is it just slow?" had no answer short of grepping the logs.
--
-- One row per (source, surface, hour, reason). `reason` is 'ok' for successes, so a
-- failure count always has its denominator next to it; the failure reasons are the
-- stable identifiers of `source_health::Reason` (blocked, not_found, timeout, …).
-- Hourly buckets keep the table bounded (pruned past 7 days by the GC sweep) and make
-- "last 24h" a short range read instead of a scan over an event log.
--
--   surface   'chapters' = chapter-list fetch (scanner + reader cache miss)
--             'pages'    = a reader opening a chapter (page-list fetch)
--             'image'    = one page image proxied through the origin
--             'browse'   = a source listing (discovery, sync, search, ingest)
--   last_message / last_at   the most recent error text in this bucket, so the admin
--             surface can show what the source actually said.
CREATE TABLE IF NOT EXISTS source_health_bucket (
    source_id    TEXT NOT NULL,
    surface      TEXT NOT NULL,
    hour_ts      INTEGER NOT NULL,
    reason       TEXT NOT NULL,
    count        INTEGER NOT NULL DEFAULT 0,
    last_message TEXT,
    last_at      TEXT NOT NULL,
    PRIMARY KEY (source_id, surface, hour_ts, reason)
) WITHOUT ROWID;

-- The window read and the prune are both ranges on the hour.
CREATE INDEX IF NOT EXISTS idx_source_health_bucket_hour
    ON source_health_bucket (hour_ts);

-- What the extension updater did, per extension. The auto-update loop only ever wrote
-- log lines, so "did the fix for this source ever get installed?" was unanswerable from
-- the admin app. `error` is NULL for a successful update. `origin` is 'auto' (the
-- background pass) or 'manual' (an admin asked for the pass).
CREATE TABLE IF NOT EXISTS extension_update_log (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    at           TEXT NOT NULL,
    pkg_name     TEXT NOT NULL,
    from_version TEXT,
    to_version   TEXT,
    origin       TEXT NOT NULL,
    error        TEXT
);
