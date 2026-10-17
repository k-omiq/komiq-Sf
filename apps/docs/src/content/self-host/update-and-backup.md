---
title: Update and back up
description: Update your Docker admin app and protect its PostgreSQL database, server files and source engine data.
---

You maintain the server you host, including its accounts, libraries and source engine data. This page covers the Docker installation of the admin app on your home server or virtual private server (VPS). The reader app connects to that server and does not manage its backups.

## Update komiq

Make a backup before updating and keep the previous image until you have checked the new version. From the repository root:

```sh
git pull
bash deploy/selfhost/install.sh
```

The installer keeps your database password and other settings in `deploy/selfhost/.env`. It starts PostgreSQL before building and starting the server. Database structure changes, called migrations, run automatically at server startup.

You can also run `docker compose up -d --build` from `deploy/selfhost` when `.env` already holds your database password.

## Old SQLite install

Komiq now stores its data only in PostgreSQL. An installation from before that kept its data in `komika.sqlite3`. This version cannot read or convert that file. The server refuses to start while the file is in the data directory and PostgreSQL is empty.

To start fresh:

1. Stop komiq.
2. Move `komika.sqlite3` and any `komika.sqlite3-wal`, `komika.sqlite3-shm` and `komika.sqlite3-journal` files out of the data directory.
3. Start komiq again. The server starts empty.

Keep the old file if you want it; komiq does not use it. In Docker, the data directory is `/data` inside the `komiq_server-data` volume. In the desktop app, it is the `server/` folder in the application data folder.

If PostgreSQL already holds data, the server logs a warning that the old file is ignored and starts.

An earlier release could convert that file into PostgreSQL. If such a conversion stopped before it finished, the server refuses to start, because the database holds only part of the data. Restore a PostgreSQL backup, or empty the database to start fresh.

### Copying the old cover cache

The server copies `covers.sqlite3` into cover files once, in the background. The copy resumes after a restart. Covers not yet copied still load from the old cache. Plan free disk space for about twice the cache size while both exist.

The log line `cover copy finished` means the server no longer opens `covers.sqlite3`. The file stays on disk. You can delete it then to free space. Covers uploaded before this update are copied too, but are not in database backups: the old cache did not record which covers were uploads.

## Where data lives

Docker named volumes are storage directories outside container images:

| Volume                | Contents                                                                                               |
| --------------------- | ------------------------------------------------------------------------------------------------------ |
| `komiq_postgres-data` | Accounts, libraries, reading progress, comments, catalogue, settings, cover index and uploaded covers. |
| `komiq_server-data`   | Cover files in `/data/covers`, the old SQLite cover cache and connector files.                         |
| `komiq_engine-data`   | Installed extensions and the source engine's own data.                                                 |

Names use your Compose project prefix if you changed it. Keep `.env` securely too: it holds the database password. Changing that password in `.env` does not change the password inside an existing PostgreSQL cluster.

## Dump the database

A dump is a consistent database export. The included `backup.sh` makes one with `pg_dump` while komiq runs: a custom-format file named `komiq-<UTC time>.dump` in the private `$HOME/komiq-backups` directory, or in the directory you pass as its first argument. It publishes a dump only after success, keeps the newest seven, and leaves earlier dumps in place when a dump fails or is empty. It includes the cover index and covers uploaded since this update. Cover files are a cache and are not backed up. The dump also excludes the source engine's data.

Run `bash deploy/selfhost/backup.sh` from the repository root once, then add this line to the host user's crontab with `crontab -e`. Replace the example repository path with yours:

```sh
0 2 * * * /bin/bash /path/to/komika/deploy/selfhost/backup.sh >> "$HOME/komiq-backups/backup.log" 2>&1
```

Run it as a host user that can access Docker. The job needs `flock` or Perl and never prints the password. Keep the directory outside the repository checkout so image builds cannot copy it, and copy completed dumps off this machine.

## Restore into a fresh install

A complete restore needs the **engine volume archive too**, alongside the PostgreSQL dump. The dump does not contain the engine's numeric series and chapter identities. Restore the `komiq_engine-data` archive before starting the server and engine. An engine's logical backup can renumber those identities, so it is not a substitute for the volume archive.

Copy the repository and create a private `.env` with a random `KOMIQ_DB_PASSWORD`, as described in [Install with Docker](/self-host/docker). Start only the database and restore into its empty `komiq` database:

```sh
cd deploy/selfhost
DUMP="$HOME/komiq-backups/komiq-20261012T020000Z.dump"
docker compose up -d --wait postgres
docker compose exec -T postgres sh -c \
  'PGPASSWORD="$KOMIQ_DB_PASSWORD" exec pg_restore -h 127.0.0.1 -U komiq -d komiq --no-owner --no-acl --exit-on-error' \
  < "$DUMP"
```

Restore before starting the server so its migrations do not create conflicting tables. Check the restore exit status. Restore the engine volume archive using the next section, then start the rest with `docker compose up -d` and inspect your library. Missing cover files refill from their sources on request, or through the background cover crawl and warmer after garbage collection notices them. Uploaded covers return from PostgreSQL, except uploads from before this update.

### Server and engine files

Stop the stack before archiving its non-database volumes:

```sh
umask 077
mkdir -p "$HOME/komiq-backups"
docker compose stop
docker run --rm -v komiq_server-data:/data -v "$HOME/komiq-backups":/backup busybox \
  tar czf /backup/komiq-server-data.tgz --exclude='./covers' --exclude='./covers.sqlite3*' -C /data .
docker run --rm -v komiq_engine-data:/data -v "$HOME/komiq-backups":/backup busybox \
  tar czf /backup/komiq-engine-data.tgz -C /data .
docker compose start
```

On a fresh restore, create these volumes with `docker compose create` and extract each archive with a temporary container. Set server file ownership to user ID `10001` and engine file ownership to `1000` before starting. The server archive excludes cover files and the old cover cache. Never treat a copied live PostgreSQL data directory as a database dump.

### If the engine was replaced

The server checks an engine marker and stored source identities. If a fresh, rebuilt or logically restored engine does not match PostgreSQL, it quarantines the engine data. Readers get a retryable unavailable error for extension-source chapters, pages and thumbnails. The server log says `engine quarantined` and gives the reason. In the admin console, **Sources**, then **Health**, shows the state, the reason and both ways out. See [Source engine state](/admin/engine).

There are two ways out:

- **Re-attach the old engine data.** Restore the matching `engine-data` volume archive and start the stack. The server leaves quarantine by itself. If only its marker was lost and its source identities still match, run `komika-server engine reattach` in the server container. It checks all stored bindings and refuses different data.
- **Start a new generation.** Keep the replacement engine. Choose **Start a new generation** on the Health page, or run `komika-server engine start-new-generation <generation-id>` in the server container. Both refuse unless that generation is current and quarantined. The server sets extension-source reading progress aside, kept but no longer shown, and sets aside comment threads on extension-source series and chapters with their replies and attachments. Library entries, reviews and view counts on extension-source series move to their works. Works, libraries and MangaDex progress stay. Scan or add the new engine's series again.

After a new generation, the server refuses progress that reader apps saved offline before the change, and the apps drop it. Apps too old to send the generation epoch cannot save extension-source progress or change library membership, shelves or favourites on extension-source series for 30 days.

An older engine without global metadata support runs with a warning, source-identity checks and write guards. Its marker protection is unavailable.

## Stop or remove komiq

`docker compose down` removes containers and keeps volumes. `docker compose down -v` deletes all three volumes and their data permanently. Keep verified backups before using `-v`.

Next: [Fix server problems](/self-host/troubleshooting).
