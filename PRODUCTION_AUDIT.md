# Komika — Production-Readiness Bug Audit

**Date:** 2026-07-18
**Method:** 10 parallel Opus 4.8 subagents, each deep-reading one subsystem read-only, then consolidated. Findings verified against source before inclusion. Line numbers reflect the working tree at audit time.

## Headline

- **No SQL injection** and **no authorization / IDOR bypass** were found anywhere. Every `format!`-built query interpolates only compile-time constants or generated `?` placeholders with all user values bound; every admin/user mutation is correctly gated server-side; per-user tables are strictly scoped by the token-derived `user.id`.
- The auth core (Argon2id + random salt, 256-bit `OsRng` tokens, SQL-enforced session expiry, ban propagation, timing-safe login) is genuinely solid and well-tested.
- The real exposure is concentrated in: **one cross-user data-corruption bug**, **resource-exhaustion DoS** (unbounded image decode, no query-cost limits, missing timeouts, no GC/quotas), **SSRF / open-proxy** on both the Worker and native image paths, and **dedup-correctness** races/thresholds.
- Several findings were reproduced independently by 2–3 agents (noted inline) — higher confidence.

## Severity legend

| Level       | Meaning                                                                                       |
| ----------- | --------------------------------------------------------------------------------------------- |
| 🔴 Critical | Data corruption or trivial full compromise; fix before any further use of the affected path   |
| 🟠 High     | Whole-service outage/crash class, or a real security boundary defeated; release-blocking      |
| 🟡 Medium   | Correctness/abuse/scaling problem that degrades or corrupts under real load; fix before scale |
| 🟢 Low      | Latent hazard, minor correctness smell, or UX defect                                          |

## Contents

