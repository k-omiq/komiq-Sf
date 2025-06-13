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
to use the console), `KOMIKA_ADMIN_USERS` (comma-separated usernames granted admin;
promoted at startup + on registration). Needs the Suwayomi container running
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

| File                   | Responsibility                                           |
| ---------------------- | -------------------------------------------------------- |
| `src/main.rs`          | axum wiring, CORS, GraphiQL, bearer → GraphQL context    |
| `src/config.rs`        | env config                                               |
| `src/db.rs`            | SQLite pool + migrations                                 |
| `src/auth.rs`          | argon2 hashing, token gen, session→user lookup           |
| `src/suwayomi.rs`      | federation client (mirrors the TS `SuwayomiBackend`)     |
| `src/graphql/types.rs` | GraphQL types + Suwayomi→Komika mapping helpers          |
| `src/graphql/mod.rs`   | `AppState`, Query + Mutation resolvers, rating aggregate |
| `migrations/`          | `users`, `sessions`, `reviews`, `comments`               |
| `bench/`               | load-test harness + measured baselines (`bench/README.md`) |

## Performance

Capacity is measured, not guessed. `bench/loadtest.mjs` is a zero-dependency load
generator that speaks the reader's real GraphQL operations; `bench/README.md` covers
usage and — importantly — what a clean run does *not* prove.

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
