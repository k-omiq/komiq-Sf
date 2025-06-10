# Komika — Full Bug & Gap Audit Findings

> Read-only static audit of the Komika codebase against its intended functionality
> (SPEC.md, CATALOGUE.md, PRODUCTION.md, deploy/README.md). Produced by 11 parallel
> domain auditors; every blocker/high finding was then re-tested by an independent
> skeptic prompted to **refute** it. No product code, servers, or deploys were touched.
>
> **Severities are post-verification.** Where verification changed a rating, both the
> original and final severity are shown (e.g. `high → medium`). Two originally-High
> findings were downgraded/refuted on verification — noted inline.

## Contents

1. [Verification verdict summary](#1-verification-verdict-summary)
2. [Biggest gaps vs the product vision](#2-biggest-gaps-vs-the-product-vision)
3. [Cross-cutting interactions](#3-cross-cutting-interactions)
4. [Domain 1 — Auth & sessions](#domain-1--auth--sessions)
5. [Domain 2 — MangaDex sync & canonical page fetch](#domain-2--mangadex-sync--canonical-page-fetch)
6. [Domain 3 — Deploy & production readiness](#domain-3--deploy--production-readiness)
7. [Domain 4 — Dedup / canonical model](#domain-4--dedup--canonical-model)
8. [Domain 5 — NSFW gating](#domain-5--nsfw-gating)
9. [Domain 6 — Image pipeline](#domain-6--image-pipeline)
10. [Domain 7 — Scanner / catalogue monitoring](#domain-7--scanner--catalogue-monitoring)
11. [Domain 8 — Social layer](#domain-8--social-layer)
12. [Domain 9 — Canonical reader path](#domain-9--canonical-reader-path)
13. [Domain 10 — Contract & schema consistency](#domain-10--contract--schema-consistency)
14. [Domain 11 — Admin console](#domain-11--admin-console)
15. [Global "could not verify statically"](#15-global-could-not-verify-statically)
16. [Methodology & coverage](#16-methodology--coverage)

---

## 1. Verification verdict summary

Every blocker/high finding, with the independent skeptic's verdict and final severity.

| #   | Finding                                                             | Original    | Verdict                                          | Final                                |
| --- | ------------------------------------------------------------------- | ----------- | ------------------------------------------------ | ------------------------------------ |
| A1  | Session tokens never expire                                         | high        | CONFIRMED                                        | **high**                             |
| A2  | Auth rate-limiter bypassable via spoofed `X-Forwarded-For`          | high        | CONFIRMED                                        | **high**                             |
| M1  | `/at-home` 40/min budget not enforced (shares 5 req/s bucket)       | high        | CONFIRMED (2 auditors)                           | **high**                             |
| M2  | No 429/5xx retry/backoff; default rate on the ban threshold         | high        | CONFIRMED                                        | **medium-high**                      |
| M3  | Chapters can be permanently missed                                  | medium/high | CONFIRMED                                        | **medium**                           |
| D1  | Committed admin password used unattended by first-run `./deploy.sh` | high        | CONFIRMED (undocumented)                         | **high**                             |
| D2  | Unauthenticated Suwayomi admin API on `0.0.0.0:4567`                | high        | CONFIRMED (documented known-limitation)          | **medium** (high if internet-facing) |
| DD1 | Dedup external-ID + cover-pHash rungs dead in the real add flow     | high        | CONFIRMED                                        | **medium**                           |
| DD2 | `add_source_series` not idempotent (orphan work / dup candidate)    | high        | CONFIRMED (conditional)                          | **medium-high**                      |
| SC1 | `poll_every_minutes` re-poll behavior unimplemented                 | high        | CONFIRMED                                        | **medium**                           |
| SC2 | Scanner covers only Suwayomi, not MangaDex canonical works          | high        | **REFUTED as a defect**                          | info/low (intended)                  |
| N1  | Tier-2 add hardcodes `is_nsfw=false`                                | blocker     | CONFIRMED (latent — behind unreachable mutation) | **medium** (high once wired)         |
| N2  | Suwayomi `series`/`chapters`/`pages`/`library` resolvers ungated    | medium      | CONFIRMED (code defect)                          | **medium** (practical low today)     |
| I1  | Worker ships as an open proxy by default                            | high        | CONFIRMED                                        | **medium**                           |
| I2  | Native `fetch_image` unvalidated (client-side SSRF)                 | high        | CONFIRMED                                        | **medium**                           |
| C1  | `addSourceSeries` has no client binding → dead review queue         | medium      | CONFIRMED                                        | **medium**                           |

---

## 2. Biggest gaps vs the product vision

1. **The Tier-2 "curated add" pillar is not wired to any client.** `addSourceSeries` — the
   mutation that runs the dedup matcher and fills the merge-review queue — exists in the SDL
   (`packages/api/src/schema/komika.graphql:61`) and server (`apps/server/src/graphql/mod.rs:1500`)
   but has **no** `operations.ts` document, no `Backend` method, no `MatchResult` type, and no admin
   UI caller. CATALOGUE.md M5 marks this "✅ Done … threaded through @komika/api + @komika/types." It
   is not. Consequence: an admin cannot add an extension series through the app, the dedup-review
   console is permanently empty, and three other confirmed defects (DD1, DD2, N1) all sit _behind_
   this unreachable mutation — latent today, live the moment it is wired.

2. **"Universal chapter updates from extensions AND MangaDex" is really two disjoint mechanisms.**
   The adaptive scanner (avg cadence, admin override, auto-pause) covers only the Suwayomi library;
   MangaDex-mirrored works are refreshed by a separate flat 6h loop with no per-series
   cadence/override/pause — and that loop, plus the entire canonical catalogue, ships **off by
   default** (`CATALOGUE_SYNC=false`). Update _delivery_ is unified at the UI (two feeds merged), but
   the adaptive guarantees are Suwayomi-only. This split is documented and intended (verification
   refuted it as a defect), so it is a clarity gap, not a bug.

3. **MangaDex proxy-compliance is under-enforced on the newest (canonical) reading path** — the two
   things §9 insists on most: the `/at-home` 40/min budget is not enforced (shares the 5 req/s bucket
   = 7.5× over), there is no 429 backoff, and the default rate sits exactly on the ban threshold.

4. **Production defaults are unsafe out of the box.** The one-command deploy provisions an admin with
   a repo-committed password (unattended, undocumented) and publishes the unauthenticated Suwayomi
   admin API on all interfaces; sessions never expire and the auth rate-limiter is trivially bypassed.

5. **NSFW gating has a hole on the federation path** (write-side never sets the flag on Tier-2 adds;
   read-side never checks it on `series`/`chapters`/`pages`/`library`). Both halves are latent today
   only because the flag is virtually never set — they activate together once Tier-2 add is wired.

---

## 3. Cross-cutting interactions

- **The `addSourceSeries` keystone.** DD1, DD2, N1 and C1 are the same root story from different
  angles: the Tier-2 add flow is implemented server-side but unreachable from any client. C1 makes
  the others latent (nothing in-app triggers them); the day someone wires `addSourceSeries` to the
  admin UI, DD2 (orphan works), DD1 (title-only matching), and N1 (NSFW mis-flagging) all become live
  in the same change. **Fix them together with the wiring, not after.**

- **NSFW write/read halves (N1 + N2) are complementary.** N1 = the flag is never _set_ on Tier-2
  writes; N2 = the flag is never _checked_ on Suwayomi reads. N2's ungated resolvers would leak
  precisely the AutoMerge-flagged works that N1 leaves correct, while N1's `New`/`Review` works are
  unflagged and leak everywhere including discovery. Both are gated by the same unreachability today.

- **Auth A2 amplifies A4 (map growth).** Every spoofed `X-Forwarded-For` both bypasses the limiter
  and inserts a permanent map key — the bypass feeds the memory-exhaustion.

- **Verification cross-check (banned users).** The Social auditor flagged uncertainty about whether a
  banned user with a surviving session can still authenticate; the Auth auditor confirmed
  `user_for_token` filters `AND u.is_banned = 0` (`auth.rs:52`) **and** `ban_user` deletes sessions
  (`mod.rs:1437`). Net: a banned user cannot post new content, but their _existing_ comments still
  render (Social finding S1).

- **Fixing N1 worsens N2.** Today most Suwayomi works are `is_nsfw=false` (flag only set via the
  unreachable add flow), so the ungated resolvers have almost nothing flagged to leak. Correcting N1
  increases the set of flagged works and therefore the exposure through N2 — fix both together.

---

## Domain 1 — Auth & sessions

Files: `apps/server/src/auth.rs`, `graphql/mod.rs`, `main.rs`, `config.rs`,
`apps/reader/src/lib/auth.svelte.ts`, `routes/(app)/login/+page.svelte`, migrations `0001`/`0004`.

### A1 — [high] Session tokens never expire — no absolute or idle timeout · CONFIRMED

**Evidence:**

- `migrations/0001_init.sql:14-18` — `sessions` has only `token, user_id, created_at`; no expiry column.
- `graphql/mod.rs:1686-1697` (`new_session`) — inserts `token, user_id, created_at` only.
- `auth.rs:49-58` (`user_for_token`) — `WHERE s.token = ? AND u.is_banned = 0`; never consults `created_at`/TTL.
- Verification: full grep for `expir/ttl/reaper/cleanup/max_age` across `apps/server/src` is empty; the only `DELETE FROM sessions` are `logout` (self-token, `mod.rs:1323`) and `ban_user` (`mod.rs:1437`) — both event-driven, neither time-based. Test harness seeds a session dated `'2020-01-01T00:00:00Z'` that still authenticates in 2026 (`mod.rs:1769`). PRODUCTION.md:27 lists "Session hardening — expiry/rotation, logout-all, brute-force lockout" as an unchecked open item.
  **Failure scenario:** A token captured once (XSS/localStorage exfil, shared/library machine, leaked backup, intercepted request) stays valid forever. Only `logout` or an admin ban ends a session — no idle timeout, no rotation, no absolute lifetime.
  **Fix direction:** Add `expires_at` (+ optional `last_used_at`); set in `new_session`; add `AND s.expires_at > ?` to `user_for_token`; GC expired rows periodically.

### A2 — [high] `X-Forwarded-For` trusted unconditionally → auth rate-limiter trivially bypassable · CONFIRMED

**Evidence:**

- `main.rs:49-65` (`resolve_client_ip`) — takes the **leftmost** XFF value from any client with no trusted-proxy allowlist; the socket-peer fallback (`:64`) only engages when XFF is entirely absent.
- Limiter keyed on it: `mod.rs:1224` `format!("login:{}", client_ip(ctx))`, `:1263` `register:{ip}`. Login is deliberately keyed on IP only (comment `:1221-1223`, to avoid victim lockout) — so per-IP is the only brute-force throttle.
- Shipped topology: `deploy/docker-compose.yml:84-85` publishes `server` `8080:8080` directly; `deploy/nginx.conf` is only the reader SPA's static file server (no `proxy_pass`, no `real_ip`/XFF handling). No `tower-governor` layer (checked `Cargo.toml` + `main.rs` layers). PRODUCTION.md:23 lists WAF/rate-limiting/bot-mitigation as unchecked.
  **Failure scenario:** Attacker brute-forces `login` (and `register`) by sending a fresh `X-Forwarded-For: <random>` per request → a new rate-limit bucket every time. Because the code reads the _leftmost_ entry, even a conventionally-appending reverse proxy doesn't help — only one that _overwrites_ XFF would.
  **Fix direction:** Honor XFF/X-Real-IP only when the socket peer is a configured trusted-proxy CIDR; otherwise use `peer.ip()`. Default to socket-peer since the shipped compose exposes 8080 directly.

### A3 — [medium] Username enumeration via login timing side-channel · reported

`mod.rs:1230-1244` — `verify_password` (Argon2id, tens of ms) runs only when the username row exists; a non-existent username returns fast. Error string is uniform (`"Invalid username or password"`) but response time is not; the distinct `"This account has been suspended."` (`:1248`) further leaks ban state once a valid password is known. → Always run an Argon2 verify against a fixed dummy hash on the missing-user path.

### A4 — [medium] Rate-limiter map grows unbounded — keys never evicted · reported

`mod.rs:48-69` — `is_limited`/`record` call `entry.retain(...)` to prune stale timestamps but never remove the map **keys** (`login:<ip>`, `register:<ip>`) even when their `Vec<Instant>` empties. Every distinct IP / spoofed XFF (see A2) permanently inserts a `String` key → steady RSS growth on a low-resource node, eventual OOM. No reaper, no cap. → Drop keys whose entry is empty after `retain`; or periodic sweep; or bounded LRU.

### A5 — [medium] Self-service admin claim window via `KOMIKA_ADMIN_USERS` · reported

`mod.rs:1284-1287` — `register` grants `is_admin` if the username is in `KOMIKA_ADMIN_USERS`, even if that name has not yet been provisioned. Startup promotion (`main.rs:147-153`) only promotes existing accounts. First person to register a listed-but-unregistered name (e.g. `admin`) on a fresh deploy becomes admin (races `bootstrap.py`). Removing a name never demotes. → Reserve configured admin names from open registration; only promote pre-existing accounts, or require a secret bootstrap.

### A6 — [low] Bearer token in localStorage · reported

`auth.svelte.ts:34-42` persists the token to `localStorage` (`TOKEN_KEY='komika-token'`). Any XSS reads it and, with A1 (no expiry), gains a permanent credential. Accepted trade-off for a static SPA + Bearer transport, but compounds A1. → Pair with server-side expiry/rotation.

### A7 — [low] No maximum password length · reported

`mod.rs:1278` checks only `len < 8`; `login` (`:1218`) has no length guard. A multi-MB password forces Argon2 to hash the whole input each attempt (CPU amplifier). → Cap ≤ 1024 bytes before hashing.

### Looks correct

- **Argon2id usage** (`auth.rs:21-38`) — `Argon2::default()`, random per-password `SaltString::generate(OsRng)`, PHC storage, non-panicking verify (`malformed_hash_is_rejected_not_panicked`).
- **Session token entropy** (`auth.rs:41-45`) — 32 bytes (256-bit) from `OsRng.fill_bytes`, hex-encoded.
- **Uniform credential error** — same string for unknown user and bad password (only timing leaks, A3).
- **Banned-user revocation (defense in depth)** — `user_for_token` filters `is_banned` (`auth.rs:52`) **and** `ban_user` deletes sessions (`mod.rs:1435-1442`); tests confirm.
- **Ban guardrails** — no self-ban (`:1414`), no ban-another-admin (`:1426`), no self-demote (`:1473`); `require_admin` chains `require_user` then `is_admin` (`:186-192`).
- **Failed-vs-successful rate accounting** — login checks `is_limited` before verify, only `record`s on failed creds; suspended rejection doesn't consume budget; register uses `check`.
- **No CSRF surface** — Bearer transport (`main.rs:38-44`), not cookies.
- **Anonymous handling** — `current_user`/`session()` fail-closed to `None`.
- **Registration validation** — username ≥ 3, email contains `@`, password ≥ 8, unique-violation mapped to a non-leaky message.
- **Security headers + explicit-list CORS** (`main.rs:234-251`, `:199-202`).

### Could not verify statically

Whether prod fronts the server with a proxy that strips/overwrites inbound XFF (decides A2 exploitability in prod); real Argon2 timing delta on target hardware (A3); actual RSS growth under sustained distinct-IP load (A4); whether an upstream body-size limit caps password length (A7); end-to-end token lifetime in practice (A1).

---

## Domain 2 — MangaDex sync & canonical page fetch

Files: `apps/server/src/mangadex.rs`, `config.rs`, `main.rs`, `graphql/mod.rs` (`canonical_pages`), migration `0006`, `phash.rs`. Limits (CATALOGUE.md §9): global ~5 req/s; `/at-home` 40/min; offset+limit ≤ 10,000; valid UA required, no Via; images must be proxied; the 5 req/s is a **fleet** budget (shared egress IP).

### M1 — [high] `/at-home` 40/min budget not enforced — shares the global 5 req/s bucket · CONFIRMED (flagged independently by the MangaDex and Canonical-reader auditors)

**Evidence:**

- `mangadex.rs:226-235` — `at_home` calls `self.limiter.acquire()` on the single shared `TokenBucket` (`:103`, `:113`); all of `list_manga`, `list_chapters`, `cover_phash`, `at_home` share it.
- `config.rs:96-100` — `mangadex_rate_per_sec` default `5.0` = 300/min = 7.5× the 40/min at-home ceiling.
- Comment `mangadex.rs:224-225` ("capped at 40/min; the global ~5 req/s bucket keeps us well under") is arithmetically inverted.
- `canonical_pages` calls `at_home` live per page-load (`graphql/mod.rs:830`); no cache in server (grep for `moka|lru|Cache|OnceCell|DashMap` empty) or Worker (`apps/worker/src/index.ts` only caches `/img` image bytes, never `/at-home` JSON). The client is always constructed and wired into `AppState` (`main.rs:160-168`) even with `CATALOGUE_SYNC=off`.
  **Failure scenario:** >40 chapter-opens/min (well within 5/s) breaches the at-home 40/min limit → 429; persistent abuse → 403/DDoS ban on the fleet-shared IP.
  **Fix direction:** Add a second dedicated at-home limiter (≤40/min) acquired in addition to the global 5/s bucket; ideally cache at_home per chapter for its TTL.

### M2 — [high → medium-high] No retry/backoff on 429/5xx; default rate sits on the ban threshold · CONFIRMED

**Evidence:**

- `mangadex.rs:148-150`, `:192-194`, `:233-235` — return `Err` on any non-success; no `Retry-After`, no backoff anywhere.
- `config.rs:100` default `5.0` = exactly MangaDex's 5 req/s ceiling, with a startup burst of 5 and no headroom; the budget is _shared_ with reader at_home traffic on one IP.
- `list_manga(...).await?` (`:523`) / `list_chapters(...).await?` (`:590-592`) propagate `Err`, unwinding the whole sweep.
  **Failure scenario:** Incremental cycles self-heal (cursor advances only on success `:682`; upserts idempotent) — a `updatedAtSince` cycle just retries the same window. But a single transient 429 mid **initial full seed** (cursor `None` → `Created` window `:673`) aborts the multi-hour walk, never sets the cursor, and the next cycle (~6h later) restarts from `createdAt`=0, re-hammering the shared IP. Sitting exactly on the ceiling makes recurring 429s plausible, so the seed can repeatedly die-and-restart.
  **Fix direction:** Target ~4 req/s; on 429/503 honor `Retry-After` + exponential backoff and retry the page instead of aborting.

### M3 — [medium/high → medium] Chapters seed can permanently miss chapters whose work wasn't catalogued yet · CONFIRMED

**Evidence:**

- `sync_chapters` skips a chapter whose manga isn't catalogued: `Ok(None) => continue` (`mangadex.rs:613`/`:616`), comment `:573-576` claims "a later catalogue sweep + re-run picks them up."
- The chapters cursor advances independently to `updatedAtSince` after the first successful chapters cycle (`:707`); cataloguing is Komika-internal and never bumps a chapter's MangaDex `updatedAt`, so an old (createdAt-ordered) chapter never reappears in an `updatedAtSince` window.
- Verification found an extra trigger: individual catalogue upsert failures are logged-and-skipped (`:539-541`) while the sweep still returns `Ok` and advances — so even a "successful" seed can leave manga uncatalogued, whose old chapters are then never retried. `sync_cycle` runs chapters **unconditionally** even when the catalogue arm erred (`:664-717`, `:686-688` just logs).
- _Nuance:_ newly-created works self-heal (a fresh chapter's `updatedAt == createdAt > cursor`); the permanent-miss set is specifically **old** chapters whose manga was uncatalogued during the chapters seed.
  **Fix direction:** Gate the chapters job on the catalogue seed having completed at least once, or re-seed chapters when the catalogue finishes its first full crawl.

### M4 — [medium] Token bucket is per-process, not fleet-wide · reported

The limiter is an in-memory `TokenBucket` owned by one `MangaDexClient` (`main.rs:160-163`). §9 says the 5 req/s is a fleet budget (shared egress IP). N replicas = N×5 req/s → 429/403. → Document/enforce single-replica for sync, or a shared (DB/Redis) limiter keyed by egress IP.

### M5 — [medium] No HTTP request timeout · reported

`mangadex.rs:107-110` builds the client with only `.user_agent(...)`; no `.timeout()`/`.connect_timeout()`. A stalled connection leaves the `await` in `list_manga`/`list_chapters`/`at_home` pending forever (stalls the sweep and, for at_home, a user request). → Set a per-request timeout (~30s).

### M6 — [medium] Initial seed is not resumable · reported

`sync_catalogue` holds `since`/`offset` in local state; `?` on the first page failure (`:523`) discards all windowing; the cursor is written only after the whole function returns `Ok` (`:682`). A single failure at page 600 restarts from `createdAt`=0. Combined with M2, the first full seed may never complete on a large catalogue. → Persist the in-progress `since` window as a provisional cursor as it advances.

### M7 — [low] >9,900 records sharing one boundary second are silently dropped · reported

Window-slide guard `:562-566` (and `:648-652`): if `next_since == since` it stops the sweep (correct anti-infinite-loop choice), silently dropping everything past offset 9,900 in that tied second. Realistic only for pathological bulk imports (more likely on the `/chapter` firehose). → If it ever trips, fall back to a secondary tiebreaker and log at error.

### M8 — [low] Docstring/behavior mismatch on page-failure handling · reported

`:506-508` says "a failed page/record is logged and does not abort the sweep," but a `list_manga` page error propagates via `?` (`:523`) and does abort. Only per-record upsert errors are non-fatal (`:541`). → Correct the comment or make page failures retry (M2).

### M9 — [low, operational] Mirror is empty in the default deployment · reported

`CATALOGUE_SYNC` (`config.rs:90-95`) and `COVER_PHASH` (`:105-110`) default `false`. As shipped, nothing populates `work`/`chapter`, so canonical updates/search/reader navigation are empty and the cover-pHash signal is never computed. Matches documented "off by default" intent, but the whole Tier-1 mirror is inert unless both flags are set.

### Looks correct

- **Paging past the 10k cap** — offset resets per window, `createdAtSince` advances to the last item's ts; `WINDOW_OFFSET_CAP = 9_900` (`:26`).
- **Window boundary — no skip** — inclusive slide (`since = last item ts`) + idempotent `upsert_work_from_mangadex`/`upsert_chapter`.
- **Token-bucket burst/refill math** — `capacity=refill=rate`, refills `elapsed*rate` capped at capacity, consume 1 (`:66-96`).
- **Cursor advances only on success, per job** — `set_sync_cursor` only in each `Ok` arm (`:682`, `:707`).
- **English-only** — source filter `translatedLanguage[]=en` (`:180`) + defensive re-guard (`:604`).
- **User-Agent present, no Via** (`:108`, `config.rs:101-104`).
- **pHash-on-ingest wiring** — `COVER_PHASH` → `catalogue_cover_phash` → `sync_catalogue(cover_phash)` → dHash over the 512px thumbnail → `COALESCE`-guarded upsert (`phash.rs:14-31`), gated off by default.

### Could not verify statically

Deployment replica count / egress topology (M4 liveness); actual MangaDex 429 behavior at exactly 5 req/s (M2 frequency); whether any layer caches at_home (found none — worsens M1).

---

## Domain 3 — Deploy & production readiness

Files: `deploy/` (docker-compose.yml, deploy.sh, bootstrap.py, server-entrypoint.sh, litestream.yml, .env.example, nginx.conf, Dockerfiles, README.md), `apps/server/src/config.rs`+`main.rs`, PRODUCTION.md, `.github/workflows/ci.yml`.

### D1 — [high] One-command `./deploy.sh` provisions the admin with a repo-committed password, unattended · CONFIRMED (and _not_ documented)

**Evidence:**

- `deploy/.env.example:16` — `KOMIKA_ADMIN_PASSWORD="change-this-admin-pw"` (committed).
- `deploy/deploy.sh:32-36` — if `.env` is missing, copies the template + prints a yellow warning, but **does not exit**: falls through to `$DC up --build -d` (`:52`) and `python3 bootstrap.py` (`:64`). The `case` (`:41-48`) only exits for `down`/`destroy`/`logs`/`bootstrap`; `up`/`""` is a bare no-op (`:46`).
- `bootstrap.py:42` — `ADMIN_PW = ENV.get("KOMIKA_ADMIN_PASSWORD", "change-this-admin-pw")`; `create_admin()` (`:131-144`) registers `admin` with it. Server accepts it (only `len>=8`; `mod.rs:1278-1280`). The account gets `is_admin` (matches default `KOMIKA_ADMIN_USERS=admin`, `docker-compose.yml:72`) and the server port binds `0.0.0.0:8080` (`:84-85`).
- Not tracked in PRODUCTION.md (which lists only `KOMIKA_ADMIN_USERS` under secrets, `:25`).
  **Failure scenario:** An operator runs the advertised `./deploy.sh` on a public host; on first run the stack bootstraps end-to-end with `admin` / `change-this-admin-pw` (public in the repo) before the warning is read. That account gates `updateSeriesAdmin`, `setUserAdmin`, ban, etc.
  **Fix direction:** After creating `.env`, halt (exit non-zero) and require the operator to edit it; bootstrap should hard-fail on the sentinel value.

### D2 — [high → medium (documented known-limitation; high if internet-facing)] Default compose publishes the unauthenticated Suwayomi admin API on `0.0.0.0:4567` · CONFIRMED

**Evidence:**

- `docker-compose.yml:33-36` — `ports: - '${SUWAYOMI_PORT:-4567}:4567'` (no `127.0.0.1`); `TACHIDESK_SERVER_IP: 0.0.0.0` (`:29`). No reverse proxy in the stack.
- Default `PUBLIC_KOMIKA_IMG_MODE=direct` (`:105`) + `SUWAYOMI_PUBLIC_URL=http://${PUBLIC_HOST}:${SUWAYOMI_PORT:-4567}` (`:70`) hand the browser `:4567` directly, so the port must be public for images.
- Documented: PRODUCTION.md:24 `[ ] Suwayomi not publicly exposed …`; README.md:142-146 warns the API is unauthenticated; compose comment `:34-35` flags it.
  **Failure scenario:** A real internet-facing deploy exposes the full Suwayomi GraphQL admin surface (add/remove sources, change settings, trigger downloads, enumerate library) with no auth. Reclassified to medium because it is honestly disclosed/tracked, not hidden — high if deployed as-is to the internet.
  **Fix direction:** Ship a restricting reverse proxy (allow only thumbnail/page routes), or default to the Worker image path and bind 4567 to loopback.

### D3 — [medium] Graceful shutdown handles SIGINT only, but PRODUCTION.md claims SIGINT/SIGTERM · reported

PRODUCTION.md:53 claims both; `main.rs:288-292` `shutdown_signal()` awaits only `ctrl_c()` (SIGINT). `docker stop`/compose-down send SIGTERM → the graceful drain + `shutdown_tx.send(true)` (stops scanner/catalogue tasks) never fires; in-flight requests dropped on every restart. (Data safe: Litestream is PID 1 under `-exec` and flushes on its own SIGTERM.) → Add a `unix::SignalKind::terminate()` branch, or correct the doc.

### D4 — [medium] GraphiQL IDE + introspection served publicly in prod · reported

`main.rs:99-101,205` mounts `/graphql` `GET` → full GraphiQL unconditionally; `build_schema` (`mod.rs:110-114`) never calls `.disable_introspection()`. Port is public. → Gate GraphiQL behind a dev flag; disable introspection in prod.

### D5 — [medium] Continuous backup OFF by default — silent data-loss risk on the "production" default · reported

`.env.example:37-41` all `LITESTREAM_*` commented; `server-entrypoint.sh:19-30` runs `komika-server` plain (backup off) with a stdout-only warning; README.md:72-73 confirms. The default stack stores accounts, argon2 hashes, session tokens, reviews, comments, scan-state on a single unbacked `server-data` volume; a volume loss or `./deploy.sh destroy` (`docker-compose.yml:43`) is unrecoverable. → Print a prominent "NO BACKUP CONFIGURED" banner at the end of `deploy.sh`.

### D6 — [medium] Canonical catalogue feature ships dark; enable-flags absent from `.env.example` · reported

`CATALOGUE_SYNC`/`COVER_PHASH` default `false` (`config.rs:90-95,105-110`); `main.rs:180-190` only spawns sync when enabled; `deploy/docker-compose.yml` sets neither and `.env.example` never mentions them. An operator can't enable the feature without reading `config.rs`. Reader degrades gracefully (`source.ts:231` `canonicalUpdates?.().catch(()=>[]) ?? []`). → Document `CATALOGUE_SYNC`/`COVER_PHASH`/`CATALOGUE_SYNC_INTERVAL_SECS`/`MANGADEX_USER_AGENT` in `.env.example`.

### D7 — [low] CI does not gate the deploy stack · reported

`.github/workflows/ci.yml` builds the SPAs + server binary and runs fmt/clippy/prettier, but nothing builds the Docker images, runs `docker compose config`, lints `bootstrap.py`/entrypoint, or runs `cargo/pnpm audit`; `e2e`/`lighthouse` are `workflow_dispatch`-only. A broken Dockerfile/compose/bootstrap passes CI green. (Tracked `[ ]` PRODUCTION.md:66-68.) → Add an image-build + `docker compose config` job.

### D8 — [low] deploy.sh runs bootstrap even if services never become healthy · reported

`deploy.sh:55-64` — the health-wait loop breaks on all-healthy or simply falls through after ~5 min, then unconditionally runs bootstrap → confusing partial failures on a persistently unhealthy stack. → `die` with the offending service names instead.

### Looks correct

- **CORS** (`main.rs:194-202`) — `AllowOrigin::list` over an explicit list, no wildcard, no `allow_credentials(true)`; reader uses Bearer not cookies.
- **Internal-vs-public Suwayomi split** — federation uses internal `suwayomi:4567`; browser image URLs rewritten to `SUWAYOMI_PUBLIC_URL`; internal host never handed to the browser.
- **Litestream restore/backup preconditions** — restore before exec with `-if-db-not-exists -if-replica-exists` (`server-entrypoint.sh:14`), then `replicate -exec`; WAL enabled (`db.rs:18`); partial-config warns loud and disables; arch resolved via `dpkg --print-architecture`.
- **Secrets hygiene** — `.gitignore` ignores `.env`/`.env.*` except `!.env.example`; no real `.env` tracked; no baked secrets; non-root uid 10001.
- **bootstrap.py idempotency** — repo-add/extension-install/register treat already-present as success.
- **Defense-in-depth headers** — nginx (`nginx.conf:15-30`, strict CSP) + server (`main.rs:234-251`).

### Could not verify statically

Litestream final-snapshot flush within the 10s grace on `docker stop`; the documented backup→wipe→restore MinIO drill (no CI exercises it); whether Suwayomi `:stable` honors the exact bootstrap GraphQL shapes; real Core Web Vitals (Lighthouse is a stub).

---

## Domain 4 — Dedup / canonical model

Files: `apps/server/src/dedup.rs`, `phash.rs`, `catalog/{mod,normalize,similarity}.rs`, `mangadex.rs` (upsert), `graphql/mod.rs` (add flow + review), migrations `0005`/`0006`. **All add-flow findings live behind the in-app-unreachable `addSourceSeries` (C1).**

### DD1 — [high → medium] External-ID (rung 1) and cover-pHash (rung 4) corroboration are dead in the real add flow · CONFIRMED

**Evidence:**

- `graphql/mod.rs:1513-1521` — the only production caller of `dedup::resolve` builds a `Candidate` with `external_ids: Vec::new()`, `cover_phash: None`, `year: None`, `alt_titles: Vec::new()`.
- `SuwayomiManga` (`suwayomi.rs:40-56`) carries no external IDs / year / pHash; `MANGA_FIELDS` (`:12-18`) never requests them. `add_source_series` hardcodes `cover_phash: None` (`:1519`) though `thumbnail_url` is in hand.
- `find_work_by_external` (rung-1 query) has one caller (`dedup.rs:68`), reachable only when `external_ids` is non-empty — never. `phash_similarity` (`similarity.rs:103`) short-circuits to `None`. MangaDex sync never calls `resolve` (`mangadex.rs:610` uses `find_source_series_id`).
  **Failure scenario:** `resolve()` degrades to `0.6*title + 0.4*description-Jaccard` (+author/year boosters) for every Tier-2 add. §4's highest-precision rung (external-ID stop-on-hit) and "strongest cheap signal" (cover pHash) never fire. Buffered by the human review queue, so quality degradation, not silent corruption. The pHash half is the more damning (feasible — thumbnail + `crate::phash` exist — yet unwired); rung 1 is arguably unpopulatable from Tachiyomi `SManga` anyway.
  **Fix direction:** Hash the Suwayomi thumbnail into `Candidate.cover_phash`; thread any source tracker links into `external_ids`.

### DD2 — [high → medium-high (conditional)] `add_source_series` is not idempotent — orphan work + alias pollution + duplicate merge_candidate · CONFIRMED

**Evidence & trace:**

- `graphql/mod.rs:1500-1610` has no `find_source_series_id` pre-check (unlike the MangaDex path). New/Review branches `create_work` (`:1571-1575`) **before** `upsert_source_series` (`:1579`); `ensure_source_series` `ON CONFLICT` updates only `last_seen`/`is_nsfw`, **never** `work_id` (`catalog/mod.rs:536-537`).
- 1st add of a series: `resolve` → `New` → creates work W1 + alias; SS1 → W1.
- 2nd add (same `suwayomi_manga_id`): `resolve` finds W1 via `find_works_by_alias` (`dedup.rs:93`), `title_sim=1.0`. Split on corroboration:
  - **Description present & stable** → `description_similarity(x,x)=1.0` → score 1.0 ≥ HIGH → **AutoMerge{W1}** (no new work, no candidate; `upsert_source_series` returns SS1). Idempotent ✅ — the refutation holds here.
  - **Description None/thin** → corroboration skipped (`dedup.rs:163` needs both `Some`; pHash always `None` per DD1) → score ~0.6 → **Review{W1}**. The Review arm then: `create_work` mints **W2** + alias (own transaction, no rollback); `upsert_source_series(W2,…)` conflicts and keeps SS1→W1, discarding W2 → **W2 orphaned** in `work`+`work_alias`; `insert_merge_candidate` (`:1597`) inserts a new row unconditionally (`catalog/mod.rs:640-663`).
- N re-adds → N orphan works + N candidate rows. Extension sources frequently lack descriptions, so Case B is realistically reachable.
  **Fix direction:** Short-circuit at the top: if `find_source_series_id(...)` is `Some`, return the existing linkage without creating a work or candidate.

### DD3 — [medium] Common-title collision guard is implicit only; no generic-token stoplist · reported

§4 step 2 requires guarding common-title collisions. The only guard is the 0.6 score cap (`dedup.rs:170`) → Review. With DD1 removing cover corroboration, a third same-titled series with merely overlapping description could cross HIGH=0.85 via description-Jaccard + boosters (`corrob ≥ 0.425` auto-merges) → wrong auto-merge, reviewer never sees it. → Require cover-pHash corroboration for exact-title auto-merge, or route ultra-common normalized titles always to Review.

### DD4 — [medium] Fuzzy blocking keys on a single longest token → recall gaps · reported

`dedup.rs:101-106` blocks on only `longest_token(...)` → one `LIKE '%token%'` (`catalog/mod.rs:118-135`). When the discriminating token isn't the longest, the block returns nothing → `Decision::New` (`:107-109`) → a missed merge silently creates a duplicate canonical work. → Block on top-N longest tokens (union) or a trigram-indexed shortlist before deciding New.

### DD5 — [low] "MinHash" is actually exact shingle Jaccard · reported

`catalog/similarity.rs:17-55` — full `HashSet` intersection/union over word-3-shingles (char-3-gram fallback), no MinHash signatures. Functionally correct/deterministic, but §4/§10 name a different algorithm.

### DD6 — [low] `merge_candidate.method` labels drift from the documented enum · reported

`dedup.rs:127-131` emits `title_corroborated`/`fuzzy`/`external_id`; migration `0005:94` documents `external_id/title_exact/fuzzy/description/cover`. Free-text column, so no integrity break.

### DD7 — [low] Non-deterministic Review candidate among equal-scoring same-title works · reported

`dedup.rs:112-121` iterates a `HashSet`, keeps first-seen best on ties → the `work_id` surfaced to the reviewer is arbitrary across runs when several works share the exact normalized title and none corroborate. Review still fires; low impact.

### Looks correct

- **External-ID rung** stops on first hit, `score=1.0`/`method=external_id` (`dedup.rs:63-75`) — ordering right when populated.
- **Threshold banding** — `[0.85,1]→auto`, `[0.6,0.85)→review`, `[0,0.6)→new`; no gap/overlap.
- **Title-only never auto-merges** — caps at 0.6 → Review (tests `title_only_goes_to_review`, `title_plus_copied_description_auto_merges`).
- **MangaDex-sync upsert idempotency** — work `ON CONFLICT(id) DO UPDATE`, aliases `INSERT OR IGNORE` under `UNIQUE(work_id,normalized_title,lang)`, external ids `PK(provider,external_id)`, source_series `UNIQUE(source_type,source_id,source_key)`, chapter `UNIQUE(source_series_id,external_id)` — unit-tested (`upsert_is_idempotent_*`).
- **`work_external_id UNIQUE(provider,external_id)`** holds as the global identity key.
- **`phash_similarity`** guards missing/non-hex/length-mismatch (`similarity.rs:103-116`).
- **`resolve_merge_candidate`** — rejects re-resolving non-pending rows, repoints `source_series` on accept, deletes the provisional work only when orphaned (`graphql/mod.rs:1615-1682`).
- **Normalization** (`normalize.rs`) — lowercase, punctuation→space, season/part/roman-numeral suffix stripping with a leading-number guard, CJK pass-through.

### Could not verify statically

Whether distinct MangaDex works ever share an external id (would make `INSERT OR IGNORE` on `work_external_id` drop the second mapping); concurrent `add_source_series` races (compounds DD2); whether cover pHash is ever compared against a candidate in production (statically it is not).

---

## Domain 5 — NSFW gating

Files: migration `0007`, `graphql/mod.rs` (`setShowNsfw`, filtering, `viewer_show_nsfw`), `mangadex.rs`+`dedup.rs` (`is_nsfw` propagation), the Tier-2 add flow. Intent: single per-user `show_nsfw` (default off) hides anything flagged by either source-level (Keiyoushi `nsfw:1`) or series-level (MangaDex `contentRating ∈ {erotica,pornographic}`).

### N1 — [blocker → medium (latent; high once wired)] Tier-2 add hardcodes `is_nsfw=false`, never reads the source signal · CONFIRMED

**Evidence:**

- `graphql/mod.rs:1533-1534` — minted work `is_nsfw: false`; `:1586-1588` — `upsert_source_series(..., false)`.
- `suwayomi.rs:12-18` — `MangaFields` fetches `source { lang }` + `genre` but no NSFW indicator; `false` is an _unfetched_ value, not the only available one.
- No correction path: `create_work` mints a Suwayomi-native work with no MangaDex source; the sync (`mangadex.rs:530`) is keyed on MangaDex ids and never touches it; grep for `set_work`/`set_nsfw`/`update_work` is empty.
- Discovery/search/updates gate on `MAX(w.is_nsfw)` (`mod.rs:255`).
  **Failure scenario:** An admin adds a Keiyoushi `nsfw:1` series; `Decision::New` mints a permanent `is_nsfw=0` work → visible to every opted-out/anonymous viewer. `AutoMerge` onto an existing NSFW work is safe (flag preserved); an _accepted_ Review inherits the target's flag; but `New` and a _rejected_ Review leave a permanent `false` work. **Severity downgraded because `addSourceSeries` has zero client bindings (C1)** — no Suwayomi-native NSFW work can be produced through the app today; becomes a live blocker the instant Tier-2 add is wired.
  **Fix direction:** Fetch the source nsfw flag (`source { … isNsfw }` / manga `isNsfw`), carry it on `SuwayomiManga`, and OR it (plus any contentRating) into both `make_work().is_nsfw` and the `upsert_source_series` arg.

### N2 — [medium (practical low today)] Suwayomi detail/reader path entirely ungated · CONFIRMED

**Evidence:**

- `graphql/mod.rs:872-906` — `series`, `chapters`, `pages`, `library` apply no `filter_nsfw`/`is_nsfw` guard; the canonical equivalents all gate (`:783`, `:801`, `:822-828`).
- `series(id)` parses an arbitrary numeric id (`id.0.parse::<i64>()`); Suwayomi ids are sequential integers, trivially enumerable — a viewer doesn't need a filtered feed to obtain one.
  **Failure scenario:** An opted-out viewer with a numeric id can fetch full detail, chapter list, and **read page images** of an NSFW series; `library()` returns NSFW series to any viewer. Blast radius near-zero _today_ because almost no Suwayomi work is _flagged_ (flag only set via the unreachable add flow); grows precisely as N1 is fixed. A clear defense-in-depth gap vs the canonical path.
  **Fix direction:** Add the `is_nsfw && !viewer_show_nsfw(ctx)` guard to `series`/`chapters`/`pages` (return not-found like canonical) and `filter_nsfw` in `library`.

### N3 — [low] `updates`/`search`/`discovery` filter NSFW post-pagination → count skew · reported

`graphql/mod.rs:702-731` — `total` = raw `COUNT(*)` with no nsfw predicate, `has_next` from the raw id count, then `filter_nsfw` drops rows after the page slice (`:724`). For opted-out viewers `total` overstates and a page can return < PAGE_SIZE while `has_next_page=true`. Same shape in `search` (`:862-864`) and `discovery` (`:646-648`). Cosmetic. → Push the predicate into SQL like `canonical_updates` (`:759`).

### N4 — [low] `source_series.is_nsfw` is effectively write-only · reported

Every gating read uses `work.is_nsfw` (`MAX(w.is_nsfw)` `:255`, `canonical_updates` `:759`, `chapter_owner_is_nsfw` `catalog/mod.rs:301`). The column + `upsert_source_series(..., is_nsfw)` arg suggest source-level gating that doesn't exist; even a corrected add-flow that set only `ss.is_nsfw` would still leak. → OR the source signal into `work.is_nsfw`.

### N5 — [medium] Source-level NSFW gating (half of the §2 two-signal spec) is entirely unimplemented · reported (NSFW-skeptic adjacent finding)

**Broader root cause behind N1, distinct from the add-flow hardcode.** CATALOGUE.md §2 defines NSFW as flagged by **either** signal: source-level (Keiyoushi index `nsfw: 1`) **or** series-level (MangaDex `contentRating`). Only the series-level half exists (`mangadex.rs:395-398`). The source-level half is never ingested at all: `MangaFields` (`suwayomi.rs:12-18`) never requests any source/extension nsfw flag, so no code path can ever set `is_nsfw=true` for a Keiyoushi source. Consequently a whole extension source flagged `nsfw:1` upstream (or any NSFW-by-nature extension series **not** linked to a MangaDex work) is flagged `false` and therefore surfaced **unfiltered across `discovery` / `search` / `library` / `updates`** to opted-out and anonymous viewers — independent of the N1 add-flow bug and independent of whether the series ever goes through `addSourceSeries`. Today this is masked only because the mirror/add paths are dark (M9/C1); it is the design-level reason N1 and N2 both exist.
**Fix direction:** Ingest the source-level nsfw signal end-to-end — fetch it in the Suwayomi query, carry it on `SuwayomiManga`, and OR it into `work.is_nsfw` at every write path (not just the Tier-2 add flow). Without this, "hide anything flagged NSFW by either signal" is only half-true.

### Looks correct

- **MangaDex sync sets `is_nsfw` from contentRating** (`mangadex.rs:395-398`, erotica/pornographic → true), propagated to source_series via `ensure_source_series`; unit-tested (`:814-818`).
- **Default-off for anonymous** — `viewer_show_nsfw` returns false for `None` (`mod.rs:269-274`); `user_show_nsfw` `unwrap_or(0)`; `register` sets `show_nsfw:false` (`:1315`).
- **Canonical feeds/detail gated** — `canonical_updates` filters in SQL `(? = 1 OR w.is_nsfw = 0)` with LIMIT/OFFSET on the filtered set (no skew); `canonical_series`/`chapters`/`pages` all guard on `work.is_nsfw`.
- **"Unknown = safe" consistent** — NULL contentRating → false; uncatalogued Suwayomi → `COALESCE(MAX(...),0)`.
- **`suggestive` intentionally excluded** — crawl stores suggestive rows (`:134`) but `is_nsfw` covers only erotica/pornographic.

### Could not verify statically

Whether Suwayomi's schema exposes an NSFW flag to fetch (feasibility of the N1 fix); the `related` surface (no server resolver exists today); the "suggestive configurable" knob (unimplemented; `config.rs` has no such setting).

---

## Domain 6 — Image pipeline

Files: `apps/worker/src/index.ts`, `wrangler.toml`, `packages/api/src/image-provider.ts`, `apps/reader/src-tauri/src/lib.rs`, `apps/reader/src/lib/data/source.ts`. Intent: web → Cloudflare Worker proxy; native (Tauri) → direct Rust `fetch_image`, never the Worker; images never stored; host allowlist; MangaDex must be proxied.

### I1 — [high → medium] Worker ships as an open proxy by default · CONFIRMED

**Evidence:**

- `wrangler.toml:22` `ALLOWED_SOURCE_HOSTS = ""` and `:31` `ALLOWED_ORIGINS = ""` (only committed values; no `[env.production]` block; deploy is plain `wrangler deploy`, `package.json:8`, no `--var`/CI injection).
- `index.ts:155-157` — `hostAllowed`: `if (list.length === 0) return true;` (fail-**open**). `originAllowed` also returns true on empty list (`:172`); hotlink also bypassed by omitting Origin/Referer (`:176`).
  **Failure scenario:** The committed artifact is an open image proxy — anyone can `GET /img?src=<any public URL>` and have Cloudflare fetch + re-serve it with `Access-Control-Allow-Origin: *` and 7-day immutable cache → bandwidth/cost abuse, content laundering under komika's CF origin. **Downgraded to medium:** it's a documented insecure default (`wrangler.toml:13-14` "NOT recommended in prod"), on Cloudflare edge (can't reach operator LAN/metadata → not internal SSRF), and off the native critical path.
  **Fix direction:** Ship non-empty prod defaults (`uploads.mangadex.org,mangadex.network` + source hosts) via `[env.production]`, or make empty fail-**closed** in prod.

### I2 — [high → medium] Native `fetch_image` is an unvalidated fetch (client-side SSRF) · CONFIRMED

**Evidence:**

- `apps/reader/src-tauri/src/lib.rs:8-15` — `reqwest::get(&url)` on a JS-supplied `url`; no host allowlist, follows redirects (reqwest default ≤10), no timeout, `resp.bytes()` with no size cap (`:13`).
- Caller `NativeImageProvider.fetchBytes` (`image-provider.ts:61-65`) passes `page.sourceUrl`/cover URL straight through; those come from third-party source page data (the reader supports arbitrary Suwayomi sources). Tauri CSP governs the _webview_, not the Rust `reqwest`; `capabilities/default.json` grants the command but places no constraint on the `url` arg.
  **Failure scenario:** A malicious/compromised source hands the desktop app `http://127.0.0.1:…`/`http://192.168.x.x/…`/`http://169.254.169.254/…` → SSRF/LAN-probe from inside the user's network, with the response bytes as a feedback channel; redirect-follow bypasses any future allowlist; no timeout → hang; no size cap → memory exhaustion. Self-scoped (no shared server pivot) → medium.
  **Fix direction:** Validate the host against the allowlist before fetching; shared client with a timeout; re-validate each redirect hop's host (or disable redirects); reject non-http(s); cap body size.

### I3 — [medium] Worker performs no Content-Type validation · reported

`index.ts:108`/`:121` — trusts and re-serves the upstream `Content-Type`, never asserts `image/*`. With I1, arbitrary `text/html`/octet-stream bodies get re-served under the trusted origin with permissive CORS + long immutable cache (content laundering / cache poisoning). → Reject non-`image/*` (or coerce to octet-stream + `Content-Disposition: attachment`).

### I4 — [low] Native fetch sends no UA/Referer · reported

The Worker sets both deliberately (`index.ts:91-95`, "some CDNs require a Referer/UA"); native `fetch_image` (`lib.rs:9`) sets neither. Hosts that gate on Referer/UA (MangaDex hotlinking "returns a wrong response," CATALOGUE.md:188) may 403 the desktop/mobile build — a web/native divergence for the exact hosts the spec calls out. → Set a Referer + descriptive UA on the native request.

### I5 — [low] Hotlink guard trivially bypassable · reported

`index.ts:176` `if (!origin && !referer) return true;` and `:179` unparseable Referer → allowed. Any non-browser client (curl/script) omitting Origin/Referer bypasses the origin allowlist. Intentional (native/cache-fill), documented — but not an access control; the host allowlist must carry abuse prevention (so fix I1).

### I6 — [low] `direct` image mode footgun · reported

`image-provider.ts:40` returns source URLs unchanged when `PUBLIC_KOMIKA_IMG_MODE=direct`; nothing scopes it to CORS-safe backends, so combining `direct` with MangaDex (`uploads.mangadex.org`) breaks images via CORS/hotlink silently. → Warn/guard when `direct` is combined with a backend whose hosts require proxying.

### I7 — [low] Stale docs describe a removed `cache=1`/B2 path · reported

SPEC.md:351-353 and `apps/worker/README.md:12,18,32` describe `cache=1` + B2 read/write-through via SigV4 + "fails open to plain proxy"; the actual `index.ts` has neither (edge cache only; B2-for-images removed in `6d06784`). The "fails open" phrasing could mislead. → Update the docs.

### Looks correct

- **Suffix allowlist is NOT substring-vulnerable** — `index.ts:161` `host === a || host.endsWith('.'+a)` on the parsed `upstream.hostname`; `uploads.mangadex.org.evil.com`, `evilmangadex.network`, `evil.com/?x=uploads.mangadex.org` all fail. The brief's SSRF payloads do not pass; only the empty _default_ (I1) is the hole.
- **Cache key normalized** — `index.ts:79-82` keys on `${origin}/img?src=${encodeURIComponent(upstream)}`, stripping `cache=1`/hotlink noise → no collision, no param leak.
- **Web/native split is clean** — `createImageProvider` (`:86-88`) returns `NativeImageProvider` when `isTauri()`, else `WebImageProvider`; native never builds a Worker URL and vice versa.
- **Protocol restriction on the Worker** (`index.ts:64`) blocks `file:`/`gopher:` upstreams (worker side).
- **Blob-URL lifecycle** — `NativeImageProvider.release` revokes `blob:` URLs; CSP `img-src` includes `blob: data:`.

### Could not verify statically

Whether the live CF deployment overrides `ALLOWED_SOURCE_HOSTS`/`ALLOWED_ORIGINS` via dashboard/`wrangler secret` (absent an override, prod is open); reqwest runtime redirect/timeout defaults; whether CF `fetch()` blocks private-IP upstreams; actual prod `PUBLIC_KOMIKA_IMG_MODE`/worker URL.

---

## Domain 7 — Scanner / catalogue monitoring

Files: `apps/server/src/scanner.rs`, migration `0003`, `updates`/`scanStatus`/`triggerScan` resolvers, `graphql/types.rs`, `suwayomi.rs`, the parallel `mangadex.rs`/`main.rs` path.

### SC1 — [high → medium] `poll_every_minutes` is dead config — the overdue "re-poll every N min until the chapter appears" is unimplemented · CONFIRMED

**Evidence:**

- SPEC.md:104-106 describes it; `scanner.rs` `ScanAdmin` reads only `override_interval_hours`/`paused_override`/`status_override` (`:62-67`); grep confirms zero scanner references to `poll_every_minutes`. The field is only stored (`mod.rs:1373`), surfaced (`:344`), validated (`:1362`), and edited in admin (`+page.svelte:61,84,193`).
- After an overdue no-new-chapter scan, `scan_series` recomputes the full avg/override interval and sets `next_scan_at = now + full interval` (`:248-261`), stamping `last_scanned_at = now`. `SCAN_TICK_SECONDS` is the global loop cadence, not per-series. There is no min-interval clamp / overdue acceleration; `next_scan_at` isn't even read for gating.
  **Failure scenario:** A weekly series overdue by hours is not re-checked for up to a week; the adaptive "tighten once overdue" promise doesn't exist. **Downgraded to medium:** overdue series are still rescanned every full interval — a missing latency optimization, not a total detection failure.
  **Fix direction:** Track an "awaiting update" state distinct from steady-state cadence; schedule at `poll_every_minutes` (clamped) until `new_found` flips.

### SC2 — [high → info/low] Scanner covers only the Suwayomi library, not MangaDex canonical works · REFUTED as a defect

**Evidence (facts accurate):** `tick` iterates only `state.suwayomi.library()` (`scanner.rs:172` → `mangas(inLibrary:true)`); MangaDex works refreshed by a separate flat `mangadex::spawn_recurring` (`:723-731`) with no per-series cadence/override/pause, off by default; `updates` is Suwayomi-only (`mod.rs:694-697`) while `canonical_updates` (`:747`) reads the mirror.
**Why refuted:** The split is documented and intended (CATALOGUE.md §5 describes `spawn_recurring` "mirrors the scanner.rs task pattern" on a global interval; §6 documents `canonicalUpdates` as the deliberate stored-delta feed; the canonical path is "a parallel path without touching the Suwayomi one"). Update _delivery_ is unified at the UI (both feeds surface). No repo text promises adaptive scanning for MangaDex works. Extension series added via the normal Suwayomi library path (`mark` → `set_in_library`) _are_ swept. Kept only as a clarity note that MangaDex-mirrored works lack per-series cadence/override/pause by design.

### SC3 — [medium] First-run flags the entire back catalogue as "new" · reported

`scan_series` computes `new_found = count > prior.known_chapter_count` (`:245`); first-ever scan has `prior=default()` (`known=0`) so any series with ≥1 chapter → `new_found=true`, writing `last_new_chapter_at=now` (`:262-264`). On a fresh deploy, `bootstrap.py` seeds the library and the first tick logs the whole library as just-updated, flooding the `updates` feed (ordered by `last_new_chapter_at DESC`). → On first observation, record the baseline count without setting `last_new_chapter_at`.

### SC4 — [medium] New-chapter detection is count-only; net-zero churn missed, `latest_number` decorative · reported

Detection is strictly `count > prior` (`:245`); `latest_number` is computed (`:123-128`) but used only in a log line (`:275`), never persisted (no column in `0003`) or compared. If upstream removes one chapter and adds another within an interval, count is unchanged → new chapter silently missed; if upstream removes chapters, `known_chapter_count` is overwritten downward (`:303`) with no signal. → Persist and compare the max chapter number (or a chapter-key set) alongside count.

### SC5 — [medium] No minimum interval clamp · reported

`avg_interval_hours` can return an arbitrarily small positive value (same-day burst → sub-hour gaps, `:97-120`); resolution only guards zero/negative (`:204-208,254-259`) and clamps the upper bound (`MAX_INTERVAL_HOURS`). A burst series (avg 0.2h) is overdue on essentially every 300s tick → refetched every tick → needless source/FlareSolverr load. → Add `MIN_INTERVAL_HOURS` and clamp into `[MIN, MAX]`.

### SC6 — [low] `triggerScan` vs tick race on the scan-state read-modify-write · reported

Both `trigger_scan` (`mod.rs:1399-1405`) and `tick` (`scanner.rs:215`) call `scan_series` (read `prior` → fetch → upsert), non-transactional last-writer-wins. An admin force-scan overlapping a tick can double-count or clobber `known_chapter_count`. Values converge next scan; intra-tick is sequential (no race). → Wrap the read-modify-write in a transaction.

### SC7 — [low] `next_scan_at` is display-only and can disagree with gating · reported

`tick` recomputes overdue live (`:210`); persisted `next_scan_at` (`:260-261`) is only read for display (`mod.rs:352`, `scanStatus` `MIN(next_scan_at)` `:1005-1006`). If an admin changes the override between scans, the console's "next due" won't match. Cosmetic.

### Looks correct

- Divide-by-zero / <2 chapters / gaps==0 → `None` → fallback (`:102-117`); negative/zero intervals filtered; upper-bound overflow clamp (test `max_interval_clamp_avoids_duration_overflow`).
- Admin-override precedence (override>0 wins, else avg, else default) consistent in both paths.
- Pause logic — forced pause wins, else auto-pause for COMPLETED/HIATUS/CANCELLED (`:162-167`, `types.rs:291-296`); paused series `continue` before any fetch.
- Per-series failure isolation — a `scan_series` error is logged and skipped, never aborts the tick (`:215-217`).
- Never-scanned = due (`is_overdue(None,…)→true`); tick config (`SCAN_TICK_SECONDS` filtered >0, default 300); `MissedTickBehavior::Delay`; clean shutdown via watch channel; `epoch_millis` sec/ms coercion.

### Could not verify statically

Whether Suwayomi `fetchMangaAndChapters(fetchChapters:true)` forces a source-side refresh vs returns cached rows (**all** detection correctness hinges on this); the real format of Suwayomi `upload_date` (the sec/ms heuristic); whether Tier-2 extension series end up `inLibrary:true` (and thus swept); live `nextDueAt`/`scan_health` under concurrent ticks.

---

## Domain 8 — Social layer

Files: migration `0009`, `graphql/mod.rs` (reviews/comments/moderation/aggregation), `apps/reader/src/lib/data/{social-repo,social}.ts`, `components/CommentThread.svelte`, `mock.ts`.

### S1 — [medium] `banCommenter` gives false "comments removed" feedback; content persists · reported

`CommentThread.svelte:108-116` — `banAuthor` filters the banned author's comments **client-side only**. `ban_user` (`mod.rs:1411-1448`) sets `is_banned` + deletes sessions but never touches the `comments` table; the `comments` query (`:957-969`) doesn't filter `is_banned`. `social-repo.ts:387-389` documents "existing comments left in place." So an admin watches comments vanish, but they return on reload. → Cascade-hide/soft-delete on ban (add `AND u.is_banned = 0` to the comments/reviews JOIN), or drop the client filter so the UI is honest.

### S2 — [low] Live-mode likes/replies/isOp and offline spoiler flag are cosmetic/lost · reported

`social-repo.ts:114-116,134-136` hardcode `likes:0, liked:false` (+`isOp:false`); the Reply button (`CommentThread.svelte:189`) has no handler; `toggleLike` mutates client state never persisted; offline `ReaderComment`/`SeriesComment` have no spoiler field (`mock.ts:494-503,540-553`) so a spoiler-flagged comment loses the flag on reload. A milder form of the "fabricated engagement" §7 targeted. → Model likes/replies server-side or drop the affordances; persist `has_spoiler` in the local shape.

### S3 — [low] `loadSeriesSocial` reads only review page 1 · reported

`social-repo.ts:143-150` calls `backend.reviews(seriesId)` with no page (defaults 1); `PAGE_SIZE=20` ordered `created_at DESC` (`mod.rs:17,920`). On a series with >20 reviews where the user reviewed early, their review falls off page 1 → widget shows them unrated with empty body; posting could overwrite their own body with empty (row itself safe via `ON CONFLICT` upsert). → Add a dedicated "my review" query.

### S4 — [low/nit] §7 said delete the `SeriesComment`/`ReaderComment` interfaces — only the arrays were removed · reported

The arrays are gone; the interfaces remain (`mock.ts:494-503`, `:540-553`), still imported type-only in `social-repo.ts:26`. Honesty claim unaffected. **S5 — [nit]** `deleteChapterComment` (`CommentThread.svelte:103`) is used for series comments too; server `delete_comment` deletes by id alone so it works — name only.

### Looks correct

- **Polymorphic comment target — no collision** — server keys reads/writes on `target_type` **and** `target_id` (`mod.rs:961,971,1186-1191`); `validate_comment_target` whitelists chapter/series (`:497-505`); local fallback namespaces the three kinds into distinct buckets (`social-repo.ts:246,336,350`; `social.ts:18,46`); ratings vs comments in separate maps.
- **Rating aggregation math** — `dist=vec![0;10]`, `idx=(s-1).clamp(0,9)` (`:208-213`), scores validated `1..=10` (`:1132`), empty set → `RatingSummary::empty()` before division. No off-by-one, no divide-by-zero.
- **One-review-per-user upsert** — `UNIQUE(series_id,user_id)` (`0001:32`) + `ON CONFLICT DO UPDATE` (`:1146-1148`) → concurrent posts resolve to one row, id/created_at untouched.
- **Moderation auth-gating** — `delete_comment`/`ban_user` both `require_admin` (`:1453,1412`); client gates via `canModerate()→isAdmin` with server re-checks.
- **Composer auth-gating** — sign-in prompt when `!auth.user`; `submitComment`/`submitSeriesReview` re-check; server `post_comment`/`post_review` `require_user`.
- **Fabricated seed genuinely removed** — no `Mika R.`/`Aria_reads`/`devon_k`, no `seriesComments`/`readerComments` arrays; both `getComments` callers pass `[]`; empty state renders "No comments yet" (§7 satisfied).

### Could not verify statically

Whether `require_user` independently re-checks `is_banned` (enforcement relies on ban having deleted sessions — Auth confirmed `user_for_token` does filter it); concurrent-`postReview` race under SQLite WAL; whether serving later-banned users' content is intended.

---

## Domain 9 — Canonical reader path

Files: `graphql/mod.rs` (`canonicalSeries`/`Chapters`/`Pages`, `map_canonical_*`), `catalog/mod.rs` (`load_canonical_chapters`, `select_reader_chapters`, `chapter_sort_key`), `mangadex.rs` (`at_home`), migration `0008`, `apps/reader/src/lib/data/source.ts`, reader routes.

### CR1 — [high] `canonicalPages` at-home fetch doesn't enforce the 40/min cap · CONFIRMED (same defect as M1)

See **M1**. `mangadex.rs:226-235` uses the shared 5 req/s bucket; `canonical_pages` (`graphql/mod.rs:830`) calls it per chapter-open; comment `:224-225` arithmetically false. → dedicated ≤40/min limiter for `/at-home`.

### CR2 — [medium] Which English scanlation of a duplicated chapter number is served is nondeterministic · reported

`catalog/mod.rs:282-294` — `load_canonical_chapters` has **no `ORDER BY`**; `select_reader_chapters` (`:324-342`) keeps the first-seen row per number, and the English-upgrade branch only fires `if is_en && existing.lang != en`, so when two rows are both English the arbitrary DB row order wins. The final `sort_by` orders output but doesn't decide the kept representative. MangaDex commonly has multiple English groups per number → the retained `external_id` (which `canonicalPages` fetches) can differ between requests → different group's pages/page count on reload. → Deterministic tiebreak (prefer latest `published_at`, else lowest `external_id`) even when both are English.

### CR3 — [medium] `w_`-prefix routing is not a reliable "MangaDex-mirrored" discriminator (latent) · reported

Backfill mints `w_<numeric-suwayomi-id>` (`0005:107-115`); `isCanonicalId` (`source.ts:30-32`) routes **any** `w_…` id to canonical resolvers. Such a work has no mangadex source → `mangadex_id=None` → empty cover, and `load_canonical_chapters` filters `source_type='mangadex'` → zero chapters. **No id collision** (MangaDex works use `w_<uuid-hex>`), and today no feed emits backfilled ids (canonicalUpdates filters `source_type='mangadex'`), so it's latent. If any future feed emits one, `canonicalSeries` returns a titleless/coverless/chapterless shell instead of an error. → Return "no such work" when `mangadex_id` is None, or gate `isCanonicalId` on a mangadex-anchored flag.

### CR4 — [low] Number-less chapters sort last server-side but jump to front in the reader · reported

`map_canonical_chapter` parses number to f64, `unwrap_or(0.0)` for oneshots (`mod.rs:439-444`); server orders number-less last via `f64::INFINITY` (`catalog/mod.rs:314-319`); the reader re-sorts by `a.number-b.number` (`source.ts:547,397`), so a oneshot (0.0) sorts first and collides with a real chapter numbered "0". Cosmetic. → Preserve server ordering client-side.

### CR5 — [low] Worker `ALLOWED_SOURCE_HOSTS` committed empty — M8 allowlist not realized in committed config · reported

Same as I1. Covers/pages proxy fine only because everything is allowed; CATALOGUE.md M8's "must include uploads.mangadex.org and mangadex.network" is satisfied only if an operator sets the env. The suffix logic itself is correct.

### CR6 — [low, confirmed doc gap] No per-user progress/library/rating for canonical works · reported

`source.ts:481` — `saveProgress` early-returns for non-numeric chapter ids (canonical ids are MangaDex uuids) → progress never persisted; `map_canonical_chapter` hardcodes `read:false, last_page_read:0` (`mod.rs:445-457`) → `getReaderChapter` always resumes at chapter 1 (`source.ts:549`); `mark()` parses `w_` as i64 and fails → `setLibraryMark` no-ops; `rating: RatingSummary::empty()` hardcoded (`:419`) → posted reviews never surface on `canonicalSeries`. Matches the acknowledged CATALOGUE.md follow-up, but user-visible (always-restart-at-ch-1, silent library no-op, empty rating).

### Looks correct

- **NSFW gating present on all three canonical resolvers** (`mod.rs:783,801,822-829`, via `chapter_owner_is_nsfw`) + `canonical_updates` (`:759`). `canonical_pages` gates on the owning work.
- **Numeric chapter ordering** (10 after 9, 10.5) — `chapter_sort_key` parses f64 (test `reader_chapters_dedupe_prefer_english_and_order`).
- **English-only** in depth (firehose filter `:180`, sync skip `:604`, query `c.lang='en'` `catalog/mod.rs:289`).
- **`w_` vs numeric — no collision** (MangaDex `w_<uuid-hex>`; Suwayomi numeric); reader passes `params.slug` through with no `parseInt`.
- **`cover_file_name` null handling** (`String::new()` / `CASE WHEN … IS NOT NULL`).
- **Contract/shape match** — canonical resolvers reuse shared `Series`/`Chapter`/`Page` structs via the same fragments; reader components render unchanged.

### Could not verify statically

`canonicalUpdates` `GROUP BY ss.work_id` with bare `c.number`/`c.title` alongside `MAX(latest_at)` relies on SQLite's bare-column-from-max-row (documented, untested here); at-home token expiry vs edge-cache-by-URL behavior at real clock; deployed `ALLOWED_SOURCE_HOSTS`.

---

## Domain 10 — Contract & schema consistency

Chain: SDL (`packages/api/src/schema/komika.graphql`) ↔ `operations.ts` ↔ `Backend`/`GraphQLBackend` ↔ Rust resolvers (`graphql/{mod,types}.rs`) ↔ `@komika/types` ↔ clients. Focus: the recent concurrent-session / catalogue-canonical merge surface.

### C1 — [medium] `addSourceSeries` has no client binding → the dedup review queue is unreachable/permanently empty · CONFIRMED (keystone gap)

**Evidence:**

- SDL `komika.graphql:61` `addSourceSeries(suwayomiMangaId: ID!): MatchResult!`; server `mod.rs:1500`.
- No `ADD_SOURCE_SERIES` doc in `operations.ts`, no `addSourceSeries` on the `Backend` interface (`backend.ts`), no `MatchResult` in `@komika/types`; full-repo grep finds only SDL + server + docs. `operations.ts` is hand-maintained (so it's a missing binding, not codegen).
- The sole `insert_merge_candidate` caller is `add_source_series` (`mod.rs:1597`, def `catalog/mod.rs:640`); the MangaDex sync never enqueues candidates.
- The admin console wires the _consumer_ half — `mergeQueue` (`data.ts:65`, `graphql-backend.ts:186`) + `resolveMergeCandidate` (`data.ts:74`) + `routes/review/+page.svelte` — which renders an empty queue gracefully.
  **Failure scenario:** CATALOGUE.md's Tier-2 flow ("admin hand-picks series → matcher → review queue → admin confirms in apps/admin") is inoperable: no client path populates the queue, admins can't add a Tier-2 series in-app, and the dedup-review console is dead. Not a runtime 500 (nothing client-side calls it); a human could hit it via a raw admin GraphQL POST, but that's not an in-app path. CATALOGUE.md M5's "threaded through @komika/api + @komika/types" is false for the producer half.
  **Fix direction:** Add an `ADD_SOURCE_SERIES` op + `MatchResult` type + `addSourceSeries?()` on `Backend`/`GraphQLBackend` + an admin "Add source series" action — OR wire `insert_merge_candidate` into the sync path so the queue fills without the mutation.

### C2 — [low] `banUser` returns `UserRef!`, which lacks `isBanned` · reported

`komika.graphql:58`/`mod.rs:1411` return `UserRef`; the users console renders `AdminUser` rows. Masked because `setUserBanned` discards the result and updates `isBanned` optimistically (`users/+page.svelte:55-56`), unlike `setUserAdmin` which returns `AdminUser` and swaps in place. Asymmetry; the ban path relies on optimistic state. → Return `AdminUser!` for parity.

### C3 — [low] `setLibraryMark` doesn't guard canonical (`w_`) ids before `mark` (which parses i64) · reported

`source.ts:461-470` calls `backend.mark(seriesId,…)` with no id-shape guard; server `mark` does `series_id.0.parse::<i64>()` (`mod.rs:1103`) and errors on `w_`. Contrast `saveProgress` (guards `/^\d+$/`) and `getSeries`/`getReaderChapter` (route via `isCanonicalId`). On a canonical series, "Add to Library" flips optimistically but never persists (caught at `:466`, logged warning). → Mirror the `saveProgress` guard — early-return the optimistic value when `isCanonicalId(seriesId)`.

### Looks correct

- **Enums map cleanly** — `ComicType`/`SeriesStatus`/`DiscoveryFeedKind` identical across SDL/types/Rust (async-graphql SCREAMING_SNAKE); Suwayomi→Komika status folding coherent.
- **Argument names line up** — `series(id)`, `chapters(seriesId)`, `pages(chapterId)`, `canonicalSeries(workId)`, `canonicalChapters(workId)`, `canonicalPages(chapterId)`, `comments(targetType,targetId)`, `updateSeriesAdmin(input.seriesId)` all agree. No `workId`/`id`/`seriesId` drift.
- **Series/Chapter/Page field + nullability parity** across SDL/`types.rs`/`@komika/types`; every field the reader dereferences non-optionally is non-null server-side incl. the canonical path (`map_canonical_series` fills `genres:[]`, `rating:empty()`, full `ScanPolicy`).
- **`GraphQLBackend` returns `@komika/types` shapes 1:1** (unwraps one data key, no client mapping).
- **Optional methods degrade safely** — `setToken`, admin/canonical `?`-methods null-checked before call.
- **`showNsfw`/`setShowNsfw` chain consistent** end-to-end (tested `mod.rs:1810`).
- **`w_`-prefix routing contract holds** — canonical ids minted via `new_id("w_")`; reader's `isCanonicalId` matches; never fall into numeric resolvers.
- **Polymorphic comments** agree (SDL/`CommentTargetType`/`validate_comment_target`); `Comment` omits `updatedAt`, `Review` carries both timestamps everywhere.
- **`mergeQueue`/`resolveMergeCandidate`/`MergeCandidate`/`CanonicalUpdate`** shapes match 1:1.

### Could not verify statically

Runtime SDL equivalence (code-first Rust schema verified field-by-field, but no generated-SDL diff / CI schema check exists); `total` nullability semantics (`search` returns null while others return counts) — whether any screen assumes non-null.

---

## Domain 11 — Admin console

Files: `apps/admin/src/**` (routes: catalog/root, review, users, updates, login; lib: auth/data/config/context), `@komika/api`, server guards in `graphql/mod.rs`.

### AD1 — [medium] `pollEveryMinutes` whole-state save always pins an explicit override · reported

`mod.rs:344` folds it with a default (`ov.poll_every_minutes.map(|v| v as i32).unwrap_or(30)`); the type is non-nullable (`packages/types:32`) unlike its three siblings (nullable); the editor decodes `fPoll` from the **effective** value (`+page.svelte:61`) not a raw override, and Save sends it back concrete (`:76,83`) → whole-state upsert (`:1371-1390`). Opening the drawer on a series with no poll override pre-fills "30", and any Save writes `poll_every_minutes=30` into `series_admin` — creating/keeping a row and pinning an override the admin never set; "clear this override" is unreachable via UI. The stated invariant ("decode straight from raw overrides so an explicit choice is never mistaken for the status default", `:57-58`) holds for status/interval/paused but is violated for poll. → Expose a nullable `pollEveryMinutesOverride` and decode `fPoll` from it (blank when unset).

### AD2 — [low] `resolveMergeCandidate` pending-check is a non-transactional TOCTOU · reported

`mod.rs:1630-1642` selects `status`, checks `!= "pending"`, then reassigns + `UPDATE … SET status` (`:1644-1680`) as separate statements with no transaction / `WHERE status='pending'` guard. Two admins resolving the same candidate can both pass; double-accept is safe (delete guarded by `NOT EXISTS`) but accept-racing-reject yields a non-deterministic final status. Client surfaces the error cleanly. → `UPDATE … WHERE id=? AND status='pending'`, treat 0 rows as already-resolved.

### AD3 — [low] Updates pager can advance onto an empty page · reported

`updates/+page.svelte:60-62` disables Next only when `updates.length===0`; `canonicalUpdates` returns a bare list with no `hasNextPage`/`total` (`operations.ts:408-421`), so a full-but-final page still enables Next → empty page. Cosmetic (the `users` pager uses a real `hasNextPage`).

### Looks correct

- **Server-side admin gating enforced on every admin op** — `require_admin` (`:186-192`) is the first statement in `scanStatus`(994), `users`(1026), `mergeQueue`(1059), `updateSeriesAdmin`(1352), `triggerScan`(1400), `banUser`(1412), `deleteComment`(1453), `setUserAdmin`(1471), `addSourceSeries`(1505), `resolveMergeCandidate`(1621). Client `isAdmin` is defense-in-depth. Tests: `admin_only_query_is_gated`, `delete_comment_requires_admin`, `ban_user_guards`, `set_user_admin_cannot_demote_self`.
- **Token separation** — admin `komika-admin-token` vs reader `komika-token`; separate `createBackend` instances.
- **Login gating on isAdmin** — `login()` refuses non-admins (and logs out the just-created session); `initAuth()` keeps a restored session only if `isAdmin`, else clears the token; transient backend failure stays logged-out without nuking the token; banned admins stopped at the server login resolver.
- **Whole-state upsert for status/interval/paused** — correct `ON CONFLICT DO UPDATE` with range validation; editor decodes these three from nullable raw fields and always sends the full form (null clears without touching others). Poll is the exception (AD1).
- **Merge confirm/reject flow** — `confirm()` dialog → `resolveMergeCandidate`; server reassigns `work_id`, GCs the orphaned provisional work under `NOT EXISTS`, stamps `confirmed`/`rejected`+`resolved_at`; optimistic row removal on success, error surfaced.
- **Contract alignment** — every admin op in `operations.ts` matches its resolver (`MERGE_QUEUE`, `Users`/`AdminUserPage`).
- **ban_user/setUserAdmin safety rails** — no self-ban/ban-admin/self-demote; ban revokes sessions; client `confirm()` on ban.
- **Error/empty/loading states** — all four routes use `loadError`/`actionError`/`loading`/empty patterns, set data to `[]` on failure; GraphQL errors thrown with server message → guard rejections show verbatim.

### Could not verify statically

CORS/origin allowlisting for admin port 5273 + prod origin; merge-accept side-effects on other pending candidates / chapter/alias references to the old provisional work; whether `KOMIKA_ADMIN_USERS` startup promotion runs; runtime redirect/403 rendering.

---

## 15. Global "could not verify statically"

Aggregated items that need a running stack to confirm:

- **Suwayomi `fetchMangaAndChapters` refresh semantics** — cached vs fresh; **all** scanner new-chapter detection depends on it. And the real `upload_date` format (the sec/ms heuristic silently drops timestamps if wrong).
- **Reverse-proxy XFF handling in prod** — decides whether A2 is exploitable in a real deploy.
- **at_home caching** — none found in server or Worker; determines how fast M1/CR1 breach in practice.
- **MangaDex `links` uniqueness** — whether distinct works share an external id (would silently drop mappings under `INSERT OR IGNORE`).
- **Deployment topology** — replica count / shared egress (M4); the deployed `ALLOWED_SOURCE_HOSTS`/`PUBLIC_KOMIKA_IMG_MODE` (I1/CR5); Litestream final flush + the restore drill (D5); whether CF `fetch()` blocks private IPs (I1/I2).
- **Concurrency under SQLite WAL** — `postReview` / `add_source_series` / scanner-vs-trigger races.
- **Runtime SDL equivalence** — no generated-SDL diff / schema CI exists to mechanically catch future drift.

---

## 16. Methodology & coverage

- **Scope.** 11 domains: Auth, MangaDex sync, Deploy/production, Dedup/canonical, NSFW, Image pipeline, Scanner, Social, Canonical reader, Contract/schema, Admin. Judged against SPEC.md, CATALOGUE.md (incl. §6/§8 tracked follow-ups), PRODUCTION.md, deploy/README.md.
- **Process.** One auditor per domain (parallel), each returning a structured finding list with `file:line` evidence, a failure scenario / missed intent, and a fix direction — plus a "looks correct" and a "could not verify statically" list. Then one independent skeptic per blocker/high finding, prompted to **refute** (find a guard, caller, config, or intended-design reason it's not a defect); findings that survived kept with their verdict, over-reaching ones dropped/downgraded.
- **Verification outcomes.** 16 blocker/high claims tested: 15 CONFIRMED (7 downgraded on severity once reachability / edge-context / documentation was accounted for), 1 REFUTED as a defect (SC2 — intended, documented design). This is the verification pass constraining the report rather than inflating it.
- **Read-only.** No product code, servers, or deploys were touched. All `file:line` references are to the state of the repo at audit time.

---

_Generated by a multi-agent read-only audit. Paths are relative to the Komika repo root._
