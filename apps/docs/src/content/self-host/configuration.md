---
title: Settings reference
description: Configure the admin app’s server with Docker Compose settings and server environment variables.
---

Configure your own server’s access, storage, sources and maintenance through the admin app. These tables cover Docker Compose settings in `deploy/selfhost/.env` and environment variables for `komika-server`.

## How settings resolve

Settings in komiq resolve in order of precedence:

1. **Environment variables**: Variables set on the server process take highest priority and cannot be changed at runtime in the console.
2. **Database settings**: Settings saved in the `instance_setting` table, modified through setup or the admin console.
3. **Built-in defaults**: Default values compiled into the application.

On a self-hosted installation where `KOMIKA_WEB_DIR` is set (Docker Compose or the desktop app), user registration and MangaDex access start disabled by default.

## Docker Compose settings

The Compose file in `deploy/selfhost/docker-compose.yml` reads optional variables from `.env`. Copy `deploy/selfhost/.env.example` to `.env` to customize them.

| Setting                | Default   | Description                                                                                                                            |
| ---------------------- | --------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `KOMIQ_PORT`           | `8080`    | Host port on which komiq listens.                                                                                                      |
| `KOMIQ_BIND`           | `0.0.0.0` | Host address to bind (`0.0.0.0` for local network access, `127.0.0.1` for this machine only).                                          |
| `COMPOSE_FILE`         | unset     | Extra Compose files that publish port 443 (`docker-compose.https.yml`) and port 80 (`docker-compose.acme.yml`). The installer sets it. |
| `TZ`                   | `UTC`     | Time zone used for container logs.                                                                                                     |
| `COMPOSE_PROFILES`     | empty     | Set to `flaresolverr` to start the FlareSolverr container.                                                                             |
| `FLARESOLVERR_ENABLED` | `false`   | Set to `true` with `COMPOSE_PROFILES=flaresolverr` to enable the solver.                                                               |

## Server network and web settings

These variables configure network listeners, web applications, and proxy endpoints.

| Variable              | Default       | Description                                                                                                                                                                                                                           |
| --------------------- | ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `BIND_ADDR`           | `0.0.0.0`     | Internal IP address the server listener binds.                                                                                                                                                                                        |
| `PORT`                | `8080`        | Internal HTTP port the server listener binds.                                                                                                                                                                                         |
| `KOMIKA_METRICS_PORT` | `9090`        | Port for Prometheus metrics output on loopback.                                                                                                                                                                                       |
| `KOMIKA_WEB_DIR`      | unset         | Path to web assets. Serves reader at `/` and console at `/admin`.                                                                                                                                                                     |
| `KOMIKA_IMG_PROXY`    | auto          | Controls `/img` proxy. Enabled when `KOMIKA_WEB_DIR` is set.                                                                                                                                                                          |
| `CORS_ORIGINS`        | local origins | Comma-separated allowed Cross-Origin Resource Sharing (CORS) origins. The native reader apps need `tauri://localhost,http://tauri.localhost,https://tauri.localhost`; the self-host Docker image and the admin desktop app set these. |
| `LOG_FORMAT`          | unset         | Set to `json` for structured JSON logs, or unset for text.                                                                                                                                                                            |
| `GRAPHIQL_ENABLED`    | `false`       | Enables interactive GraphiQL query explorer at `/graphql`.                                                                                                                                                                            |

## Database and storage settings

These variables configure database connections and file storage paths.

| Variable              | Default                   | Description                                                       |
| --------------------- | ------------------------- | ----------------------------------------------------------------- |
| `DATABASE_URL`        | `sqlite://komika.sqlite3` | SQLite URL for accounts, library data, and settings.              |
| `COVERS_DATABASE_URL` | sibling `covers.sqlite3`  | SQLite URL for cached cover storage, separate from main database. |
| `EXT_ICONS_DIR`       | `/srv/ext-icons`          | Directory on disk where source extension icons are cached.        |

## Source engine settings

These variables configure connections to the Suwayomi source engine.

| Variable              | Default                 | Description                                              |
| --------------------- | ----------------------- | -------------------------------------------------------- |
| `SUWAYOMI_URL`        | `http://localhost:4567` | URL of the internal Suwayomi API.                        |
| `SUWAYOMI_PUBLIC_URL` | unset                   | Public base URL for Suwayomi for direct client fetching. |
| `SUWAYOMI_SOURCE_ID`  | unset                   | Pins requests to a specific Suwayomi source identifier.  |

## Accounts and authentication

These variables control administrator bootstrapping and authentication limits.

| Variable                           | Default   | Description                                                                                                |
| ---------------------------------- | --------- | ---------------------------------------------------------------------------------------------------------- |
| `KOMIKA_ADMIN_USERS`               | empty     | Comma-separated admin usernames to bootstrap at boot.                                                      |
| `KOMIKA_ADMIN_PASSWORD`            | empty     | Password for bootstrap admin creation or promotion.                                                        |
| `KOMIKA_ADMIN_EMAIL`               | empty     | Email address assigned to bootstrapped administrators.                                                     |
| `KOMIKA_SETUP_CODE`                | unset     | Read by the `komika-server setup` command instead of its `--setup-code` option.                            |
| `SESSION_TTL_SECS`                 | `2592000` | User session lifetime in seconds (30 days). Admin sessions cap at 8 hours.                                 |
| `AUTH_RATE_LIMIT_MAX`              | `10`      | Maximum login attempts per address per window. One account takes 3 times this from all addresses together. |
| `AUTH_RATE_LIMIT_WINDOW_SECS`      | `300`     | Window in seconds for login rate limiting (5 minutes).                                                     |
| `FEDERATED_RATE_LIMIT_MAX`         | `20`      | Maximum requests per window for federated engine operations.                                               |
| `FEDERATED_RATE_LIMIT_WINDOW_SECS` | `60`      | Window in seconds for federated rate limits (1 minute).                                                    |

