-- Circuit breaker for chronically-failing extension subscriptions.
--
-- `mark_subscription_synced` stamped `last_synced_at = now()` on EVERY pass, success
-- or failure, and recorded the error only in `last_error`. Nothing counted failures
-- and nothing ever escalated, so a permanently-broken subscription was retried on
-- every interval forever with no signal beyond a log line. Observed in production:
--   drakescans -> "flaresolverr: Temporary failure in name resolution" (every pass)
--   suryascans -> "HTTP error 404"                                     (every pass)
--
-- Two columns:
--   consecutive_failures  reset to 0 on any clean pass; incremented otherwise.
--   disabled_at           set when the breaker trips; NULL means active. A disabled
--                         subscription is skipped by the sync loop until an admin
--                         re-enables it, which clears both columns.
--
-- `last_synced_at` keeps its meaning (last pass ATTEMPT) for the admin surface; the
-- decision to skip now reads `disabled_at`, so no existing behaviour depends on the
-- old success-stamping.
ALTER TABLE extension_subscription ADD COLUMN consecutive_failures INTEGER NOT NULL DEFAULT 0;
ALTER TABLE extension_subscription ADD COLUMN disabled_at TEXT;

-- Seed the counter for subscriptions already sitting in a known-failed state, so an
-- existing breakage is one strike from the breaker rather than starting from zero.
UPDATE extension_subscription SET consecutive_failures = 1 WHERE last_error IS NOT NULL;
