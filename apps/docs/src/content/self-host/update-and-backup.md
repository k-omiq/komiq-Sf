---
title: Update and back up
description: Update your Docker admin app and protect its PostgreSQL database, server files and source engine data.
---

You maintain the server you host, including its accounts, libraries and source engine data. This page covers the Docker installation of the admin app on your home server or virtual private server (VPS). The desktop app runs the same SQLite conversion in its own `server/` folder. The reader app connects to that server and does not manage its backups.

## Update komiq

Make a backup before updating and keep the previous image until you have checked the new version. From the repository root:

```sh
git pull
bash deploy/selfhost/install.sh
```

The installer keeps your database password and other settings in `deploy/selfhost/.env`. It starts PostgreSQL before building and starting the server. Database structure changes, called migrations, run automatically at server startup.

For an installation already using PostgreSQL, you can also run `docker compose up -d --build` from `deploy/selfhost`. An old SQLite installation needs the installer once to add a random database password to its existing `.env`.

### Automatic SQLite conversion

The first start after the update converts an old SQLite installation. Before it listens, the server looks for `komika.sqlite3` in the server volume. If PostgreSQL is empty, it copies the data in, checks the copy against the source, and only then renames the file to `komika.sqlite3.imported-<UTC time>`, a time like `20261012T093000Z`. It serves only after that check passes. The server removes only its own work files. It never deletes the old file, the renamed file or a kept copy (below). Delete them yourself once you have checked your data.

The installer detects the old file, explains the conversion and shows progress. Watch from another terminal:

```sh
cd deploy/selfhost
docker compose logs -f server
```

Progress lines begin with `sqlite import:`. Advancing progress extends the installer's wait. A stop in the middle resumes at the next start. If PostgreSQL already holds application data, the server leaves the SQLite file alone, logs the warning `PostgreSQL already holds data or a completed import; leaving SQLite in place` and starts.

### Kept copies and their two warnings

During conversion the server keeps a saved copy of the SQLite file, `komika.sqlite3.import-work-source`, and a record, `komika.sqlite3.import-work-state`. If the SQLite file differs from that copy at a later start, for example because you put back another file, the server converts again from the beginning. First it moves the saved copy aside as `komika.sqlite3.import-kept-<UTC time>` (with `-wal` or `-journal` files if the original had them) and logs:

```text
sqlite import: warning: the copy of the SQLite database that an earlier import attempt made differs from the file now at ...
```

The line says whether the kept file is an exact copy. A second warning, `the database being imported is smaller than the copy kept at`, appears when one of `users`, `user_library`, `comments` or `work` holds fewer rows now than in the kept copy. It lists the counts, for example `users: 0 now, 3 before`. The conversion goes on after both warnings. Check which database holds your data, and delete the kept copy only then.

### If conversion fails

The server exits and leaves the SQLite file in place. The installer prints the failure line and stops the server to prevent repeated restarts. Some failures are refusals that name exact commands: read [Fix server problems](/self-host/troubleshooting).

Before a successful conversion, you can return to the previous SQLite image. The installer tags it as `komiq-selfhost:<project>-pre-postgres` and records its immutable image ID beside `.env`. For the default `komiq` project:

```sh
cd deploy/selfhost
KOMIQ_IMAGE=komiq-selfhost:komiq-pre-postgres docker compose \
  -f docker-compose.yml -f docker-compose.rollback.yml \
  up -d --no-build --no-deps server
```

If none was saved, use your previous release image or its archive: this repository specifies no published SQLite tag. Add your HTTPS and certificate-check Compose files if you used them. After a successful conversion and new writes, the kept SQLite file is behind and cannot restore your latest reading history. Use a current PostgreSQL backup instead. Do not return to an older SQLite release once PostgreSQL holds data it cannot store, such as scanlator ratings: the export back to SQLite refuses and writes no file. The export does leave out the stored source keys (the source engine's own manga and chapter keys), even when they hold rows, because the server captures them again from the engine. Its report lists how many rows it left out. Keep PostgreSQL and its backups.

## Where data lives

Docker named volumes are storage directories outside container images:

| Volume                | Contents                                                                           |
| --------------------- | ---------------------------------------------------------------------------------- |
| `komiq_postgres-data` | Accounts, libraries, reading progress, comments, catalogue and settings.           |
| `komiq_server-data`   | SQLite cover cache, connector files, and the old SQLite file with its kept copies. |
| `komiq_engine-data`   | Installed extensions and the source engine's own data.                             |

Names use your Compose project prefix if you changed it. Keep `.env` securely too: it holds the database password. Changing that password in `.env` does not change the password inside an existing PostgreSQL cluster.

## Dump the database

A dump is a consistent database export. The included `backup.sh` makes one with `pg_dump` while komiq runs: a custom-format file named `komiq-<UTC time>.dump` in the private `$HOME/komiq-backups` directory, or in the directory you pass as its first argument. It publishes a dump only after success, keeps the newest seven, and leaves earlier dumps in place when a dump fails or is empty. A dump excludes the refetchable `covers.sqlite3` cache and the source engine's data.

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

Restore before starting the server so its migrations do not create conflicting tables. Check the restore exit status. Restore the engine volume archive using the next section, then start the rest with `docker compose up -d` and inspect your library.

### Server and engine files

Stop the stack before archiving its non-database volumes:

```sh
umask 077
mkdir -p "$HOME/komiq-backups"
docker compose stop
for v in server engine; do
  docker run --rm -v "komiq_$v-data":/data -v "$HOME/komiq-backups":/backup busybox \
    tar czf "/backup/komiq-$v-data.tgz" -C /data .
done
docker compose start
```

On a fresh restore, create these volumes with `docker compose create` and extract each archive with a temporary container. Set server file ownership to user ID `10001` and engine file ownership to `1000` before starting. The cover cache can be omitted and fetched again. Never treat a copied live PostgreSQL data directory as a database dump.

### If the engine was replaced

The server checks an engine marker and stored source identities. If a fresh, rebuilt or logically restored engine does not match PostgreSQL, it quarantines numeric engine data. Readers get a retryable unavailable error for those chapters, pages and thumbnails. The server log says `engine quarantined` and gives the reason. Admins can read `engineGenerationState { id state reason since unfenced }` through GraphQL. The admin console screen for this state is not available yet.

Restore the matching engine volume to resume automatically. If only its marker was lost and its source identities still match, run `komika-server engine reattach` in the server container. It checks all stored bindings and refuses different data.

If you intend to keep the replacement engine, run `komika-server engine start-new-generation`, or use the admin GraphQL mutation `startNewEngineGeneration { id state }`. The server sets numeric reading progress aside, kept but no longer shown, and detaches old numeric comment threads with their replies and attachments. Works, work-level libraries and MangaDex progress stay. Scan or add the new engine's series again. Numeric progress from apps that send no generation epoch is refused for 30 days; an explicitly stale epoch is always refused. SQLite reverse export refuses while detached progress or the queued-write epoch exists.

An older engine without global metadata support runs with a warning, source-identity checks and write guards. Its marker protection is unavailable.

## Stop or remove komiq

`docker compose down` removes containers and keeps volumes. `docker compose down -v` deletes all three volumes and their data permanently. Keep verified backups before using `-v`.

Next: [Fix server problems](/self-host/troubleshooting).
