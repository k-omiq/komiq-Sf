---
title: The API server
description: Understand the Rust server inside the self-hostable admin app, its background jobs and its operational lifecycle.
---

The server is part of the self-hostable admin app. It serves GraphQL, covers, avatars and configured web apps. PostgreSQL 18 stores accounts, libraries, social data and catalogue state. Suwayomi supplies source content.

Run it through the desktop admin app, Docker or a standalone binary. See [Reach it from the internet](/self-host/internet-access) for remote connections.

## Framework and architecture

The server uses axum for HTTP routing, Tokio for asynchronous tasks, async-graphql for the schema and sqlx for PostgreSQL and the SQLite cover cache. Reqwest handles upstream HTTP; Tower middleware handles tracing, CORS and compression.

### Middleware stack

```flow
HTTP request -> Catch panic: turns panics into JSON 500 responses
Catch panic -> Security headers: sets nosniff, DENY and referrer policy
Security headers -> CORS: validates allowed origins, headers and methods
CORS -> Request ID and tracing: assigns x-request-id and logs access spans
Request ID and tracing -> Compression: applies gzip compression
Compression -> Route budget: admits GraphQL and uploads; images have separate deadlines
Route budget -> Route handler: routes the request
```

Compression targets text and JSON. WebP covers and avatars skip compression.

GraphQL admits 64 local/reader requests, two federated searches and two maintenance operations. Deadlines are 20s locally, 300s for reader fields, 1,800s for search and 3,600s for maintenance. Engine/API calls take 16 slots, at most three per client; hits take none. Uploads have four slots/120s. Images keep limits; health bypasses admission. Saturation/deadlines return 503 with `Retry-After: 1` and `Cache-Control: no-store`.

Search accepts at most 256 characters and 12 distinct terms, with a two-second database deadline. Heavy GraphQL root fields are limited to four per operation. Upstream misses share a 60-per-minute client budget. Library and progress queries return the complete library, hydrating metadata in batches of 256 identities from local data without upstream requests.

A failed session lookup remains an error. GraphQL GET errors are never publicly cached.

## Core modules

Modules in `apps/server/src/`:

| Module         | Responsibility                                                      |
| -------------- | ------------------------------------------------------------------- |
| `main.rs`      | Application entry point, router assembly and shutdown handling      |
| `config.rs`    | Environment variable parsing, validation and defaults               |
| `db.rs`        | Boot safety, migrations, retries and database configuration         |
| `database.rs`  | Separate read and write pools and bounded cache-write queue         |
| `auth.rs`      | Password hashing with Argon2id, token creation and session lookup   |
| `suwayomi.rs`  | HTTP client communicating with the internal Suwayomi instance       |
| `mangadex.rs`  | MangaDex API client, rate limiting and catalog synchronization      |
| `scanner.rs`   | Adaptive chapter scan scheduler for library series                  |
| `browse.rs`    | Multi-criteria catalog filtering, facets and sorting logic          |
| `web_dir.rs`   | Static file delivery and SPA fallback routing for web apps          |
| `setup_cli.rs` | `komika-server setup`: first-run setup in a terminal                |
| `lan.rs`       | Home network addresses of the server, for the console to show       |
| `cover.rs`     | Cover fetching, WebP encoding, variant resizing and caching         |
| `gc.rs`        | Cleanup of orphaned covers, staged media and old read notifications |
| `instance.rs`  | Instance settings, setup code generation and access modes           |

`komika-server setup` calls the running server without opening its database. It matches `/admin/setup` and refuses plain HTTP over the internet.

Access modes are `local`, `internet` and `direct`. The last serves the server's own public address without a domain or Cloudflare. Encryption adds an HTTPS listener. Leaving `internet` stops its connector or origin-certificate listener. Returning to it reuses the stored setup.

`accessStatus.lanAddresses` lists local network addresses. It is empty for a loopback-only listener or a container.

## Startup sequence

`komika-server` initializes in this order:

