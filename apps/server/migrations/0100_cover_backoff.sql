-- Cover retry backoff: stop the warmer/crawl re-fetching covers that keep failing.
--
-- Measured on production 2026-08-11: the Suwayomi cover warmer reported
-- `saved=0 failed=92` on EVERY 5-minute tick — 92 covers that can never succeed as
-- things stand, re-fetched forever (~26k outbound fetches/day, ~8k of them at a single
-- source whose WAF then banned us). The failures split three ways:
--   * deterministic  — the extension stored a `data:` thumbnail (Suwayomi answers 400
--     "Expected URL scheme 'http' or 'https' but was 'data'"), an over-cap GIF, an AVIF
--     we have no decoder for. Re-fetching is pure waste.
--   * source-side    — 403 (Cloudflare WAF) / 429 / 5xx. These MAY lapse, so they must
--     back off, not be abandoned.
--   * per-source     — 28 of the 92 were one host. Even a per-cover backoff still means
--     28 requests per tick at a host that is actively banning us.
--
-- `work_cover_issue` (0043) is the existing skip-marker and its SHAPE is reused here,
-- but it cannot carry these rows: it is keyed by `work_id` with an FK to `work`, and
-- 10,494 of 16,646 `suwayomi_series` have no work at all (Browse/unenrolled). Hence a
-- sibling table keyed by the Suwayomi manga id, plus the per-source table, plus a
-- `retry_after` on `work_cover_issue` itself so its markers expire rather than
-- excluding a work from the crawl for ever.
--
-- SELF-HEALING (nothing here is a permanent tombstone):
--   * a successful fetch DELETEs the marker;
--   * a marker is ignored once `retry_after` has passed (permanent failures get a long
--     window, transient ones an exponential 30 min .. 24 h window);
--   * a Suwayomi marker is also ignored as soon as the series' `last_fetched_at`
--     differs from the value recorded with it — i.e. the extension re-fetched the manga
--     and the stored thumbnail may now be a real URL.

-- `work_cover_issue` markers written before this migration meant "never retry". Give
-- them an expiry too, so a decoder/cap improvement (AVIF support, a raised cap) is
-- picked up without an operator DELETE. NULL keeps the old meaning for any writer that
-- does not set it.
ALTER TABLE work_cover_issue ADD COLUMN retry_after TEXT;
UPDATE work_cover_issue
   SET retry_after = strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '+30 days')
 WHERE retry_after IS NULL;

-- Per-cover marker for the `suwayomi_cover_blob` warmer (keyed by Suwayomi manga id,
-- which is what that cache and `/api/v1/manga/{id}/thumbnail` are keyed by).
CREATE TABLE IF NOT EXISTS suwayomi_cover_issue (
    manga_id   INTEGER PRIMARY KEY REFERENCES suwayomi_series(id) ON DELETE CASCADE,
    -- Denormalised so the per-source aggregate never has to join back.
    source_id  TEXT,
    -- Machine code: 'data_uri' | 'not_found' | 'bad_request' | 'unsupported_media' |
    -- 'too_large' | 'unsupported' | 'empty' | 'encode' | 'store' (permanent) and
    -- 'blocked' | 'rate_limited' | 'upstream_5xx' | 'truncated' | 'fetch' (transient).
    reason     TEXT NOT NULL,
    detail     TEXT,
    -- 1 = deterministic given the same input, 0 = may lapse on its own.
    permanent  INTEGER NOT NULL DEFAULT 0,
    attempts   INTEGER NOT NULL DEFAULT 1,
    first_seen TEXT NOT NULL,
    last_seen  TEXT NOT NULL,
    -- The marker is only honoured until this instant (RFC3339, UTC).
    retry_after TEXT NOT NULL,
    -- `suwayomi_series.last_fetched_at` when the failure was recorded. A different
    -- value means the extension re-fetched the manga, so the marker is void.
    source_fetched_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_suwayomi_cover_issue_retry
    ON suwayomi_cover_issue (retry_after);
CREATE INDEX IF NOT EXISTS idx_suwayomi_cover_issue_last_seen
    ON suwayomi_cover_issue (last_seen DESC);

-- Per-SOURCE park. One row per Suwayomi source id that failed transiently for a whole
-- tick; while `retry_after` is in the future NONE of that source's covers are candidates
-- — including ones with no per-cover marker of their own, which is the point (a WAF ban
-- is a property of the host, not of any one cover).
CREATE TABLE IF NOT EXISTS cover_source_backoff (
    source_id   TEXT PRIMARY KEY,
    reason      TEXT NOT NULL,
    detail      TEXT,
    -- Consecutive parks; drives the exponential window. Reset to 0 by any success.
    failures    INTEGER NOT NULL DEFAULT 1,
    -- Covers that failed in the tick that (re)parked the source — the "is this a real
    -- regression?" signal for the admin Bugs panel.
    failed_covers INTEGER NOT NULL DEFAULT 0,
    first_seen  TEXT NOT NULL,
    last_seen   TEXT NOT NULL,
    retry_after TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_cover_source_backoff_retry
    ON cover_source_backoff (retry_after);
