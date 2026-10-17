# Komika deploy

Stand up the **full public stack** — a hosted Mihon/Tachiyomi with a public social
layer (ratings, reviews, per-chapter comments), an auto-served catalog, and an
**adaptive scanner that keeps every library series updated** so users never have to
manually refresh.

## One command

```sh
cd deploy
cp .env.example .env      # edit: PUBLIC_HOST, KOMIKA_ADMIN_PASSWORD, KOMIKA_POSTGRES_PASSWORD…
./deploy.sh
```

`deploy.sh` builds the images, starts everything, waits for health, then **bootstraps**
the stack: registers the Keiyoushi extension repo, installs the **MangaDex** source,
turns on the **FlareSolverr Cloudflare bypass**, creates the admin account, and seeds
the library. When it finishes it prints the URLs.

```
Reader      not started (compose profile 'selfhost' — see below)
API         http://127.0.0.1:8080/graphql   (loopback-only; POST only unless GRAPHIQL_ENABLED=on)
Suwayomi    http://127.0.0.1:4567           (loopback-only; source management / images)
admin login: <KOMIKA_ADMIN_USERS> / KOMIKA_ADMIN_PASSWORD
```

**Both published ports bind to `127.0.0.1` only.** Docker's DNAT bypasses the host
firewall's `filter/INPUT` chain entirely, so a `0.0.0.0` publish here is _not_ covered
by your host firewall rules — and it is reachable from containers in unrelated
compose stacks on the same machine, via any bridge. Put a reverse proxy or a
Cloudflare Tunnel in front (see `CLOUDFLARE-RUNBOOK.md`) rather than widening these.

**The `reader` container is opt-in.** It sits behind the compose profile `selfhost`,
because in production the reader is a Cloudflare Worker and starting the nginx SPA
would publish an unused world-bound port. To run it:

```sh
./deploy.sh up --with-reader     # or: export COMPOSE_PROFILES=selfhost
```

> **Plain-HTTP warning for the self-host path:** with `--with-reader`, the reader SPA
> is served over plain HTTP (`nginx.conf` has no TLS listener) and calls the API with
> an `http://` URL. Every session token (`Authorization: Bearer …`) then crosses the
> network in cleartext — anyone on the path (LAN, public Wi-Fi) can capture a token and
> replay it for 30 days. This path is meant for a _loopback or trusted-LAN evaluation_,
> not public exposure. On an untrusted network, put TLS in front (Caddy/nginx + LE, or
> a Cloudflare Tunnel) and flip the API URL + CSP `connect-src` in `docker-compose.yml`
> to `https://`. Production (Cloudflare Worker reader) already terminates TLS at the edge.

Other commands: `./deploy.sh logs`, `./deploy.sh bootstrap` (re-run bootstrap only),
`./deploy.sh down` (stop, keep data), `./deploy.sh destroy` (stop + delete volumes).

**`up` runs a preflight** before it starts anything: `TRUSTED_PROXY_CIDRS` must cover
the live `komika_komika` bridge gateway, and may only be set while the API port is
loopback-bound. Both failures are otherwise silent — a stale gateway collapses every
user into one auth rate-limit bucket, and a world-bound port lets clients forge
`X-Forwarded-For` past the limiter. It prints the correct value when it aborts;
`SKIP_PROXY_CIDR_CHECK=1` bypasses it. A bare `docker compose up -d` skips the check,
which is why `./deploy.sh` is the documented path.

**Resource + log limits** are set per-service in `docker-compose.yml`: `mem_limit`
(suwayomi 8g / server 4g / flaresolverr 2g / postgres 4g / backup 512m;
18.5 GiB of about 23 GiB; the server reduction needs a Phase 6 load test) so a runaway can't
make the kernel pick an OOM victim from an unrelated stack, and `json-file` logging
capped at `20m × 5` so container logs can't fill the disk. `/etc/docker/daemon.json`
carries the same log cap as a **daemon-wide default** for containers started by other
stacks — but daemon.json is only read at Docker-daemon start, so it takes effect at
the next daemon restart, not at the next `docker compose up`.

## Services

