-- Re-materialize the covers that were stored from a TRUNCATED source download.
--
-- WHY THESE ROWS EXIST
-- `image` 0.25 / `zune-jpeg` returns `Ok` for a JPEG whose body was cut off mid-scan:
-- it decodes to a correct-size image whose missing rows carry the decoder's filler
-- value. `suwayomi::cover_bytes` had no integrity check, so a well-formed HTTP 200
-- carrying an incomplete image was accepted, re-encoded by `process_cover`, written to
-- `work_cover_blob`, and pinned by `work.cover_cached_version` behind a
-- `max-age=31536000, immutable` edge TTL. Nothing downstream could ever tell the
-- filler apart from real art, so these covers were frozen permanently.
--
-- The gate that stops NEW ones (`avatar::ensure_complete`, an end-of-stream marker
-- check applied in `decode_limited`, `process_cover` and both Suwayomi fetch paths)
-- ships in the same change set. This migration repairs the ones already stored.
--
-- HOW THE SET WAS IDENTIFIED (conservative — precision over recall)
-- Every one of the 111,806 blobs in `covers.sqlite3` was decoded and scanned for a
-- perfectly flat band of rows at the bottom (per-row max-min <= 2 per channel).
-- 3,475 blobs have such a band >= 2% of their height — overwhelmingly legitimate white
-- or black margins in cover art. Only two fill COLOURS indicate a decoder, not an
-- artist:
--
--   (0,135,0)     -- RGB of a YCbCr all-zero fill (what zune-jpeg leaves behind)
--   (128,128,128) -- neutral-DC fill, +/-6 for lossy-WebP re-encode drift
--
-- Those colours were reproduced experimentally by truncating a real cover at ten
-- points and re-encoding, and they sit in a clean gap: the nearest non-matching greens
-- and grays in the corpus are (2,163,5) and (142,142,142). 12 blobs match. Two more
-- are flat over 100% of their height — an image carrying literally no information,
-- unusable as a cover whatever produced it — bringing the set to 14, of which 13 are
-- still live `work` rows.
--
-- Deliberately NOT included: the ~40 blobs with a flat gray band at 114/142/146/152/160
-- and the 3,461 with white/black margins. Those are plausibly real art, and a false
-- positive costs a needless refetch of a perfectly good cover.
--
-- WHAT THIS DOES
-- Nulls `cover_cached_version`, which (a) puts the work back into the cover drainer's
-- `WHERE cover_cached_version IS NULL` selection so the blob is re-fetched and
-- overwritten, and (b) stops the resolver emitting `/covers/{id}.webp?v=<old>`, so the
-- immutable edge entry for the old versioned URL is bypassed rather than waited out.
-- Re-materialization mints a fresh `?v=` and the cycle closes.
--
-- TRANSIENT COST, be aware of it: what (b) emits INSTEAD depends on the work
-- (`cover::work_cover_url`). Only a work with BOTH a MangaDex source and a
-- `cover_file_name` falls back to the unversioned `/covers/{id}.webp`; a Suwayomi-only
-- work gets an EMPTY cover URL and shows the reader's placeholder until the drainer
-- re-materializes it. 7 of the 13 live works here are Suwayomi-only, so 7 covers go
-- blank for up to one drainer tick (~300 s) after deploy, rather than showing the
-- corrupt image a moment longer. `cover::pending_cover_count` counts these works, so
-- the tick is guaranteed to run.
--
-- Ids that are no longer live works (one has since been merged away) simply match
-- nothing — the statement is a no-op for them and the migration is idempotent.
--
-- SCOPE NOTE: this repairs 14 covers, NOT the 88,852 blobs that merely exceed the
-- current 150 KB budget. Those are intact images written under the old 200 KB lossless
-- budget; re-materializing them means ~71 GB of source refetch traffic and is a
-- separate, throttled, owner-approved campaign — not something to run during a latency
-- incident, and explicitly not done here.

UPDATE work
SET cover_cached_version = NULL
WHERE id IN (
    -- flat tail in a decoder-fill colour
    'w_5feb140bfeb944f7ad5af6c116c5ba0e',  -- 100% flat, (132,132,132)
    'w_5a111868ffca4dae91e45edc837339d3',  --  85% flat, (132,132,132)
    'w_77e7ec0f929b4808bb2d4ca1d232bab2',  --  84% flat, (133,130,134)
    'w_f36971b5c7ad45dbbd2d9504dc2a08d4',  --  74% flat, (128,128,128)  [not a live work]
    'w_2ecea57fa6da429b8c04239c00a36514',  --  72% flat, (128,128,128)
    'w_3c3ccd7c8df14b5886be8ccb6dea529b',  --  42% flat, (0,135,0)
    'w_7a8c6527419d4eb488011898be64781e',  --  38% flat, (133,130,134)
    'w_bf6775c23c5b4c7d97c8976b9e9922e7',  --  37% flat, (0,135,0)
    'w_2b48dd59909948d38bab1645b58f35f7',  --  21% flat, (128,128,128)
    'w_c9e2b4d0c914404a8196bcbb1d4fc80c',  --  20% flat, (128,128,128)
    'w_2532c20254f04e0db3aa9afc3ab33c3a',  --   6% flat, (0,135,0)
    'w_6985db482fa24bef91158e69791df64a',  --   3% flat, (0,135,0)
    -- carries no information at all: flat over the entire image
    'w_65a1ed8a47224ddab86cebfffb0659cd',  -- 100% flat, (150,119,137)
    'w_551a78c9c36f4e6e83fdf509957355f5'   -- 100% flat, (0,0,0)
);

-- Defensive: a recorded cover issue permanently excludes a work from every crawl
-- SELECT, so it would silently defeat the reset above. None of these 14 has one today;
-- this keeps that true if one is recorded between now and the deploy.
DELETE FROM work_cover_issue
WHERE work_id IN (
    'w_5feb140bfeb944f7ad5af6c116c5ba0e',
    'w_5a111868ffca4dae91e45edc837339d3',
    'w_77e7ec0f929b4808bb2d4ca1d232bab2',
    'w_f36971b5c7ad45dbbd2d9504dc2a08d4',
    'w_2ecea57fa6da429b8c04239c00a36514',
    'w_3c3ccd7c8df14b5886be8ccb6dea529b',
    'w_7a8c6527419d4eb488011898be64781e',
    'w_bf6775c23c5b4c7d97c8976b9e9922e7',
    'w_2b48dd59909948d38bab1645b58f35f7',
    'w_c9e2b4d0c914404a8196bcbb1d4fc80c',
    'w_2532c20254f04e0db3aa9afc3ab33c3a',
    'w_6985db482fa24bef91158e69791df64a',
    'w_65a1ed8a47224ddab86cebfffb0659cd',
    'w_551a78c9c36f4e6e83fdf509957355f5'
);
