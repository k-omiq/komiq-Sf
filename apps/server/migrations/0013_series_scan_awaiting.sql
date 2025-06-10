-- Track when a series entered the "awaiting an overdue chapter" state, so the
-- accelerated poll cadence (SC1) is bounded rather than indefinite.
--
-- Without a start marker the scanner would re-poll a genuinely stalled (but still
-- ONGOING, so not auto-paused) series at poll_every_minutes forever — and a series
-- whose inferred interval underestimates its true cadence would poll aggressively
-- for the whole (long) gap until the real chapter lands. awaiting_since lets the
-- scanner poll fast only for a bounded window past the due time, then fall back to
-- the steady cadence. NULL = not awaiting (a chapter is on schedule).
ALTER TABLE series_scan_state ADD COLUMN awaiting_since TEXT;  -- ISO 8601, when the current overdue-awaiting streak began; NULL = not awaiting
