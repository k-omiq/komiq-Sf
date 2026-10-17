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
- Refuses `/media/` paths on every host and the hosts in `MEDIA_ROUTE_HOSTS`, on `src` and every redirect hop. Read [Media routes](/developers/media-routes).

> [!WARNING]
> Deploy the Worker with the media route rule before you set `KOMIKA_MEDIA_MANIFESTS=on` on the global server. `api.komiq.cc` is on the Worker's host allowlist, so an older Worker would fetch and cache exact objects from `/media/v1/o` outside the server's policy.

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

### The fallback listener

`deploy/server.Dockerfile` also builds the `embedded` reader and copies it to `/srv/web/reader`. It sets `KOMIKA_FALLBACK_READER_DIR` to that folder for the fallback HTTPS listener only. The main port still serves no web files. The fallback serves the reader, the API and media, never the admin console.

The listener is off unless `KOMIKA_FALLBACK_CERT_FILE` is set. The operator supplies a publicly trusted PEM certificate chain and key, and renews them; the server reloads changed files within five minutes. Read [Fallback address](/self-host/internet-access#fallback-address) for the variables, the `421` host check and the daily media budget.

`deploy/docker-compose.fallback.yml` adds the variables, a read-only mount of the certificate folder and the published port (8443 by default). In `deploy/.env`, set `KOMIKA_FALLBACK_TLS_DIR` to a folder with `fullchain.pem` and `privkey.pem` that user 10001 can read, `KOMIKA_FALLBACK_HOSTS` to the fallback host names and `KOMIKA_MEDIA_ROUTES` with the owner `base` set to the main public origin. Then run:

```sh
docker compose -f deploy/docker-compose.yml -f deploy/docker-compose.fallback.yml up -d
```

The embedded reader reads the owner entry of `mediaRoutes` and rewrites image addresses on that origin onto its own origin. With the default empty `base` it cannot tell the main origin, so images that name the main host still point there. The static build never rewrites.

### The Suwayomi engine

Suwayomi runs Tachidesk, which executes source extensions, resolves chapter pages and retrieves covers. Its administrative API has no authentication, so public clients must never reach it directly. Compose publishes port 4567 on loopback only; the server uses the private Docker address `http://suwayomi:4567`.

The image is pinned by digest. Its memory cap is 8 GB, with `JAVA_TOOL_OPTIONS=-XX:MaxRAMPercentage=70` limiting JVM heap allocation.

### The FlareSolverr sidecar

FlareSolverr handles browser-verification pages on protected sources. It runs at `http://flaresolverr:8191` inside Docker, with no public ports and a 2 GB memory cap.

## PostgreSQL and backups

PostgreSQL 18 stores accounts, sessions, progress, libraries, discussions and catalogue state in the private `postgres-data` volume. The server uses a non-superuser role. Initialization fixes the encoding and locale.

`postgres-backup` runs pgBackRest for full and incremental backups to object storage. It also archives write-ahead logs (WAL), the transaction records needed for recovery. Configuration and restore procedures live in `deploy/postgres/`.

Cover images and extension icons are files in `/data/covers` (`KOMIKA_COVERS_DIR`), inside `server-data`. Files are named by their SHA-256 content hash at `objects/<2 hex>/<sha256>`; `tmp/` holds writes in progress. PostgreSQL holds the cover index and uploads since this update. Backups include these records, not the cover files. After a database restore, missing files refill from sources on request or through the background crawl and warmer after garbage collection notices them. Uploads return from PostgreSQL. Connector files remain separate; a database backup does not preserve filesystem credentials.

Old installations copy `/data/covers.sqlite3` into files in the background. Open or corruption failures quarantine only this old cache during the copy. Earlier uploads copy too, but are not in database backups. Read [Update and back up](/self-host/update-and-backup) for disk space, restart recovery and when the owner can delete the old cache.

### Restoration

Hosted recovery uses pgBackRest restore into an empty cluster directory with PostgreSQL stopped. The procedure, with its rehearsal, is `deploy/postgres/RESTORE.md`; it is a runbook for operators of this packaging, and this page does not repeat it. A point-in-time restore combines a backup with archived WAL up to the chosen recovery point. Review the restore tooling and verify the restored database before making it available to users. Keep recovery separate from ordinary startup; never substitute an empty database for a failed restore.

`COVERS_DATABASE_URL` still places `.komiq-server.lock`, the default covers directory and the old copy source. Only the owner server writes covers under that data-directory lock. `phase-f` requires an existing, compatible schema and neither migrates nor recreates covers. Garbage collection removes cover files older than 24 hours with no entry in the index.

### Self-host and desktop

The self-host Compose file runs stock PostgreSQL privately alongside the server and source engine. `deploy/selfhost/install.sh` generates the password and keeps `.env` private. Self-host backups use `deploy/selfhost/backup.sh`, a documented `pg_dump` job rather than the global deployment's pgBackRest setup. Read [Update and back up](/self-host/update-and-backup) for dump and fresh-install restore commands.

The desktop admin app bundles PostgreSQL and saves a local dump while running when the newest is at least 20 hours old. It keeps seven completed dumps. The reader app does not run the account database.

### Optional standby

The global packaging can run a second PostgreSQL host as a standby: a copy that streams the primary's WAL and can be promoted by hand. It is opt-in. Without `KOMIKA_NODE_ID` the stack starts as before. The procedure is `deploy/postgres/STANDBY.md`; production has no standby yet.

- **Transport.** Replication runs over TLS. The role `komika_replica` has no password and logs in only with a client certificate from `hosted.py standby-certs`. That command refuses to write the keys inside a git work tree unless you pass `--inside-git-work-tree`. One physical replication slot keeps WAL for the standby, capped at 2 GB.
- **Fence.** `fence.json` sits in the backup bucket beside the pgBackRest path and names the one node that may be the primary, with an epoch and a timeline. PostgreSQL checks it before it starts as a primary, and the backup scheduler checks it before each job. A node the fence does not name refuses to start as a primary. A promoted node that restarts before its first checkpoint still starts: its data directory already holds the new timeline's history file.
- **Node name.** Set `KOMIKA_NODE_ID` in `deploy/.env`. The base `docker-compose.yml` passes it to PostgreSQL, the backup scheduler and the server, so `deploy.sh` keeps the fence on. The standby overlay adds the replication port and certificates. After a passed fence check, the data volume keeps the node name in a marker file, and a later start as a primary refuses when `KOMIKA_NODE_ID` is unset or different.
- **Promotion.** `hosted.py promote` runs only with `--old-primary-stopped` and the old primary's node name, the operator's word that this node is powered off or stopped. It moves the fence with a conditional write, so two promotions cannot both win, then calls `pg_promote()`. Nothing promotes automatically.
- **Server check.** With `KOMIKA_NODE_ID` set, the server refuses to start on a database in recovery or on one that another node's fence check started.
- **Lag.** `hosted.py status`, health and `/metrics` show the sent, flushed and replayed positions and the replay lag.

> [!WARNING]
> Replication is not backup. A standby copies a mistake at once. pgBackRest stays the backup and the only way back to an earlier time.

The engine volume, the cover cache, the media signing key, `deploy/.env` and the fallback certificate files are not in PostgreSQL. `STANDBY.md` lists what to copy to the standby host.

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
- `deploy/server.Dockerfile`: Build stages for the Rust server container and the embedded reader for the fallback listener.
- `deploy/docker-compose.fallback.yml`: Optional fallback listener: variables, certificate mount and port.
- `apps/server/src/fallback.rs` and `apps/server/src/fallback/tls.rs`: Fallback listener, host check, media budget and certificate reload.
- `packages/api/src/image-provider.ts` and `apps/reader/src/lib/media.ts`: Owner origin lookup and image address rewriting.
- `deploy/server-entrypoint.sh`: Container startup checks and server process execution.
- `deploy/postgres/`: PostgreSQL initialization, pgBackRest backup scheduling and restore tooling.
- `apps/server/src/cover_store.rs` and `apps/server/src/cover_copy.rs`: cover files, index and legacy copy.
- `deploy/postgres/RESTORE.md`: the hosted backup, restore and rehearsal procedure.
- `deploy/postgres/STANDBY.md` and `deploy/docker-compose.standby.yml`: the optional standby, its fence and manual promotion.
- `deploy/postgres/hosted.py`: fence checks, `standby-init`, `promote` and the replication status.
- `apps/server/src/postgres_metrics.rs` and `apps/server/src/node_fence.rs`: replication gauges and the server's start check.
