-- Data fix: clear NSFW over-flagging on MangaDex-rated safe/suggestive works.
--
-- `work.is_nsfw` had accumulated false positives from two paths that OR'd a
-- source-level signal onto the work: (1) `rederive_suwayomi_nsfw` / `mark_work_nsfw`
-- flipping a work NSFW because a linked Suwayomi source was flagged adult — but a
-- source flagged NSFW at the SOURCE level (an aggregator hosting adult content)
-- taints every mainstream series it also carries; and (2) stale rows from earlier
-- ingest logic. Measured 2026-07-23: 2,541 works with MangaDex `content_rating` in
-- {safe, suggestive} carried `is_nsfw = 1` — including One Piece, Naruto, Chainsaw
-- Man — hiding them from every anonymous surface (browse, updates, home, search).
--
-- MangaDex's per-title `content_rating` is authoritative and is what the ingest rule
-- (`mangadex.rs` to_work_input) and `genre_is_nsfw` already treat as the source of
-- truth ("suggestive is kept SFW-visible"). So a work MangaDex rates safe/suggestive
-- is not NSFW, regardless of any source-level flag. This resets those rows; the
-- guards added to `mark_work_nsfw` and `rederive_suwayomi_nsfw` stop them recurring.
--
-- Scope: touches only the base `is_nsfw` column, never `is_nsfw_override` — an admin
-- who manually marked a work NSFW keeps that decision (the gate reads
-- COALESCE(is_nsfw_override, is_nsfw)). erotica/pornographic rows are untouched.
-- Verified: 0 erotica/pornographic works are currently under-flagged, and 0 NSFW
-- works have a NULL/other content_rating, so this is purely a false-positive cleanup.
-- The derived `feed_updates` table copies `is_nsfw`; it is rebuilt at boot by
-- `refresh_feed_updates` right after migrations run, so it picks up the corrected
-- values. `work_fts` joins `work` live, so search reflects the fix immediately.
UPDATE work
SET is_nsfw = 0
WHERE is_nsfw = 1
  AND content_rating IN ('safe', 'suggestive');