| Service          | What it does                                                                                                                                                                                     |
| ---------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **suwayomi**     | Runs the source extensions (MangaDex, …): catalog, chapters, **and page/cover images**.                                                                                                          |
| **flaresolverr** | Headless-browser sidecar that solves Cloudflare "checking your browser" challenges. Internal-only; Suwayomi calls it at `http://flaresolverr:8191`.                                              |
| **server**       | `komika-server` — federates Suwayomi + adds accounts/social/admin, and runs the **adaptive scan scheduler** (auto-updates the library on a cadence derived from each series' chapter frequency). |
| **reader**       | Static SvelteKit SPA (nginx), pointed at the server. **Opt-in** — compose profile `selfhost`; in production the reader is a Cloudflare Worker instead.                                           |

| **postgres** | PostgreSQL 18.6, private port 5432, main data and pgBackRest WAL archive. |
| **postgres-backup** | Weekly full, daily incremental and daily check, serialized inside Compose. |

## How it's wired (important gotcha)

- **GraphQL federation is internal:** the server reaches Suwayomi at
  `SUWAYOMI_URL=http://suwayomi:4567` over the private compose network.
- **Image URLs are public:** page/cover images are `<img src>`s the _browser_ loads, so
  they can't use the internal `suwayomi:4567` hostname. The server rewrites them to
  **`SUWAYOMI_PUBLIC_URL`**. Point this at the **komika server**, not at Suwayomi:
  Suwayomi's `:4567` is loopback-bound (its admin API is unauthenticated, so it must
  never be internet-reachable), and the server proxies the image bytes at
  `/api/v1/manga/…`. In prod that is `SUWAYOMI_PUBLIC_URL=https://api.komiq.cc`. Set
  `PUBLIC_HOST` to your domain/IP in `.env` for a real deploy.
- The reader's backend config is baked in **at build time** (Vite inlines `PUBLIC_*`), so
  the compose `reader` build args carry `PUBLIC_KOMIKA_API` / `PUBLIC_KOMIKA_IMG_MODE`.
  Rebuilding is automatic via `deploy.sh`.
- **Persistence:** main data and avatars live in `postgres-data`. Covers remain a
  refetchable SQLite cache in `server-data:/data/covers.sqlite3`; pgBackRest excludes
  it. Suwayomi uses `suwayomi-data`. `down` retains volumes; `destroy` deletes all of them.

## Auto-updating catalog

`SEED_SERIES` in `.env` (comma-separated titles) is added to the library on bootstrap.
The scanner (`SCAN_TICK_SECONDS`, default 300) then re-checks each library series on a
cadence derived from its average time-between-chapters, refetches overdue ones from the
source, and records new chapters — so the Updates/Library screens stay current with no
user action. Admins can add/adjust more series and per-series scan policy in the
**admin "manga DB" console**.

## Backups and restore (pgBackRest)

