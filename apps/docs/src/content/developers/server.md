---
title: The API server
description: Understand the Rust server inside the self-hostable admin app, its background jobs and its operational lifecycle.
---

The server is part of the self-hostable admin app. It serves GraphQL, covers, avatars and configured web apps. SQLite stores accounts, libraries, social data and catalogue state. Suwayomi supplies source content.

Run it through the desktop admin app, with Docker, or as a standalone binary. The reader app connects to your server. See [Internet access](/self-host/internet-access) to make your server reachable outside your network.

## Framework and architecture

The server uses axum for HTTP routing, Tokio for asynchronous tasks, async-graphql for the schema and sqlx for SQLite. Reqwest handles upstream HTTP; Tower middleware handles tracing, CORS and compression.

### Middleware stack

Incoming HTTP requests pass through this middleware:

```flow
HTTP request -> Catch panic: turns panics into JSON 500 responses
Catch panic -> Security headers: sets nosniff, DENY and referrer policy
Security headers -> CORS: validates allowed origins, headers and methods
CORS -> Request ID and tracing: assigns x-request-id and logs access spans
Request ID and tracing -> Admission: limits active handlers and request time
Admission -> Compression: applies gzip compression
Compression -> Route handler: routes the request
```

Compression targets text and JSON. WebP covers and avatars skip compression.

The public router admits up to 64 active handlers. Excess requests receive HTTP 503 with `Retry-After: 1` and `Cache-Control: no-store`. Handler execution has a 20-second deadline and returns the same response when it expires.

Search accepts at most 256 characters and 12 distinct terms, with a two-second SQLite deadline. Heavy GraphQL root fields are limited to four per operation. Upstream misses share a 60-per-minute client budget. Library and progress queries return at most 1,000 entries.

Pool saturation also returns a retryable 503. A failed session lookup returns an error rather than treating a signed-in reader as anonymous. GraphQL GET responses containing errors are never publicly cached.

## Core modules

The modules in `apps/server/src/` include:

| Module         | Responsibility                                                    |
| -------------- | ----------------------------------------------------------------- |
| `main.rs`      | Application entry point, router assembly and shutdown handling    |
| `config.rs`    | Environment variable parsing, validation and defaults             |
| `db.rs`        | Boot safety, migrations, retries and SQLite configuration         |
| `database.rs`  | Separate read and write pools and bounded cache-write queue      |
| `auth.rs`      | Password hashing with Argon2id, token creation and session lookup |
| `suwayomi.rs`  | HTTP client communicating with the internal Suwayomi instance     |
| `mangadex.rs`  | MangaDex API client, rate limiting and catalog synchronization    |
| `scanner.rs`   | Adaptive chapter scan scheduler for library series                |
| `browse.rs`    | Multi-criteria catalog filtering, facets and sorting logic        |
| `web_dir.rs`   | Static file delivery and SPA fallback routing for web apps        |
| `setup_cli.rs` | `komika-server setup`: first-run setup in a terminal              |
| `lan.rs`       | Home network addresses of the server, for the console to show     |
| `cover.rs`     | Cover fetching, WebP encoding, variant resizing and caching       |
| `gc.rs`        | Garbage collection for unreferenced covers and staged media       |
| `instance.rs`  | Instance settings, setup code generation and access modes         |

`komika-server setup` is a client of the running server. It opens no database. It asks the same owner, access, account and source questions as `/admin/setup`, and refuses plain HTTP over the internet.

Access modes are `local`, `internet` and `direct`. The last serves the server's own public address without a domain or Cloudflare. Encryption adds an HTTPS listener. Leaving `internet` stops its connector or origin-certificate listener. Returning to it reuses the stored setup.

`accessStatus.lanAddresses` lists local network addresses. It is empty for a loopback-only listener or a container.

## Startup sequence

`komika-server` initializes in this order:

1. **Environment and logging**: loads variables and initializes text or JSON logs.
2. **Configuration**: parses runtime settings and handles utility commands before creating server state.
3. **Database initialization**: takes a lifetime lock on `.komiq-server.lock`, opens the main database and applies migrations. A second server using the directory cannot boot.
4. **Cover recovery**: preserves an unavailable or corrupt cache and recreates it. Checks saved cover pointers in batches, including when only part of the cache is missing.
5. **Accounts and clients**: provisions configured administrators, creates source clients and issues a setup code if no administrator exists.
6. **Startup sweep**: marks interrupted source ingestion jobs as failed.
7. **Background tasks**: starts supervised recurring jobs.
8. **Routes and listeners**: builds the schema and router, mounts configured web apps, binds metrics on loopback, then binds the public listener.

The `phase-f` maintenance command opens existing database files without migrations or cache recovery. It refuses a missing, behind or incompatible main schema.

### Graceful shutdown

On `SIGTERM` or `Ctrl+C`, the server notifies background tasks and stops accepting connections. It gives active handlers and buffered view and health writes up to 20 seconds to finish. Docker allows 30 seconds before forcing a stop, leaving time for Litestream to sync the backup.

## Background jobs

The main database has an eight-connection read pool and a single-connection write pool. Reads use SQLite's `query_only` mode so misplaced writes fail. Read acquisition waits at most 3 seconds; writer acquisition waits at most 10 seconds. Write transactions start with `BEGIN IMMEDIATE`. A shared helper retries whole transactions with a bounded delay and a fresh transaction each time.

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

For fast browse responses, the server stores catalogue query results in dedicated SQLite tables:

- `feed_series_updates`: stores recent chapter releases with upstream publication dates.
- `browse_catalogue`: maintains a flat, indexed projection of browsable titles for filtering by genre, demographic, status, year, rating and follow counts.
- `work_fts`: provides an SQLite FTS5 full-text search index across canonical titles and aliases.

Catalogue writes update browse and search together. A local reconciliation loop rebuilds these tables daily, even with MangaDex disabled. Rebuilds stage expensive reads separately and publish bounded batches.

### Cover cache and garbage collection

The cover drainer in `apps/server/src/cover.rs` downloads and resizes uncached covers every 5 minutes (`cover_cache_interval_secs = 300`), in batches of 500.

The garbage collector in `apps/server/src/gc.rs` runs hourly. It deletes orphaned cover blobs from the cover database when their owning work record is removed. It also cleans up staged comment media uploads that were never submitted.

### Database statistics refresh

The database loop in `apps/server/src/db.rs` refreshes query planner statistics every 6 hours. It samples one index per writer turn and yields between indexes.

## Serving web applications

The server can serve the reader at `/` and the admin console at `/admin`, on the same origin.

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

Missing app paths return that app’s `index.html`, so client-side routes work on reload. API routes take precedence.

HTML carries `Cache-Control: no-cache` to prevent an old page requesting scripts removed by an update.

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

`/health/ready` runs `SELECT 1` on the read pool. It returns HTTP 200 and `ready` on success, or 503 and `not ready` on failure.

### Operational metrics

The server serves operational metrics in Prometheus text format on a separate listener bound strictly to loopback:

```sh
curl http://127.0.0.1:9090/metrics
```

The listener binds to `127.0.0.1` on `KOMIKA_METRICS_PORT` (default 9090). It exports scan counts, circuit breaker state and feed freshness, plus WAL file size, pool size and idle connections, writer queue wait totals and observed SQLite lock errors. These aggregate metrics contain no user data.

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
