---
title: Fix server problems
description: Fix setup, container and source problems on the komiq server you host with Docker.
---

Fix Docker setup, container and source problems by checking your server logs. For reader app problems, read [Fix reader problems](/guide/troubleshooting).

## The installer stops with "problem:"

`bash deploy/selfhost/install.sh` prints one line that says what to fix:

- "Port 8080 is in use. Try --port 8081.": another program has the HTTP port. Run the installer again with the port it names.
- "Port 443 is in use. Free it before continuing.": another program, often a web server, holds a port komiq needs. Stop it, or choose `tunnel`.
- "Could not check ports.": install `ss`, `lsof` or `netstat`.
- "Server did not become healthy.": startup stopped making progress. Read the server log, as described below. Import progress extends the wait.

## No setup code in the server log

The server issues a setup code only while no administrator account exists. If you already created an owner account, open `http://192.168.1.20:8080/admin` and sign in. On a fresh installation, read the log with `docker compose logs server`, and check directory permissions and environment variables if initialization fails.

## Setup displays "Invalid setup code"

The server makes a new setup code each time it starts, and the old one stops working. Retrieve the active code from the running container:

```sh
docker compose logs server | grep "setup code" | tail -n 1
```

Type the code shown in the last log line. The server ignores spaces, dashes and letter case.

## Setup cannot continue

For "Too many setup attempts", wait for the time shown before retrying. If the Owner step cannot check the connection, click **Try again**.

## Server container is not healthy

The health check queries `/health/ready` to check the server and database. Read the logs with `docker compose logs server`.

For a published-port conflict, set `KOMIQ_PORT` to a free port in `.env` and run `docker compose up -d`.

For file permission errors, ensure `komiq_server-data` permits writes by server user ID `10001`.

If the data directory is locked, stop the duplicate container or desktop server: `.komiq-server.lock` keeps two servers off the same files and releases when the process exits. Do not delete it while a server runs.

For a PostgreSQL connection error, inspect `docker compose logs postgres`. Keep the original `KOMIQ_DB_PASSWORD` in `.env`; editing it does not change an existing cluster's password.

If Compose says `Run install.sh to generate KOMIQ_DB_PASSWORD`, run `bash deploy/selfhost/install.sh` from the repository root. It adds the password without removing old settings.

### SQLite conversion stops or is refused

On an update from SQLite, the server converts `/data/komika.sqlite3` into an empty PostgreSQL database: read [Update and back up](/self-host/update-and-backup). A failed or refused start prints `sqlite import: error: <stage> stage: ...` and exits. The installer stops the server.

A refusal usually says `Nothing was changed`, names the files involved and gives the exact renames and deletions for each way out. When it offers a kept copy, it gives that copy's row counts (or `empty`), so you can pick the copy that holds your data. This is the end of one such line, when only the work copy is left:

```text
To import it, rename /data/komika.sqlite3.import-work-source to /data/komika.sqlite3 and start again. Or, to start with an empty database, move the listed files out of the data directory and start again
```

In Docker those paths are inside the `komiq_server-data` volume (the prefix follows your Compose project name). Stop the server with `docker compose stop server` (in `deploy/selfhost`), make each rename in a temporary container, then run `docker compose up -d`:

```sh
docker run --rm -v komiq_server-data:/data busybox \
  mv /data/komika.sqlite3.import-work-source /data/komika.sqlite3
```

| The line says                                                                       | What to do                                                                                                           |
| ----------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| `is missing`, `cannot be used`                                                      | Put the SQLite file back, or rename the saved copy as the line says.                                                 |
| `from a newer release`, `belongs to a newer release`, `differs from the migration`  | A newer komiq wrote the SQLite file or the PostgreSQL database. Start that release. |
| `already holds other data, while`                                                   | Wrong database: correct `DATABASE_URL`. Right one: delete the record and work files the line names.                  |
| `changed after PostgreSQL received its data`                                        | To keep PostgreSQL's data, move the SQLite file elsewhere and delete the record and work files.                      |
| `started with the import-sqlite-to-postgres command`                                | The server never resets that import. Finish it with the command, or follow the line's other way if PostgreSQL holds nothing you need. |
| `must be owned by the user that runs the server`                                    | Run the `chmod 700` it names, and `chown` the folder to the server user (ID `10001` in Docker).                      |
| `not enough free disk space`                                                        | Free 8 times the SQLite files' size plus 256 MiB in the folder the line names.                                       |

## Port 443 or 80 does not answer

Docker publishes these ports only when `COMPOSE_FILE` in `.env` names the extra Compose files. Run the installer with `--access public-ip` or `--access direct`, or read [Reach it from the internet](/self-host/internet-access). Then check the firewall on the machine and at your cloud provider.

If **Settings › Access** shows HTTPS as "Not running", it names the reason, and the server tries again by itself.

## The console stops answering at its public address

With **Only my devices**, the server refuses plain HTTP from the internet. Open the console at the server's home network address, or run `docker compose exec server komika-server setup` and choose a public mode.

## Cloudflare setup problems

If setup cannot get `cloudflared`, check outbound access to GitHub and retry, or install the connector yourself and set `CLOUDFLARED_BIN`. If a step fails, add the token permission it names and retry.

## Source engine container fails or stays in starting

On first start, Suwayomi unpacks assets and creates its database. Let that finish and read `docker compose logs suwayomi`. If the container stops repeatedly, check the host's memory.

## The catalogue is empty

A fresh installation has no sources. Open **Settings › Sources** to add an extension repository, then use **Sources › Extensions** to install extensions. Read [Add sources](/self-host/sources). MangaDex has its own switch in source settings.

If adding a repository fails, check `docker compose ps`. The server cannot install extensions while the engine is booting.

## Covers or chapter pages do not load

If the log says the cover cache was recreated, komiq preserved an unavailable or corrupt cache beside `covers.sqlite3` with a `.quarantined-<id>` suffix. Check disk space while both cache files exist.

This happens if the source engine is stopped, an extension is down, or the source website uses Cloudflare bot protection. Check `docker compose ps`, then open **Sources › Health** in the admin console.

If the upstream source website requires solving a Cloudflare "checking your browser" challenge, enable FlareSolverr, a headless browser that solves challenge pages for the engine. In `.env`, set:

```sh
COMPOSE_PROFILES=flaresolverr
FLARESOLVERR_ENABLED=true
```

Apply the changes by running `docker compose up -d`.

If the chapter list loads but the page images fail, and `docker compose logs flaresolverr` says `Cloudflare has blocked this request`, the site blocks your server's address. A solver cannot get past a block. Send the engine's requests through Cloudflare WARP instead. Read [Add sources](/self-host/sources).

## Extension chapters say unavailable after a restore

If MangaDex series read but extension-source chapters, pages and thumbnails stay unavailable, and the server log says `engine quarantined`, the source engine does not match the server's data. This follows a database-only restore, a rebuilt engine or an engine restored from its own backup. Open **Sources**, then **Health**, in the admin console. It shows the reason and the two ways out. Read [Source engine state](/admin/engine) before you choose.

## Build fails or the machine runs out of memory

If the compiler stops for lack of memory, add memory or swap, or build the image on another computer, load it here and run `docker compose up -d` without `--build`.

## Disk space runs low after updates

Docker keeps earlier container images, build steps and compiler caches. Remove unused ones:

```sh
docker image prune
docker builder prune
```