1. **Environment and logging**: loads variables and initializes text or JSON logs.
2. **Configuration**: parses runtime settings and handles utility commands before creating server state.
3. **Database initialization**: takes a lifetime lock on `.komiq-server.lock`, then runs the SQLite import check, `pg_import::startup::run`, before anything listens. It converts an old source when eligible, verifies it and keeps the original file; [Data model](/developers/data-model) lists the cases. Then it connects to PostgreSQL and applies migrations. A second server using the directory cannot boot.
4. **Cover recovery**: preserves an unavailable or corrupt cache and recreates it. Checks saved cover pointers in batches, including when only part of the cache is missing.
5. **Accounts and clients**: provisions configured administrators, creates source clients and issues a setup code if no administrator exists.
6. **Startup sweep**: marks interrupted source ingestion jobs as failed.
7. **Background tasks**: starts supervised recurring jobs.
8. **Routes and listeners**: builds the schema and router, mounts configured web apps, binds metrics on loopback, then binds the public listener.

The `phase-f` maintenance command connects to existing databases without migrations or cache recovery. It refuses a missing, behind or incompatible main schema.

### Graceful shutdown

On `SIGTERM` or `Ctrl+C`, the server stops accepting connections and notifies background tasks. Active handlers and buffered writes get up to 20 seconds to finish; Docker allows 30 seconds before termination.

## Background jobs

The main database uses PostgreSQL pools. Reader sessions are query-only; background cache writes have a separate bounded lane. Transactions use PostgreSQL locking and constraints. The shared transaction helper retries serialization failures and deadlocks with a fresh transaction and a bounded delay. SQLite locks belong only to the cover cache and retained source tooling.

Public cover and source-cache misses enqueue persistence in a bounded background queue. The request serves fetched data without waiting for that cache write. A full queue leaves the result uncached, so a later request can retry.

Background jobs refresh the catalogue, process images and update extensions:

### Adaptive scanner

The scanner in `apps/server/src/scanner.rs` runs per-source loops over enrolled series. It schedules `next_scan_at` using observed chapter intervals and pauses completed or hiatus series. The legacy `SCAN_TICK_SECONDS` setting no longer controls these loops.

### Discovery polling

`apps/server/src/discovery.rs` polls subscribed sources' latest releases every 15 minutes (`discovery_interval_secs = 900`). A new or advanced series gets a targeted scan, avoiding a full catalogue sweep.

### Extension sync and auto-update

Two maintenance loops manage extensions:

- `sync::spawn`: wakes daily (`source_sync_interval_seconds = 86400`) to walk subscribed extensions, reconcile library membership and discover newly added titles.
- `maintenance::spawn`: runs two maintenance tasks. It wakes hourly (starting 30 seconds after startup) to recheck pending deduplication reviews against current catalog data. It also wakes every 6 hours by default (`extension_auto_update_interval_seconds = 21600`, starting 45 seconds after startup) to refresh configured extension repositories and update installed extensions sequentially.

### Materialized feeds and search indexes

For fast browse responses, the server stores catalogue query results in dedicated PostgreSQL tables:

- `feed_series_updates`: stores recent chapter releases with upstream publication dates.
- `browse_catalogue`: maintains a flat, indexed projection of browsable titles for filtering by genre, demographic, status, year, rating and follow counts.
- `work_search_alias`: stores normalized titles and aliases with full-text vectors and trigram indexes for typo candidates.

Catalogue writes update browse and search together. A local reconciliation loop rebuilds these tables daily, even with MangaDex disabled. Rebuilds stage expensive reads separately and publish bounded batches.

### Cover cache and garbage collection

The cover drainer in `apps/server/src/cover.rs` downloads and resizes uncached covers every 5 minutes (`cover_cache_interval_secs = 300`), in batches of 500.

The garbage collector in `apps/server/src/gc.rs` runs hourly. It deletes orphaned cover blobs, abandoned staged comment media, and read notifications created more than 90 days ago. Notifications use batches of 128, up to 1,024 per tick. Unread notifications and release events are kept.