This tree packages PostgreSQL 18.6 with pgBackRest 2.59.x. The global server has run on
this package since the cutover on 2026-10-13. Its rehearsal gate of two clean rehearsals
in a row was waived by the owner, not passed, and a restore or point-in-time drill against
the live repository has not been recorded yet. For old SQLite installations, see
[Old SQLite install](https://docs.komiq.cc/self-host/update-and-backup#old-sqlite-install).
This version cannot read or convert the old main database.
Fill all five `KOMIKA_R2_*` values in `.env`, with a dedicated R2 prefix. No backup
variables means local initdb with a loud warning and archiving disabled. Partial
configuration, repository errors and failed restores stop startup.

A fresh data volume restores the latest valid backup. A reachable empty prefix
requires `KOMIKA_PG_INIT_NEW=1`; it never silently creates a replacement database.
Create the stanza and first full explicitly using [postgres/RESTORE.md](postgres/RESTORE.md),
then return `KOMIKA_PG_INIT_NEW` to 0. Existing cluster credentials must match `.env`.
Missing-path repository errors require a separate signed HTTPS emptiness check;
authentication, network and metadata errors never authorize initdb.

Recovery health is read with `hosted.py status` (JSON: job ages, latest backups,
archiver, backlog, free space, dropped-WAL gap and the recoverable start) and the
budget with `hosted.py reading`, both run in the `postgres-backup` container. After
a deploy, `./watch.sh --url http://127.0.0.1:8080/health/ready --minutes 62` watches
health, restarts, log severities and the backup side until a wall-clock deadline.
When pgBackRest drops WAL, PITR cannot cross the gap until a new full backup
(`backup.sh full`) completes. Commands and meanings: "Recovery health" in
[postgres/RESTORE.md](postgres/RESTORE.md).

For database rehearsals only the directory `postgres` (this repository's
`deploy/postgres`) goes to the host, and `./rehearsal.sh` is run from inside
that copy; the rest of this directory (the hosted Compose file with project
`komika`, `deploy.sh`) does not belong on a rehearsal host. On a host that
also runs production the order is fixed: inventory and measurements first;
the image is built and saved on another machine, its hash is checked on the
host and the disk is measured again before `docker load`; the kit is unpacked
into a new directory (never `rsync --delete` into an existing one); nothing is
built or pruned on the host; a baseline of every other container (ID, start
time, restart count) is recorded before the first step and compared after
every large one; the whole run happens in one tmux session, with a read-only
watch loop in a second window while `restore` runs and abort
criteria agreed before it starts; the restored directory and, at the very
end, the run's directory are each removed by their literal path as the last
link of an identity check, never as a separate command, and that check
stops when `findmnt` or `docker ps` itself fails (a command that prints
nothing is no proof that nothing is mounted or running); the records and
reports are copied off the host before the last removal. The pre-flight
blocks are shell functions: paste one block, read it, then the next; a
failed check prints `STOP`, and no later block runs in that shell. `sudo -n true || sudo -v`
is run alone before the two blocks that need root, so that no password
prompt reads a pasted line; the baseline of the other containers must not be
empty; the drill lines are entered one at a time and never pasted whole,
because they do not stop themselves. The exact
commands are the sections "Pre-flight on the host", "While `restore` runs"
and "Full drill order" of
[postgres/RESTORE.md](postgres/RESTORE.md).

`rehearsal.sh` has a fixed list of subcommands (`preflight`, `up`, `backup`,
`restore`, ...) and builds every Docker command line
itself: no argument is passed through to Docker, Compose or pgBackRest, and
`restore.sh`, `pitr-drill.sh` and `deploy.sh` refuse rehearsal selections.
Export only `KOMIKA_REHEARSAL_PROJECT` (`komika-pg-rehearsal-*`),
`KOMIKA_COMPOSE_ENV_FILE` and `KOMIKA_REHEARSAL_ROOT`; all settings, the
`rehearsal/` R2 prefix and the actual nonsecret `KOMIKA_PRODUCTION_R2_PREFIX`
live in the mode-0600 env file. What the wrapper checks:

- One operation at a time. Every subcommand except `status`, `logs`,
  `psql-check` and `archive-status` takes a host-wide lock
  (`/run/lock/komika-pg-rehearsal.lock`) before it validates or measures
  anything and holds it through its cleanup. A second invocation is refused
  at once. The lock directory `/run/lock` must be owned by root and sticky;
  when it is missing, not root's or not sticky, the refusal says which.
- Before each subcommand: the pinned hash of `rehearsal-compose.yml`, the
  rendered Compose configuration against an exact expectation, resource
  labels, disjoint prefixes, and no `DOCKER_*` or `COMPOSE_*` variable.
- A label is not proof of storage. An existing source volume must be a plain
  `local` volume without driver options whose mountpoint is its own directory
  below Docker's data root, with nothing mounted on it and no other container
  mounting or overlapping it; the containers' real mount sources must be
  those directories. `preflight --init-root` creates a root only for a project
  of which no container, volume or network exists.
- Before every `docker exec` and after `up`: the live container's real memory,
  swap, CPU, PID, shm and log limits and its restart policy equal the
  validated model, and its environment holds no name outside the model and
  the image's own six.
- Before a container starts from the image: the seven kit files inside the
  image equal the kit directory (file identity only, not the base image).
- The root is created by `preflight --init-root` and fenced against Docker
  data, volume mountpoints and other containers' bind mounts by path, inode
  and mount table.

The two services have their own volumes and bridge, no published ports, and
memory, CPU and PID ceilings (PostgreSQL 3 GiB and 1 CPU by default); every
job, reader and restore container has explicit log bounds; block I/O is not
limited. Each step that writes estimates what it will write and refuses when
that would leave less than a floor of free disk space (3 GiB by default). The
estimate is measured once, under the lock, before the step: it is no
reservation, and production can still use the disk meanwhile, so the runbook
measures after every step. A rehearsal `up` never restores: with an empty or
absent data volume and a prefix that already holds a backup it refuses and
names `restore --target`. Repository readers are
removed when their step ends, also after an interrupt. If a container's
creation was interrupted and it did not appear in time, the kit reports it as
unresolved, every later step refuses, and `stop-source` or `down` removes it.
The runbook's order writes one marker before and one after the target time
and makes `restore` check both. Record source settings before deleting the
source volume; `--source-settings` retains later recovery-ceiling increases.
Restore-only works after source removal; its restore bridge is separate from
production and the selected source bridge. `info --json --out FILE` and
`repo-usage --out FILE` keep the sizes and the object count of the rehearsal
prefix for the storage and request budget. Keep a `docker save` file of the
image off the host: the pgBackRest package is pinned to one exact version
that the package repository will stop offering.

`KOMIKA_PG_ALLOWED_CIDRS` defaults to `172.21.0.0/16,fd00:21::/64`; match it to
your bridge. Only strict RFC1918/IPv6 ULA subnets are allowed. Application access
uses TCP SCRAM; only postgres can use the Unix socket. Readiness stays false
through recovery.

The scheduler runs a full Sunday 02:00 UTC, a check daily 02:00 UTC, and an
incremental daily 03:00 UTC. It retains two full chains (roughly 7–14 days of
history, not a guaranteed fourteen days). It catches up the latest missed slot,
and records attempts before execution so restart cannot duplicate a slot.
It recomputes due slots after each job, including jobs that cross 03:00 UTC.
Interrupted/failed jobs require an explicit retry or wait for the next slot.
Health fails on incremental/check age >26 hours, full age >8 days, or a job error.

WAL switches every five minutes while active. The 1 GiB archive queue sacrifices
PITR continuity if uploads fall behind; it is not a hard disk bound. Alert on
pgBackRest dropped-WAL warnings, archiver lag/failures, free disk and backup health.
After dropped WAL, repair archiving, take a new full, and repeat the restore drill.
The spike proved synthetic restores; real WAL/day, R2 peak size/request counts,
whole-host memory and restore duration still need Phase 6 measurement.

Credentials enter only PostgreSQL and backup container environments. Startup
writes a postgres-owned 0600 config on container-private tmpfs and removes R2
variables before launching PostgreSQL. Host root, Docker administrators and the
container's root/postgres user can still read `.env`, Docker environment metadata,
the config or process memory. Keep `.env` mode 0600, use a bucket-scoped token,
and rotate it by recreating both database containers. No secrets are in images
or command arguments.

Offline checks: `sh server-entrypoint.test.sh` and `sh postgres/tests.sh`.
After building, `sh image-tests.sh` also runs the owned PostgreSQL image checks
against the files inside the image (the kit is mounted only for its tests).
Restore/latest/PITR commands are in the runbook.

## Notes for a real (internet-facing) deploy

- **TLS + reverse proxy.** Put Caddy/Traefik/nginx (or Cloudflare) in front and terminate
  TLS. Ideally serve everything under one domain and route: `/` → reader,
  `/graphql`+`/health` → server, and the image routes → Suwayomi. Then set `PUBLIC_HOST`
  to that domain and `SUWAYOMI_PUBLIC_URL` to the image path.
- **Lock down Suwayomi.** Its API/UI on `:4567` is unauthenticated. For direct-mode images
  you must expose the image routes (`/api/v1/manga/*/thumbnail`, `.../chapter/*/page/*`),
  but you should **not** expose the rest of the Suwayomi admin API publicly — restrict it
  at the reverse proxy (allow only the image paths), or move to the Cloudflare Worker image
  path (`apps/worker`, `PUBLIC_KOMIKA_IMG_MODE=proxy`) which caches and hides Suwayomi.
- **FlareSolverr cost.** It runs a real headless Chromium — hundreds of MB RAM, spiky CPU,
  and it breaks when Cloudflare updates. It's only needed for CF-protected sources
  (MangaDex is an open API and doesn't use it). Drop the service if you only run API-based
  sources.
- **Data durability.** Continuous backup is built in — see **Backups & restore** above;
  set all `KOMIKA_R2_*` vars and bootstrap the stanza/full backup.
  The PostgreSQL volume holds password hashes, tokens and social data; keep R2 private.
- **Secrets.** Nothing sensitive in the image or compose file; `deploy/.env` is
  git-ignored. Use your platform's secret store for `KOMIKA_ADMIN_PASSWORD` and any keys.

## Build images individually

```sh
# from repo root
docker build -t komika-postgres:18.6-pgbackrest-2.59 deploy/postgres
docker build -f deploy/server.Dockerfile -t komika-server .
docker build -f deploy/reader.Dockerfile -t komika-reader \
  --build-arg PUBLIC_KOMIKA_API=https://komika.example.com/graphql .
```

The self-host reader Docker build generates hashes of its final static bootstrap
scripts into the nginx CSP template (`deploy/static-csp.mjs`). Deploy the generated
template with its matching HTML; copying `deploy/nginx.conf` directly omits the
build step. Script policy allows same-origin external scripts and these exact
hashes; inline style attributes still require `unsafe-inline`. Runtime image/API
origins remain configured through the existing `CSP_*` variables.
