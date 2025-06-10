# Deploy note — follow-up session, 2026-07-26

Four workstreams. **Nothing in this change set has been deployed or exercised against the live
API** — the session's constraints forbade rebuilding or restarting any container, so every result
below is a unit test, or a measurement of the shipping SQL against a _copy_ of production. No
number here is a projection presented as a measurement.

## Verified state at hand-off

| check                                 | baseline at session start         | now                                   |
| ------------------------------------- | --------------------------------- | ------------------------------------- |
| `cargo test --bin komika-server -j 2` | 300 passed / 0 failed / 2 ignored | **353 passed / 0 failed / 2 ignored** |
| `cargo fmt --check`                   | dirty in places                   | **clean**                             |
| reader `svelte-check`                 | 399 files / 0 errors / 1 warning  | **401 files / 0 errors / 1 warning**  |
| admin `svelte-check`                  | 303 files / 0 errors / 0 warnings | **303 / 0 / 0** (untouched)           |

An adversarial review pass then ran over everything above — six reviewers, five with fix
authority over disjoint file sets, one read-only on cross-cutting contracts. It found **four
critical/high defects in the same day's work**, listed under "Review pass" below. Treat the
numbers in this table as post-review.

The one reader warning is the pre-existing `Cover.svelte:26`. Confirmed benign: the `$effect` at
`:66` re-resolves on every `src` change on both the web and native branches, so there is no
wrong-cover-in-a-recycled-list risk.

## New migrations — 0063, 0064, 0065

All three are new and unapplied in production. Migrations run in `db::init` (`main.rs:1100`)
**before the server binds**, so ordering is safe — but the binary must not ship ahead of them.

- **0063** `updates_release_order_indices` — 3 covering indexes + per-table `ANALYZE`. ~14.9 MiB,
  ~4 s to apply. **The binary hard-depends on one of these via `INDEXED BY`**; without it the
  planner picks the PK autoindex and the Updates feed degrades badly. It fails loudly rather than
  silently if dropped.
- **0064** `feed_series_updates` — new materialized table backing the paginated Updates feed.
  ~21.8 MiB (10.8 table + 11.0 index), ~48,409 rows, builds in ~3.6 s.
- **0065** `reset_truncated_covers` — nulls `cover_cached_version` so the drainer re-materializes.
  It lists **14** ids, of which **13 are live works** (the 14th is annotated in the SQL as not a
  live work). Note **7 of the 13 are Suwayomi-only**: for those, `work_cover_url` returns an empty
  URL until re-materialized, so the cover goes to the reader's placeholder rather than to an
  unversioned URL. Self-healing — `pending_cover_count` counts them, so a drainer tick is
  guaranteed. Tiny, idempotent, safe to re-run.

## Deploy order

1. **Server first** (rebuild + restart). The reader's Updates page calls a new `updatesFeed`
   resolver that does not exist in the running binary.
2. **Reader second** (`wrangler`).
3. Admin is untouched.

Browse pagination is reader-only with no server dependency and could ship independently if you
want to decouple. The Updates reader changes cannot.

Restarting the server interrupts ingest — expected, and the backfill is resumable by design.

### Confirmed against the running binary, 2026-07-27

Server-first is **not** optional, and the blast radius is wider than `updatesFeed`. Probing the
deployed server directly shows it also rejects the Browse arguments the reader already ships:

```
{"errors":[{"message":"Unknown type \"BrowseSort\""},
           {"message":"Unknown argument \"sort\" on field \"search\" of type \"QueryRoot\""},
           {"message":"Unknown argument \"hasChapters\" on field \"search\" of type \"QueryRoot\""}]}
```

`types`, `status` and `contentRating` are rejected too. The live surface is only
`query`, `page`, `genres`, `includeNsfw`.

This matters more than a missing resolver: an unknown argument fails GraphQL **validation**
and takes the whole document with it, and — per the `SEARCH` comment in
`packages/api/src/operations.ts` — Browse has **no fallback path**. Shipping the reader first
does not degrade Browse, it **breaks it for everyone**.

(Surfaced while validating load-test queries against the live schema; see
`docs/plans/2026-07-27-performance-investigation.md` §4.6.)

