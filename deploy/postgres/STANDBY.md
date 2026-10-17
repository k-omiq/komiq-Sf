# Fenced standby (optional)

A second PostgreSQL host can stream the primary's WAL and be promoted by hand
when the primary is lost. It is opt-in. Without `KOMIKA_NODE_ID` nothing in
this file applies and the stack starts as before.

**Replication is not backup.** A standby copies every change at once,
including a wrong `DELETE` and a corrupted page. pgBackRest (RESTORE.md)
stays the backup and the only way back to an earlier time. Keep its jobs and
its drills.

Status: proven on the test host only (two containers on separate Docker
networks). Production has no standby; adding one waits for a second host.

## What it is

- **Transport.** `hostssl replication` with a client certificate. The role
  `komika_replica` has no password; HBA lets it in only with a certificate
  signed by the standby CA, and only from `KOMIKA_PG_REPLICA_CIDRS`. The
  standby checks the primary's certificate and host name (`verify-full`).
- **Slot.** One physical slot, `komika_standby`, keeps WAL the standby has not
  received. `max_slot_wal_keep_size` caps it at 2 GB; past that the slot is
  lost and the standby must be built again (`standby-init`). Health warns
  from 1 GiB.
- **Fence.** `fence.json` sits in the backup bucket beside the pgBackRest path
  (`<prefix>.fence.json`, never inside `<prefix>/`):

  ```json
  {"at": "2026-10-16T12:00:00Z", "epoch": 2, "primary_node": "node-b", "timeline": 3}
  ```

  Every node has a `KOMIKA_NODE_ID`. Before PostgreSQL starts as a primary
  (an existing cluster without `standby.signal`, a restore, or a new cluster),
  `hosted.py` reads the fence. It starts only if the fence names this node and
  the local timeline is not older than the fence's. The first primary with no
  fence claims epoch 1 with a create-only write. An unreadable fence is a
  refusal, so with a node name set, a primary start needs the bucket.
- **Who checks the fence.** PostgreSQL's start, the backup scheduler (backups
  run only on the fenced primary; a standby pauses them and checks again each
  minute), `backup.sh` jobs, and `hosted.py fence-check`. The server checks the
  database it connects to: with `KOMIKA_NODE_ID` set it refuses a database in
  recovery or one whose `komika.node_id` is another node. PostgreSQL sets that
  value only after its fence check passed. The server does not read the bucket
  itself: that would put backup credentials in the internet-facing container.
- **Promotion is manual.** Nothing promotes on its own.

## What the fence does not do

The fence is read when PostgreSQL starts. A primary that was already running
when the fence moved keeps running and keeps taking writes until it is
stopped. That is why `promote` needs `--old-primary-stopped`. The fence stops
the old primary from **coming back** as a second writer, after a restart, a
reboot or a restore.

## Set up

On your own machine (needs `openssl`), one CA and one directory per node. Use
the address or name each node is reached at; the standby checks it:

```sh
python3 deploy/postgres/hosted.py standby-certs ~/komika-standby-certs node-a=PRIMARY_HOST node-b=STANDBY_HOST
```

`ca.key` stays on your machine. Copy `~/komika-standby-certs/node-a/` to the
primary and `node-b/` to the standby, for example to `/srv/komika-tls`.
Certificates last 825 days; issue new ones before then (remove the node's
directory first; the tool never overwrites).

In `deploy/.env` on each host:

```sh
KOMIKA_NODE_ID=node-a                      # node-b on the standby
KOMIKA_PG_TLS_DIR=/srv/komika-tls
KOMIKA_PG_REPLICATION_BIND=10.0.0.5        # the host address the other node connects to
KOMIKA_PG_REPLICA_CIDRS=10.0.0.6/32        # where the other node connects from
```

The standby uses the same five `KOMIKA_R2_*` values and the same
`KOMIKA_POSTGRES_PASSWORD` as the primary: after a promotion it archives into
the same repository, and the application role is the same.

`KOMIKA_PG_REPLICA_CIDRS` is the address PostgreSQL sees. Behind Docker's
port publishing that can be a bridge gateway rather than the other host; check
`client` in `hosted.py status` on the primary once the standby streams, then
narrow the range.

Use both Compose files on both hosts:

```sh
dc() { docker compose -f docker-compose.yml -f docker-compose.standby.yml "$@"; }
```

On the primary, restart PostgreSQL and the scheduler so they pick up the
settings. The scheduler creates `komika_replica` and grants `pg_read_all_stats`
to `komika` (for the `/metrics` lag gauges):

```sh
dc up -d postgres postgres-backup
dc exec -T --user postgres postgres python3 /opt/komika-postgres/hosted.py fence-check
```

