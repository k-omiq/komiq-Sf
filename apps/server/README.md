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

Build from this directory, or from the repository root with `--manifest-path
apps/server/Cargo.toml`. The root `.cargo/config.toml` fixes bundled SQLite's mmap
ceiling at 8 GiB; both Docker builders copy it. The main pool requests that amount
of virtual address space, without eagerly loading the database into memory.

Config (env / `.env`): `PORT`, `DATABASE_URL` (default `sqlite://komika.sqlite3`),
`SUWAYOMI_URL` (default `http://localhost:4567`), `SUWAYOMI_SOURCE_ID` (optional pin),
`CORS_ORIGINS` (reader dev/preview origins — add the admin origin `http://localhost:5273`
to use the console), `KOMIKA_ADMIN_USERS` (comma-separated reserved admin usernames;
provisioned at startup, never through registration). Needs the Suwayomi container running
(`docker start suwayomi`) for the federated half.

Self-host settings: `BIND_ADDR` (listener address, default `0.0.0.0`), `KOMIKA_WEB_DIR`
(optional; serves `reader/` at `/` and `admin/` at `/admin` with an `index.html` SPA
fallback, below every API route), `KOMIKA_INSTANCE_NAME`, `KOMIKA_REGISTRATION_OPEN` and
`KOMIKA_APPS_FETCH_THEMSELVES` (on: reader apps with their own source engine fetch chapter
pages themselves and do not offer fetching through this server; the web reader is not
affected).
Instance settings read the env variable first, then the `instance_setting` table (changed
from the admin console), then a default, so a value pinned in env cannot be changed at
runtime. `registration_open` defaults to open, except on a self-hosted install
(`KOMIKA_WEB_DIR` set), which starts closed until the owner opens it.

An account created or reset by an admin holds a temporary password
(`must_change_password`). Until it calls `changePassword`, the server answers only the
`session` query (and `logout`) for it: calls that need a signed-in user are refused,
and calls that work signed out answer as if it were signed out.

MangaDex has two separate settings. **Usage** (`mangadex_enabled`: may this server talk
to MangaDex at all — reading, covers, metadata, `/img`) resolves `KOMIKA_MANGADEX`
(on/off, pins it), then `CATALOGUE_SYNC=on` (pins it on), then the admin's switch, then
the default: off on a self-hosted install (`KOMIKA_WEB_DIR` set), on everywhere else.
The **catalogue crawl** runs where `CATALOGUE_SYNC=on` (one replica only — see
`deploy/.env.example`), never where it is `off`, and, when it is unset, only on a
self-hosted install while usage is on. `METADATA_BACKFILL`/`COVER_PHASH` act only while
usage is on; `COVER_CACHE=off` still stops the whole cover drainer. A third, unrelated
switch decides what readers are SHOWN: `show_mangadex_ratings` (`KOMIKA_MANGADEX_RATINGS`
pins it, default on). Off, no response carries `mangadexRating` and every surface shows
our own readers' rating only. `/img` (the image
proxy) is mounted when `KOMIKA_WEB_DIR` is set or `KOMIKA_IMG_PROXY=on`.

### First-run setup

An instance is set up once any admin account exists. With no admin at boot (none in the
database and none provisioned from `KOMIKA_ADMIN_USERS`), the server prints a one-time
setup code to the log. The public `instance { setupRequired }` query reports the state,
and `createOwner(setupCode, username, email, password)` creates the first admin with that
code. The code lives in memory only, is never returned by the API, and is retired once the
owner exists; a restart before then prints a new one.

`komika-server setup` runs the same first-run steps in a terminal (`src/setup_cli.rs`),
for a server with no screen. It is a GraphQL client of the running server
(`http://127.0.0.1:$PORT`, or `--server <address>`): it opens no database and builds no
server state, so every rule stays the server's own. `--setup-code <code>` (or
`KOMIKA_SETUP_CODE`) supplies the setup code, and
`--access <local|internet|tunnel|public-ip|direct>` skips the access question; `tunnel`
and `public-ip` also choose the Cloudflare connection. `--encrypt <yes|no>` goes with
`--access direct` only and answers the HTTPS question. `deploy/selfhost/install.sh` passes
all three. The command refuses a plain `http://` `--server` address on the internet before
any password is typed, and asks an account with a temporary password to choose a new one.
A setup code given to a server that already has an owner changes nothing: the command says
so and stops.

Before a public choice, the terminal setup and the console show what the server found on
its own machine (`accessPreflight`, `cloudflare::Manager::preflight`): whether
`cloudflared` is found, will be downloaded or cannot be had, the state of the HTTPS port
and of the certificate-check port (`free`, `serving`, `in_use`, `denied` or `unknown`),
whether the server runs in a container, and its public address. Tunnel setup looks for
`cloudflared` in `CLOUDFLARED_BIN`, then `PATH`, then `bin/` inside the Cloudflare folder
(`KOMIKA_CLOUDFLARE_DIR`, default `cloudflare/` next to the database). When it finds none,
the checklist line "Get the connector (cloudflared)" downloads the release pinned in
`src/cloudflare/fetch_connector.rs` and checks its SHA-256 before saving it there. With
`CLOUDFLARED_BIN` set it never downloads.

The access choice (`setAccessMode`, shown by `accessStatus`) is `local`, `internet` (a
domain with Cloudflare in front) or `direct` (public at the server's own address, no
domain and no Cloudflare; `domain` then carries that address). `internet` and plain
`direct` are display only: they do not change what the server listens on. With `local`
the plain HTTP port refuses clients on the internet (`plain_http_guard` in
`src/cloudflare/direct.rs`); the home network, a VPN, this computer and trusted proxies
are let through. `direct` with `encrypt: true` changes what the server listens on: the
server gets a Let's Encrypt certificate for the
address (`src/cloudflare/acme.rs`, http-01 on port 80), serves HTTPS on
`KOMIKA_HTTPS_PORT` and renews the certificate by itself (`src/cloudflare/direct.rs`).
`cloudflare::plain_http_guard`, a layer of the public router, then stops handling plain
HTTP requests from internet addresses once one of them has used HTTPS. `accessStatus.lanAddresses` lists the
server's home network addresses (`src/lan.rs`); it is empty for a loopback-only listener
and inside a container. `accessStatus.lanClientSeenAt` is the last time since boot that a
device on the home network reached the server (never set inside a container); the desktop
app's Home network step waits on it. HTML served from `KOMIKA_WEB_DIR` is sent with
`Cache-Control: no-cache`, so a browser never reuses a page from before an update.

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