---

## A — covers: many not loading, some rendering "half broken"

**Both hypotheses in the original brief were wrong, and were killed with evidence.**

- **The GC sweep is safe. Keep it running.** 0 live works are missing a blob; 0 of the 1,718
  remaining orphans is referenced by a live work; backlog draining 8,868 → 1,718 as designed.
- **The merge-survivor dangling-pointer state does not exist**: 0 rows globally, not just among the
  1,627 merges. All 1,612 survivors already had their own `cover_cached_version` and blob.

**Real cause.** The home/discovery feeds request `/api/v1/manga/{id}/thumbnail`, served from
`suwayomi_cover_blob` — a cache **nothing warmed**. The drainer's Suwayomi pass keyed on
`work.cover_cached_version IS NULL` and wrote `work_cover_blob` under the _work_ id; the two never
met, so it selected ~1 row while coverage sat at 9%. Every cold request was an unbounded
synchronous fetch to the source CDN.

**Why it became visible on 2026-07-26** (the timing story the original brief lacked): a scan-rate
step change. Migration 0057 plus the new `ACTIVE_MAX_INTERVAL_HOURS = 12.0` took Suwayomi traffic
from ~11/hr to ~1,273/hr (83 scans pre-deploy vs 8,402 post), consuming the per-source budget the
cover route shares. Steady state settles near ~475/hr as the one-time 0057 catch-up drains.

**Deliberately NOT done:** cutting the scan cadence. That would undo the fix for the original
late-chapters complaint (discovery lag p50 ≈ 29h). `ACTIVE_MAX_INTERVAL_HOURS` (12.0) and
`SCAN_CONCURRENCY` (3) are unchanged. Covers were taken off the contended path instead.

**Shipped:** a `suwayomi_cover_blob` warmer (500/tick, newest-first, ~2h to cover the ~12.1k
backlog, no new config knob); a separate 8s cover HTTP client with a 12-permit semaphore where the
background warmer may hold at most 3, so on-demand requests always have headroom; an instant
transparent placeholder (`no-store`) on contention instead of a 12–15s wait; a JPEG/WebP/PNG
terminator check so truncated sources are rejected rather than frozen into the cache.

**"Half broken" — the premise did not hold.** Byte-level corruption is real but affects **~20–30
covers store-wide (~0.02%)**, all stamped 2026-07-20/21 (the seed crawl, not this deploy). Root
cause is `zune-jpeg` returning `Ok` for truncated data. Ruled out with evidence: `read_capped` and
`MAX_SOURCE_BYTES` both **reject** rather than truncate; blob writes are atomic under WAL; Range
handling is byte-exact; 348 HTTP fetches including a 30-way burst were byte-identical to the DB; 0
malformed containers in 113,656; 0 decode failures in 112,808. What users are seeing is the
page-level effect of slow, oversized covers on a contended path.

**Also fixed, and arguably the most valuable part:** transient truncations were being written to
`work_cover_issue`, which excludes a work from _every_ crawl SELECT forever — turning one flaky CDN
response into a permanently coverless work. Truncation is now classified transient and never
recorded.

**Watch after deploy:** `suwayomi cover warmer: complete` every 5 min, coverage climbing ~500/tick.
`suwayomi cover: pool saturated, 503 warming` at info is expected in the first hour and should
approach zero. A sustained stream after that means demand is outrunning the cache and
`COVER_FETCH_CONCURRENCY` needs raising — note that is a **compile-time `const` in
`suwayomi.rs`, not an env var**, so changing it needs a rebuild and restart.