## Instance configuration

These variables override settings normally managed in the admin console.

| Variable                       | Default             | Description                                                          |
| ------------------------------ | ------------------- | -------------------------------------------------------------------- |
| `KOMIKA_INSTANCE_NAME`         | `komiq`             | Server name shown on page headers and reader screens.                |
| `KOMIKA_REGISTRATION_OPEN`     | `false` (self-host) | Controls user registration. Pins the setting on or off.              |
| `KOMIKA_MANGADEX`              | `false` (self-host) | Controls MangaDex integration. Pins the setting on or off.           |
| `KOMIKA_MANGADEX_RATINGS`      | `true`              | Shows MangaDex ratings to readers. Pins the setting on or off.       |
| `CATALOGUE_SYNC`               | unset               | Set to `on` to force MangaDex usage on and run catalogue crawl.      |
| `KOMIKA_APPS_FETCH_THEMSELVES` | `false`             | Tells reader apps that have their own engine to fetch chapter pages. |

> [!NOTE]
> With `KOMIKA_APPS_FETCH_THEMSELVES=on`, a reader app that has its own source engine (the desktop app, or an Android app built with the engine) greys out "The server" on its "Who gets the chapters?" screen and fetches chapter pages itself. The server still answers page requests. The web reader and phone apps without an engine read through the server, and an app with an engine falls back to the server when its own fetch fails.

## Cloudflare and proxy settings

These variables configure Cloudflare and reverse proxies, services that forward requests to your server. CIDR notation describes the IP address ranges you trust to forward visitor addresses.

| Variable                | Default             | Description                                                                                                                                                                                         |
| ----------------------- | ------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `TRUSTED_PROXY_CIDRS`   | empty               | Comma-separated CIDR blocks of trusted reverse proxies.                                                                                                                                             |
| `CLOUDFLARED_BIN`       | unset               | Path to the `cloudflared` program for tunnels. Unset, the server searches `PATH`, then its tool folder, and tunnel setup downloads the program when it finds none. Set, the server never downloads. |
| `KOMIKA_HTTPS_PORT`     | `443`               | Port used to bind HTTPS: Public IP mode, and "Public without a domain". The address komiq shows names this port, so a change shows after a restart.                                                 |
| `KOMIKA_PUBLISHED_PORT` | value of `PORT`     | The plain HTTP port as other machines reach it, when a container's host publishes the server's port under another number. The self-host compose file sets it from `KOMIQ_PORT`.                     |
| `KOMIKA_ACME_PORT`      | `80`                | Port the server binds for the Let's Encrypt check. Let's Encrypt always connects to port 80 of the public address.                                                                                  |
| `KOMIKA_ACME_DIRECTORY` | Let's Encrypt       | ACME directory address of another certificate authority.                                                                                                                                            |
| `KOMIKA_ACME_CA_FILE`   | unset               | PEM root certificate to trust when talking to that directory.                                                                                                                                       |
| `KOMIKA_CLOUDFLARE_DIR` | `<data>/cloudflare` | Directory where Cloudflare credentials, keys and certificates are stored. Its `bin/` folder is the tool folder for a downloaded `cloudflared`.                                                      |

## Scanner and maintenance tasks

These variables control background scanning, extension updates, and cache maintenance.

| Variable                                 | Default | Description                                                                                                                                                                                  |
| ---------------------------------------- | ------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `SCAN_GLOBAL_CONCURRENCY`                | `12`    | Maximum concurrent chapter scans across all sources. Minimum is 1.                                                                                                                           |
| `SCAN_SOURCE_RATE_LIMITS`                | empty   | Per-source scan limits (`source_id=requests/window_seconds[@cost]`). You can also set limits in the admin app, in Settings. A limit set there replaces the one set here for the same source. |
| `SCAN_TICK_SECONDS`                      | `3600`  | Cadence in seconds between scanner scheduler sweeps (1 hour).                                                                                                                                |
| `SOURCE_SYNC_INTERVAL_SECONDS`           | `86400` | Cadence in seconds between source discovery scans (24 hours).                                                                                                                                |
| `SOURCE_SYNC_MAX_PAGES`                  | `10`    | Maximum pages browsed per source during discovery passes.                                                                                                                                    |
| `EXTENSION_AUTO_UPDATE_INTERVAL_SECONDS` | `21600` | Cadence in seconds between extension update checks (6 hours). 0 disables.                                                                                                                    |
| `COVER_CACHE`                            | `true`  | Enables the background cover thumbnail cache drainer.                                                                                                                                        |
| `COVER_CACHE_INTERVAL_SECS`              | `300`   | Interval in seconds between cover cache passes (5 minutes).                                                                                                                                  |
| `COVER_CACHE_BATCH`                      | `500`   | Maximum cover images downloaded per cache pass.                                                                                                                                              |
| `COVER_RECOMPRESS`                       | `true`  | Enables automatic recompression of stored cover images.                                                                                                                                      |
| `COVER_RECOMPRESS_INTERVAL_SECS`         | `60`    | Interval in seconds between cover recompression passes (1 minute).                                                                                                                           |
| `COVER_RECOMPRESS_BATCH`                 | `200`   | Maximum cover images evaluated per recompression pass.                                                                                                                                       |
| `COVER_PHASH`                            | `false` | Computes perceptual hashes for covers to assist deduplication.                                                                                                                               |

For detailed instructions on Docker installation, read [Install with Docker](/self-host/docker). For source setup, read [Add sources](/self-host/sources). For internet connectivity options, read [Reach it from the internet](/self-host/internet-access).

Next: [Update and back up](/self-host/update-and-backup).
