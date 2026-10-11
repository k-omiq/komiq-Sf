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

The Compose file in `deploy/selfhost/docker-compose.yml` requires `KOMIQ_DB_PASSWORD` in `.env`. Run `deploy/selfhost/install.sh` to generate it and preserve existing settings. Other variables are optional. Keep the file private with `chmod 600 deploy/selfhost/.env`.

| Setting                | Default   | Description                                                                                                                            |
| ---------------------- | --------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `KOMIQ_DB_PASSWORD`    | required  | Random URL-safe database password. Generated once by the installer. Keep it unchanged on an initialized cluster.                       |
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

| Variable                    | Default                       | Description                                                                                                                |
| --------------------------- | ----------------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| `DATABASE_URL`              | `postgres://localhost/komika` | PostgreSQL URL for accounts, libraries and settings. Compose supplies the private `postgres` service and application role. |
| `COVERS_DATABASE_URL`       | `sqlite://covers.sqlite3`     | SQLite URL for cached cover storage, separate from main database.                                                          |
| `KOMIKA_SQLITE_IMPORT_PATH` | `komika.sqlite3` beside the covers file | Absolute path of the old main SQLite file to convert; the server refuses a relative path. Compose sets `/data/komika.sqlite3`. |
| `EXT_ICONS_DIR`             | `/srv/ext-icons`              | Directory on disk where source extension icons are cached.                                                                 |

The main database lives in `postgres-data`; covers and other server files live in `server-data`. Use a PostgreSQL dump for account and library backups. Copying the cover file does not back up those records. See [Update and back up](/self-host/update-and-backup).

## Source engine settings

These variables configure connections to the Suwayomi source engine.

| Variable              | Default                 | Description                                              |
| --------------------- | ----------------------- | -------------------------------------------------------- |
| `SUWAYOMI_URL`        | `http://localhost:4567` | URL of the internal Suwayomi API.                        |
| `SUWAYOMI_PUBLIC_URL` | unset                   | Public base URL for Suwayomi for direct client fetching. |
| `SUWAYOMI_SOURCE_ID`  | unset                   | Pins requests to a specific Suwayomi source identifier.  |

## Accounts and instance settings

Create the owner in [First-run setup](/self-host/first-run), then manage registration, sources and user accounts in the admin console. Environment overrides take precedence over those choices. For example, `KOMIKA_REGISTRATION_OPEN` pins registration and `KOMIKA_MANGADEX` pins MangaDex access.

`KOMIKA_APPS_FETCH_THEMSELVES=on` asks a reader with its own engine to fetch chapters on that device. The web reader and apps without an engine still use the server. The server continues answering page requests.

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

Use the admin console's source settings for scan limits and extension subscriptions. Environment settings can also set source rate caps with `SCAN_SOURCE_RATE_LIMITS`, discovery page limits with `SOURCE_SYNC_MAX_PAGES` and update cadence with `EXTENSION_AUTO_UPDATE_INTERVAL_SECONDS`.

The cover cache has separate download, recompression and perceptual-hash controls. Those settings affect the refetchable SQLite cache, not the PostgreSQL backup. For the full environment list, inspect `apps/server/src/config.rs` in the version you run.

For detailed instructions on Docker installation, read [Install with Docker](/self-host/docker). For source setup, read [Add sources](/self-host/sources). For internet connectivity options, read [Reach it from the internet](/self-host/internet-access).

Next: [Update and back up](/self-host/update-and-backup).