**Regression looks like:** coverage flat (warmer not running — check `COVER_CACHE` isn't `off`); any
new `truncated` rows in the admin Bugs panel (should be impossible now — such a row is a
classification bug); covers rendering blank rather than as a grey placeholder card (the `no-store`
placeholder is being cached — check the edge); or 502s on `/api/v1/manga/*/thumbnail` rising, which
means the 8s cover timeout is too tight (measured max at cap-concurrency was 6.41s — real headroom,
but thin).

**Confirm coverage (expect >95% within ~2h):**

```sh
sudo python3 -c "
import sqlite3
d='/var/lib/docker/volumes/komika_server-data/_data/'
m=sqlite3.connect('file:'+d+'komika.sqlite3?mode=ro',uri=True)
c=sqlite3.connect('file:'+d+'covers.sqlite3?mode=ro',uri=True)
t=m.execute(\"SELECT COUNT(*) FROM suwayomi_series WHERE thumbnail_url IS NOT NULL AND thumbnail_url<>''\").fetchone()[0]
k=c.execute('SELECT COUNT(*) FROM suwayomi_cover_blob').fetchone()[0]
print('coverage: %d/%d = %.1f%%'%(k,t,k*100.0/t))"
```

Baseline at hand-off: **1,961/13,847 = 14.2%** (up from 9.0% purely from organic traffic).

---

## B — Updates feed ordered by detection time, labelled with release time

Confirmed exactly as reported. Live page 1 had a chapter released 2026-01-23 sitting at **position
6** displaying "184d".

`canonical_updates` was **already correct** and is unchanged. The defect was feed 1's sort key
_plus_ a client-side plain concatenation (not a merge) that made the list descend through release
times and then jump back to "now" at card 21.

**Shipped:** the `updates` resolver now drives from `suwayomi_series`,
`ORDER BY latest_chapter_at DESC NULLS LAST, id DESC`. `EXPLAIN QUERY PLAN` shows **no temp
B-tree** on either NSFW branch (the old query had one). Cold page 64: **13.5 ms**. Client gains
`Card.timeAt` (epoch-ms) and `mergeByRecency`, used by both `getUpdates` and `getHome`.

NULL handling is explicit `NULLS LAST` — not `COALESCE`, which would defeat the index, compare two
incompatible encodings, and reintroduce the original bug. 0 of 1,316 current feed members have a
NULL release time.

`detectedAt` is preserved and still rendered as the tooltip.

---

## C — real pagination on Browse and Updates

**Browse** needed no server change (`search(page:)` already existed). URL-driven `?page=`, omitted
at page 1. Back-nav returns to the correct page and scroll position. All incremental-loading
machinery removed — `loadMore`, `loadMoreError`, `loadingMore`, `rowsGen`, `catalogPage` and their
CSS. Verified zero remaining references.

**Updates** is backed by the new materialized `feed_series_updates` (0064), interleaving both feeds
on one clock. Measured on a production copy built from the byte-identical shipping SQL: 48,409 rows,
**no temp B-tree on any of the four resolver shapes**, page 1 ~0.06 ms warm, and page boundaries
verified pairwise disjoint with release times descending across every boundary in a full
43,618-row walk.

`released_at` is stored as **INTEGER epoch-millis, not TEXT** — load-bearing. The two halves use
different encodings (ISO vs 13-digit millis), and under BINARY collation every `'2…'` sorts above
every `'1…'`, so a TEXT key would have sorted the entire mirror half above the entire scanner half
and called it chronological, silently undoing 0063.

Feed freshness is preserved: `scanner::persist_scan` upserts the one affected row on a detection,
so a new chapter appears immediately rather than waiting for the next periodic refresh. Gated on
`new_found`, so ~475 scans/hr produce a handful of writes.

---

## D — catalogue backfill reported success while never enumerating ~4,500 works

**Root cause proven.** MangaDex emits an explicit `"links": null` on a closed legacy cohort.
`#[serde(default)]` covers an _absent_ field, not a null one, so serde calls `HashMap`'s
deserializer on a `Null` token and **the entire record fails**. `Err(_) => skipped += 1` discarded
the error, `skipped` was never returned, and `is_clean_completion()` had no term for it — so the
walk latched a permanent marker while reporting a clean sweep.

Gap is bijective: `109,266 = 113,759 − 4,493`, three-way split **unseen 0 / nonexistent 0 /
seen-and-dropped 4,493**, 100% of the missing set carrying `links: null`. Era distribution stops
dead at 2021.

Hypotheses killed: the slide is lossless (`createdAtSince` is inclusive, measured);
`to_since_next_second` never fired; `total` is fully enumerable (independent walk found exactly
113,759 distinct uuids); `scanned` overlap is ~13 records. The `offset + limit <= 10000` theory is
also dead.

**Shipped:** `null_as_default` on nine fields — including `RawList::data`, where a null would have
cost **100 records instead of one**. Three fields deliberately left strict (notably `RawList::total`,
where null→0 could latch completion over an unwalked catalogue). Drops now log at **ERROR with the
uuid and the serde error**. `dropped` added to `BackfillOutcome` and folded into **both** existing
predicates. Both markers versioned to `_v2` — renaming only the flag would resume from the
2026-07-25 cursor and skip 100% of the cohort.

Offline proof: **4,493 → 0 dropped**, `parsed_with_title=4493`.

**`CATALOGUE_SYNC=on` in the live container, so the corrected walk runs on the next restart.**

**Expected on the completing pass:**

```
backfill: complete scanned≈113,760 ingested≈4,493 failed=0 truncated=0 dropped=0
```

- `SELECT COUNT(DISTINCT source_key) FROM source_series WHERE source_type='mangadex'`:
  **109,266 → ~113,760** — this is the pass/fail check.
- Completes on **pass 2**, ~55–60 min after restart.
- Net new `work` rows **~+3,950 to +4,350** (a range, not a point — the alias fold could not be
  confirmed precisely).
- **~2,032 arrive `is_nsfw=1`** (1,964 pornographic + 68 erotica) and stay hidden from anonymous
  surfaces. Expected, not a regression.
- Upstream total grows daily; 113,759 is a floor, not an equality.

**Another silent no-op looks like:** `scanned≈109,2xx` with `ingested=0`. Any non-zero `dropped`
must now leave the marker unset and log a rewind.

---

## Open decisions for the owner — deliberately not made

1. **Image Worker rate limiter is not enforcing.** 600 unique cache misses from one IP in 10s
   returned zero 429s, against a documented 200/60s budget. The `[[unsafe.bindings]]` ratelimit form
   may not be applied by the deployed wrangler version. Separate security/bandwidth ticket.
2. **88,852 of 111,806 cover blobs (79.5%, 16.0 GB) exceed `MAX_COVER_BYTES`**, pinned by
   `cover_cached_version` under the old 200 KB lossless budget. Re-materializing reclaims ~5 GB and
   cuts ~33% of bytes per cover request, but costs **~71 GB of inbound source traffic** (~8h at the
   warmer's concurrency). **Do not run during a latency incident** — it is the contention being
   fixed. Wants its own throttled, scheduled campaign.
3. **Browse's sort chips and Format/Status filters are client-side only** and now say so ("Sort this
   page" + a scope note). Making them real needs server-side `sort`/`status` args. `status` is a
   one-line filter on an existing column; `sort` needs per-ordering `EXPLAIN` checks and an `s.id`
   tiebreaker; Format needs the comic-type column materialized (which `feed_series_updates` now
   does, so it would pay for two features) or the facet removed.
4. **The Updates pager makes the feed's full depth linkable** — 2,181 pages anonymous, page 2,000
   showing 2018 chapters. Not new data (`canonicalUpdates(page:)` already paged it), just newly
   reachable. If "Updates" should mean "recent", that's a `WHERE released_at >= ?` window.
5. **`suwayomi.rs::parse_records` has the same swallow-the-error pattern** that caused D. It needs
   the same drop-counting treatment.
6. **`enrich_works` marks every requested id synced**, including ones whose record failed to
   deserialize, so a parse regression silently costs enrichment coverage instead of announcing
   itself. Deliberately the lesser evil (the alternative re-fetches an unparseable id forever), but
   making it self-healing needs a bounded retry — an attempt counter, hence a migration. Documented
   at the call site.
7. **Nothing on this host monitors availability** — the 13-minute outage on 2026-07-26 surfaced only
   because an agent mentioned a 502 in passing. Still the top operational gap.

---

## Review pass — defects found in this same day's work

Six reviewers (five with fix authority over disjoint files, one read-only on contracts). The
cross-cutting reviewer confirmed the `updatesFeed` contract agrees across all seven layers, the
`INDEXED BY` ↔ migration coupling is exact and fails loudly rather than degrading, and the three
writers on `feed_series_updates` converge. What the others found:

**CRITICAL — `updatesFeed(type:)` served an empty feed on every rebuild.** The type fill ran
_outside_ the rebuild transaction, so every row read `comic_type IS NULL` for its duration and both
the page and `total` came back empty. Worse than it sounds: the reader's Updates route sets
`s-maxage=30`, so that empty feed would have been **edge-cached for 30 s for every viewer**. Fixed
by folding the fill into the transaction (measured ~0.6 s added to a ~3 s write lock, against a 15 s
busy timeout); a failing fill now rolls back instead of committing an untyped generation.

**MAJOR (content gating) — materializing `is_nsfw` re-opened a leak.** `updatesFeed` reads a copy of
`COALESCE(is_nsfw_override, is_nsfw)` refreshed only by the periodic rebuild, and none of the three
admin NSFW mutations touched it — so an admin-marked work kept reaching opted-out viewers for hours.
The older `updates` resolver evaluates it live and has a Rust backstop; the new feed superseded that.
Fixed with `resync_feed_nsfw` at all three mutation sites.

**HIGH — the same silent-drop bug was still live in the chapter firehose.** `sync_chapters` advanced
`offset` by the **parsed** count, and a short page _is_ the end-of-window test — so one unparseable
chapter ended the window early, and the forward-only cursor never re-offered the rest. Both seeds are
`seed_done=1` in production, so this was live on the incremental sweep. Fixed the same way as the
catalogue path (`raw_len` drives pagination, `dropped` blocks completion).

**CRITICAL (reader) — a page race put the wrong results under the wrong page.** The Updates
streamed-promise effect had no cancellation, so paging 1→2→3 quickly left two in-flight requests and
**whichever resolved last won**. The home route's identical block was already guarded; it simply
wasn't carried over.

**The `FFD9`-in-last-64-bytes truncation check false-rejected real covers.** Measured against the
real corpus (4,562 files from Suwayomi's own thumbnail cache): 41 JPEGs carry up to 1,373 bytes after
EOI, so 64 bytes false-rejects 0.16% — and because truncation is now classified _transient_, a false
reject is never recorded, so the work is re-fetched and re-rejected **forever**. The obvious fix is
worse: 12.4% of these JPEGs contain an `FFD9` before their real EOI (EXIF thumbnails), giving 12.3%
false accepts. Settled at `TAIL_WINDOW = 4096`: 0 false rejects, 0.013% false accepts.

**The contention placeholder was reversed.** A 200 + transparent 1×1 fires `onload`, leaves a
see-through hole (the card container has no background of its own), and is invisible in status
metrics. Now **503 + `Retry-After: 2` + `no-store`**, which the reader's bounded jittered retry
turns into a cover appearing rather than a grey block persisting.

Also fixed: a warmer livelock (a persistently-failing newest block would be retried every 300 s
forever while ~12k candidates were never reached — now a rotating cursor); a coverage gauge that
could permanently disable the warmer; `pending_cover_count` reporting 27 pending against 1 actually
selectable; a Pager rendering "showing 0–80 of 63"; a production-fatal `each_key_duplicate` latent
in the legacy fallback; and an Updates page that told users "nothing matches this filter" during a
backend outage.

Two latent encoding hazards were closed by hand: `derive_latest_chapter_at` now zero-pads to 13
digits (the column is ordered as TEXT to keep its index, while `feed_series_updates.released_at`
orders numerically — they agreed only while every value was the same width), and the `AtHome`/`total`
null guards were half-applied, where an _absent_ field decoded to the exact `0` the comment argued
must never happen.

## Corrections to earlier reporting

- **Litestream is healthy.** An interim report flagged replication as possibly stalled; it is not.
  739 successful `wal segment written` against 25 sporadic checkpoint contentions over the same
  window, with the latest write advancing. Checkpoint failures under a busy writer do not stop WAL
  shipping.
- One agent ran `cargo fmt` (not `--check`), reformatting `catalog/mod.rs`, `graphql/mod.rs` and
  `series_cache.rs` — files it did not own. rustfmt is semantics-preserving, `cargo fmt --check` is
  now clean and all 339 tests pass, but the pre-format bytes in those files could not be restored.