1. [Cross-cutting / corroborated](#1-cross-cutting--corroborated)
2. [Server — GraphQL layer](#2-server--graphql-layer)
3. [Server — catalog / dedup / ingest / scanner](#3-server--catalog--dedup--ingest--scanner)
4. [Server — auth / avatar / media / migrations](#4-server--auth--avatar--media--migrations)
5. [Server — external sources & caching](#5-server--external-sources--caching)
6. [Reader — data layer & @komika/api](#6-reader--data-layer--komikaapi)
7. [Reader — UI routes & components](#7-reader--ui-routes--components)
8. [Reader — Tauri native engine](#8-reader--tauri-native-engine)
9. [Admin app](#9-admin-app)
10. [Cloudflare Worker / image pipeline](#10-cloudflare-worker--image-pipeline)
11. [Prioritized remediation](#11-prioritized-remediation)

---

## 1. Cross-cutting / corroborated

These were found by multiple independent agents; treat as high confidence.

- **Suwayomi client has no timeout** (H2 below) — flagged by the catalog/ingest agent and the external-sources agent independently. Freezes the scan scheduler and reader paths.
- **Image decompression-bomb OOM** (H1 below) — flagged by the server-security, auth/avatar/media agents (and echoed by the native agent's 32 MiB analysis). No `image::Limits` on any decode.
- **Comment-media uploads have no GC and no quota** (H8 below) — flagged by both the server-security and auth/media agents; migration 0023 promises age-based GC that does not exist.
- **`std::sync::Mutex::lock().unwrap()` poisoning** — flagged in the GraphQL layer (rate limiter, scan_health), the scanner tick loop, and the native engine state locks. One panic-while-held bricks that subsystem.
- **NSFW gate fail-open** — the `is_nsfw` half defaults to `false` on DB error / un-catalogued series while the viewer-preference half fails closed. Inconsistent.

---

## 2. Server — GraphQL layer

Files: `apps/server/src/graphql/mod.rs`, `graphql/types.rs`, `main.rs`, `db.rs`, `auth.rs`.
Pool reality: `max_connections(8)`, WAL, `busy_timeout(5s)` (`db.rs:15-22`).

### 🟠 H5 — Uncapped public `workSourcesBatch` → connection-pool exhaustion

`mod.rs:1334-1347`. `work_sources_batch` accepts `work_ids: [ID]` with **no length cap** and loops serially, each iteration ≈3 DB round-trips (`load_work_sources` + `authoritative_key_set`). It is **public** (no `require_user`). One anonymous request with 50,000 ids → ~150,000 serial queries pinning a connection; a few concurrent such requests drain all 8 pool connections → full outage (readiness probe also starves). Sibling `series_sources_batch` correctly caps at 200 and is admin-only (`mod.rs:1997`).
**Fix:** cap input ≤200 and batch via the existing `IN (...)` loader `load_work_sources_batch`.

### 🟠 H7 — No GraphQL depth/complexity limit on a public endpoint

`mod.rs:145-152` (`build_schema`) sets only `disable_introspection()`; no `limit_depth` / `limit_complexity`. Combined with per-item `#[ComplexObject]` resolvers on `Series` (`is_marked`, `covers`, `credits`, `localized_descriptions` — each a DB query per series, `mod.rs:203-282`), one aliased/nested `discovery { items { covers{…} credits{…} localizedDescriptions{…} isMarked } }` fans out to thousands of queries, unauthenticated.
**Fix:** `.limit_depth(...)` and `.limit_complexity(...)` on the builder.

### 🟡 M — Systemic N+1 in `map_series` across every feed

`mod.rs:442-503` (`map_series`), `604-610` (`map_series_list`), `1016-1096` (`discovery`). `map_series` issues ~5 independent queries per series (`rating_summary`, `admin_overrides`, `scan_state`, `canonical_alt_titles`, `canonical_is_nsfw`), serially, for every item. `discovery` maps up to 3×20 series → ~300 serial queries per home load (comment at `mod.rs:441` admits "batch them if list sizes grow"). Same in `updates`, `search`, `library`. `rating_summary` (`mod.rs:328`) additionally pulls **all** review rows for a series into Rust to aggregate — a series with thousands of reviews on the feed loads tens of thousands of rows per request.
**Fix:** batch into grouped queries keyed by series id (as already done for `libraryProgress`).

### 🟡 M — `current_user` re-runs the session lookup per call; amplified by `isMarked`

`mod.rs:299-305` (`current_user` → `auth::user_for_token`, a DB join), `203-216` (`is_marked`). No per-request memoization. A feed selecting `isMarked` does one `sessions⋈users` lookup per series plus one `user_library` EXISTS per series. Some resolvers call `viewer_show_nsfw` (→`current_user`) then `current_user` again (`mod.rs:1218`+`1224`), doubling lookups.
**Fix:** resolve the user once per request into the async-graphql context (or a DataLoader).

### 🟡 M — `library` unbounded serial fan-out including live network fetches

`mod.rs:1524-1577`. Selects **all** of a user's `user_library` rows (no LIMIT), loops, and on cache miss falls back to a **live Suwayomi HTTP call** (`st.suwayomi.series(n)`, `mod.rs:1569`) per series, serially. A large library or cold cache hangs the request and ties up a connection for many sequential network round-trips (compounds H2's missing timeout).
**Fix:** paginate; batch cache reads; drop per-item live fetches from the hot path.

### 🟡 M — No rate limiting on authenticated write mutations

`mark` (`mod.rs:2716`), `set_progress` (`2775`), `post_review` (`2858`), `post_comment` (`2911`), uploads (`main.rs:195,327`). Only `login`/`register` (`auth_limiter`) and `searchAllSources` (`federated_limiter`) are throttled. `/graphql` sets no `DefaultBodyLimit`, so axum's ~2 MB default applies; neither `post_review.body` nor `post_comment.body` has an application-level length cap (contrast `update_profile` capping bio at 500, `mod.rs:3194`). A script inflates the DB with 2 MB comments / deep `parent_id` reply chains as fast as the network allows; recursive-CTE comment reads then get expensive.
**Fix:** per-user write limiter + explicit body-length caps.

### 🟡 M — Raw internal/DB error strings leaked to clients

`gql_err` (`mod.rs:323`) is `Error::new(e.to_string())`; nearly every `.map_err(gql_err)` wraps a `sqlx::Error` whose `Display` returns verbatim in `errors[].message` (table/column names, SQL fragments). `ErrorLogger` (`mod.rs:171-188`) only logs, doesn't sanitize. Undermines the deliberate introspection-off / generic-500 hardening. `register` is the only path that scrubs (UNIQUE → friendly text, `mod.rs:3136`).
**Fix:** map to generic client messages; keep detail server-side only.

### 🟡 M — NSFW gate fails **open** on DB error / un-catalogued series

`canonical_is_nsfw` (`mod.rs:382-395`) returns `false` on any query failure (`.ok().flatten().unwrap_or(0)`), and only hides "what we positively know is NSFW". A transient DB error, or a genuinely-NSFW source series not yet linked to an NSFW `work`, reads as SFW to an opted-out viewer via `series`/`chapters`/`pages` (`mod.rs:1466/1478/1504`). The viewer-preference half (`viewer_show_nsfw`, `mod.rs:398-415`) correctly fails **closed**, so the two halves are inconsistent.
**Fix:** fail closed on error; decide the un-catalogued posture consciously.

### 🟢 L — Nondeterministic pagination (missing ORDER BY tiebreakers)

`reviews` (`mod.rs:1651`), `users` (`1812`), `comments` roots (`1728`), `canonical_updates` (`1190`) order by a timestamp only. Rows sharing a `created_at`/`latest_at` (bulk imports, same-second posts) can duplicate or skip across page boundaries. `updates` (`mod.rs:1123`) gets it right with `, series_id ASC`; the others weren't updated to match.

### 🟢 L — `mark` writes an unvalidated `series_id` before validating it

`mark(marked:true)` (`mod.rs:2723-2734`) inserts any string id into `user_library`, then only afterward resolves it (`starts_with("w_")` / `parse::<i64>()`, `2754/2770`). A non-`w_`, non-numeric id persists and then errors on the return path; `library` later silently skips it (`1562`). Self-inflicted orphan library rows.

### 🟢 L — `pages` bypasses NSFW gate for an uncached chapter

`pages` (`mod.rs:1497`) gates on the owning series' NSFW flag only when `chapter_manga_id(n)` resolves (`1504`); if the chapter isn't cached it returns `None` and page URLs are fetched ungated. Defense-in-depth gap (ids normally come from the gated `chapters` query).

### 🟢 L — `register` charset/length gaps; byte-based length checks

`register` (`mod.rs:3095-3108`) enforces `username.len() >= 3` and password bounds but no max username/email length and no charset rules; `.len()` is bytes, so a 1-glyph multibyte username (emoji = 4 bytes) passes ≥3. Allows usernames with control chars or up to ~2 MB.

### 🟢 L — Error-swallowing behind defaults

`rating_summary` (`mod.rs:333`), `admin_overrides` (`434`), `user_profile_fields` (`910`), `canonical_progress_map`/`suwayomi_progress_map` (`751/776`), `scan_status.next_due_at` (`1791`) convert DB errors into empty/zero defaults, silently rendering a series as unrated, a profile as blank, or all chapters unread.

### 🟢 L — Poisoned-mutex panic risk

`RateLimiter` (`mod.rs:50,71`) and `scan_health` (`1778`) use `.lock().unwrap()`. A panic-while-held poisons the lock; all later `login`/`register`/`scanStatus` calls then panic → 500. Critical sections are small (low likelihood) but reachable from request paths.

### 🟢 L — `to_iso` integer overflow

`types.rs:695-703`: `n * 1000` on a seconds timestamp can overflow `i64` for a pathological source string (panic in debug; wrong ISO date in release). Impact: wrong `uploadedAt`/`createdAt`. Use `checked_mul`.

### 🟢 L — Schema/resolver default drift

SDL `packages/api/src/schema/komika.graphql:90` declares `backfillMangadexMetadata(limit: Int = 200)`; resolver declares `#[graphql(default = 50)]` (`mod.rs:3688`). Code-first schema wins at runtime; the hand-maintained contract disagrees. Admin-only.

### ✅ Verified clean (GraphQL)

No SQL injection (all dynamic SQL interpolates constants or `?` placeholders; user values bound). Authorization tight — every admin op funnels through `require_admin` (`mod.rs:315`); ban/promote can't target admins or self; all per-user reads (`is_marked`, `library`, `library_progress` joined on `ul.user_id = sp.user_id`) are per-viewer; **no IDOR, no missing-auth mutation, no cross-user leak**. Auth crypto correct (Argon2id + `OsRng` salt, 256-bit `OsRng` hex tokens, SQL-enforced expiry + ban, dummy-hash timing defense, IP-keyed limiter, over-long-password pre-check). Good transactional discipline (avatar upsert `main.rs:256`; `post_comment`+media claim `mod.rs:2946`; `delete_comment` subtree `3379`; atomic `resolve_merge_candidate` claim `3536`). No `Mutex`/`RwLock` held across `await`. Comment recursion indexed (`idx_comments_parent`); banned-author subtrees pruned at every depth.

---

## 3. Server — catalog / dedup / ingest / scanner

Files: `catalog/mod.rs`, `catalog/normalize.rs`, `catalog/similarity.rs`, `dedup.rs`, `ingest.rs`, `scanner.rs`, `phash.rs`.

### 🟠 H6 — Dedup resolve→create/link is not atomic (false splits + orphan works)

`graphql/mod.rs:4166-4244`, `catalog/mod.rs:679-706`. `add_source_series_core_ex` runs `dedup::resolve` (read) then `create_work` / `upsert_source_series` (writes) as separate unsynchronized ops — no wrapping transaction, no lock. There is no global unique on `work_alias.normalized_title` (only `UNIQUE(work_id, normalized_title, lang)`, `0005:37`). Ingest deliberately runs `ITEM_CONCURRENCY = 6` (`ingest.rs:336-349`) and federated/admin adds race across the 8-conn pool.

- **False split:** two concurrent items that are the _same_ series both see no existing work → both `Decision::New` → **two canonical works for one series**.
- **Orphan + inconsistent result:** two concurrent adds of the same `source_key` both pass the `find_source_series` pre-check as `None`, both `create_work`. `ensure_source_series` does `ON CONFLICT(...) DO UPDATE SET last_seen, is_nsfw` — **not `work_id`** (`catalog/mod.rs:683-684`). Loser's fresh work is orphaned; returned `work_id` disagrees with the `ssid`.
  **Fix:** wrap resolve+create+link in one transaction, or add a partial unique index / natural-key `ON CONFLICT` claim re-checked inside the write; consider a per-normalized-title advisory lock.

### 🟠 H9 — Leading-wildcard `LIKE` fuzzy-block → full-table scans at scale

`catalog/mod.rs:139-156`. `candidate_work_ids_by_token` builds `format!("%{token}%")` + `LIKE` — a leading `%` **cannot use** `idx_work_alias_norm` (`0005:40`), so every call is a full scan. `dedup::resolve` fires it for the top 3 tokens (`FUZZY_BLOCK_TOKENS`, `dedup.rs:63`) on every new/unknown series. A 20k-item ingest against a mirrored MangaDex `work_alias` (hundreds of thousands to millions of rows) → ~3 full scans/item, 6-way concurrent → throughput cliff + write-path lock pressure.
**Fix:** prefix-anchor (`'token%'`, index-usable) or add FTS/trigram/token-inverted index; cap unioned candidates.

### 🟡 M/H — pHash corroboration too loose → false auto-merge of distinct series

`dedup.rs:67,153-162`, `similarity.rs:105-118`, `phash.rs:14-31`. `PHASH_CORROBORATION = 0.8` on a 64-bit dHash means up to **12 differing bits still "corroborates"**. With an exact normalized-title hit (`title_sim = 1.0`): score `= 0.6·1.0 + 0.4·0.8 = 0.92 ≥ HIGH(0.85)` and `cover_corroborated = true` → **`AutoMerge`**. dHash uses strict `left > right` on a 9×8 resize; flat/letterboxed/dark covers collapse to near-identical hashes, so two _different_ series sharing a normalized-title collision and both with generic covers can auto-merge irreversibly.
**Fix:** tighten `PHASH_CORROBORATION` (≤6-bit Hamming, ~0.90+); reject low-entropy/near-uniform hashes from corroboration; require two independent signals for auto-merge.

### 🟡 M — `merge_works` orphans `reviews` / `user_library` rows

`catalog/mod.rs:902-965`. `reviews` has `UNIQUE(series_id,user_id)` (`0001:32`); `user_library` PK `(user_id,series_id)` (`0024`). When a user has a row on _both_ merged works, `UPDATE OR IGNORE ... SET series_id = target` silently skips the source row; the cleanup loop deletes only `work_alias/work_external_id/work_description/work_credit/work_cover` (`946-957`), not `reviews`/`user_library`. The source `work` is then deleted (`962`), leaving orphan rows referencing a nonexistent work (phantom library entry). (`canonical_progress`'s plain `UPDATE` is genuinely safe — `work_id` isn't in its PK and merged works have disjoint chapter uuids.)
**Fix:** reconcile colliding rows (delete-loser or merge) before repointing; extend cleanup to `reviews`/`user_library`.

### 🟡 M — `known_max_chapter` high-water mark never heals

`scanner.rs:421-433`. `known_max = p.max(l)` is monotonic. A single garbage upstream number (a chapter labeled `9999`, or Suwayomi's `-1.0` sentinel) permanently pins it; afterward `advanced_number` (`l > prev + EPS`) can never fire again → SC4 number-based detection is dead, leaving only the fragile count path.
**Fix:** derive `known_max` from a robust statistic of the _current_ list (max of parseable numbers, ignoring outliers/sentinels), or clamp/validate incoming numbers first.

### 🟡 M — Missed new chapter when a removal offsets an insertion at/below max

`scanner.rs:427`. `new_found = count > prior.known_chapter_count || advanced_number`. If upstream removes one old chapter and adds a genuinely new one numbered at/below the current max in the same interval, `count` is flat and `advanced_number` is false → the new chapter is never flagged (no `updates` entry, no `last_new_chapter_at`).
**Fix:** detect via set-difference of chapter identities against the prior snapshot, not count+max heuristics.

### 🟡 M — MangaDex >9,900-per-second boundary silently drops records

`mangadex.rs:794-807` (catalogue), `900-911` (chapters). When >9,900 records share a 1-second `createdAt`/`updatedAt` boundary, the window can't advance, the loop `break`s (logs `error!`), and every record past offset 9,900 in that second is never fetched. The incremental cursor only moves forward, so those records are lost until a full reseed. Acknowledged in comments; realistic on the high-volume chapter firehose.

### 🟢 L/M — Title normalization edge cases: empties legit titles, manufactures collisions

`catalog/normalize.rs:12-76`. `is_roman_numeral` (`72-76`) treats any string over `{i,v,x,l,c,d,m}` as a numeral — `"mix"`, `"did"`, `"civic"`, `"mimic"` are "roman". With trailing-noise stripping, `"The Mix"` → `""` (dropped from the alias index via `insert_aliases` skip, `catalog/mod.rs:650`) → that series can never match by title → false split. `NOISE_TAIL` stripping folds `"Overlord Season 2"` → `"overlord"`, widening the collision surface feeding the pHash issue.
**Fix:** only strip roman-numeral tokens in numeral context; never reduce a title to `""` (fall back to pre-strip form).

### 🟢 L — Numeric edge cases in cross-source chapter aggregation

`aggregate_chapter_count` buckets by `(number*100.0).round() as i64` (`catalog/mod.rs:1090`); a NaN `chapter_number` from Suwayomi (`f64`) casts to `0`, colliding with chapter 0. The MangaDex arm's `CAST(c.number AS REAL)` (`1065`) turns a non-numeric number string into `0.0`. Minor count skew.

### 🟢 L — `load_canonical_work` picks an arbitrary MangaDex anchor with `LIMIT 1`

`catalog/mod.rs:265-271` (`WHERE ... source_type='mangadex' LIMIT 1`, no ORDER BY). After a merge folds two MangaDex-anchored works, the target has two `mangadex` source_series; the reader's cover URL and page anchor become nondeterministic across loads.

### 🟢 L — Ingest progress counters drop `review_consolidated`

`ingest.rs:85-93` + `graphql/mod.rs:4205-4211`. `Progress::record_decision` matches `new`/`auto_merge`/`review`/`existing`; a `review_consolidated` decision increments `succeeded` but no category (`_ => {}`). Not on the current path (ingest uses `consolidate=false`) but breaks admin totals if the federated path is wired through the job.

### 🟢 L — Scanner health mutex `.lock().unwrap()` can panic the tick loop

`scanner.rs:555`. Poisoning limits blast radius to the scan scheduler task. Use `lock().unwrap_or_else(|e| e.into_inner())` or a non-poisoning lock.

### ✅ Verified clean (catalog)

Ingest job state machine (unique-index race via `SQLITE_CONSTRAINT_UNIQUE` 2067, cancel-wins-over-finish, `MAX_PAGES`, startup interrupted-job recovery) correct and tested. `record_scan` read+upsert wrapped in one transaction (relies on WAL `SQLITE_BUSY_SNAPSHOT`). `resolve_interval` clamped to `MAX_INTERVAL_HOURS` (no chrono overflow). `avg_interval_hours` guards `<2` timestamps and `gaps==0`. `phash_similarity` guards non-hex/empty/length-mismatch. No unhandled panics on malformed external data in audited paths (`chapter_number` required `f64` → null fails deserialization and skips, not panic). MangaDex client is production-grade (timeouts, bounded retries, `Retry-After`, dual rate buckets, cursor-only-on-success).

---

## 4. Server — auth / avatar / media / migrations

Files: `auth.rs`, `avatar.rs`, `media.rs`, `config.rs`, `db.rs`, migrations.

### 🟠 H1 — Image decompression-bomb → memory-exhaustion DoS _(corroborated ×3)_

`avatar.rs:20,43`, `media.rs:19,53`. Both call `image::load_from_memory(bytes)` (image 0.25) with **no `.limits()` and no dimension check** before `to_rgba8()`/`crop_imm()`/`resize()`. The `MAX_UPLOAD_BYTES = 8 MB` cap (commented "a decode bomb guard") bounds only the _compressed_ input. A highly-compressible PNG/WebP (e.g. 30000×30000 single-color, a few KB on the wire) decodes to ~3.6 GB RGBA; the avatar path then runs up to 7 Lanczos re-encodes. Any authenticated user (open registration) can OOM-kill the process; `spawn_blocking` doesn't bound allocation. Note: image 0.25 may apply a default ~512 MB allocation ceiling that caps the very worst case, but dimensions stay uncapped and 512 MB per concurrent decode still OOMs small instances.
**Fix:** `image::ImageReader` with `Limits { max_image_width/height, max_alloc }`, or read `dimensions()` and `bail!` above ~40 MP before rasterizing.

### 🟠 H8 — Comment-media uploads: no GC and no per-user quota _(corroborated ×2)_

`main.rs:327-409`, migration `0023:19`. Every `POST /comment-media` inserts a ≤500 KB (server-security agent read 8 MB cap; media path caps smaller) WebP BLOB with `comment_id = NULL`. Migration 0023 says these "can be garbage-collected by age" — **no GC exists** (grep: only the `delete_comment` cascade). An authenticated account loops uploads it never attaches → unbounded SQLite growth, all shipped to R2 via Litestream → disk-full crash + storage cost. `/avatar` is self-limited (one row per user via upsert); comment-media is one row per upload.
**Fix:** scheduled `DELETE FROM comment_media WHERE comment_id IS NULL AND created_at < now-24h` (index `(comment_id, created_at)`) + staged-per-user cap.

### 🟡 M — REST upload endpoints have no rate limiting

`main.rs:598-613`. `/avatar` and `/comment-media` are auth-gated but not behind any limiter. Each runs CPU-bound decode + Lanczos resize + lossless-WebP encode on `spawn_blocking` (pool up to 512 threads), encoding up to 7 candidates. A single client flood — combined with H1 (large allocations) and H8 (unbounded storage) — exhausts CPU/memory/blocking-pool.
**Fix:** per-user/IP limiter on both routes (reuse the sliding-window limiter).

### 🟡 M — Session tokens stored in plaintext at rest

`0001_init.sql:14-18` (`sessions.token` PK), `auth.rs:60-71` (`WHERE s.token = ?`). Tokens are generated well (256-bit CSPRNG) but persisted verbatim. The DB is continuously replicated to R2 (Litestream). A leaked snapshot / compromised R2 object / read replica yields every live session token → immediate account takeover with no crack needed (unlike the Argon2 password hashes in the same DB).
**Fix:** store `sha256(token)`; hash on insert in `new_session` and on lookup in `user_for_token`.

### 🟢 L — Username uniqueness is case-sensitive → impersonation

`graphql/mod.rs:3095-3138`, `users.username TEXT UNIQUE` (`0001`). Registration only trims/length-checks; the DB `UNIQUE` is byte-exact. `mika` and `Mika` register as distinct accounts. Admin-reserved names use `eq_ignore_ascii_case` (safe), but ordinary users can be impersonated on a social platform.
**Fix:** normalized/lowercased username column with UNIQUE index, or `UNIQUE COLLATE NOCASE`.

### 🟢 L — Login distinguishes "suspended" from "invalid credentials"

`graphql/mod.rs:3072-3074`. The ban check runs only after a successful password verify and returns `"This account has been suspended."`, whereas wrong password returns `"Invalid username or password"` — a credential oracle confirming a valid username+password pair even when login is refused.
**Fix:** return the generic message for banned accounts too (or accept deliberately).

### 🟢 L — Avatar cache version has 1-second granularity

`main.rs:251`. `version = Utc::now().timestamp()` (whole seconds) feeds `avatar_url(...?v=<version>)`. Two uploads within one second yield an identical `?v=`, so the immutable/1-year-cached `/avatars/{id}.webp` won't refresh. Use `timestamp_millis()` or a monotonic counter.

### ✅ Verified clean (auth/media/migrations)

Argon2id default params, random salt, PHC round-trip, malformed-hash rejection (`auth.rs:22-39`). Timing-uniform login via `DUMMY_PASSWORD_HASH` + pre-Argon2 over-length reject (`mod.rs:3039-3062`). Session expiry enforced in SQL (`expires_at > now`, lexically-sortable `format_ts`), banned users excluded, opportunistic + ban-triggered session deletion. No cookies — Bearer tokens in `Authorization` (SameSite/Secure/HttpOnly N/A). Comment-media linking ownership-checked and single-use (`SET comment_id ... WHERE id=? AND user_id=? AND comment_id IS NULL`, `mod.rs:2965-2983`); serve routes use bind params + `.strip_suffix(".webp")` (no path traversal). Uploads re-decoded + re-encoded to lossless WebP served with fixed `image/webp` + global `nosniff` (SVG/HTML can't survive → no upload-XSS). Migrations: cascades rely on `foreign_keys(true)` (set by prod pool, `db.rs:17`); 0009 comment rebuild and 0010 expiry backfill are data-preserving; PKs/indexes on 0014/0016/0023/0024/0025/0026 present and correct. Config: no hardcoded secrets, `Secret` redacts in `Debug` (`config.rs:8`), admin password env-only, insecure surfaces (GraphiQL/introspection, catalogue sync, XFF trust) default off.

---

## 5. Server — external sources & caching

Files: `mangadex.rs`, `suwayomi.rs`, `series_cache.rs`, caching call sites in `graphql/mod.rs`.

### 🟠 H2 — Suwayomi HTTP client has no timeouts _(corroborated ×2)_

`suwayomi.rs:170` — `http: reqwest::Client::new()` sets **no connect/request timeout** (reqwest default is none). Contrast `MangaDexClient` with `connect_timeout(10s)` + `timeout(30s)` (`mangadex.rs:141-143`). Suwayomi proxies to source sites (often via FlareSolverr, which routinely stalls). A hung upstream makes `chapters()`/`series()`/`cover_bytes()`/`fetch_extensions()` hang forever. Because `scanner::tick` scans sequentially (`scanner.rs:314-348`) and is awaited in the scheduler `select!` (`551-553`), **one hung series freezes the entire scan scheduler for the process lifetime** — no further ticks, catalogue never refreshes. Reader cache-miss paths (`resolve_*_cached` → `st.suwayomi.*()`) also hang, accumulate held request slots, and exhaust the runtime/DB pool → site-wide hang.
**Fix:** build the Suwayomi client via `reqwest::Client::builder().connect_timeout(...).timeout(...)`, mirroring MangaDex.

### 🟡 M — Unsupervised background loops; one panic silently kills the subsystem

`scanner::spawn` (`scanner.rs:545`), `mangadex::spawn_recurring` (`mangadex.rs:1040`), `spawn_metadata_backfill` (`graphql/mod.rs:3991`) are plain `tokio::spawn`s whose `JoinHandle` is dropped (`main.rs:534-565`). No `catch_unwind`, no restart. Reachable panic: `state.scan_health.lock().unwrap()` (`scanner.rs:555`) on a poisoned mutex. On panic the loop dies permanently and silently — operator sees only a permanently stale catalogue.
**Fix:** supervise each loop (restart with backoff; recover from mutex poisoning).

### 🟡 M — One malformed record fails the entire Suwayomi page/list parse

`suwayomi.rs:320-330` (`browse_source`), `364-403` (`chapters`), `449-463` (`library`). These deserialize `Vec<SuwayomiManga>` / `Vec<SuwayomiChapter>` atomically. `SuwayomiManga.title`/`status` and `SuwayomiChapter.name`/`chapter_number` are **non-`Option`**, so one record with a null title/name fails the whole page. Contrast `mangadex.rs:225-233` (per-record parse + `skipped` counter). A source returning 60 mangas with one `title: null` returns an error instead of 59 good results; a single bad chapter breaks the whole chapter list + reader.
**Fix:** parse the `nodes`/`mangas` array element-by-element, skip/log bad records; or make fields `Option` with defaults.

### 🟡 M — Reader-served cache never refreshed on hit; no TTL; stampede on miss

`graphql/mod.rs:579-602` (`resolve_series_cached`, `resolve_chapters_cached`). On a hit the reader returns cached rows and never re-validates — no TTL, no `last_fetched_at` staleness gate. Refresh happens only when the background scanner calls `put_chapters`; a series a user browses but that's **not in the scan rotation** (e.g. a non-library series) is frozen forever after first view (new upstream chapters invisible). On a genuine miss there is no single-flight — N concurrent requests for the same uncached series each live-fetch in parallel (amplifies H2's hang risk).
**Fix:** freshness gate (refetch when `now - last_fetched_at > TTL`) + per-key single-flight (e.g. `DashMap` of in-flight fetches).

### 🟢 L/M — `get_manga_by_ids` silently truncates to 100 ids

`mangadex.rs:239-267`. `limit = ids.len().min(100)` and `.take(100)` drop everything past the 100th id. A caller passing >100 (backfill/S2 path) silently never fetches those works.
**Fix:** error when `>100`, or chunk internally and concatenate.

### 🟢 L — MangaDex `get_with_retry` doesn't retry transport/timeout errors

`mangadex.rs:172`. Only 429/5xx are retried; a connection reset or 30s timeout propagates via `?` with no retry. Documented as intentional for sweeps, but the reader-facing `at_home` path fails a page-load on a single transient blip. Consider a bounded transport-error retry on the reader path.

### 🟢 L — `resolve_source` holds the async mutex across a network round-trip

`suwayomi.rs:241-279`. The `source_id` mutex is locked at `242` and held across `self.gql(...).await` (`263`); concurrent first-callers serialize behind one request. Also cached for process lifetime (installing a new source needs a restart). Double-checked locking fixes both.

### 🟢 L — Home `latest` feed swallows upstream errors as empty

`graphql/mod.rs:1047-1052`. The fresh-install branch does `fetch_source(Latest…).await.map(|r| r.1).unwrap_or_default()`; a Suwayomi error becomes an empty "Latest" row (and `recent` clones it). Fresh-install path only.

### 🟢 L — Genre filter treats `%`/`_` in genre names as LIKE wildcards

`series_cache.rs:273-283`. Patterns `%"{g}"%` used with `LIKE` and no `ESCAPE`. A genre containing `%`/`_` (source-controlled) matches more broadly than intended (not injection — properly bound). Escape metacharacters + `ESCAPE '\'`.

### ✅ Verified clean (sources)

Per-user read state does NOT leak through the shared `suwayomi_chapter` cache — the viewer's `suwayomi_progress` is overlaid over the global row (`graphql/mod.rs:1483-1494`). Cache miss doesn't negative-cache errors (`?`-propagated; `put_*` only on success). Token-bucket capacity floor prevents the sub-1/s at-home bucket deadlocking (tested). `put_series` bind/placeholder count correct (17 binds ↔ 16 VALUES + 1 ON CONFLICT); `created_at` preserved via `COALESCE`. `put_chapters` DELETE+INSERT is transactional (no partial-replace window).

---

## 6. Reader — data layer & @komika/api

Files: `apps/reader/src/lib/data/source.ts`, `social-repo.ts`, `auth.svelte.ts`, `config.ts`, `context.ts`; `packages/api/src/*`.

### 🔴 C1 — Offline queue replays one user's writes under another user's account

`context.ts:30-36`, `composite-backend.ts:89-101,194-227`, `auth.svelte.ts:107-115`. `OfflineWriteQueue` (localStorage `komika.offlineWrites`) holds `mark`/`setProgress` ops keyed only by canonical `seriesId`/`chapterId` — **no user binding**. `logout()` clears only the bearer token; it never clears/drains the queue. The queue drains on the `online` event and on any successful `mark`/`setProgress` (`flushQueue`), using whatever token is currently installed. User A toggles library/progress offline → ops enqueue; A logs out; B logs in on the same device; B marks anything (→`flushQueue`) or the device reconnects → the queue replays A's ops under **B's** token, corrupting B's library and progress. Reachable whenever `PUBLIC_KOMIKA_NATIVE_ENGINE=on` under Tauri.
**Fix:** stamp `userId` at `enqueue`; on drain skip/drop ops whose `userId !== currentUser.id`. At minimum drain-and-flush (or hard-clear) inside `logout()` before the token is dropped.

### 🟠 H — CompositeBackend silently drops hosted capabilities (native mode)

`composite-backend.ts` (whole class). `CompositeBackend implements Backend` but forwards a _subset_ of `GraphQLBackend`'s methods. Because the un-forwarded methods are all declared optional (`?`) on `Backend` (`backend.ts:79,82,89,161`), TS doesn't flag the omissions, and callers feature-detect them as `undefined`. On the native path (`nativeEngine && isTauri()`) these vanish or throw:

- `aggregatedChapters` missing → `source.ts:993` sees `undefined`, `agg=[]` → native series page loses the cross-source chapter union.
- `searchAllSources` missing → `getFederatedSearch` (`source.ts:466`) returns `{ kind: 'unauthenticated' }`, downgrades to single-source.
- `genreFacets` missing → `getGenreFacets` (`source.ts:528-533`) returns `[]` → empty browse genre filter.
- `uploadCommentMedia` missing → `social-repo.ts:418-420` throws "Image upload requires the Komiq backend." → native users get a hard error attaching comment images.
- Also un-forwarded: `mergeWorks`, `extensions`/`sources`/`sourceBrowse`/`installExtension` (admin surface).
  **Fix:** forward every optional hosted method (or make the composite a default-delegating `Proxy`/base so new methods can't be forgotten).

### 🟠 H — CompositeBackend.search discards the `filters` argument

`composite-backend.ts:164-166`. Contract is `search(query, page?, filters?)` (`backend.ts:71`) but the override is `search(query, page?)` calling `this.opts.hosted.search(query, page)` — `filters` dropped (TS allows the narrower arity via contravariance). On native, `getBrowseCatalog(filters)` → `getNativeSearch('', filters)` → `backend.search(query, 1, {genres,minRating,maxRating})` → the composite ignores genre/rating filters; browse's server-side filtering silently no-ops.
**Fix:** `search(query, page?, filters?) { return this.opts.hosted.search(query, page, filters); }`.

### 🟡 M — Native page blob URLs created but never released (memory leak)

`image-provider.ts:120-124,181-183`; `source.ts:1201,1223`. `NativeImageProvider.resolvePage` allocates `URL.createObjectURL(blob)` per page; `source.ts` builds reader views with `Promise.all(domainPages.map(p => images.resolvePage(p)))` and never calls `release(url)`. Web returns a plain proxy string (no leak); native leaks a blob per page per chapter until document unload — long webtoon sessions grow unbounded.
**Fix:** call `images.release?.(url)` on page unmount / chapter change; track+revoke per-chapter object URLs.

### 🟢 L — `getUpdates` fails-all when `updates()` rejects (inconsistent with `getHome`)

`source.ts:610-617`. `Promise.all([discovery(), updates(), canonicalUpdates?.().catch(...)])` — `discovery()` and `updates()` are not individually caught, so one `updates()` outage collapses the whole screen (incl. Trending) to empty. `getHome` (`source.ts:566-569`) wraps `updates()` in `.catch()` for exactly this. Mirror it.

### 🟢 L — `resolveWork` "backend fully down" detection defeated when methods absent

`source.ts:335-348`. The "if all three reject, rethrow honest error" guard is bypassed when a method is _absent_: `backend.workSources ? … : Promise.resolve([])` resolves rather than rejects, so `wsRes.status` is `'fulfilled'` during a true outage and the all-rejected branch never fires → outage silently degrades to "not found" for any backend missing an optional method.
**Fix:** track "present-but-rejected" vs "absent" separately.

### ✅ Verified clean (data layer)

GraphQL client error handling correct — `gql` (`graphql-backend.ts:66-81`) checks `!res.ok`, non-empty `errors[]`, and `data == null` before returning (same in `suwayomi-backend.ts:266-278`, `local-suwayomi-backend.ts:93-102`). Auth token-swap races guarded — both `initAuth` (`auth.svelte.ts:55-63`) and `revalidateSession` (`86-92`) re-check `readToken() === token` before clobbering `auth.user`; `GraphQLBackend.setToken` reads `this.config.token` synchronously per request. Offline queue idempotency/ordering sound — `enqueue` collapses by target (latest-wins), `drain` oldest-first, stops on failure preserving order, drops poison ops after `MAX_TRIES`; the only defect is the missing user binding (C1). `setLibraryMark`/`saveProgress` id-shape routing (`source.ts:1028-1059`) intentional and correct.

---

## 7. Reader — UI routes & components

Files: `routes/(app)/series/[slug]/+page.svelte`, `routes/read/[slug]/+page.svelte`, `browse`, `library` (+`.ts`), `profile`, `updates`, `(app)/+page.svelte`, `components/{CommentThread,Header,SearchOverlay,Icon}.svelte`. Svelte 5 runes.

### 🟡 M/H — Series page: stale async response overwrites current series (no cancellation)

`series/[slug]/+page.svelte:25-34`. The load `$effect` calls `data.series.then(...)` with **no `cancelled` guard** (unlike the social-load effect right below, `98-114`). Navigate A→B quickly; if A resolves after B, A's `.then` sets `view = A.view` while the URL says B → wrong series' hero/chapters/rating. `retrySeries` (`36-44`) has the same shape (lower risk, user-triggered).
**Fix:** capture `let cancelled = false`, check inside `.then`, cleanup sets it true.

### 🟡 M — Reader: mid-chapter reading position lost on browser-back / link exit

`read/[slug]/+page.svelte` (no `onDestroy`/`beforeNavigate`; scroll effect `130-173` never saves). `saveProgress` fires only from `openChapter` (in-app Prev/Next/menu), `switchTranslator`, and `maybeMarkRead` (only at `progressPct >= 98`). Scroll to 50% then press browser Back or click the "Back to series" header link (`href={seriesHref}`, `253/257`) → nothing persists; "Continue" resumes from the last explicitly-saved position. Core reader data loss.
**Fix:** `beforeNavigate` / `onDestroy` / `visibilitychange` handler calling `saveProgress(...)`.

### 🟡 M — Reader: image load errors unhandled → large empty gaps

`read/[slug]/+page.svelte:421-428` (strip), `445-450` (paged). `<img>` have `onload` (aspect measurement) but **no `onerror`**. A failed image keeps its cell at `DEFAULT_ASPECT` (800/1200) forever + shows the broken-image glyph. With 37+ images from flaky sources, any 404 yields a full-height gray box mid-strip with no retry.
**Fix:** `onerror` sets per-index error state → render a "failed — tap to retry" placeholder.

### 🟡 M — Browse: "Newest" sort does not sort by recency

`browse/+page.svelte:152` + `source.ts:152` (`added: i`). `added` is the positional index of each result (`toCatalogEntry(s, i)`), not a date; the `newest` sorter `a.added - b.added` just preserves backend order. Silently wrong sort.
**Fix:** carry a real added/updated timestamp on `CatalogEntry` and sort on it, or remove the option.

### 🟡 M — SearchOverlay: advanced-search filters are decorative

`SearchOverlay.svelte:61-73`. The advanced panel renders genre/status chips (with hover styling) but **none have `onclick`**; "Reset" (`72`) has no handler; "Search" (`73`) calls `submit()` using only free-text `q`. Selected filters are ignored → user taps genres, hits Search, gets an unfiltered `/browse?q=…`.
**Fix:** wire chips into query params, or remove the panel until implemented.

### 🟢 L — SearchOverlay: focus `setTimeout` never cleared

`SearchOverlay.svelte:16-18`. `$effect` schedules `setTimeout(...,30)` with no cleanup; rapid open/close leaks orphan timers. Return a `clearTimeout`.

### 🟢 L — Reader: scroll effect coupled to `total`/`lockChrome`

`read/[slug]/+page.svelte:130-173`. The effect calls `update()` synchronously (`171`), reading `total`/`lockChrome`, making them deps → every chrome-lock toggle or chapter change tears down + re-adds the scroll listener. Not a leak (cleanup correct) but wasteful; split listener setup from the restore/update logic.

### 🟢 L — Series "Readers Also Enjoyed" keyed by title

`series/[slug]/+page.svelte:471`. `{#each relatedSeries as item (item.title)}` uses title as key though `item.id` exists. Two related works sharing a title collide/reuse DOM. Key by `item.id ?? item.title`.

### 🟢 L — Browse: client state doesn't react to same-route URL changes

`browse/+page.svelte:23-27`. `query`/`types`/`selectedGenres` are `$state` initialized once from `page.url.searchParams` at script init. Client-side nav to a new `/browse?q=…` or `/browse?genre=…` (home genre links, `(app)/+page.svelte:174`) reuses the component and doesn't update inputs/filters. Only a fresh load reflects params.

### 🟢 L — Non-functional action controls (vibe smell)

`series/[slug]/+page.svelte:305` — "Share" button has no `onclick`. `routes/(app)/+page.svelte:78` — hero "+" is `aria-label="Add to library"` but is just an `<a>` to the series page; never adds to library.

### 🟢 L — Home hero auto-rotate overrides manual selection

`routes/(app)/+page.svelte:29-38`. The 5s `setInterval` keeps advancing `heroIndex` even after the user clicks a dot (`81-87`), replacing their chosen slide within 5s. Pause/reset the timer on manual interaction.

### 🟢 L — `deleteChapterComment` name misleading for series threads

`CommentThread.svelte:216` calls `deleteChapterComment` for both `targetType` values. Functionally OK (delegates to generic `backend.deleteComment(commentId)`, `social-repo.ts:494-501`) but the chapter-specific name in a shared component is a maintenance trap.

### ✅ Verified clean (reader UI)

No `{@html}` anywhere — comment bodies (`CommentThread.svelte:335`), synopsis (`series:329`), genres all render as escaped text. Prior "stuck loading" fixes are in place: library resolves in `load` with `ssr = false` (`+layout.ts:4`, `library/+page.ts:7`); profile distinguishes in-flight vs settled-null via `profileLoaded`/`loadFailed` (`profile:47-49`); each-blocks key on series `id` (`?? title` fallback) in library/profile/series/chapters. Every `Icon` `name` used is a member of the `IconName` union. `CommentThread` reply tree, load effect, and the browse search effect all have correct `cancelled` guards + `clearTimeout` debounce (280ms/160ms).

---

## 8. Reader — Tauri native engine

Files: `src-tauri/src/{lib,main,cloudflare,suwayomi,suwayomi_ios,suwayomi_mobile}.rs`. Note: no `panic = "abort"` in any profile, so a thread panic unwinds rather than killing the iOS app (lowers `unwrap` severity).

### 🟠 H4 — DNS-rebinding TOCTOU defeats the `fetch_image` SSRF guard

`lib.rs:81` (validate) vs `82-87` (fetch). `validate_image_url` resolves the host via `lookup_host`, checks every IP against `is_blocked_ip`, then returns the parsed `reqwest::Url` **still carrying the hostname**. `client.get(target.clone()).send()` (`82`) makes reqwest do its **own second** DNS resolution at connect time — no pinning. An untrusted source supplies an image URL on an attacker domain with short-TTL/round-robin DNS: validation lookup returns a public IP (passes), reqwest connect lookup moments later returns `169.254.169.254` or a LAN address → guard bypassed, fetched bytes returned to JS (internal-GET / metadata exfil). Applies to the initial fetch and every redirect hop (`97-101`, re-validated the same racy way). Unit tests only cover IP literals.
**Fix:** resolve once and connect to the validated `IpAddr` — `ClientBuilder::resolve(host, socket_addr)` / per-request `.resolve()`, or a custom connector re-applying `is_blocked_ip` to the dialed address.

### 🟡 M — CF shim serializes all solves on one thread; blocks shutdown up to `maxTimeout`

`cloudflare.rs:275-279` (`serve_loop`) + `315` (`block_on(handle_v1…)`). The listener processes `incoming_requests()` sequentially; `handle_request` runs `block_on(handle_v1(...))` awaiting the WebView solver up to `maxTimeout` (default 60s, `cloudflare.rs:45`). While one challenge solves, no other `/v1` request is serviced. Two extensions hitting CF challenges concurrently → the second FlareSolverr POST queues behind a ~60s solve and the engine's client times out. At shutdown, `CfShim::shutdown` (`259-264`) calls `handle.join()`, blocking until the in-flight `block_on` returns — up to 60s hung exit.
**Fix:** cap solve wall-clock below the join/shutdown budget; dispatch each `/v1` request on its own worker; cancel the in-flight solve on `unblock()`.

### 🟡 M — Unbounded 32 MiB per-request image buffering, no concurrency cap (OOM / iOS jetsam)

`lib.rs:174-181` (`read_capped`), `suwayomi.rs:681-688`, `suwayomi_ios.rs:609-616`. Each image command buffers the full body into one `Vec<u8>` up to `MAX_IMAGE_BYTES` = 32 MiB before IPC; no global cap on concurrent count. A reader prefetching N pages (or a malicious source serving large images) drives N × 32 MiB of native heap. On iOS the in-process JVM runs under `-Xmx256m` (`suwayomi_ios.rs:302`) in a jetsam-limited process — a handful of concurrent 32 MiB buffers + JVM heap trips the OS OOM killer.
**Fix:** bounded semaphore around image commands; smaller per-page cap; or stream to JS.

### 🟡 M — `validate_image_path` doesn't decode percent-encoding → traversal guard bypassable

`suwayomi.rs:625-643` and the verbatim copy `suwayomi_ios.rs:566-583`. The guard rejects literal `..` and `\` but `path` is later interpolated raw into `format!("http://127.0.0.1:{port}{path}")`. Percent-encoded forms (`%2e%2e`, `%5c`) pass untouched. A source-supplied `/api/v1/%2e%2e/...` clears the guard; whether it traverses depends on the engine's router normalization (Ktor likely normalizes it — hence Medium, but the stated contract isn't met).
**Fix:** percent-decode before the `..`/`\` checks, or allowlist concrete route shapes.

### 🟢 L — iOS single-boot + port-broker TOCTOU = permanent `degraded` with no self-heal

`suwayomi_ios.rs:446-449` (one boot ever) and `broker_port` at `199-204` / `suwayomi.rs:220-225`. `broker_port` binds `:0`, reads the port, drops the listener, and the engine binds it later — a bind race. Desktop tolerates this (supervision loop restarts with backoff, `suwayomi.rs:370-440`); iOS has no restart loop and refuses a second boot, so a transient port steal (or any one-time boot failure) leaves the engine `degraded` until app relaunch. Same code, materially different resilience.
**Fix:** on iOS allow bounded boot retries with a fresh brokered port, or bind-retry inside the JVM.

### 🟢 L — Mutex-poisoning cascade across state accessors

`suwayomi.rs` / `suwayomi_ios.rs`: every `self.inner.lock().unwrap()` (e.g. `149,158,169,179,189,203`). A panic-while-held poisons `inner` (or `cf_shim_url`/`lock`); every later `.lock().unwrap()` (incl. `suwayomi_status`/`suwayomi_gql`) then panics → permanently unusable command surface. Latent (small critical sections). Use `lock().unwrap_or_else(|e| e.into_inner())` or `parking_lot`.

### 🟢 L — `notify_waiters()` signal loss vs a 6s shutdown deadline

`suwayomi.rs:200` + `198-210`. `Notify::notify_waiters()` stores no permit — if the supervisor isn't parked in `notified()` at that instant, the wake is lost and shutdown relies on the `stopping` flag at the next loop check. `stop_and_wait` gives up after 6s and marks `Stopped` (`209`) even though a boot in progress may run the full `READY_TIMEOUT` = 30s with a live child (reaped by `kill_on_drop` at process exit). Correctness/reporting smell, not a leak.

### 🟢 L — `is_blocked_ip` reserved-range gaps

`lib.rs:139-164`. Misses IPv4 `240.0.0.0/4` (class-E; only `255.255.255.255` caught), `198.18.0.0/15` (benchmarking), `192.0.0.0/24` (IETF); IPv6 `2001:db8::/32` (documentation) and deprecated IPv4-compatible `::a.b.c.d` (only IPv4-_mapped_ `::ffff:` is folded). Low real-world impact but this is the security boundary.

### 🟢 L — CF challenge WebView can leak if the builder times out

`cloudflare.rs:363-397`. `solve_in_webview` only `close_window`s after `build_challenge_window` returns `Ok`; that waits on `rx.recv_timeout(10s)` (`395`). If the window build completes just after the timeout, the function returns `Err` and the created hidden window is never closed (persists for the app's lifetime). Best-effort `close_window` on the timeout path too.

### ✅ Verified clean (native)

No command injection — `spawn_engine` uses `Command::new(&cfg.java).arg(...)` with no shell (`suwayomi.rs:257-276`). No host injection via `suwayomi_image` path — the authority is terminated by the literal `http://127.0.0.1:{port}` prefix before `{path}`. `unwrap`/`expect` are on deterministic client construction, static headers, JNI pointers; with no `panic = "abort"`, a JVM-thread panic unwinds + degrades via the readiness timeout rather than killing iOS. `kill_on_drop(true)` + explicit `stop_child` prevent orphaned JVM children (asserted by an ignored integration test).

---

## 9. Admin app

Files under `apps/admin/src`.

### 🟠 H1(admin) — Catalog dashboard renders the entire library unbounded; provenance silently truncated at 200

`routes/+page.svelte:147-152,26-33,353`, `lib/data.ts:31-41`. `loadCatalog('')` returns `backend.library()` (or `search('')`) — the **whole** catalog as one array — rendered via `{#each series as s}`, each with a lazy `<img>`; no `page` state, no pager. After a production ingest of thousands of works, opening `/` renders thousands of DOM rows → severe jank / hung tab. Separately, `loadProvenance` does `list.slice(0, 200)`, so rows 201+ get `provenance[s.id] === undefined`: their Source column shows "—" and Merge/workId features silently disappear with no "truncated" indicator, making the admin believe those works have no source mappings.
**Fix:** paginate the catalog query; fetch provenance per rendered page; or virtualize. At minimum surface a "showing first N" notice past the provenance cap.

### 🟡 M — Users page double-fetches on every pagination click

`routes/users/+page.svelte:21-24,34,140-146`. The load effect `$effect(() => { if (!auth.user) return; void refresh(page); })` **tracks `page`**, but `refresh(p)` also **writes** `page = res.page` (`34`) and the pager buttons call `refresh(page ± 1)` **directly** (`140/144`). Each navigation runs `refresh` twice (click, then effect re-fire) → two identical `loadUsers` round-trips that can resolve out of order + flicker. Converges, not infinite.
**Fix:** follow the updates page (`updates/+page.svelte:39-43`) — pager only sets `page`, effect does the single fetch; stop reassigning `page` in `refresh`.

### 🟡 M — Extension uninstall has no confirmation

`routes/sources/+page.svelte:963-968,122-147`. `Uninstall` calls `extAction(e, 'uninstall')` immediately, no `confirm()` — every other consequential action is guarded (ban `users:51`; merge modal `+page.svelte:504`; persist two-step `sources:817`). A misclick uninstalls the wrong source (then invalidates the catalogue picker, `sourcesLoaded = false`); reversible only by reinstall + re-ingest.
**Fix:** `confirm()` (or the inline persist-style affordance) before uninstall.

### 🟢 L — Review resolution removes the row optimistically regardless of server result

`routes/review/+page.svelte:57-59`. `resolveMergeCandidate` returns a boolean (whether the row was actually closed) but the code always `queue = queue.filter(...)`, ignoring it. On a concurrent-resolve race the server returns `false` (already claimed) yet the UI shows resolved. Self-corrects on Refresh. Honor the boolean.

### 🟢 L — Merge success refetches provenance but not the series list

`routes/+page.svelte:122-128`. After `mergeWorks`, only `loadProvenance(series)` re-runs; the `series` array isn't refetched, so the deleted work's row remains (phantom). Re-run `refresh(query)` or drop the merged row locally.

### 🟢 L — No centralized route guard; per-page redirect duplication + content flash

`routes/+layout.svelte:67-69` (+ duplicated `$effect` redirect in `+page.svelte:137`, `users:17`, `review:14`, `updates:17`, `sources:30`). The layout renders `{@render children()}` regardless of `auth.user`; each page separately runs `if (auth.ready && !auth.user) goto('/login')`. Server gating makes it safe (no leak — queries/mutations 401) but it's fragile duplication with a brief empty-page flash; a new route that forgets the effect renders unguarded. Centralize in the layout.

### 🟢 L — "Add" (Tier-2) button lacks a concurrency guard

`routes/+page.svelte:186-196`. `onAdd` sets `addingId = s.id` without `if (addingId) return`, so starting Add on A then B re-enables A's button mid-flight. Harmless (idempotent server-side) but inconsistent with other single-flight guards (`unpause:53`, `extAction:127`).

### ✅ Verified clean (admin)

**Authorization is server-side enforced, not just UI-hidden** — every admin mutation begins with `require_admin(ctx)` (`graphql/mod.rs` lines 1775, 1807, 3327, 3417, 3486, 3506, 3591, 3613, 3706, …); non-admin tokens are rejected. Self-protection guards exist (can't ban self `3330`, can't ban another admin `3343`, can't demote self `3419`). A normal user calling `banUser`/`mergeWorks`/`persistCatalogue` directly is rejected — no auth-bypass. No `{@html}` anywhere; source/extension metadata renders as text; cover/icon URLs use `referrerpolicy="no-referrer"`. Search inputs are submit-driven (no per-keystroke fetches); extension filtering is a client-side `$derived`. Editor validates interval/poll numerics before mutation. Sources page stops both ingest pollers on unmount, with bounded exponential backoff + give-up + generation tokens guarding source-switch races. `void`-ed async calls each have internal try/catch (no unhandled rejections). The work-merge flow has an explicit two-step target-pick + "DESTRUCTIVE · IRREVERSIBLE" modal showing both titles + work IDs.

---

## 10. Cloudflare Worker / image pipeline

Files: `apps/worker/src/index.ts`, `wrangler.toml`, `packages/api/src/image-provider.ts`, `packages/types/src/index.ts`, `apps/reader/src-tauri/src/cloudflare.rs` + `lib.rs` (native contrast). Architecture note: **proxy-only by design — no object storage; do not add one.** Litestream→R2 is DB backup, not images.

### 🟠 H3 — Redirect following bypasses the host allowlist (SSRF / open proxy / cache poisoning)

`index.ts:68-99` (allowlist at `68`, `redirect:'follow'` at `98`). The Worker validates `upstream.hostname` against `ALLOWED_SOURCE_HOSTS` **once**, then `fetch(upstream, {redirect:'follow'})` — every subsequent hop is unchecked. An allowlisted upstream returning a 3xx to any host is followed and re-served under the trusted proxy origin with `Access-Control-Allow-Origin: *` and a 7-day `immutable` cache, and stored **under the original allowlisted URL's key** (`80-83,120`) → also poisons that key. Not theoretical: the shipped allowlist (`wrangler.toml:24`) includes `mangadex.network`, whose @Home nodes are community-operated — a malicious node can 302 the Worker to an arbitrary host, laundering third-party content through `img.komika.app`. The **native side already does this correctly** — `lib.rs:45` uses `redirect::Policy::none()` + a manual loop (`89-105`) that re-runs `validate_image_url` on every hop (comment: "a 3xx to `http://169.254.169.254/…` must not slip past the host guard"). Classic internal-network SSRF isn't reachable from Workers `fetch`, but open-proxy + cache-poisoning + CORS laundering are.
**Fix:** `redirect:'manual'`, follow yourself re-running `hostAllowed()` on each hop's resolved `Location` (bounded hop count), mirroring `fetch_image`.

### 🟡 M — `image/svg+xml` accepted → stored XSS on the proxy origin

`index.ts:155-157` (`isImageContentType`), served `128-136`. `isImageContentType` allows any `image/*`, incl. `image/svg+xml` — an active document that executes embedded `<script>` in the `img.komika.app` origin when opened as a document. The Worker sets no `X-Content-Type-Options: nosniff` and no `Content-Disposition`, and the malicious SVG is cached `public, max-age=604800, immutable`. Combined with H3 (or any allowlisted host serving SVG), persistent script execution on the proxy subdomain.
**Fix:** reject `image/svg+xml`; add `nosniff` + `Content-Disposition: inline`/`attachment`.

### 🟡 M — No effective hotlink protection or rate limiting → free open proxy / DoS amplifier

`wrangler.toml:33` (`ALLOWED_ORIGINS = ""`), `index.ts:187-198`. The shipped config sets `ALLOWED_ORIGINS = ""` and `originAllowed` returns `true` for an empty list (`189`) — hotlink protection **off in committed config**. Even when configured, `originAllowed` returns `true` whenever the request has no `Origin` **and** no `Referer` (`193`) — any non-browser client (curl/script) trivially omits both. No auth, no rate limiting anywhere → anyone can use `img.komika.app` as an unlimited free proxy for allowlisted hosts (and, via H3, beyond) and as a DoS amplifier against MangaDex.
**Fix:** ship a non-empty `ALLOWED_ORIGINS` for prod + a Cloudflare Rate Limiting rule keyed on client IP (not object caching).

### 🟢 L — No upstream response-size cap

`index.ts:119-121`. Streamed (`new Response(source.body)`), so Worker memory is safe, but there's no cap on upstream body size before it's streamed to the client and written into the edge cache via `waitUntil(edge.put(...))`. A hostile allowlisted host pushes very large bodies (bandwidth/cache abuse). Native caps at 32 MiB (`lib.rs:33`). Reject when upstream `Content-Length` exceeds a sane image ceiling.

### 🟢 L — Upstream `Content-Length` copied onto a re-served body

`index.ts:134-135`. `finalizeImage` copies upstream `Content-Length` verbatim onto the new streamed response; if upstream applied transfer-encoding/compression the declared length can differ from delivered bytes → client sees a truncated/stalled response. Don't forward `Content-Length` on a re-streamed body.

### ℹ️ Info — Unsafe-if-unoverridden prod defaults

`config.ts:12` (`imgWorkerBaseUrl` defaults to `http://localhost:8787`), `wrangler.toml:33` (`ALLOWED_ORIGINS=""`). Dev-appropriate but foot-guns in prod if env vars aren't set. Add a deploy-time assertion.

### ✅ Verified clean (worker) + type contract

Genuine streaming pass-through (no in-memory buffering → 128 MB limit not at risk); errors not cached; cache key correctly normalized; allowlist fails closed (empty `ALLOWED_SOURCE_HOSTS` denies all); http(s)-only; rejects non-image upstreams (content-laundering guard). **Type contract:** `Page.sourceUrl` is `String!` in the SDL (`komika.graphql:212`), required `string` in `types/index.ts:480`, backed by `types.rs:161` — consistent, no optional/required drift on the page-image path; empty-string handled by both providers (`image-provider.ts:53,166`). `WorkSource.sourceUrl` / `SourceBrowsePage.sourceUrl` are nullable but are series/browse URLs, not Worker inputs.

---

## 11. Prioritized remediation

**Fix first (release-blocking):**

| Rank | ID  | Fix                                                                                    | Effort  |
| ---- | --- | -------------------------------------------------------------------------------------- | ------- |
| 1    | C1  | Bind offline-queue ops to `userId`; drop non-matching on drain / clear on logout       | Small   |
| 2    | H1  | `image::Limits` (dimensions + `max_alloc`) before decode in `avatar.rs` + `media.rs`   | Small   |
| 3    | H2  | Suwayomi client `connect_timeout` + `timeout` (copy MangaDex)                          | Trivial |
| 4    | H3  | Worker `redirect:'manual'` + per-hop `hostAllowed` (copy native `fetch_image`)         | Small   |
| 5    | H4  | Pin validated IP in native `fetch_image` (`ClientBuilder::resolve` / custom connector) | Small   |
| 6    | H5  | Cap `workSourcesBatch` input ≤200 + use `IN(...)` batch loader                         | Trivial |
| 7    | H7  | `.limit_depth()` + `.limit_complexity()` on the GraphQL schema                         | Trivial |
| 8    | H8  | Comment-media GC task + staged-per-user cap                                            | Small   |
| 9    | H6  | Wrap dedup resolve+create+link in a transaction / natural-key claim                    | Medium  |
| 10   | H9  | Prefix-anchor or FTS-index the fuzzy-block `LIKE`                                      | Medium  |

**Fix before scale (Medium):** session-token hashing at rest; N+1 batching in `map_series`/`library`; per-user write/upload rate limits + body-length caps; error-string sanitization; NSFW fail-closed; Suwayomi per-record parse; reader cache TTL + single-flight; background-loop supervision; `merge_works` orphan reconciliation; scanner `known_max` healing + set-diff new-chapter detection; composite-backend capability + `filters` forwarding; native image concurrency cap + percent-decode path guard + CF-shim concurrency; reader stale-async guard + progress-on-exit + `onimg-error`; browse "Newest" real timestamp; SearchOverlay filters; admin catalog pagination + provenance-truncation notice + users double-fetch + uninstall confirm; Worker SVG reject + hotlink/rate-limit.

**Low:** pagination tiebreakers; mutex-poison recovery everywhere; `to_iso` `checked_mul`; case-insensitive usernames; `get_manga_by_ids` chunking; assorted UI keying/leaks/dead-controls; `is_blocked_ip` range gaps; native single-boot self-heal.

---

_This document reflects a point-in-time read-only audit. Line numbers may drift as the tree changes; each finding names the function/symbol to re-anchor._
