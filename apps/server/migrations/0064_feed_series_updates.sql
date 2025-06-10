-- The reader's merged "Updates" feed, materialized as one row per canonical work.
--
-- WHY THIS TABLE EXISTS
--
-- `/updates` showed the union of TWO server feeds: `canonicalUpdates` (the MangaDex
-- mirror, served from `feed_updates`, migration 0051) and `updates` (our scanner's
-- new-chapter detections on Suwayomi library series). The reader fetched page 1 of
-- each, concatenated them, sorted the union by release time, deduped by LOWERCASED
-- TITLE and sliced the first 60. That cannot be paginated, for three independent
-- reasons:
--
--   1. A page of the merged list is not page N of either feed. Interleaving two DESC
--      streams means merged row 40 might be mirror row 37 and scanner row 3, so any
--      "ask each feed for page N and splice" scheme either SKIPS rows at a page
--      boundary or emits one on both pages. The second is not cosmetic: the grid keys
--      its {#each} on the row id, and Svelte 5 throws `each_key_duplicate` in
--      PRODUCTION, so a boundary bug kills the page rather than showing a dupe.
--   2. Dedupe-after-paging punches holes. The title dedupe is documented as removing
--      the COMMON case, not a rare one, so paging first and deduping second yields
--      pages of 20, 17, 20, 14 — and then `total` / `hasNextPage`, which the pager
--      renders as "Page 3 of 812 · showing 41-60 of 16,231", are simply wrong.
--   3. `total` over a union is not the total of either half.
--
-- Taking the union ONCE, here, keyed by canonical work, fixes all three: the resolver
-- becomes an indexed range scan of 20 rows, the dedupe is by work IDENTITY (strictly
-- more correct than a lowercased title), and page fullness / `total` are honest.
--
-- Read-time `UNION ALL` was rejected for the same reason 0058 and 0063 exist: SQLite
-- cannot merge two indexed streams to satisfy an ORDER BY, so it materializes a temp
-- B-tree over both halves on every request.
--
-- PURE CACHE. Every column is derivable from work / source_series / chapter /
-- suwayomi_series / series_scan_state, so this table is rebuildable at any time and is
-- rebuilt wholesale by `catalog::refresh_feed_updates` (which now refreshes both feed
-- tables under one call, so the existing boot + post-sync call sites cover it).
CREATE TABLE IF NOT EXISTS feed_series_updates (
    work_id              TEXT PRIMARY KEY REFERENCES work(id) ON DELETE CASCADE,

    -- The id the reader NAVIGATES to, which is not always `work_id`.
    --
    -- `canonicalSeries` rejects a work with no MangaDex anchor outright
    -- (graphql::canonical_series: `if work.mangadex_id.is_none() { return Err(...) }`),
    -- so handing the reader a bare `w_…` for a Suwayomi-only work produces a card that
    -- 404s on click. Mirror-half rows therefore carry their `w_…` id (they are
    -- MangaDex-anchored by construction — `refresh_feed_updates` joins
    -- `source_series WHERE source_type = 'mangadex'`), and scanner-half rows carry
    -- their numeric Suwayomi series id. That is exactly the split the two feeds have
    -- today; the merge does not change any card's destination.
    reader_id            TEXT NOT NULL,

    title                TEXT NOT NULL,

    -- A READY-TO-USE, origin-relative cover path (`/covers/{work_id}.webp[?v=N]`),
    -- exactly as `cover::work_cover_url` builds it. NULL when the work has no cached
    -- blob and no MangaDex cover file name.
    cover_url            TEXT,
    -- FALLBACK for Suwayomi-only works, which have no `/covers/` entry: the RAW
    -- Suwayomi thumbnail reference. It is stored raw and NOT resolved here because
    -- absolutizing it needs `SuwayomiClient::image_base_url`, which is RUNTIME CONFIG,
    -- not data — baking a host into a cache table would survive a config change and
    -- serve dead image URLs. The resolver runs it through `SuwayomiClient::abs`, the
    -- same call `map_series` makes. Two columns rather than one overloaded one so
    -- neither has two meanings.
    suwayomi_thumbnail   TEXT,

    -- The effective format, materialized. `resolve_comic_type` is a READ-TIME
    -- derivation (override -> original_language -> genre tags -> title script ->
    -- Manga) over data with no single column to filter on, so without this column the
    -- reader's format tabs could only filter the 20 rows of the current page — the
    -- same dishonesty the page-1 cap had. Stored as the enum WORD ('MANGA' | 'MANHWA'
    -- | 'MANHUA'), COLLAPSED the way every reader surface collapses it:
    -- WEBTOON -> MANHWA and COMIC -> MANGA (see the reader's `toViewType`). Collapsing
    -- here rather than at read time keeps the resolver's filter a single equality, so
    -- one index serves filter AND order. It is a no-op on today's data: those two
    -- variants are only reachable through `work.content_type_override`, which is NULL
    -- for all 111,161 works in production.
    comic_type           TEXT,

    -- Chapter NUMBER of the newest mirrored chapter (mirror half only, e.g. "10.5"),
    -- and the series' total chapter count (scanner half only). The two halves label
    -- their cards from different fields today and both are kept so the merge changes
    -- no card's text: the reader renders `Ch. {latest_chapter ?? chapter_count}`.
    latest_chapter       TEXT,
    latest_chapter_title TEXT,
    chapter_count        INTEGER,

    -- THE shared sort key: the newest REAL UPSTREAM RELEASE time across the work's
    -- MangaDex mirror and its Suwayomi sources — the same clock the card LABELS with.
    --
    -- EPOCH MILLISECONDS, deliberately, because the two source columns are stored in
    -- INCOMPATIBLE TEXT encodings that cannot be compared or MAX()ed:
    -- `feed_updates.latest_at` is ISO-8601 ('2026-07-26T13:46:51+00:00') and
    -- `suwayomi_series.latest_chapter_at` is 13-digit epoch-millis TEXT
    -- ('1785071625000'). Under BINARY collation every '2…' ISO string sorts above
    -- every '1…' millis string, so a TEXT column here would have put the entire mirror
    -- half above the entire scanner half and called it chronological — the exact class
    -- of bug 0063 was written to fix, reintroduced by the merge. graphql::updates
    -- documents the same mismatch as the reason the two clocks cannot be COALESCEd.
    --
    -- NOT NULL: a work with no dated chapter is not an "update", so it is EXCLUDED from
    -- the feed rather than sorted last. Two reasons to decide it here rather than at
    -- read time: (a) the pager's arithmetic (`total`, `hasNextPage`, "showing 41-60 of
    -- N") is only honest if every counted row is also a placeable row, and (b) it
    -- costs nothing today — 0 of the 1,316 current scanner-feed members and 0 of the
    -- 47,515 mirror members have a NULL release time. NULL is a real state for the
    -- 2,312 in-library Suwayomi series with no `latest_chapter_at`, but none of them
    -- have a detection either, so none of them are feed members.
    released_at          INTEGER NOT NULL,

    -- When OUR scanner noticed (ISO-8601, scanner half only). Tooltip only, NEVER a
    -- sort key — sorting by detection while labelling with release time is precisely
    -- the bug 0063's header describes.
    detected_at          TEXT,

    -- The EFFECTIVE flag `COALESCE(is_nsfw_override, is_nsfw)`, not the raw column:
    -- both admin "mark NSFW"/"mark SFW" mutations write ONLY the override, so copying
    -- `work.is_nsfw` would make this feed the one surface that ignores every manual
    -- reclassification. Same choice `feed_updates` had to be corrected into.
    is_nsfw              INTEGER NOT NULL DEFAULT 0
);

-- FOUR indexes, because the resolver has FOUR shapes: {NSFW hidden, NSFW shown} x
-- {all formats, one format}.
--
-- The resolver BRANCHES its SQL on the viewer's NSFW preference instead of writing
-- `WHERE (? = 1 OR is_nsfw = 0)`: that OR is opaque to the planner, which then cannot
-- tell `is_nsfw` is constant and falls back to sorting the whole table through a temp
-- B-tree. `canonical_updates` documents the same thing. The `type` filter is branched
-- for the same reason — as an equality it becomes an index prefix, so one index serves
-- filter AND order.
--
-- The two `is_nsfw`-leading indexes serve the ANONYMOUS branch (`WHERE is_nsfw = 0`),
-- which pins the leading column to a constant and leaves the rest of the index in
-- exactly ORDER BY order. That is the hot, edge-cacheable path, and it is the same
-- shape as idx_feed_updates_order (0051).
--
-- The two WITHOUT `is_nsfw` serve the OPTED-IN branch, which has no `is_nsfw`
-- predicate at all. Measured on a copy of production (48,409 rows), the opted-in
-- branch on the `is_nsfw`-leading index alone plans as
-- `SCAN … USING COVERING INDEX idx_fsu_order` + `USE TEMP B-TREE FOR ORDER BY`, i.e.
-- it sorts all 48k rows to return 20 — precisely what 0051/0058/0063 exist to prevent.
-- 0051 had to add idx_feed_updates_latest to `feed_updates` for exactly this reason;
-- idx_fsu_all_order is its counterpart here, and idx_fsu_type_all_order does the same
-- job for the opted-in branch of the format filter, which `feed_updates` has no
-- equivalent of. The cost is ~5 MiB of extra index.
--
-- `work_id DESC` is the tiebreaker in all four, matching `canonical_updates`
-- (`latest_at DESC, work_id DESC`) so the merged order is a refinement of the mirror
-- half's. A total order is mandatory, not cosmetic: without one, LIMIT/OFFSET can
-- repeat or skip rows between pages wherever two rows share a `released_at` — and 34
-- groups of Suwayomi rows share one release timestamp in production today.
CREATE INDEX IF NOT EXISTS idx_fsu_order
    ON feed_series_updates(is_nsfw, released_at DESC, work_id DESC);
CREATE INDEX IF NOT EXISTS idx_fsu_type_order
    ON feed_series_updates(is_nsfw, comic_type, released_at DESC, work_id DESC);
CREATE INDEX IF NOT EXISTS idx_fsu_all_order
    ON feed_series_updates(released_at DESC, work_id DESC);
CREATE INDEX IF NOT EXISTS idx_fsu_type_all_order
    ON feed_series_updates(comic_type, released_at DESC, work_id DESC);
