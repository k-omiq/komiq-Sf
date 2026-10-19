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

| Setting                | Default   | Description                                                                                                                                 |
| ---------------------- | --------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| `KOMIQ_DB_PASSWORD`    | required  | Random URL-safe database password. Generated once by the installer. Keep it unchanged on an initialized cluster.                            |
| `KOMIQ_PORT`           | `8080`    | Host port on which komiq listens.                                                                                                           |
| `KOMIQ_BIND`           | `0.0.0.0` | Host address to bind (`0.0.0.0` for local network access, `127.0.0.1` for this machine only).                                               |
| `COMPOSE_FILE`         | unset     | Extra Compose files that publish port 443 (`docker-compose.https.yml`) and port 80 (`docker-compose.acme.yml`). The installer sets it.      |
| `TZ`                   | `UTC`     | Time zone used for container logs.                                                                                                          |
| `COMPOSE_PROFILES`     | empty     | Optional containers to start: `flaresolverr`, `warp`, or both as `flaresolverr,warp`.                                                       |
| `FLARESOLVERR_ENABLED` | `false`   | Set to `true` with `COMPOSE_PROFILES=flaresolverr` to enable the solver.                                                                    |
| `WARP_ENABLED`         | `false`   | Set to `true` with the `warp` profile to send the engine's source requests through Cloudflare WARP. Read [Add sources](/self-host/sources). |

## Server network and web settings

These variables configure network listeners, web applications, and proxy endpoints.

| Variable                   | Default       | Description                                                                                                                                                                                                                           |
| -------------------------- | ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `BIND_ADDR`                | `0.0.0.0`     | Internal IP address the server listener binds.                                                                                                                                                                                        |
| `PORT`                     | `8080`        | Internal HTTP port the server listener binds.                                                                                                                                                                                         |
| `KOMIKA_METRICS_PORT`      | `9090`        | Port for Prometheus metrics output on loopback.                                                                                                                                                                                       |
| `KOMIKA_METRICS_BIND_ADDR` | `127.0.0.1`   | IP address the metrics listener binds. Keep the default outside a container. In Docker, set `0.0.0.0` and publish the port on host `127.0.0.1` only.                                                                                  |
| `KOMIKA_WEB_DIR`           | unset         | Path to web assets. Serves reader at `/` and console at `/admin`.                                                                                                                                                                     |
| `KOMIKA_IMG_PROXY`         | auto          | Controls `/img` proxy. Enabled when `KOMIKA_WEB_DIR` is set.                                                                                                                                                                          |
| `CORS_ORIGINS`             | local origins | Comma-separated allowed Cross-Origin Resource Sharing (CORS) origins. The native reader apps need `tauri://localhost,http://tauri.localhost,https://tauri.localhost`; the self-host Docker image and the admin desktop app set these. |
| `LOG_FORMAT`               | unset         | `json` for structured JSON logs; unset for text, coloured only at a terminal without `NO_COLOR`.                                                                                                                                      |
| `GRAPHIQL_ENABLED`         | `false`       | Enables interactive GraphiQL query explorer at `/graphql`.                                                                                                                                                                            |
| `KOMIKA_PERSISTED_ONLY`    | `off`         | `on` makes a GraphQL GET name one of the documents the apps ship, by id. A GET with the document text gets `405`, and the apps send it by POST. Read [Edge rendering and caching](/developers/edge-caching).                          |

### Fallback listener

