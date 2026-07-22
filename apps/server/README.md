# komika-server

The **Komika unified GraphQL API**. Implements `packages/api/src/schema/komika.graphql`
by federating a Suwayomi/Tachidesk server (catalog, chapters, pages, library,
progress) and adding Komika-native **multi-user social + auth + discovery** on SQLite.

Chosen in Rust for the smallest process footprint (~5–15 MB RSS, no GC) and to allow
sharing crates with the Tauri Rust core (`apps/reader/src-tauri`) later.

## Stack

- **axum 0.8** + **async-graphql 7** (code-first schema, a 1:1 mirror of the SDL)
- **SQLite** via **sqlx** (single file, `create_if_missing`, migrations at startup)
- **argon2id** password hashing + opaque session tokens (`Authorization: Bearer <token>`)
- **reqwest** for server-side Suwayomi federation

## Run

```sh
cargo run                      # dev, reads .env (see .env.example)
GraphiQL:  http://localhost:8080/graphql  (GET)
health:    http://localhost:8080/health
```

Config (env / `.env`): `PORT`, `DATABASE_URL` (default `sqlite://komika.sqlite3`),
`SUWAYOMI_URL` (default `http://localhost:4567`), `SUWAYOMI_SOURCE_ID` (optional pin),
`CORS_ORIGINS` (reader dev/preview origins — add the admin origin `http://localhost:5273`
to use the console), `KOMIKA_ADMIN_USERS` (comma-separated reserved admin usernames;
provisioned at startup, never through registration). Needs the Suwayomi container running
(`docker start suwayomi`) for the federated half.

The admin "manga DB" console (`apps/admin`) uses the admin-gated `updateSeriesAdmin`
mutation to upsert per-series overrides (`series_admin` table): scan-interval override,
poll cadence, forced pause, and status flag. These fold into every `Series.scan`/`status`.

## Point the reader at it

In `apps/reader/.env`:

```sh
PUBLIC_KOMIKA_BACKEND=on
PUBLIC_KOMIKA_BACKEND_KIND=komika
PUBLIC_KOMIKA_API=http://localhost:8080/graphql
PUBLIC_KOMIKA_IMG_MODE=direct
```

## Layout

| File                   | Responsibility                                             |
| ---------------------- | ---------------------------------------------------------- |
| `src/main.rs`          | axum wiring, CORS, GraphiQL, bearer → GraphQL context      |
| `src/config.rs`        | env config                                                 |
| `src/db.rs`            | SQLite pool + migrations                                   |
| `src/auth.rs`          | argon2 hashing, token gen, session→user lookup             |
| `src/suwayomi.rs`      | federation client (mirrors the TS `SuwayomiBackend`)       |
| `src/graphql/types.rs` | GraphQL types + Suwayomi→Komika mapping helpers            |
| `src/graphql/mod.rs`   | `AppState`, Query + Mutation resolvers, rating aggregate   |
| `migrations/`          | `users`, `sessions`, `reviews`, `comments`                 |
| `bench/`               | load-test harness + measured baselines (`bench/README.md`) |

## Performance

Capacity is measured, not guessed. `bench/loadtest.mjs` is a zero-dependency load
generator that speaks the reader's real GraphQL operations; `bench/README.md` covers
usage and — importantly — what a clean run does _not_ prove.

Baseline 2026-07-27: **~78 origin rps ≈ ~390 concurrent users**. The bottleneck is the
`discovery` resolver (~63 ms CPU/request, viewer-invariant, uncached). Benchmark
instructions and captured results are in `bench/`.

Re-run the `discovery` scenario as the acceptance test for any caching work.

## Not yet done

- Reader UI still reads social via `social.ts` (localStorage); swap to the server's
  `reviews/comments/postReview/postComment` once a login/register UI exists.
- `scan` policy is defaulted (paused for completed/hiatus); the adaptive scanner +
  admin overrides are a separate workstream.
- Per-user library/progress are proxied to Suwayomi's single global account for now.

## Automatic maintenance

Series names are compared article-insensitively ("The X" matches "X"). A matching
name with author names at least 90% similar auto-merges. Author comparison uses
Unicode character edit distance, ignoring case, punctuation and word order, and
matches through any listed person or parenthesised transliteration
("Dedart (디다트)" matches "Dedart"). Missing authors remain in review. External-ID
and corroborated-cover matching still apply.

One rule set (`catalog::pair::assess`) judges every pair of works. It gives one of
three answers: merge, different works, or review. The duplicate sweep (admin
Settings, and after catalogue ingestion) and the recheck of waiting reviews both
use it.

- Evidence is a matching author or artist, a matching cover, a similar
  description, or a shared title in Japanese, Korean or Chinese script.
- A site copy merges into a work on the same title or alternative title with one
  piece of evidence, or on a long specific title alone. An article-only difference
  ("The X" / "X") counts as the same title.
- Two MangaDex entries merge only on the same title with author or cover evidence,
  and never when one is a sequel ("X" / "X Season 2").
- A merge needs exactly one target. A work with "novel" in its name never merges
  with one without. Edition words (colored, pre-serialization, anthology, side
  story) and a different number in the title send the pair to review.
- A pair judged as different works is not queued. A waiting review judged that way
  is closed as `dismissed`. A dismissal is the machine's answer, so it does not
  stop a later merge when new evidence arrives; a `rejected` review is the admin's
  answer and does.
- The review queue holds only pairs the rules cannot decide, ordered by score.

Waiting reviews are rechecked at startup (after 30 seconds), hourly and after
catalogue ingestion. Confirmed/rejected reviews and admin splits are respected;
accepted waiting reviews are merged and confirmed atomically.

An admin can set the author, artist, year, original language, status, demographic
and content rating of a work. The value is written to the `work` row and recorded
in `work_field_pin`; triggers put it back when a sync overwrites it, and keep the
source's latest value so removing the pin restores it.

Installed Suwayomi extensions update automatically at startup (after 45 seconds)
and every six hours. The updater refreshes configured stores first, processes updates
sequentially, logs individual failures and retries them on the next pass. It never
installs an uninstalled extension. Set `EXTENSION_AUTO_UPDATE_INTERVAL_SECONDS`
to change the cadence, or `0` to disable it. After an update, cached extension/source
coordinates are refreshed for the scanner and reader.

### Bootstrap admin accounts safely

Set `KOMIKA_ADMIN_USERS` and `KOMIKA_ADMIN_PASSWORD` through your deployment secret
configuration. A missing account is created with that password. To promote an
existing non-admin, supply **that account’s current password** as the bootstrap
password, then restart. The account must not be suspended; ambiguous legacy
case-insensitive usernames are refused. Promotion preserves its password and
revokes all prior sessions, so the new admin must sign in again. A missing or
mismatched password never promotes an account. Existing admins remain unchanged
on subsequent starts; remove the bootstrap password after provisioning.

New admin sessions expire after at most eight hours (a shorter `SESSION_TTL_SECS`
is respected). Regular-user lifetimes remain configured by `SESSION_TTL_SECS`.
Existing sessions are not rotated by this change: revoke exposed sessions as part
of incident response. Browser bearer storage is still JavaScript-accessible.