On the standby, with an empty data volume, copy the primary and start only
the database and its scheduler:

```sh
dc run --rm --no-deps postgres standby-init --primary PRIMARY_HOST:5432
dc up -d postgres postgres-backup
```

`standby-init` refuses a data directory that is not empty, a missing fence,
and a node the fence names as primary. It creates the slot when the primary
does not have it yet, and drops and makes it again when the slot was lost.

## Watch the lag

- `hosted.py status` (both nodes): `replication` holds the sender rows,
  LSNs and `replay_lag_seconds` on the primary, and the receiver, received and
  replayed LSN and `replay_lag_seconds` on the standby. `fence` and `node_id`
  are there too.
- `/metrics` on the primary's server: `komika_postgres_replication_standbys`,
  `komika_postgres_current_wal_lsn_bytes`,
  `komika_postgres_replication_{sent,flush,replay}_lsn_bytes{standby}`,
  `komika_postgres_replication_replay_lag_seconds{standby}`,
  `komika_postgres_replication_slot_retained_bytes{slot}` and
  `komika_postgres_replication_slot_active{slot}`. Without
  `pg_read_all_stats`, `komika_postgres_metric_available{metric="replication"}`
  is 0 and the values are left out.
- Health (`postgres-backup`): on the primary, a slot over 1 GiB, a lost slot or
  a standby more than 300 seconds behind; on the standby, not streaming or
  more than 300 seconds behind. Replay lag on an idle standby is 0 while all
  received WAL is replayed.

## Promote

1. Make sure the old primary cannot take writes. Power its host off in the
   provider console, or confirm on the host that its PostgreSQL process has
   stopped. An unreachable port is not proof: a network split can hide a
   primary that still takes writes.
2. Note the time. On the standby host, from `deploy/`:

   ```sh
   postgres/promote.sh --old-primary-stopped node-a -f docker-compose.yml -f docker-compose.standby.yml
   ```

   `hosted.py promote` refuses without that word, and refuses when
   `fence.json` names another node than the one you said is stopped (so a
   second candidate cannot take over a fence a first promotion already
   moved). It checks that the bucket honours conditional writes
   (`If-None-Match` and `If-Match`), writes the next fence epoch naming this
   node only if `fence.json` is unchanged since it read it (two promotions
   cannot both win), then runs `pg_promote()`, a checkpoint and sets
   `komika.node_id`/`komika.fence_epoch`. It prints the last replayed LSN and
   commit time: anything the old primary committed after that is lost (the
   RPO). `promote.sh` then starts the rest of the stack.
3. Restore what PostgreSQL does not hold (next section) before you send users.
4. Move DNS or the tunnel to this host.
5. The scheduler on the new primary starts its due backups within a minute.
   Run `backup.sh full` yourself if you want a new full on the new timeline at
   once. WAL is archived on the new timeline into the same repository.

A rerun of `promote` after a crash continues: a fence that already names this
node is not written again, and a node already out of recovery only finishes the
checkpoint and the settings.

## After a promotion

The old primary refuses to start as a primary: the fence names another node,
and its timeline is older. To use it again, make it the new standby: move its
data volume aside (do not delete it until the new pair is healthy), then run
`standby-init --primary NEW_PRIMARY_HOST:5432` on it with an empty volume.

## What is not in PostgreSQL

A promoted database alone is not a working server. Keep these on the standby
host, or accept what the right column says:

| Item | Where | On the standby |
| --- | --- | --- |
| Engine volume (`suwayomi-data`: extensions, engine database, numeric IDs) | Docker volume | Copy it nightly. Without a copy, re-attach a matching engine or start a new generation (`komika-server engine ...`). |
| Cover files (`covers.sqlite3`) | `server-data` volume | They refill on demand. |
| Media signing key (`media-signing.key`) | `server-data` volume | Copy it once. The same key keeps trust; a new key is a new signing identity for pinned regional caches. Only the promoted host may use it. |
| `deploy/.env`, TLS fallback files (`docker-compose.fallback.yml`) | host | Copy them; keep `KOMIKA_NODE_ID` per host. |
| Engine `server.conf` and source settings | engine volume | Part of the engine volume copy. |
| Backup state (`status.json`, dropped-WAL record) | `postgres-data` volume | Starts empty on the new primary; the repository holds the backups. |

## Not covered

- One standby only, no cascading, no synchronous commit: a promotion can lose
  the last moments of commits (measured RPO in the test-host record).
- The fence was tested against MinIO on the test host. Cloudflare R2 documents
  conditional `PutObject`; `promote` checks it on the real bucket each time
  and refuses if the bucket ignores it.
