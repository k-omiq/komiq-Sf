---
title: Deployment
description: Understand how the project deploys the global server with Cloudflare, Docker, and backups.
---

This page describes the global server at `komiq.cc`, the demo. The admin app supplies its server, console and engine; the reader supplies its reading interface. For your own server, see [Install with Docker](/self-host/docker) and [Internet access](/self-host/internet-access).

The global server combines Cloudflare Workers, Docker services and the optional image proxy. PostgreSQL backups use object storage.

## Workers and edge routing

In this deployment, Cloudflare Workers serve public traffic at the edge, near the reader. Each component runs under its own hostname with dedicated routing and caching behavior:

```flow
Reader app -> Image proxy: requests chapter page image
Image proxy -> Cloudflare edge cache: checks for cached image
Image proxy -> API server: fetches non-canonical source image
API server -> Suwayomi engine: retrieves image bytes from source
Image proxy -> Reader app: delivers image bytes
```

### The reader Worker

The [reader app](/developers/reader-app) runs on `komiq.cc` and `www.komiq.cc`. It is deployed from `apps/reader` using the Worker name `komika-reader`:

- Built with `@sveltejs/adapter-cloudflare`.
- Executes server-side rendering for `/series/*` only, preserving series text and share previews.
- Serves an adapter-generated `index.html` SPA shell for every other document, including home, browse, reading, library and unknown paths, without invoking the User Worker.
- Serves static assets, including styles, bundled scripts, icons, and fonts, through Cloudflare Workers Static Assets.
- Inlines public configuration like the API endpoint and image proxy URL during build time.
- Requires no edge secrets because public page rendering only accesses unauthenticated catalog data.

The web build's Workers configuration is:

```toml
[assets]
binding = "ASSETS"
directory = ".svelte-kit/cloudflare"
not_found_handling = "single-page-application"
run_worker_first = ["/series/*", "!/series/"]
```

The adapter generates the shell from the SPA fallback setting. `_routes.json` applies only to Pages. Wrangler's bundled schema sends `run_worker_first` matches to the User Worker; its asset router resolves other paths through the SPA fallback. The negative pattern takes precedence, so `/series/` with no slug uses the static shell. Warm series HTML still invokes the Worker: caching saves rendering, not invocations.

The split has been checked offline against the bundled router and generated output. Production invocation counts have not been measured. No deployment or billing result follows from the local check.

For a pure static alternative, build with `KOMIKA_TARGET=static pnpm -C apps/reader build`. `apps/reader/wrangler.static.toml` serves `build` without a script, including series routes. It uses the separate name `komika-reader-static`, has no routes and disables `workers_dev`. An operator adds a test hostname when ready. Both builds generate `_headers` with exact inline-script CSP hashes, `Cache-Control: no-cache` for HTML and `public, max-age=86400` for `/_app/immutable/*`. The bundled asset worker can return the SPA shell for a missing chunk, even without `Sec-Fetch-Mode: navigate`. Since `_headers` matches the requested URL, an immutable rule would cache that HTML under the missing chunk URL for a year. One day bounds it, and missing assets never receive an immutable cache policy.

Before releasing either build, copy the previous output's immutable files into the new output:

```sh
node apps/reader/deploy/retain-assets.mjs /path/to/previous-build apps/reader/.svelte-kit/cloudflare
# For the static target, use apps/reader/build as the second path.
```

The script copies only `_app/immutable`, preserves current files and refuses same-name files with different bytes. It never copies old HTML or headers. Keep a snapshot of each release output before the next build clears that directory. When starting without a previous release, omit this copy step. The page reloads once on a failed bootstrap import or Vite preload, then offers a manual Reload button. A successful root mount clears the guard so a later failure can recover again.

### The admin app's console Worker

This deployment uses the secondary `hosted` build of the [admin app](/developers/admin-app) on `admin.komiq.cc`. It is deployed from `apps/admin` using the Worker name `komika-admin`:

