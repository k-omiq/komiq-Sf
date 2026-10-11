---
title: Install with Docker
description: Host the komiq admin app on a home server or VPS with one install command or with Docker Compose.
---

Host the komiq admin app with Docker Compose on a home server or a virtual private server (VPS), a rented server on the internet. The admin app is your server and its management portal. This page covers installation and health checks.

## Requirements

You need:

- Linux, macOS or Windows with Docker Engine 24+ and Docker Compose v2 (`docker compose version`). Docker is a container system: it runs an application and its dependencies in an isolated environment.
- Enough memory for PostgreSQL, the server and the source engine. The installer warns below its build memory threshold. Building compiles the Rust server and both web apps.
- Free disk space for images, PostgreSQL data and backups. An upgrade also keeps your original SQLite database.
- A copy of the komiq repository: a release archive, or `git clone`.

## Install with one command

From the repository folder:

```sh
bash deploy/selfhost/install.sh
```

The installer checks Docker, Compose, architecture, memory, disk space and ports. It asks for the HTTP port, access mode and home-network access. It offers host firewall changes on Linux.

It writes `.env`, preserving existing settings and backing it up once to `.env.bak`. It adds a random database password when missing and keeps both files private. It starts PostgreSQL first, then builds and starts the server. For an old SQLite volume, it announces conversion and waits while progress advances.

Once ready, it starts terminal setup. Read [First-run setup](/self-host/first-run).

| Choice      | Who reaches komiq                            | Ports to open                                 |
| ----------- | -------------------------------------------- | --------------------------------------------- |
| `local`     | This machine or your home network, over HTTP | The HTTP port                                 |
| `tunnel`    | A domain through a Cloudflare Tunnel         | No inbound ports                              |
| `public-ip` | A domain through Cloudflare DNS              | 443                                           |
| `direct`    | Your public IP address, without a domain     | 443 and 80. Without encryption: the HTTP port |

For the last three, read [Reach it from the internet](/self-host/internet-access). In those modes the installer keeps the plain HTTP port on this machine only (`KOMIQ_BIND=127.0.0.1`), unless you answer yes to the home-network question. The exception is `direct` without encryption. Readers use the HTTP port itself, so the installer opens it to every address (`KOMIQ_BIND=0.0.0.0`) and publishes no other port.

> [!NOTE]
> On a cloud server, open the same ports in your provider's firewall too. The installer cannot do that for you.

### Installer options

Choose settings with `--port PORT`, `--access MODE`, `--lan yes|no` and `--firewall yes|no|ask`. `--encrypt yes|no` applies to `--access direct`. `--yes` accepts defaults; `--no-setup` skips terminal setup. `--help` prints the full list.

A later run keeps defaults from `.env` and preserves an existing server's setup choices. Without a terminal, supply `--yes` or every needed answer as a flag. Setup still needs a terminal, so an unattended run prints its command for later.

## Start it by hand

For a new installation without the installer:

```sh
cd deploy/selfhost
umask 077
cp .env.example .env
awk '!/^KOMIQ_DB_PASSWORD=/' .env > .env.password
printf 'KOMIQ_DB_PASSWORD=' >> .env.password
openssl rand -hex 32 >> .env.password
mv .env.password .env
chmod 600 .env
docker compose up -d
```

These commands replace the empty password line privately. Custom passwords may contain only letters, digits, `_`, `.` or `-`. Keep that password on subsequent runs. Compose refuses to start with a missing or empty password. It builds the komiq image (`komiq-selfhost:local`) and starts three services:

- `server`: Your komiq server and management portal. It serves the web reader at `/`, the admin console at `/admin`, the GraphQL application programming interface (API), and the image proxy on a single port.
- `postgres`: PostgreSQL 18, the main accounts, library and catalogue database. It publishes no port. The server uses an application role with password authentication.
- `suwayomi`: The source engine that runs manga extensions. It connects only to the internal Docker network and publishes no ports, because its interface has no authentication.

> [!NOTE]
> A fresh installation has no accounts and no sources. You create the owner account and choose sources during setup.

## Check container health

Run:

```sh
docker compose ps
```

- `postgres` must pass an authenticated database query before the server starts.
- `server` becomes `healthy` after startup and any conversion finish. Its health check queries `http://localhost:8080/health/ready`. Health grace defaults to a day; the installer increases it for larger sources.
- `suwayomi` sets up its own database on first run. Its health check queries `http://localhost:4567/api/v1/settings/about`.

Read the server log:

```sh
docker compose logs server
```

Once the server is healthy, go to [First-run setup](/self-host/first-run) to create the owner account. On a server with no screen, do it over SSH with `docker compose exec server komika-server setup`.

## Environment settings

The password is required. Other settings have defaults. The installer writes `.env` for you and preserves the password on later runs. Do not copy the example over an existing `.env`.

Edit settings, then run `docker compose up -d`. Use `KOMIQ_PORT` for the published HTTP port. `COMPOSE_FILE` selects HTTPS and certificate-check overrides; the installer sets it from your access choice. `COMPOSE_PROFILES=flaresolverr` and `FLARESOLVERR_ENABLED=true` enable the optional challenge solver. Read [Settings reference](/self-host/configuration) for other settings.

Use `KOMIQ_BIND=127.0.0.1` on a VPS that you reach through a Cloudflare Tunnel. Keep the default `0.0.0.0` if devices on your home network connect directly. Read [Connect your devices](/self-host/other-devices).

> [!NOTE]
> Accounts, sign-up, sources and domain names are not set in `.env`. You choose them during setup or in the admin console.

## Storage and upgrades

PostgreSQL data lives in `komiq_postgres-data`. Covers remain a SQLite cache in `komiq_server-data`, alongside connector files and any old `komika.sqlite3`. Engine data lives in `komiq_engine-data`.

Run the installer when updating an old SQLite release. The server copies the old database into an empty PostgreSQL database, verifies it and only then renames the source to `komika.sqlite3.imported-<UTC time>`. It never deletes that file. A failure or refusal exits without serving or changing the original bytes. The installer prints the error and how to start the previous image. Follow [Update and back up](/self-host/update-and-backup) for kept copies, recovery and dump commands, and [Fix server problems](/self-host/troubleshooting) for refusals.

## Status

Capacity and conversion duration for your library have not been measured. Allow extra disk space for both databases during conversion. The installer allows one hour per 2.75 GB of source, with a one-hour floor. An unseen stage or a larger row or byte count resets that stall timer. Repeated lines and log rotation do not extend it. For manual upgrades, set `KOMIQ_IMPORT_START_PERIOD` to at least that allowance if it exceeds the default day.

Next: [First-run setup](/self-host/first-run).
