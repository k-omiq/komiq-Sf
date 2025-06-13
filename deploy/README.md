# Komika deploy

Stand up the **full public stack** — a hosted Mihon/Tachiyomi with a public social
layer (ratings, reviews, per-chapter comments), an auto-served catalog, and an
**adaptive scanner that keeps every library series updated** so users never have to
manually refresh.

## One command

```sh
cd deploy
cp .env.example .env      # edit: PUBLIC_HOST, KOMIKA_ADMIN_PASSWORD, SEED_SERIES…
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
firewall's `filter/INPUT` chain entirely, so a `0.0.0.0` publish here is *not* covered
by your host firewall rules — and it is reachable from containers in unrelated
compose stacks on the same machine, via any bridge. Put a reverse proxy or a
Cloudflare Tunnel in front (see `CLOUDFLARE-RUNBOOK.md`) rather than widening these.

**The `reader` container is opt-in.** It sits behind the compose profile `selfhost`,
because in production the reader is a Cloudflare Worker and starting the nginx SPA
would publish an unused world-bound port. To run it:

```sh
./deploy.sh up --with-reader     # or: export COMPOSE_PROFILES=selfhost
```

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
(suwayomi 8g / server 6g / flaresolverr 2g, of a 23.4 GiB host) so a runaway can't
make the kernel pick an OOM victim from an unrelated stack, and `json-file` logging
capped at `20m × 5` so container logs can't fill the disk. `/etc/docker/daemon.json`
carries the same log cap as a **daemon-wide default** for containers started by other
stacks — but daemon.json is only read at Docker-daemon start, so it takes effect at
the next daemon restart, not at the next `docker compose up`.

## The four services

| Service          | What it does                                                                                                                                                                                     |
| ---------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **suwayomi**     | Runs the source extensions (MangaDex, …): catalog, chapters, **and page/cover images**.                                                                                                          |
| **flaresolverr** | Headless-browser sidecar that solves Cloudflare "checking your browser" challenges. Internal-only; Suwayomi calls it at `http://flaresolverr:8191`.                                              |
| **server**       | `komika-server` — federates Suwayomi + adds accounts/social/admin, and runs the **adaptive scan scheduler** (auto-updates the library on a cadence derived from each series' chapter frequency). |
| **reader**       | Static SvelteKit SPA (nginx), pointed at the server. **Opt-in** — compose profile `selfhost`; in production the reader is a Cloudflare Worker instead.                                            |

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
- **Persistence:** the SQLite DB (accounts, reviews, comments, admin overrides) lives on
  the `server-data` volume at `/data`; Suwayomi's library + downloaded images on
  `suwayomi-data`. Both survive `down`; `destroy` deletes them.

## Auto-updating catalog

`SEED_SERIES` in `.env` (comma-separated titles) is added to the library on bootstrap.
The scanner (`SCAN_TICK_SECONDS`, default 300) then re-checks each library series on a
cadence derived from its average time-between-chapters, refetches overdue ones from the
source, and records new chapters — so the Updates/Library screens stay current with no
user action. Admins can add/adjust more series and per-series scan policy in the
**admin "manga DB" console**.

## Backups & restore (Litestream)

The server DB (`/data/komika.sqlite3` — accounts, sessions, reviews, comments,
admin overrides, scan-state) is backed up with **Litestream**: it streams the SQLite
WAL to an S3-compatible bucket continuously (point-in-time restore, ~seconds RPO) and
**restores the latest snapshot automatically** when the server starts on a fresh volume.

- **Off by default.** Leave the `LITESTREAM_*` vars blank in `.env` and the server runs
  without backup (fine for local/testing). Set them to enable it — no code change.
- **Enable it:** create a **Cloudflare R2** bucket (S3-compatible) + an API token with
  Object Read & Write, then fill `LITESTREAM_BUCKET` / `LITESTREAM_ENDPOINT` /
  `LITESTREAM_REGION` / `LITESTREAM_ACCESS_KEY_ID` / `LITESTREAM_SECRET_ACCESS_KEY` in
  `deploy/.env` and `./deploy.sh` (or `docker compose up -d --build server`). For R2 use
  `LITESTREAM_ENDPOINT=https://<accountid>.r2.cloudflarestorage.com` and
  `LITESTREAM_REGION=auto`. WAL mode is already on in the DB, and the server runs under
  `litestream replicate -exec` inside its container. (Any S3-compatible store works — the
  vars are storage-agnostic — but R2 is the reference target.)
- **Disaster recovery** is automatic: on a brand-new host/volume with the vars set, the
  server pulls the newest snapshot before it starts. To restore manually to a file:
  ```sh
  docker compose exec server litestream restore -o /tmp/komika.sqlite3 /data/komika.sqlite3
  ```
- Litestream config: `deploy/litestream.yml`; the container entrypoint is
  `deploy/server-entrypoint.sh` (runs the server plain when Litestream is unconfigured).

### Verify backup → restore locally (tested procedure)

Don't trust a backup you've never restored. This exercises the **real** path — the
server image's bundled Litestream + `server-entrypoint.sh` + `litestream.yml` —
against a local **MinIO** standing in for R2 (both are S3-compatible). Verified on
2026-07-11: a user registered before a full volume wipe was recovered intact.

```sh
# 0. A MinIO container as an R2/S3 stand-in, on a docker network, + a bucket.
docker network create lstest-net
docker run -d --name lstest-minio --network lstest-net -p 9000:9000 \
  -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin123 \
  minio/minio:latest server /data
docker run --rm --network lstest-net --entrypoint sh minio/mc:latest -c \
  "mc alias set l http://lstest-minio:9000 minioadmin minioadmin123 && mc mb -p l/komika-backup"

# Reusable env pointing Litestream at MinIO (R2 in prod: endpoint=https://<acct>.r2…, region=auto).
LS="-e LITESTREAM_BUCKET=komika-backup -e LITESTREAM_ENDPOINT=http://lstest-minio:9000 \
 -e LITESTREAM_REGION=us-east-1 -e LITESTREAM_ACCESS_KEY_ID=minioadmin \
 -e LITESTREAM_SECRET_ACCESS_KEY=minioadmin123 -e SUWAYOMI_URL=http://localhost:9999 \
 -e DATABASE_URL=sqlite:///data/komika.sqlite3 -e KOMIKA_ADMIN_USERS=admin"

# 1. Boot the server under Litestream on a fresh volume, write data, let it replicate.
docker build -f deploy/server.Dockerfile -t komika-server .   # DOCKER_BUILDKIT=0 on old buildx
docker run -d --name s1 --network lstest-net -v lstest-data:/data -p 8791:8080 $LS komika-server
#   → docker logs s1 should show: [entrypoint] Litestream backup ENABLED
curl -s localhost:8791/graphql -H 'content-type: application/json' --data \
  '{"query":"mutation{register(input:{username:\"u\",email:\"u@x.dev\",password:\"pw12345\"}){token}}"}'
sleep 4    # let the WAL replicate; `mc ls -r l/komika-backup` now shows a snapshot + WAL

# 2. Graceful stop (flushes a final snapshot), then DESTROY the volume.
docker stop s1 && docker rm s1 && docker volume rm lstest-data

# 3. Boot fresh: the entrypoint restores from MinIO before the server starts.
docker run -d --name s2 --network lstest-net -v lstest-data:/data -p 8791:8080 $LS komika-server
#   → docker logs s2 shows: msg="restoring snapshot" … then "restoring wal files"

# 4. Confirm the data came back: login must succeed (proves the user row survived the wipe).
curl -s localhost:8791/graphql -H 'content-type: application/json' --data \
  '{"query":"mutation{login(username:\"u\",password:\"pw12345\"){user{username}}}"}'
#   → {"data":{"login":{"user":{"username":"u"}}}}

# Cleanup.
docker rm -f s2 lstest-minio && docker volume rm lstest-data && docker network rm lstest-net
```

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
  just set the `LITESTREAM_*` vars. (The `server-data` volume holds argon2 password hashes
  - session tokens + all social data, so keep the bucket private.)
- **Secrets.** Nothing sensitive in the image or compose file; `deploy/.env` is
  git-ignored. Use your platform's secret store for `KOMIKA_ADMIN_PASSWORD` and any keys.

## Build images individually

```sh
# from repo root
docker build -f deploy/server.Dockerfile -t komika-server .
docker build -f deploy/reader.Dockerfile -t komika-reader \
  --build-arg PUBLIC_KOMIKA_API=https://komika.example.com/graphql .
```