- Built with `@sveltejs/adapter-static` with fallback `index.html`.
- Serves prebuilt client-side assets through Cloudflare Workers Static Assets.
- Sets `not_found_handling = "single-page-application"` so browser reloads and deep links resolve to `index.html` with an HTTP 200 response.
- Talks directly to the GraphQL API at `https://api.komiq.cc/graphql`.

### The image proxy Worker

The [image proxy Worker](/developers/image-proxy) runs on `img.komiq.cc`. It is deployed from `apps/worker` using the Worker name `komika-img`:

- Proxies and caches images. `ALLOWED_SOURCE_HOSTS` permits `uploads.mangadex.org`, `mangadex.network` and `api.komiq.cc`; other upstream hosts are rejected.
- Checks browser Origin and Referer headers against `ALLOWED_ORIGINS`. Requests without either header remain permitted.
- Limits each IP to 600 cache misses per minute through `IMG_RATE_LIMITER`. Cache hits do not count.

### The documentation Worker

The public documentation runs on `docs.komiq.cc`. It is deployed from `apps/docs` using the Worker name `komika-docs`:

- Built with `@sveltejs/adapter-static` as a prerendered static site.
- Serves static HTML and client assets directly from Cloudflare Workers Static Assets.
- Sets `not_found_handling = "404-page"` to return a pre-rendered 404 page for missing URLs.

### The API origin

The backend API is reachable at `api.komiq.cc`. This hostname routes public API and proxy traffic to the containerized server.

## Admin app server and engine

The admin app's server and Suwayomi engine run alongside a FlareSolverr sidecar inside Docker on a private bridge network (`komika`), configured in `deploy/docker-compose.yml`:

| Service           | Image                                   | Ports                              | Memory limit              |
| ----------------- | --------------------------------------- | ---------------------------------- | ------------------------- |
| `postgres`        | Built from `deploy/postgres/Dockerfile` | None, internal network             | See Compose configuration |
| `postgres-backup` | Same database tooling image             | None, internal network             | See Compose configuration |
| `server`          | Built from `deploy/server.Dockerfile`   | `127.0.0.1:8080`, `127.0.0.1:9090` | 6 GB                      |
| `suwayomi`        | Digest-pinned `tachidesk`               | `127.0.0.1:4567`                   | 8 GB                      |
| `flaresolverr`    | Digest-pinned `flaresolverr`            | None (internal network only)       | 2 GB                      |

### The server container

The Rust server uses Axum, async-graphql and PostgreSQL. It serves GraphQL at `/graphql`, database readiness at `/health/ready`, and source images at `/api/v1/manga/...`. Chapter-id page locators use `/media/v1/c/{chapterId}/{page}`. With `KOMIKA_MEDIA_MANIFESTS=on` it also serves exact objects and signed manifests under `/media/v1/`; read [Exact media and signing](/developers/media-manifests). It also runs catalogue scans and updates.

Public ingress reaches port 8080 through a reverse proxy or tunnel. Compose publishes it on loopback only. Metrics use port 9090, also published on loopback. The server has a 6 GB memory cap.

### The Suwayomi engine

Suwayomi runs Tachidesk, which executes source extensions, resolves chapter pages and retrieves covers. Its administrative API has no authentication, so public clients must never reach it directly. Compose publishes port 4567 on loopback only; the server uses the private Docker address `http://suwayomi:4567`.

The image is pinned by digest. Its memory cap is 8 GB, with `JAVA_TOOL_OPTIONS=-XX:MaxRAMPercentage=70` limiting JVM heap allocation.

### The FlareSolverr sidecar

FlareSolverr handles browser-verification pages on protected sources. It runs at `http://flaresolverr:8191` inside Docker, with no public ports and a 2 GB memory cap.

## PostgreSQL and backups

PostgreSQL 18 stores accounts, sessions, progress, libraries, discussions and catalogue state in the private `postgres-data` volume. The server uses a non-superuser role. Initialization fixes the encoding and locale.