These variables turn on a second HTTPS listener for a fallback address. Read [Fallback address](/self-host/internet-access#fallback-address).

| Variable                              | Default                         | Description                                                          |
| ------------------------------------- | ------------------------------- | -------------------------------------------------------------------- |
| `KOMIKA_FALLBACK_CERT_FILE`           | unset                           | PEM certificate chain file. Unset or empty turns the listener off.   |
| `KOMIKA_FALLBACK_KEY_FILE`            | unset                           | The matching PEM private key file. Required when the listener is on. |
| `KOMIKA_FALLBACK_HOSTS`               | unset                           | Required. Comma-separated host names, each with an optional `:port`. |
| `KOMIKA_FALLBACK_PORT`                | `8443`                          | HTTPS port of the listener.                                          |
| `KOMIKA_FALLBACK_BIND_ADDR`           | the `BIND_ADDR` value           | Address the listener binds.                                          |
| `KOMIKA_FALLBACK_READER_DIR`          | `KOMIKA_WEB_DIR` plus `/reader` | Reader build folder with `index.html`.                               |
| `KOMIKA_FALLBACK_MEDIA_BYTES_PER_DAY` | `50000000000`                   | Media bytes the listener sends per UTC day before media gets `503`.  |

## Database and storage settings

These variables configure database connections and file storage paths.

| Variable                    | Default                                | Description                                                                                                                                                                                                                |
| --------------------------- | -------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `DATABASE_URL`              | `postgres://localhost/komika`          | PostgreSQL URL for accounts, libraries and settings. Compose supplies the private `postgres` service and application role.                                                                                                 |
| `COVERS_DATABASE_URL`       | `sqlite://covers.sqlite3`              | Places the data-directory lock, default covers directory and old cache read by the one-time copy.                                                                                                                          |
| `KOMIKA_COVERS_DIR`         | `covers` beside the covers SQLite file | Cover and extension icon file cache. Docker uses `/data/covers`; the desktop app uses `server/covers` in its data folder.                                                                                                  |
| `EXT_ICONS_DIR`             | `/srv/ext-icons`                       | Directory of bundled extension icons. Fetched icons are cached under `KOMIKA_COVERS_DIR`.                                                                                                                                  |
| `KOMIKA_BACKGROUND_WRITERS` | `1`                                    | Database connections for background jobs. With `1`, every background job writes on one connection, one at a time. A higher number keeps one connection for that and lets the jobs that are safe to overlap use the others. |

Each long background job (a catalogue sync, a source import, a cover crawl) also holds one extra database connection while it runs, to mark that it owns the job. Leave a few spare connections in PostgreSQL for them.

Run one komiq server per database. A second server skips the background jobs the first one runs, but it counts source rate limits on its own, so your sources get twice the requests.

The main database lives in `postgres-data`; cover files and other server files live in `server-data`. PostgreSQL dumps include the cover index and covers uploaded since this update. Cover files are not backed up: missing covers refill from sources, and uploads return from PostgreSQL. See [Update and back up](/self-host/update-and-backup).

## Source engine settings

These variables configure connections to the Suwayomi source engine.

| Variable              | Default                 | Description                                             |
| --------------------- | ----------------------- | ------------------------------------------------------- |
| `SUWAYOMI_URL`        | `http://localhost:4567` | URL of the internal Suwayomi API.                       |
| `SUWAYOMI_PUBLIC_URL` | unset                   | Public base URL for page and cover URLs. See below.     |
| `SUWAYOMI_SOURCE_ID`  | unset                   | Pins requests to a specific Suwayomi source identifier. |

Set `SUWAYOMI_PUBLIC_URL` to this komiq server's public address, or to `/` as the Docker image does. Readers then fetch pages and covers through komiq. When the variable is unset, or names the same address as `SUWAYOMI_URL`, page and cover URLs point at Suwayomi itself, and readers must be able to reach it. A user name and password in `SUWAYOMI_URL` do not count as a different address.

Most pages use a chapter address (`/media/v1/c/...`) that only komiq answers. The server never builds it on `SUWAYOMI_PUBLIC_URL`, because that address can be a public Suwayomi server. The chapter address is a path, and the reader loads it from the server it is connected to. When the page and cover URLs point at Suwayomi itself, pages keep Suwayomi's own URLs.

## Exact media settings

These variables turn on signed media manifests: a list of the exact bytes of each chapter page, signed by your server. They are for a server that feeds other caches. A server that serves only its own readers leaves them unset.

| Variable                        | Default                                                       | Description                                                                                                                                                                                                                                                                                            |
| ------------------------------- | ------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `KOMIKA_MEDIA_MANIFESTS`        | off                                                           | `on` makes the signing key, files opened chapters under their byte hashes and serves `/media/v1/` objects and manifests.                                                                                                                                                                               |
| `KOMIKA_MEDIA_ROUTES`           | one owner route on the server's own origin                    | A JSON list of `owner`, `regional` and `helper` origins that serve exact objects, for a web reader built with a matching `PUBLIC_KOMIKA_MEDIA_HOSTS`. The server refuses to start when it is not valid. Read [Media routes](/developers/media-routes) and [Run a media cache](/self-host/media-cache). |
| `KOMIKA_MEDIA_SIGNING_KEY_FILE` | `media-signing.key` in the working folder (`/data` in Docker) | The Ed25519 signing key. The server creates it with owner-only access when it is missing. Back it up: every cache that trusts the old key must be given a new one.                                                                                                                                     |

With `KOMIKA_MEDIA_MANIFESTS` off, the server makes no key file and writes nothing new. Read [Exact media and signing](/developers/media-manifests).

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

### Bot checks

Cloudflare Turnstile is a browser check that tells people from scripts. With it on, sign-up, sign-in, password changes, comments, reviews and reports need the check to pass. Make a Turnstile widget in the Cloudflare dashboard, list the website names people use, such as `example.com` and `admin.example.com`, and copy its two keys.

| Variable                    | Default | Description                                                                                                       |
| --------------------------- | ------- | ----------------------------------------------------------------------------------------------------------------- |
| `KOMIKA_TURNSTILE`          | `off`   | `on` asks for the check. The server refuses to start when `on` is set and either key is missing.                  |
| `KOMIKA_TURNSTILE_SITE_KEY` | unset   | The widget's site key. It is public: the server gives it to the web apps.                                         |
| `KOMIKA_TURNSTILE_SECRET`   | unset   | The widget's secret key. Keep it out of shared files. The server sends it only to Cloudflare to check each token. |

The self-host compose file passes these three from `.env`. Set `TRUSTED_PROXY_CIDRS` too when a proxy stands in front of the server, so Cloudflare sees the visitor's address.

> [!NOTE]
> The check works only on the website. The native apps cannot show it, so on a server with the check on, people sign in and post on the website. Run `komika-server setup` while the check is off, because it signs in without a browser.

## Scanner and maintenance tasks

Use the admin console's source settings for scan limits and extension subscriptions. Environment settings can also set source rate caps with `SCAN_SOURCE_RATE_LIMITS` (one cap covers every background fetch from that source), discovery page limits with `SOURCE_SYNC_MAX_PAGES` and update cadence with `EXTENSION_AUTO_UPDATE_INTERVAL_SECONDS`.

The cover cache has separate download, recompression and perceptual-hash controls. Covers and extension icons are files under `KOMIKA_COVERS_DIR`, named by their SHA-256 content hash at `objects/<2 hex>/<sha256>`. Each file is written once; `tmp/` holds files being written. PostgreSQL holds the index. Only the owner server writes covers under its data-directory lock. Garbage collection removes files older than 24 hours with no entry in the index. For the full environment list, inspect `apps/server/src/config.rs` in the version you run.

For detailed instructions on Docker installation, read [Install with Docker](/self-host/docker). For source setup, read [Add sources](/self-host/sources). For internet connectivity options, read [Reach it from the internet](/self-host/internet-access).

Next: [Update and back up](/self-host/update-and-backup).
