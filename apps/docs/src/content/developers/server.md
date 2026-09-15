---
title: The API server
description: Understand the Rust server inside the self-hostable admin app, its background jobs and its operational lifecycle.
---

Komiq is self-hostable: the admin app is the server and its management portal in one, and the reader app connects to a server. This page covers the Rust server in `apps/server`, which uses an internal Suwayomi engine for comic content and SQLite for identity, social features, catalogue discovery and background task scheduling.

You can run the admin app on your computer with its desktop app, or with Docker on a home server or a virtual private server (VPS). You can also run a standalone server binary. It serves the GraphQL API, binary assets such as covers and avatars, and configured web user interfaces.

The desktop admin app can limit the server to your computer or enable "Allow other devices" for your network. To reach your server over the internet, you can use a domain with Cloudflare in front, or your server's public address without a domain. See [Internet access](/self-host/internet-access).

## Framework and architecture

The server binary is built in Rust to keep memory usage low and process startup fast. It uses these crates:

- `axum 0.8`: provides the HTTP routing layer and request handlers.
- `tokio`: powers the asynchronous multi-threaded runtime.
- `async-graphql 7`: builds the code-first GraphQL schema matching the project schema contract.
- `sqlx 0.8`: manages asynchronous SQLite connection pools, transactions and migrations.
- `reqwest 0.12`: sends outbound HTTP requests to the Suwayomi engine and upstream sources.
- `tower` and `tower-http`: provide HTTP middleware for request tracing, panic recovery, CORS, security headers and response compression.

### Middleware stack

Incoming HTTP requests pass through this middleware:

```flow
HTTP request -> Catch panic: turns panics into JSON 500 responses
Catch panic -> Security headers: sets nosniff, DENY and referrer policy
Security headers -> CORS: validates allowed origins, headers and methods
CORS -> Request ID and tracing: assigns x-request-id and logs access spans
Request ID and tracing -> Compression: applies gzip compression to responses
Compression -> Route handler: routes to GraphQL, static files or binary routes
```

Compression targets text and JSON, including GraphQL payloads. WebP covers and avatars skip compression to save CPU time.

## Core modules

The modules in `apps/server/src/` include:

| Module         | Responsibility                                                    |
| -------------- | ----------------------------------------------------------------- |
| `main.rs`      | Application entry point, router assembly and shutdown handling    |
| `config.rs`    | Environment variable parsing, validation and defaults             |
| `db.rs`        | SQLite pool initialization, PRAGMA configuration and migrations   |
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

`komika-server setup` is a GraphQL client of the running server at `http://127.0.0.1:<PORT>`, or at `--server <address>`. It opens no database and builds no server state. It asks the same questions as the `/admin/setup` screens: owner account, access, accounts and sources. `--setup-code` (or `KOMIKA_SETUP_CODE`), `--access` and `--encrypt` answer three of them. It refuses a plain HTTP address on the internet.

The access mode is `local`, `internet` or `direct`. `direct` means public at the server's own address, with no domain and no Cloudflare. `setAccessMode` stores it, and `accessStatus.address` returns what readers type. The mode decides what the console shows and what the server runs. `direct` with encryption adds an HTTPS listener. Leaving `internet` stops the tunnel connector, or the HTTPS listener that serves the Cloudflare origin certificate. What setup made at Cloudflare stays, and a return to `internet` starts it again with no token.

`accessStatus.lanAddresses` lists `http://<address>:<port>` for each way a device on the home network reaches the server. It is empty when the server listens on loopback only, or runs in a container.

## Startup sequence

`komika-server` initializes in this order:

1. **Environment and logging**: loads variables using `dotenvy`. It initializes `tracing-subscriber` with standard text formatting or JSON output when `LOG_FORMAT=json`.
2. **Configuration**: parses runtime options into a `Config` struct. It inspects command-line arguments to handle utility commands before starting network listeners.
3. **Database initialization**: opens the main SQLite database and the separate cover cache database. It applies pending migrations on the main database.
4. **Disaster recovery check**: if the cover database is empty after restoring the main database from backup, the server clears cached cover version pointers so images re-cache cleanly.
5. **Admin provisioning**: provisions or promotes reserved admin accounts configured in `KOMIKA_ADMIN_USERS`.
6. **Client setup**: instantiates Suwayomi and MangaDex clients with configured rate limits.
7. **First-run setup check**: if no administrator exists in the database or configuration, the server prints a one-time setup code to standard output and activates setup mode.
8. **Startup sweep**: marks interrupted source ingestion jobs from previous server runs as failed.
9. **Background task launch**: spawns recurring background loops using supervised Tokio tasks.
10. **Router assembly**: builds the GraphQL schema, registers REST endpoints, attaches middleware and mounts static web directories if configured.
11. **Listener binding**: binds a loopback-only Prometheus metrics listener on port `9090` by default. It binds the public TCP listener on `BIND_ADDR` and `PORT` (default `0.0.0.0:8080`).

### Graceful shutdown

The server listens for operating system signals (`SIGTERM` or `Ctrl+C`). When a shutdown signal arrives, the server notifies background tasks through a `tokio::sync::watch` channel. The axum listener stops accepting new connections and drains in-flight requests. In-memory flushers write remaining series views and health metrics to SQLite before the process exits.

