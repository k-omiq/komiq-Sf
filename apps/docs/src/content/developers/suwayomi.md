---
title: Suwayomi, the source engine
description: Understand the admin app's server-side Suwayomi integration, extension management and Docker setup.
---

The self-hostable admin app runs the komiq server, management portal and Suwayomi engine. Reader apps connect to that server. This page covers engine requests, extensions and deployment.

## What Suwayomi is

Suwayomi fetches metadata, chapter lists and images through extensions you install in the admin console.

Suwayomi exposes an internal GraphQL API alongside endpoints for covers and page images. The komiq server uses this engine:

```flow
Reader app -> komiq server: requests chapter pages
komiq server -> Suwayomi API: fetches page URL list
komiq server -> Image proxy: routes image requests
Image proxy -> Suwayomi endpoint: fetches source image bytes
Suwayomi endpoint -> Source site: downloads image through extension
```

Komiq provides the interface, accounts, libraries, [canonical works](/developers/canonical-works) and [catalogue sync](/developers/catalogue-sync).

## How the server communicates with Suwayomi

The komiq server communicates with Suwayomi using the `SuwayomiClient` in `apps/server/src/suwayomi.rs`.

### Network isolation and address configuration

The main engine in self-hosted Compose has no authentication or published port. Komiq reaches it over Docker's internal network. `SUWAYOMI_URL` defaults to `http://localhost:4567`; the Docker image uses `http://suwayomi:4567`. `SUWAYOMI_PUBLIC_URL` sets the browser image base. Docker uses `/`, so images load through komiq.

### Request concurrency and timeouts

External comic sites vary widely in speed and reliability. To prevent slow sources from exhausting server resources, `apps/server/src/suwayomi.rs` isolates requests into separate concurrency pools:

- **Cover pool**: bounded by `COVER_FETCH_CONCURRENCY` (12 permits). Cache warming (`WARM_COVER_CONCURRENCY = 3`) and background materializations (`BG_MATERIALIZE_CONCURRENCY = 4`) together leave 5 permits for other requests. Enrolment-time cover hashing also uses this pool and has no separate concurrency bound. Cover requests timeout after 8 seconds (`COVER_TIMEOUT_SECS`), and response bodies are capped at 24 MiB (`MAX_COVER_SOURCE_BYTES`).
- **Page pool**: bounded by `PAGE_FETCH_CONCURRENCY` (16 permits). When the pool is saturated, incoming page fetches wait up to 6 seconds (`PAGE_ACQUIRE_WAIT_SECS`) before shedding load with a retryable 503 error. Page requests timeout after 20 seconds (`PAGE_TIMEOUT_SECS`), and response bodies are capped at 32 MiB (`MAX_PAGE_SOURCE_BYTES`).

Streamed reading functions buffer bodies within the cap and reject a declared length or incoming chunk that would exceed it.

### Provenance tracking

When an upstream fetch mutation fails, komiq's `SuwayomiClient` falls back to queries that read Suwayomi's local database. This behavior helps reader apps during transient outages, but it can deceive background scanners:

- A query reading stale data returns a successful response with zero new chapters.
- If recorded as a clean scan, the scanner cannot tell a healthy source from a broken source.

To prevent this issue, `SuwayomiClient` tags results with a `Provenance` flag:

- `Provenance::Upstream`: the upstream site answered the request directly.
- `Provenance::CachedFallback`: the fetch mutation failed and the client read stored data with a query.

The reader accepts cached fallbacks, while the adaptive scanner treats cached fallbacks as upstream failures and schedules retries.

## Extension repositories and extensions

Extensions are packaged APK files that Suwayomi installs and runs. You manage extensions through the admin console at [Sources](/admin/sources), and repositories under [Settings](/admin/settings).

### Managing extension repositories

An extension repository is an external index URL listing available extension packages. Suwayomi supports multiple repositories simultaneously:

- Administrators add or remove repository index URLs using `add_extension_store` and `remove_extension_store`.
- Calling `refresh_extensions` prompts Suwayomi to fetch updated repository manifests.
- The `list_extensions` method returns available packages, their version numbers, language codes, and NSFW flags.
- When an administrator installs or updates an extension, the server records each of its sources in `source_extension`: the package name, the repository it came from, and the NSFW flag. Reader apps with their own engine read these coordinates through `chapterOrigin`, and ingestion reads the flag.