### Database statistics refresh

The database loop in `apps/server/src/db.rs` refreshes query planner statistics every 6 hours. It uses PostgreSQL `ANALYZE` for main tables; cover maintenance remains separate.

## Serving web applications

The server serves the reader at `/` and console at `/admin` on one origin.

Set `KOMIKA_WEB_DIR` to a directory containing built Single Page Applications (SPAs):

```text
/path/to/web/
├── reader/
│   ├── index.html
│   └── ...
└── admin/
    ├── index.html
    └── ...
```

The router in `apps/server/src/web_dir.rs` mounts these applications below API routes:

- `/admin`: serves the admin console from `admin/`.
- `/`: serves the reader application from `reader/`.

Missing app paths return that app’s `index.html`, so client-side routes work on reload. Missing files under `/_app/` or `/admin/_app/` return `404` with `Cache-Control: no-store` instead. API routes take precedence.

HTML carries `Cache-Control: no-cache` so the browser checks the page with the server before reusing it. At mount time, the server reads each app's `index.html` and computes a Content Security Policy (CSP) with SHA-256 hashes of its inline scripts. The policy permits same-origin scripts, inline styles, HTTP(S) connections to servers, and HTTP(S), data and blob images. It blocks objects and framing. The header applies to HTML only. If an index cannot be read at mount time, the server logs a warning and omits that app's CSP header.

> [!NOTE]
> When `KOMIKA_WEB_DIR` is unset, the server acts as an API-only service and serves no static web files.

## Health checks and monitoring

Health checks use the public listener. Metrics use a separate loopback listener.

### Liveness check

```sh
curl http://localhost:8080/health
```

`/health` returns HTTP 200 and `ok`, without querying the database.

### Readiness check

```sh
curl http://localhost:8080/health/ready
```

`/health/ready` runs `SELECT 1` on a reserved query-only connection. Regular read-pool exhaustion does not affect it. Database failures return 503; success returns 200 and `ready`.

### Operational metrics

The server exposes Prometheus metrics on loopback:

```sh
curl http://127.0.0.1:9090/metrics
```

The listener binds to `KOMIKA_METRICS_BIND_ADDR` (default `127.0.0.1`) on `KOMIKA_METRICS_PORT` (default 9090). The Compose deployment binds `0.0.0.0` inside the container, because Docker forwards a published port to the container network address and not to its loopback. It publishes the port on host `127.0.0.1` only. It exports scan counts, circuit breaker state and feed freshness, plus database storage, pool size and idle connections, writer queue waits and database lock errors. Historical metric names may still contain `sqlite` for compatibility. These aggregate metrics contain no user data.

PostgreSQL recovery health is in the same output: archived and failed WAL counts, seconds since the last success and the last failure, `komika_postgres_archive_failing` (1 when the last failure is newer than the last success), raw WAL generated, the WAL directory size and the archive backlog (`.ready` files). The last two need EXECUTE on `pg_ls_waldir()` and `pg_ls_archive_statusdir()` (or `pg_monitor`). Without that grant the value is left out, never shown as -1, and `komika_postgres_metric_available{metric="..."}` reads 0.

## Where to look

- `apps/server/src/main.rs`: axum router configuration, middleware ordering and server lifecycle management.
- `apps/server/src/config.rs`: runtime settings, defaults and environment variable parsing.
- `apps/server/src/db.rs`: boot locks, recovery, retries, migrations and statistics refresh.
- `apps/server/src/database.rs`: read and write pools and deferred cache persistence.
- `apps/server/src/web_dir.rs`: static file serving and SPA fallback mechanics.
- `apps/server/src/scanner.rs`: adaptive scan scheduler and cadence calculations.
- `apps/server/src/maintenance.rs`: extension auto-update tasks and duplicate queue rechecks.
- `apps/server/src/gc.rs`: hourly garbage collection for orphaned cover blobs and staged media.
- `apps/server/src/browse.rs`: catalog browse queries, genre facets and tag search.