## Background jobs

Background jobs refresh the catalogue, process images and update extensions:

### Adaptive scanner

Defined in `apps/server/src/scanner.rs`, the scanner tracks enrolled series in the catalog library. It calculates a rolling average interval between chapter releases for each series. Completed or hiatus series pause automatically. Active series receive an adaptive `next_scan_at` timestamp. The scanner wakes every hour by default (`scan_tick_seconds = 3600`) to evaluate due series.

### Discovery polling

Defined in `apps/server/src/discovery.rs`, this task polls the first page of latest releases from subscribed sources every 15 minutes (`discovery_interval_secs = 900`). When a series advances or appears, the job queues an immediate targeted scan for that series, avoiding brute-force sweeps across the entire catalog.

### Extension sync and auto-update

Two maintenance loops manage extensions:

- `sync::spawn`: wakes daily (`source_sync_interval_seconds = 86400`) to walk subscribed extensions, reconcile library membership and discover newly added titles.
- `maintenance::spawn`: runs two maintenance tasks. It wakes hourly (starting 30 seconds after startup) to recheck pending deduplication reviews against current catalog data. It also wakes every 6 hours by default (`extension_auto_update_interval_seconds = 21600`, starting 45 seconds after startup) to refresh configured extension repositories and update installed extensions sequentially.

### Materialized feeds and search indexes

For fast browse responses, the server stores catalogue query results in dedicated SQLite tables:

- `feed_series_updates`: stores recent chapter releases with upstream publication dates.
- `browse_catalogue`: maintains a flat, indexed projection of browsable titles for filtering by genre, demographic, status, year, rating and follow counts.
- `work_fts`: provides an SQLite FTS5 full-text search index across canonical titles and aliases.

A background task spawned 20 seconds after boot (`catalog::refresh_feed_updates`) rebuilds these tables. It refreshes them periodically after catalog synchronization cycles.

### Cover cache and garbage collection

The cover drainer in `apps/server/src/cover.rs` runs every 5 minutes (`cover_cache_interval_secs = 300`) to download uncached covers, downscale them and encode them as lossy WebP files in batches of 500.

The garbage collector in `apps/server/src/gc.rs` runs hourly. It deletes orphaned cover blobs from the cover database when their owning work record is removed. It also cleans up staged comment media uploads that were never submitted.

### Database statistics refresh

The database loop in `apps/server/src/db.rs` runs an exact `ANALYZE` query every 6 hours on high-traffic tables. This updates SQLite internal query planner statistics (`sqlite_stat1`), ensuring index selections remain optimal as table row counts grow.

## Serving web applications

The server inside the admin app can serve both web interfaces directly: the reader app at `/` and the management console at `/admin`. The embedded builds use the same origin as the server, so you do not need an external web server.

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

Both applications use SPA fallback routing. When a requested path has no file on disk, the server returns the corresponding `index.html` with HTTP 200. Client-side routes therefore work when you reload a page. API routes take precedence over static files.

Every HTML answer carries `Cache-Control: no-cache`. A page names its scripts by content hash, so a browser that reused an old page after an update would ask for scripts that no longer exist and show a blank page.

> [!NOTE]
> When `KOMIKA_WEB_DIR` is unset, the server acts as an API-only service and serves no static web files.

## Health checks and monitoring

Use these endpoints for health checks, container orchestration and metrics scraping.

### Liveness check

```sh
curl http://localhost:8080/health
```

The `/health` endpoint returns a plain text `"ok"` response with HTTP status 200. It performs no database queries and confirms that the network listener is responsive.

### Readiness check

```sh
curl http://localhost:8080/health/ready
```

The `/health/ready` endpoint verifies database connectivity by executing `SELECT 1` against the main SQLite connection pool. If the query succeeds, it returns HTTP 200 with text `"ready"`. If the database is locked or unreachable, it logs a warning and returns HTTP 503 with text `"not ready"`.

### Operational metrics

The server serves operational metrics in Prometheus text format on a separate listener bound strictly to loopback:

```sh
curl http://127.0.0.1:9090/metrics
```

The metrics listener binds to `127.0.0.1` on port `KOMIKA_METRICS_PORT` (default 9090). Because it binds to the loopback interface, external clients cannot reach it. The endpoint exports gauge values derived directly from database state at scrape time, including tracked series counts, overdue scan counts, circuit breaker states and feed freshness.

## Where to look

- `apps/server/src/main.rs`: axum router configuration, middleware ordering and server lifecycle management.
- `apps/server/src/config.rs`: runtime settings, defaults and environment variable parsing.
- `apps/server/src/db.rs`: SQLite connection options, PRAGMA configuration and recurring ANALYZE jobs.
- `apps/server/src/web_dir.rs`: static file serving and SPA fallback mechanics.
- `apps/server/src/scanner.rs`: adaptive scan scheduler and cadence calculations.
- `apps/server/src/maintenance.rs`: extension auto-update tasks and duplicate queue rechecks.
- `apps/server/src/gc.rs`: hourly garbage collection for orphaned cover blobs and staged media.
- `apps/server/src/browse.rs`: catalog browse queries, genre facets and tag search.