`postgres-backup` runs pgBackRest for full and incremental backups to object storage. It also archives write-ahead logs (WAL), the transaction records needed for recovery. Configuration and restore procedures live in `deploy/postgres/`.

The separate `/data/covers.sqlite3` cache stays in `server-data` and is excluded from PostgreSQL backups. An open or corruption failure preserves its files with a `.quarantined-<id>` suffix and recreates the cache. Accounts and libraries remain in PostgreSQL. Connector files are separate too; a database backup does not preserve filesystem credentials.

### Conversion and restoration

The server can import an old main SQLite database at first start when PostgreSQL is empty. It verifies the converted data before serving, renames the original after success and never deletes it. Progress starts with `sqlite import:`. A failure exits without serving partial data. A populated PostgreSQL database is never overwritten by automatic import. [Data model](/developers/data-model) lists what the import decides and refuses.

Hosted recovery uses pgBackRest restore into an empty cluster directory with PostgreSQL stopped. The procedure, with its rehearsal, is `deploy/postgres/RESTORE.md`; it is a runbook for operators of this packaging, and this page does not repeat it. A point-in-time restore combines a backup with archived WAL up to the chosen recovery point. Review the restore tooling and verify the restored database before making it available to users. Keep recovery separate from ordinary startup; never substitute an empty database for a failed restore.

`export-postgres-to-sqlite` creates a SQLite export for older builds. It refuses unsupported data, such as per-user scanlator ratings. It leaves out the three `engine_*` key tables even when they hold rows, because the server fills them again, and the report lists their row counts. Its report lists deleted-comment markers lost by a migration 0121 target.

`.komiq-server.lock` prevents concurrent servers using one covers directory. `phase-f` requires an existing, compatible schema and neither migrates nor recreates covers.

### Self-host and desktop

The self-host Compose file runs stock PostgreSQL privately alongside the server and source engine. `deploy/selfhost/install.sh` generates the password, keeps `.env` private and detects old SQLite volumes. It extends readiness waits as import progress arrives. Self-host backups use `deploy/selfhost/backup.sh`, a documented `pg_dump` job rather than the global deployment's pgBackRest setup. Read [Update and back up](/self-host/update-and-backup) for dump and fresh-install restore commands.

The desktop admin app bundles PostgreSQL, converts an old SQLite database at first start and saves a local dump while running when the newest is at least 20 hours old. It keeps seven completed dumps. The reader app does not run the account database.

## Local validation

Build the static target, then web, before running the reader tests. The tests inspect both emitted shells:

```sh
KOMIKA_TARGET=static pnpm -C apps/reader build
pnpm -C apps/reader build
pnpm -C apps/reader test
```

The repository uses Node's test runner. These commands need no additional test dependency.

## Status

Production cutover, restore rehearsal and desktop installer validation are planned. Backup costs and recovery duration remain unmeasured. Packaging code does not establish production state.

## Where to look

- `apps/reader/wrangler.toml`: Worker routing, build output paths, and domain bindings for the reader app.
- `apps/reader/wrangler.static.toml`: Pure static alternative with no script.
- `apps/reader/deploy/static-assets.mjs` and `deploy/static-csp.mjs`: Shell headers and exact inline-script hashing.
- `apps/reader/deploy/retain-assets.mjs`: Previous-release chunk retention.
- `apps/admin/wrangler.toml`: Static asset configuration and custom domain setup for the admin console.
- `apps/worker/wrangler.toml`: Host allowlists, hotlink restrictions, and rate limiter bindings for the image proxy.
- `apps/docs/wrangler.toml`: Static documentation site routing on Cloudflare Workers.
- `deploy/docker-compose.yml`: Service definitions, memory caps, network settings, and volume mappings.
- `deploy/server.Dockerfile`: Build stages for the Rust server container.
- `deploy/server-entrypoint.sh`: Container startup checks and server process execution.
- `deploy/postgres/`: PostgreSQL initialization, pgBackRest backup scheduling and restore tooling.
- `deploy/postgres/RESTORE.md`: the hosted backup, restore and rehearsal procedure.