### Stored engine keys

Suwayomi numbers its own rows, and a chapter's real address on the source is a pair of keys, one for the manga and one for the chapter. `apps/server/src/engine_keys.rs` keeps these keys, with the web links, in PostgreSQL:

- **Start**: the server adopts the engine as the active generation when PostgreSQL has none. After that it checks the engine's generation marker and a sample of stored keys at start, on each scan tick and after a lost connection. `apps/server/src/engine_generation.rs` quarantines numeric reads and writes when the engine was replaced. The admin console's Health page shows the state and both ways out, see [Source engine state](/admin/engine).
- **Capture**: when the server already fetches a series or its chapter list, it writes the keys in the same transaction as the cache. It skips a write when nothing changed.
- **Backfill**: a background task runs once per scan tick, 60 seconds, and reads up to four series per run from the engine to fill keys that are missing.
- **`chapterOrigin`**: the server answers from the stored keys. It asks the engine only on a miss, then stores the answer.

The tables are described in [Data model](/developers/data-model).

### Regional engines

`apps/server/src/engine_route.rs` adds engines that fetch pages and covers. The main engine (`SUWAYOMI_URL`) owns catalogue ids, scanning and chapter lists. See [Add a regional engine](/self-host/regional-engine) for private tunnels and Basic authentication.

The resolver uses active-generation source keys, like `packages/api/src/local-suwayomi-backend.ts`: `mangas(sourceId, url)`, then `chapters(mangaId, url)`. On a miss it searches by title or fetches chapters, matching the source URL. Memoized results expire after 1 hour for manga, 5 minutes for chapters and pages, and 10 minutes for absences.

`Engines::decide` returns `Owner` for no route or an unusable non-strict route, `Regional` when health and extension versions match, or `Refuse` for an unusable strict route. Missing content or errors fall back and increment `fallbacks`; strict routes return 503. `Busy` returns the usual retryable page 503 with `Cache-Control: no-store`.

Migration `apps/server/migrations_pg/0015_source_engine_route.sql` stores `source_id`, the configured `engine_id`, `strict`, `set_at` and `set_by`. Regional manga and chapter numbers and page paths stay in memory, never in PostgreSQL or reader responses.

Counters separate engine database lookups from source requests. Background work shares `SourceBudgets` by source across engines: `spend` charges the operation's cost; `spend_media_page` admits archived pages. Changing engines creates no separate budget.

- A chapter whose source keys are not stored yet is served by the main engine even under a strict route.
- A cold regional cover lookup may make one source search before fetching the thumbnail.
- During an owner extension-update batch, a routed source may use the regional engine's older version until the batch finishes and versions are compared again.
- If a regional update fails part-way and the engine loses the extension, its routes stay off. Strict routes return 503 until you run `komika-server engine provision <id>` again.
- After replacing a regional engine's data, set the route again to clear its resolved ids.

### Automatic extension updates

Source websites frequently update their page structures, breaking older scrapers. To keep sources functional without manual maintenance, the background worker in `apps/server/src/maintenance.rs` checks for updates automatically:

1. The updater begins its first run 45 seconds after server startup.
2. It repeats every 6 hours by default (`EXTENSION_AUTO_UPDATE_INTERVAL_SECONDS = 21600`). Setting this variable to `0` disables automatic updates.
3. The worker refreshes configured repository indexes, identifies installed extensions that report newer versions, and updates them sequentially.
4. Each attempt is recorded in `extension_update_log`, which stores the last 500 entries (`UPDATE_LOG_KEEP`).
5. After updating an extension, the server refreshes source identifiers in `source_extension` so the scanner and reader know current source coordinates.

The updater only upgrades extensions that an administrator previously installed. It never installs uninstalled packages automatically.

## FlareSolverr integration

Some comic sources protect their sites with Cloudflare challenge screens ("checking your browser" interstitial pages). Standard scraping requests fail against these challenges.

FlareSolverr is an optional companion service that runs a headless browser to handle challenge pages for Suwayomi.

### How FlareSolverr connects

The self-hosted Compose configuration passes the enable flag and FlareSolverr service address to Suwayomi:

