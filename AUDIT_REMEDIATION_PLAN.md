# Komika (YOMU) — Audit Remediation Plan

Remediation plan for the independent verification audit (2026-07-07). Every finding
from the audit is mapped to a phase in the **Coverage matrix** below so nothing is
dropped. Severities and IDs match the audit report; the "Src" column is the
originating subagent area (A=server, B=reader, C=admin, D=deploy/infra, E=contract/docs).

## Method / baseline

- Toolchain: Rust 1.96 (`cargo`), pnpm 11 (`pnpm@11.9.0`, requires Node **≥22.13**), Node ≥22, Docker.
- Live harness: `docker start suwayomi` (:4567, MangaDex, manga id 3 = "The Eminence in Shadow").
  Server recipe: `cd apps/server && PORT=8790 KOMIKA_ADMIN_USERS=admin SUWAYOMI_URL=http://localhost:4567 CORS_ORIGINS="http://localhost:5173,http://localhost:5273" ./target/debug/komika-server`.
- Dev accounts: admin/`adminpass1`, alice/`hunter2pw`, bob/`password9`, carol/`carolpass1`.
- Green sweep = `cargo build --release` + `cargo clippy --all-targets -- -D warnings` + `cargo fmt --check`;
  `pnpm --filter @komika/{reader,admin,worker} check` 0/0; `pnpm run lint`;
  `docker build` both images; `docker compose -f deploy/docker-compose.yml config`.

---

## Coverage matrix (every audit finding → phase)

