---
title: Deployment
description: Understand how the project deploys the global server with Cloudflare, Docker, and backups.
---

This page describes how the project deploys the global server at `komiq.cc`, its free work-in-progress demo. It is one example of the self-hostable komiq model: the admin app provides the server, management portal and Suwayomi engine, and the reader app connects to that server. For your own installation, start with [Install with Docker](/self-host/docker) and [Internet access](/self-host/internet-access).

The global server combines Cloudflare Workers with backend services running in Docker containers, isolated service environments. This deployment isolates untrusted extensions, uses the optional image proxy, and continuously replicates the database to object storage.

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
- Executes server-side rendering for public catalog routes, including home feeds, browse filters, series overviews, and updates.
- Serves static assets, including styles, bundled scripts, icons, and fonts, through Cloudflare Workers Static Assets.
- Inlines public configuration like the API endpoint and image proxy URL during build time.
- Requires no edge secrets because public page rendering only accesses unauthenticated catalog data.

### The admin app's console Worker

This deployment uses the secondary `hosted` build of the [admin console](/developers/admin-app) on `admin.komiq.cc`. It is deployed from `apps/admin` using the Worker name `komika-admin`:

- Built with `@sveltejs/adapter-static` with fallback `index.html`.
- Serves prebuilt client-side assets through Cloudflare Workers Static Assets.
- Sets `not_found_handling = "single-page-application"` so browser reloads and deep links resolve to `index.html` with an HTTP 200 response.
- Talks directly to the GraphQL API at `https://api.komiq.cc/graphql`.

### The image proxy Worker

The [image proxy](/developers/image-proxy) runs on `img.komiq.cc`. It is deployed from `apps/worker` using the Worker name `komika-img`:

- Provides an edge proxy and caching layer for chapter pages and cover images.
- Enforces an upstream host allowlist through `ALLOWED_SOURCE_HOSTS`. The proxy allows `uploads.mangadex.org`, `mangadex.network`, and `api.komiq.cc`. Any request targeting an unapproved host is rejected.
- Restricts hotlinking through `ALLOWED_ORIGINS`. Browser requests carrying an Origin or Referer header must match `https://komiq.cc` or `https://www.komiq.cc`. Direct requests without browser origin headers remain permitted.
- Enforces per-IP rate limiting through a Cloudflare Rate Limiting binding (`IMG_RATE_LIMITER`). The limiter allows 600 requests per 60 seconds per client IP address. Rate limits are evaluated only on cache misses, ensuring that cached image traffic is never throttled.

### The documentation Worker

The public documentation runs on `docs.komiq.cc`. It is deployed from `apps/docs` using the Worker name `komika-docs`:

- Built with `@sveltejs/adapter-static` as a prerendered static site.
- Serves static HTML and client assets directly from Cloudflare Workers Static Assets.
- Sets `not_found_handling = "404-page"` to return a pre-rendered 404 page for missing URLs.

### The API origin

The backend API is reachable at `api.komiq.cc`. This hostname routes public API and proxy traffic to the containerized server.

## Admin app server and engine

The admin app's server and Suwayomi engine run alongside a FlareSolverr sidecar inside Docker on a private bridge network (`komika`), configured in `deploy/docker-compose.yml`:

| Service        | Image                                 | Ports                              | Memory limit |
| -------------- | ------------------------------------- | ---------------------------------- | ------------ |
| `server`       | Built from `deploy/server.Dockerfile` | `127.0.0.1:8080`, `127.0.0.1:9090` | 6 GB         |
| `suwayomi`     | Digest-pinned `tachidesk`             | `127.0.0.1:4567`                   | 8 GB         |
| `flaresolverr` | Digest-pinned `flaresolverr`          | None (internal network only)       | 2 GB         |

### The server container

The `server` container runs `komika-server`, a Rust application based on Axum, async-graphql, and SQLite:

- Binds internal port 8080 to loopback `127.0.0.1:8080`. Public ingress reaches it through a reverse proxy or tunnel.
- Serves GraphQL queries and mutations at `/graphql`.
- Exposes readiness health checks at `/health/ready`, which performs an active database query (`SELECT 1`) to ensure connection pool availability.
- Proxies cover and page images from the internal Suwayomi engine at `/api/v1/manga/...` via `SUWAYOMI_PUBLIC_URL`.
- Runs background tasks including library scan schedules, chapter update discovery, and canonical catalog synchronization.
- Serves Prometheus metrics on an isolated internal listener bound to loopback `127.0.0.1:9090` at `/metrics`.
- Memory is capped at 6 GB to prevent runaway allocations from affecting host stability.

### The Suwayomi engine

The `suwayomi` container runs Tachidesk, a Java service executing source extensions:

- Binds port 4567 to loopback `127.0.0.1:4567`. Loopback binding is critical because Suwayomi's administrative API lacks authentication. External clients must never reach this port directly.
- The `server` container communicates with Suwayomi over the internal Docker network at `http://suwayomi:4567`.
- Manages source extensions, chapter page resolution, and non-canonical cover retrieval.
- Image tags are pinned by cryptographic sha256 digests to ensure reproducible deployments.
- Memory is capped at 8 GB. JVM ergonomics are constrained using `JAVA_TOOL_OPTIONS: -XX:MaxRAMPercentage=70`, ensuring heap allocation stays within container limits.

### The FlareSolverr sidecar

The `flaresolverr` container runs a headless Chromium browser designed to solve challenge pages on protected sources:

- Operates purely on the internal Docker network at `http://flaresolverr:8191`.
- Exposes no host ports.
- Memory is capped at 2 GB.

## Database replication and backups

The core application database lives in SQLite at `/data/komika.sqlite3` on a persistent Docker volume (`server-data`). It stores user accounts, hashed credentials, sessions, reading history, library entries, and review discussions.

Continuous backups are managed by Litestream, configured in `deploy/litestream.yml`:

```yaml
dbs:
  - path: /data/komika.sqlite3
    replicas:
      - type: s3
        bucket: ${LITESTREAM_BUCKET}
        path: komika
        endpoint: ${LITESTREAM_ENDPOINT}
        region: ${LITESTREAM_REGION}
        force-path-style: true
        sync-interval: 10s
        retention: 24h
        snapshot-interval: 24h
```

### Replication mechanics

Litestream runs alongside the server process inside the `server` container. It is invoked via `litestream replicate -config /etc/litestream.yml -exec "komika-server"` in `deploy/server-entrypoint.sh`.

- SQLite operates in Write-Ahead Logging (WAL) mode.
- Litestream monitors the WAL and streams changes to an S3-compatible bucket, such as Cloudflare R2.
- The synchronization interval is set to 10 seconds (`sync-interval: 10s`), giving a recovery point objective (RPO) of approximately 10 seconds.
- A full database snapshot is generated every 24 hours (`snapshot-interval: 24h`).
- WAL segments older than 24 hours are pruned automatically (`retention: 24h`) to bound storage costs.

### Recovery and restoration

Restoration is automatic on initial deployment. When the `server` container starts with Litestream variables configured and discovers an empty `/data` volume, `deploy/server-entrypoint.sh` executes a restore operation before launching the server:

```sh
litestream restore -if-db-not-exists -if-replica-exists -config /etc/litestream.yml /data/komika.sqlite3
```

This downloads the latest snapshot and replays recent WAL files, recovering database state up to the last replicated transaction.

Administrators can also perform manual point-in-time restores:

```sh
docker compose exec server litestream restore -o /tmp/komika.sqlite3 /data/komika.sqlite3
```

## Where to look

- `apps/reader/wrangler.toml`: Worker routing, build output paths, and domain bindings for the reader app.
- `apps/admin/wrangler.toml`: Static asset configuration and custom domain setup for the admin console.
- `apps/worker/wrangler.toml`: Host allowlists, hotlink restrictions, and rate limiter bindings for the image proxy.
- `apps/docs/wrangler.toml`: Static documentation site routing on Cloudflare Workers.
- `deploy/docker-compose.yml`: Service definitions, memory caps, network settings, and volume mappings.
- `deploy/server.Dockerfile`: Build stages for the Rust server container.
- `deploy/server-entrypoint.sh`: Container startup script handling automatic Litestream restoration and process execution.
- `deploy/litestream.yml`: Database replication schedules, snapshot intervals, and S3 replica settings.