```flow
Compose -> Suwayomi: sets FLARESOLVERR_ENABLED
Compose -> Suwayomi: sets FLARESOLVERR_URL=http://flaresolverr:8191
```

### Configuration and memory caps

FlareSolverr runs as an optional Docker Compose service using the `flaresolverr` profile:

```yaml
flaresolverr:
  profiles: [flaresolverr]
  image: ghcr.io/flaresolverr/flaresolverr@sha256:139dfee1c6f89249c8d665d1333a42e8ec74ec0a86bc6bb1c8461e10d3a66a47
  restart: unless-stopped
  mem_limit: 2g
  environment:
    TZ: ${TZ:-UTC}
    LOG_LEVEL: info
```

Because headless browsers can consume significant memory on complex pages, `mem_limit: 2g` bounds memory usage so runaway browser tabs cannot starve the server.

To activate FlareSolverr on a self-hosted installation, set both values in your `.env` file:

```sh
COMPOSE_PROFILES=flaresolverr
FLARESOLVERR_ENABLED=true
```

In `deploy/selfhost/docker-compose.yml`, Suwayomi receives `FLARESOLVERR_ENABLED` with a default of `false` and points to `FLARESOLVERR_URL=http://flaresolverr:8191`.

## How it runs in Docker

Komiq packages Suwayomi inside `deploy/selfhost/docker-compose.yml` alongside the server container.

### Suwayomi container definition

```yaml
suwayomi:
  image: ghcr.io/suwayomi/tachidesk@sha256:e59f212ccf91b26de8676a063a2db90256c148c6261535c7c477d47a43d9751b
  restart: unless-stopped
  environment:
    TZ: ${TZ:-UTC}
    BIND_IP: 0.0.0.0
    BIND_PORT: '4567'
    WEB_UI_ENABLED: 'false'
    FLARESOLVERR_ENABLED: ${FLARESOLVERR_ENABLED:-false}
    FLARESOLVERR_URL: http://flaresolverr:8191
  volumes:
    - engine-data:/home/suwayomi/.local/share/Tachidesk
  healthcheck:
    test:
      ['CMD-SHELL', 'curl -fsS http://localhost:4567/api/v1/settings/about >/dev/null || exit 1']
    interval: 20s
    timeout: 5s
    retries: 8
    start_period: 60s
```

### Key operational settings

- **Image pinning**: the image is pinned to release `v2.3.2243` by SHA256 digest (`e59f212ccf...`). Upgrades occur through deliberate edits rather than arbitrary image pulls.
- **Web UI disabled**: `WEB_UI_ENABLED: 'false'` disables Suwayomi's built-in web frontend. This prevents the container from downloading web assets from GitHub on every startup.
- **Persistent storage**: the `engine-data` volume mounts to `/home/suwayomi/.local/share/Tachidesk`. It stores installed extensions and Suwayomi's internal database.
- **Health monitoring**: a Docker healthcheck curls `/api/v1/settings/about` every 20 seconds.
- **Boot coordination**: the `server` container specifies `depends_on: suwayomi: condition: service_started`. The komiq server boots without waiting for Suwayomi to become healthy, retrying requests while the engine initializes.

To install and run the stack, see the [Docker installation guide](/self-host/docker).

## Where to look

- `apps/server/src/suwayomi.rs`: Suwayomi client implementation, GraphQL queries, concurrency semaphores, and provenance checks.
- `apps/server/src/engine_keys.rs`: stored engine keys, generation adoption, `chapterOrigin` lookup and the backfill task.
- `apps/server/src/engine_route.rs`: regional resolution, routes, version checks and counters.
- `apps/server/src/source_budget.rs`: shared source request budgets.
- `apps/server/migrations_pg/0015_source_engine_route.sql`: per-source routes and strict mode.
- `apps/server/src/maintenance.rs`: automatic extension update supervisor and log history.
- `deploy/selfhost/docker-compose.yml`: container definitions, network links, volume mounts, and memory limits.
- `deploy/selfhost/Dockerfile`: internal engine address and browser image URL configuration.
- `deploy/selfhost/.env.example`: self-hosted environment flags for FlareSolverr and network binding.
- `docs/self-host.md`: user instructions for volume backups, updates, and port configurations.