| ID      | Sev      | Src       | Finding                                                                                                                                    | Phase |
| ------- | -------- | --------- | ------------------------------------------------------------------------------------------------------------------------------------------ | ----- |
| C1      | CRITICAL | D1+D2     | Reader Docker image + all CI red: Node 20 vs pnpm 11.9.0 (`node:sqlite` / engines ≥22.13)                                                  | 1a    |
| C2      | CRITICAL | D3        | `pnpm run lint` broken: prettier `SyntaxError` on `komika.graphql` (`//` vs `#`) + eslint not installed/no config                          | 1b    |
| M1      | MAJOR    | A1        | Rate limiter: targeted account-lockout DoS (username-only key, counts all attempts)                                                        | 2     |
| M2      | MAJOR    | A2        | Rate limiter: counts successful logins (contradicts "failed attempts" doc)                                                                 | 2     |
| M3      | MAJOR    | D4        | Litestream binary wrong arch on arm64 (`ARG TARGETARCH=amd64` hardcoded) → prod outage when enabled                                        | 3a    |
| M4      | MAJOR    | D5        | `deploy.sh` shell-sources `.env`; unquoted `SEED_SERIES` with spaces → empty library seed, silently                                        | 3b    |
| M5      | MAJOR    | B1        | Reader auth race: stale token + login → `session()` null → wipes fresh token (silent logout)                                               | 4a    |
| M6      | MAJOR    | C1(admin) | Admin scanner tri-state loses forced choice when it equals status default → wrong scanner behavior after re-save + status drift            | 4b    |
| m1      | minor    | A3        | Scanner re-scans every tick forever for series with <2 dated chapters (interval 0 → always overdue)                                        | 5     |
| m2      | minor    | A4a       | Unbounded `overrideIntervalHours` → `chrono::Duration` overflow panic inside the per-series tick loop                                      | 5     |
| m3      | minor    | C2(admin) | Fractional poll passes client check, server rejects `Int` with a raw GraphQL error                                                         | 6     |
| m4      | minor    | E1        | `scanStatus`/`ScanStatus` server-only; absent from `komika.graphql`/`operations.ts`/`backend.ts` (falsifies "1:1 mirror")                  | 6     |
| m5      | minor    | E2        | Reader default `apiEndpoint` = Suwayomi `:4567` while default `backendKind='komika'` (misconfig footgun)                                   | 6     |
| m6      | minor    | B2        | social-repo mutations lack defense-in-depth (no `auth.user` / `score>0` check; UI-gated only)                                              | 6     |
| m7      | minor    | B3        | `openChapter` JS smooth scroll ignores `prefers-reduced-motion`                                                                            | 6     |
| m8      | minor    | A5        | Admin promotion case-sensitive (`KOMIKA_ADMIN_USERS=admin` won't match `ADMIN`)                                                            | 6     |
| m9      | minor    | C3(admin) | Admin `initAuth` keeps stale token on transient `session()` failure (no `persist(null)`)                                                   | 6     |
| m10     | minor    | D6        | No `.dockerignore` → 3.9 GB build context (`target`, `node_modules`, committed `.zip`)                                                     | 6     |
| m11     | minor    | D7        | Litestream guard passes without `LITESTREAM_SECRET_ACCESS_KEY` → S3 auth failure instead of clean fallback                                 | 6     |
| m12     | minor    | E3+E4     | Stale docs (`SPEC.md:65`/`backend.ts:22` "TODO/stubbed"; `toViewType` folds `COMIC`/`WEBTOON`)                                             | 6     |
| **m13** | minor    | A4b       | `setProgress(lastPageRead)` forwarded to Suwayomi with no `>= 0` validation (`mod.rs:565-579`) — **restored; was dropped from the report** | 6     |

All 22 audit findings are covered. Phases 1–5 are independent; land Phase 1 first so the rest verify under green CI.

---

## Phase 0 — Prep

- Branch `fix/audit-remediation` off `main` (repo currently has no commits; make the initial structure or work on `main` per preference).
- `docker start suwayomi`; keep the `AUTH_RATE_LIMIT_MAX=3` server recipe for re-testing M1/M2.

## Phase 1 — Unblock deploy & CI (CRITICAL)

**1a. Node 20 → 22 (C1)**

- `deploy/reader.Dockerfile:14`: `node:20-slim` → `node:22-slim` (only `node:` ref in deploy/).
- `.github/workflows/ci.yml:23`: `NODE_VERSION: "20"` → `"22"`; refresh the stale comment at `:21-22`.
- `package.json:8`: `engines.node` `">=20"` → `">=22.13"`.

**1b. Lint gate (C2)**

- Add `.prettierignore` with `packages/api/src/schema/*.graphql` (SDL uses `//`, which prettier's graphql parser rejects). _Alt:_ rewrite its header to `#`.
- `pnpm format` to fix the ~20 unformatted files.
- **Decision (eslint):** recommend `"lint": "prettier --check ."` (drop `eslint .`) — the per-app `svelte-check`/`tsc` `check` jobs already cover it. _Alt:_ add eslint + flat config (follow-up).

**Gate:** `pnpm install --frozen-lockfile` (Node 22), `pnpm run lint`, `docker build -f deploy/reader.Dockerfile .` all pass.

## Phase 2 — Auth limiter security (MAJOR M1 + M2) — one change closes both

`apps/server/src/main.rs` + `apps/server/src/graphql/mod.rs`:

- **Plumb client IP.** Handler at `main.rs:37` builds only `RequestAuth` from headers. Extract `X-Forwarded-For` (server sits behind `deploy/nginx.conf`), fallback to `ConnectInfo<SocketAddr>` for dev; add a `ClientIp(Option<String>)` context datum.
- **Split limiter** (`mod.rs:46-58`) into `is_limited(key) -> Option<retry>` (read) and `record(key)` (write).
- **Rework `login`/`register`** (`:665-702`): `is_limited` before verify → reject if limited; run verify; **`record` only on failed verify**. Key = `login:{ip}` (or `login:{ip}:{username}`).

**Gate (live):** with `AUTH_RATE_LIMIT_MAX=3` — (a) N correct-password logins no longer lock the user out; (b) an attacker's wrong-password flood no longer blocks the victim from a different IP. Then clippy/fmt clean.

## Phase 3 — Deploy correctness (MAJOR M3 + M4)

**3a. Litestream arch (M3)** — `deploy/server.Dockerfile:27,32`: drop the hardcoded `TARGETARCH=amd64`; inside the `RUN`, `arch=$(dpkg --print-architecture)` (emits `amd64`/`arm64`, matching Litestream asset names) and interpolate. Switch `deploy.sh` build to `docker buildx build`.

**3b. SEED_SERIES (M4)** — `deploy/.env.example:23`: quote → `SEED_SERIES="Chainsaw Man,Solo Leveling,One Piece,Jujutsu Kaisen,The Eminence in Shadow"`. Audit `.env.example` for other unquoted spaced values. Optional: stop `deploy.sh:36` blind-sourcing `.env`.

**Gate:** re-source `.env.example` → `SEED_SERIES` non-empty; `docker buildx build -f deploy/server.Dockerfile .`, inspect litestream ELF arch = host.

## Phase 4 — Client correctness (MAJOR M5 + M6)

**4a. Reader auth race (M5)** — `apps/reader/src/lib/auth.svelte.ts:45-59`: capture `token` at call time; after `await session()`, skip the `auth.user`/`persist(null)` writes if `readToken() !== token`.

**4b. Admin tri-state (M6)** — DB already stores a faithful tri-state (`paused_override` NULL/0/1) but `ScanPolicy` exposes only the folded bool (same latent bug affects the status override). Fix additively:

- `apps/server/src/graphql/types.rs` (`ScanPolicy`, ~`:53`): add `status_override: Option<SeriesStatus>` + `paused_override: Option<bool>`, populated from `admin_overrides` where `scan` is built (`mod.rs:~199`).
- `packages/api/src/schema/komika.graphql` + `packages/api/src/operations.ts` (`SeriesFields.scan`, `:31-35`): add the two fields.
- `apps/admin/src/routes/+page.svelte:58-63`: decode `fStatus`/`fScanner` from the raw overrides (`pausedOverride` null→auto/false→running/true→paused; `statusOverride` null→source) instead of the folded heuristic.

**Gate (live):** Force-run an ONGOING series → reopen still shows "Force run" → re-save persists; reader `series()` still folds. `check` 0/0 for admin + reader.

## Phase 5 — Scanner minors (m1 + m2) — server

- **m1** (`scanner.rs:189-192,209,213-216`): add `const DEFAULT_INTERVAL_HOURS`; use it when `avg_interval_hours == 0.0` so <2-dated-chapter series stop re-scanning every tick.
- **m2** (`scanner.rs:217-219`): clamp `next_interval` and the admin `overrideIntervalHours` to a sane max before the `chrono::Duration` conversion; validate the override in `update_series_admin`.

**Gate:** clippy clean; unit-check `is_overdue`/`next_scan_at` for interval-0 and huge-override.

## Phase 6 — Remaining minors (batch)

| ID  | File                                             | Fix                                                                                   |
| --- | ------------------------------------------------ | ------------------------------------------------------------------------------------- |
| m3  | `admin/+page.svelte:79-80`                       | reject non-integer poll (`Number.isInteger`) with a clear message                     |
| m4  | SDL + `operations.ts` + `backend.ts`             | add `scanStatus`/`ScanStatus` to the contract; optionally surface in admin            |
| m5  | `reader/src/lib/config.ts:10`                    | default `apiEndpoint` → `http://localhost:8080/graphql`                               |
| m6  | `reader/.../social-repo.ts:186-200,261-271`      | add `auth.user` + `score>0` guards in submit fns                                      |
| m7  | `reader read/[slug]/+page.svelte:140`            | gate smooth scroll on `matchMedia('(prefers-reduced-motion: reduce)')`                |
| m8  | `server/src/main.rs:68-73`, `mod.rs:717`         | case-insensitive admin-username match                                                 |
| m9  | `admin/src/lib/auth.svelte.ts:47-49`             | `persist(null)` in the catch                                                          |
| m10 | new `.dockerignore`                              | exclude `**/target`, `**/node_modules`, `**/build`, `*.zip`, `.git`                   |
| m11 | `deploy/server-entrypoint.sh:9`                  | also require `LITESTREAM_SECRET_ACCESS_KEY` in the guard                              |
| m12 | `SPEC.md:65`, `backend.ts:22`, `source.ts:31-35` | drop stale "TODO/stubbed" notes; optionally extend `toViewType` for `COMIC`/`WEBTOON` |
| m13 | `server/src/graphql/mod.rs:565-579`              | validate `last_page_read >= 0` in `set_progress` before forwarding to Suwayomi        |

**Gate:** full green sweep (see baseline).

---

## Regression guardrails — verified-good behaviors to NOT break

These held up under live adversarial testing in the audit; re-check after the relevant phase:

- **Long-strip reader** (protect during Phase 4/6): all page `<img>` in DOM, `loading="lazy"`, measured aspect-ratio, 0 letterbox/gap, progress→100%, end-of-chapter mark-read, paged mode, keyboard nav, chapter-change reset.
- **Rating aggregate** (Phase 4 touches `SeriesFields`): scores 5,7,9 → avg 7.0, distribution buckets at index score−1.
- **Review upsert** one-per-(user,series); **comment/review pagination** `hasNextPage` no off-by-one.
- **Admin gating** (Phase 2/4): anon → "Not authenticated", non-admin → "Admin access required", admin OK.
- **Override fold** into public `series()` (Phase 4b): status/scan overrides fold; whole-state null clears to source.
- **`SUWAYOMI_PUBLIC_URL` split** (Phase 3): public image URLs, internal federation.
- **SQL injection**: all sqlx bind params — keep it that way in Phase 2 (IP key is server-generated, not user SQL).
- **Schema/contract 1:1** (Phase 4b/6 add fields): keep SDL ↔ Rust ↔ `operations.ts` in lockstep.
- **RateLimiter thread-safety** (Phase 2): the `Mutex<HashMap>` is race-free — preserve under the is_limited/record split.
- **`type=text inputmode=numeric`** admin inputs (no `type=number`/`.trim()` regression) — Phase 6 m3 must not reintroduce `type=number`.

## Decisions (recommendation in bold)

1. **eslint** → **drop `eslint .` from the lint script** (per-app `check` covers it); alt = add full flat config later.
2. **M6** → **additive `ScanPolicy` override fields**; alt = UI-only mitigation (weaker).

## Commit batching

1. `ci/deploy: Node 22 + lint gate` (Phase 1)
2. `server: auth rate-limiter keys on IP, counts only failures` (Phase 2)
3. `deploy: litestream arch + SEED_SERIES quoting` (Phase 3)
4. `client: auth-race guard + admin scanner tri-state` (Phase 4)
5. `server: scanner interval fallback + overflow clamp` (Phase 5)
6. `chore: remaining minors` (Phase 6)

---

## Execution status (all phases applied)

| ID  | Status      | Verification                                                                                                                                                                                     |
| --- | ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| C1  | ✅ done     | `node:20-slim`→`22-slim`, CI `NODE_VERSION`→`22`, `engines.node`→`>=22.13`. pnpm 11.9's Node-≥22.13 requirement is registry-confirmed. Docker image build blocked by sandbox network (see note). |
| C2  | ✅ done     | Added `.prettierrc.json` (tabs/singleQuote/pw100 — the dominant style) + `.prettierignore` (SDL); dropped `eslint .`. `pnpm run lint` = "All matched files use Prettier code style!"             |
| M1  | ✅ done     | **Live**: attacker flooding a username locks only the attacker IP; victim logs in from her own IP.                                                                                               |
| M2  | ✅ done     | **Live**: 5 correct-password logins no longer lock out (successes don't count).                                                                                                                  |
| M3  | ✅ done     | Dockerfile now resolves arch via `dpkg --print-architecture` (builder-agnostic). Image build blocked by sandbox network.                                                                         |
| M4  | ✅ done     | **Verified**: sourcing the fixed `.env.example` yields the full `SEED_SERIES` (was empty).                                                                                                       |
| M5  | ✅ done     | Token-capture guard in reader `initAuth`; reader `check` 0/0.                                                                                                                                    |
| M6  | ✅ done     | **Live**: `pausedOverride` round-trips forced-run (`false`) vs auto (`null`) through `series()`; admin decodes from raw overrides.                                                               |
| m1  | ✅ done     | `DEFAULT_INTERVAL_HOURS` fallback; clippy/release clean.                                                                                                                                         |
| m2  | ✅ done     | **Live**: absurd override + poll=0 rejected; clamp in scanner.                                                                                                                                   |
| m3  | ✅ done     | Poll now `Number.isInteger`; admin `check` 0/0.                                                                                                                                                  |
| m4  | ✅ done     | `scanStatus`/`ScanStatus` added to SDL + `operations.ts` + `@komika/types` + `Backend`/`GraphQLBackend`.                                                                                         |
| m5  | ✅ done     | Reader default endpoint → `:8080/graphql`.                                                                                                                                                       |
| m6  | ✅ done     | `auth.user`/`score>0` guards in social-repo submits.                                                                                                                                             |
| m7  | ✅ done     | `openChapter` honors `prefers-reduced-motion`.                                                                                                                                                   |
| m8  | ✅ done     | Case-insensitive admin match (`eq_ignore_ascii_case` + `COLLATE NOCASE`).                                                                                                                        |
| m9  | ⚠️ deviated | Kept the token on transient `session()` failure (matches the reader; server re-validates) instead of `persist(null)` — clarifying comment added. Avoids forcing re-login on a blip.              |
| m10 | ✅ done     | Added `.dockerignore`.                                                                                                                                                                           |
| m11 | ✅ done     | Entrypoint guard now requires `LITESTREAM_SECRET_ACCESS_KEY`; warns on partial config.                                                                                                           |
| m12 | ✅ mostly   | `SPEC.md` + `backend.ts` stale notes fixed. `toViewType` left as an intentional 3-bucket narrowing (extending needs new UI concepts).                                                            |
| m13 | ✅ done     | **Live**: `setProgress(lastPageRead:-5)` rejected.                                                                                                                                               |

**Green sweep results:** `cargo fmt --check` OK · `cargo clippy --all-targets -- -D warnings` clean · `cargo build --release` OK · reader/admin `check` 0/0 · worker `tsc` clean · `pnpm run lint` clean · `docker compose config` valid.

**Environment caveat:** the reader/server **Docker image builds** could not complete end-to-end in this session — `docker buildx` is unavailable here (legacy builder only) and the Docker Hub pull of `node:22-slim` repeatedly timed out (TLS handshake). The C1/M3 fixes are correct by construction and the audit already confirmed the legacy build pipeline works; a rebuild should be run on a host with working registry access.

**Deviations from the plan:** (1) m9 keeps the token (reasoned above) rather than clearing it; (2) m12's `toViewType` left by design; (3) C2 added a `.prettierrc.json` (repo had none, so bare-default formatting would have churned tabs→spaces) and normalized 66 files to the SvelteKit-standard style.
