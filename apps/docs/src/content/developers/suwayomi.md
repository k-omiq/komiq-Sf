---
title: Suwayomi, the source engine
description: Understand the admin app's server-side Suwayomi integration, extension management and Docker setup.
---

Komiq is self-hostable: the admin app is the server and its management portal in one, and the reader app connects to a server. This page covers the source engine that runs with the server inside the admin app. It explains server communication, extension updates, FlareSolverr challenge handling and Docker setup.

## What Suwayomi is

Suwayomi is the source engine packaged with the admin app. It fetches comic metadata, chapter lists and page images from external sources. It runs the extension packages you install through the admin console to communicate with individual websites.

Suwayomi exposes an internal GraphQL API alongside endpoints for covers and page images. The komiq server uses this engine:

```flow
Reader app -> komiq server: requests chapter pages
komiq server -> Suwayomi API: fetches page URL list
komiq server -> Image proxy: routes image requests
Image proxy -> Suwayomi endpoint: fetches source image bytes
Suwayomi endpoint -> Source site: downloads image through extension
```

The admin console and reader app replace Suwayomi's user interface entirely. Suwayomi handles source-specific scraping, while komiq provides:

- Multi-user authentication, accounts, and session tokens.
- Cross-source deduplication into [canonical works](/developers/canonical-works).
- Reading progress, library organization, and notifications.
- Adaptive background scheduling in the [catalogue sync](/developers/catalogue-sync).
- Web, desktop, and mobile reader applications.

## How the server communicates with Suwayomi

The komiq server communicates with Suwayomi using the `SuwayomiClient` in `apps/server/src/suwayomi.rs`.

### Network isolation and address configuration

The self-hosted Compose configuration treats Suwayomi's API as unauthenticated and does not publish its port to the host network or the internet. Only the komiq server communicates directly with Suwayomi over Docker's internal container network.

The connection address is set by the `SUWAYOMI_URL` environment variable, which defaults to `http://localhost:4567`. The self-hosted Docker image sets it to `http://suwayomi:4567`. `SUWAYOMI_PUBLIC_URL` specifies the base used for browser image URLs. The self-hosted image sets it to `/`, so browsers request images from komiq and the server proxies them from Suwayomi.

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
- `apps/server/src/maintenance.rs`: automatic extension update supervisor and log history.
- `deploy/selfhost/docker-compose.yml`: container definitions, network links, volume mounts, and memory limits.
- `deploy/selfhost/Dockerfile`: internal engine address and browser image URL configuration.
- `deploy/selfhost/.env.example`: self-hosted environment flags for FlareSolverr and network binding.
- `docs/self-host.md`: user instructions for volume backups, updates, and port configurations.
