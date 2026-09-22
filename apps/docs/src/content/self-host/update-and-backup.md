---
title: Update and back up
description: Update your Docker installation of the admin app and back up or restore your own server’s data.
---

You maintain the server you host, including its accounts, libraries and source engine data. This page covers the Docker installation of the admin app on your home server or VPS. Use it to update, back up and restore your data, or remove the installation.

## Update komiq

To update your server, fetch the latest code and rebuild the container image.

From the repository root, pull the latest changes and rebuild the containers:

```sh
git pull
cd deploy/selfhost
docker compose up -d --build
```

Your data persists across updates because it is stored in Docker named volumes outside the container images.

When an update includes changes to the database structure, komiq applies database migrations automatically when the server boots. Always make a backup before applying updates so you can revert if necessary.

Only one server can use a data directory at a time. The server holds a file lock on `.komiq-server.lock` in that directory until it exits. If startup reports that the directory is locked, stop the other server using the same volume. Deleting the lock file while a server is running can allow a second server to start.

### Clean up disk space

Docker keeps old image layers and build cache on disk after each build. Over time, these files consume disk space.

Run these cleanup commands periodically to reclaim storage:

```sh
docker image prune
docker builder prune
```

## Back up your data

For the Docker installation, your server’s persistent data resides in two named volumes, storage directories Docker manages outside the containers:

| Volume              | Contents                                                                                                       |
| ------------------- | -------------------------------------------------------------------------------------------------------------- |
| `komiq_server-data` | Accounts, personal libraries, reading progress, comments, settings, cached covers, and Cloudflare credentials. |
| `komiq_engine-data` | Installed source extensions and the Suwayomi source engine database.                                           |

### Stop containers before copying

Komiq uses SQLite to store your account and library data. The server writes to the database frequently using Write-Ahead Logging (WAL). If you copy the database files while the server is actively writing, the resulting copy may be corrupted or unreadable.

Always stop the containers before creating a backup.

Both Docker configurations give the server 30 seconds to stop. The server spends up to 20 seconds finishing active requests and buffered writes. Wait for `docker compose stop` to return before copying files. A forced stop can interrupt that drain.

The cover cache is a separate `covers.sqlite3` file in the server volume. Its contents can be fetched again. If SQLite cannot open it or finds corruption during the startup checks, komiq renames it to `covers.sqlite3.quarantined-<id>` and creates a fresh cache. SQLite's related `-wal` and `-shm` files move with it when present. The main account and library database stays in place.

After a cache loss, komiq clears saved cover pointers in small batches when the corresponding cover is missing. Covers return as the cache fills. The quarantined files remain in the volume for inspection and consume disk space until you remove them. Keep them until the new cache is working.

> [!NOTE]
> Timed automated backups are planned for a later release. For now, create backups manually before system maintenance.

### Create the backup archives

Stop the stack, then use a temporary container to create compressed archives of both volumes:

```sh
cd deploy/selfhost
docker compose stop
docker run --rm -v komiq_server-data:/data -v "$PWD":/backup busybox \
  tar czf /backup/komiq-server-data.tgz -C /data .
docker run --rm -v komiq_engine-data:/data -v "$PWD":/backup busybox \
  tar czf /backup/komiq-engine-data.tgz -C /data .
docker compose start
```

This procedure produces two compressed archive files in your current directory: `komiq-server-data.tgz` and `komiq-engine-data.tgz`. Once the archives are created, `docker compose start` resumes normal server operations.

Store these backup files in a safe location outside the host machine.

## Restore from a backup

You can restore your backups onto a new server installation or after resetting a broken environment.

First, create the containers and volumes without starting them:

```sh
cd deploy/selfhost
docker compose create
```

Next, extract the backup archives into the corresponding volumes and restore file ownership:

```sh
docker run --rm -v komiq_server-data:/data -v "$PWD":/backup busybox \
  sh -c 'tar xzf /backup/komiq-server-data.tgz -C /data && chown -R 10001:10001 /data'
docker run --rm -v komiq_engine-data:/data -v "$PWD":/backup busybox \
  sh -c 'tar xzf /backup/komiq-engine-data.tgz -C /data && chown -R 1000:1000 /data'
```

Finally, start the restored stack in detached mode:

```sh
docker compose up -d
```

### File ownership requirements

The `chown` command in the restore procedure is necessary for security and file access.

The komiq server runs as an unprivileged system user with user identifier (UID) `10001` (`komika`). The Suwayomi source engine runs under UID `1000`. If you extract the backup without setting the correct user ownership, the processes cannot write to their data directories and the containers will fail to start.

## Remove komiq

If you want to stop or remove your komiq installation, choose one of these options based on whether you want to preserve your data.

### Stop and keep data

To shut down the containers while preserving your libraries and accounts:

```sh
cd deploy/selfhost
docker compose down
```

Your Docker volumes remain intact on the host disk. Running `docker compose up -d` at any point restarts the server with all your data present.

### Remove containers and delete all data

To completely remove the containers, networks, and data volumes:

```sh
cd deploy/selfhost
docker compose down -v
```

> [!WARNING]
> The `-v` flag permanently deletes the `komiq_server-data` and `komiq_engine-data` volumes. All user accounts, reading history, saved bookmarks, and installed source extensions will be deleted.

Next: [Fix server problems](/self-host/troubleshooting).
