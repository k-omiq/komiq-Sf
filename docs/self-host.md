# Self-host komiq

Run your own komiq server with Docker. One address serves everything: the reader at
`/`, the admin console at `/admin`, the API and the images. A fresh install has no
accounts and no sources; the setup screens walk you through both.

This guide covers a home server or a VPS. To run komiq on your own computer without
Docker, use the desktop app instead.

## Requirements

- Linux, macOS or Windows with Docker Engine 24+ and Docker Compose v2
  (`docker compose version`).
- 2 GB of memory to run, 4 GB to build the image. The first build takes 15 to 30
  minutes; most of it is compiling the server.
- About 3 GB of disk for the images, plus room for your library.
- A copy of this repository (`git clone`, or a downloaded release archive).

## Install

```sh
cd deploy/selfhost
docker compose up -d
```

This builds the komiq image, pulls the source engine (Suwayomi), and starts both.
Check that they are up:

```sh
docker compose ps
```

`server` should be `healthy` within a minute of the build finishing. The engine takes
a minute longer on its first start.

To change the port or who can reach it, copy the settings file first, edit it, then run
`docker compose up -d` again:

```sh
cp .env.example .env
```

| Setting      | Default   | What it does                                                                   |
| ------------ | --------- | ------------------------------------------------------------------------------ |
| `KOMIQ_PORT` | `8080`    | Port komiq answers on.                                                         |
| `KOMIQ_BIND` | `0.0.0.0` | `0.0.0.0`: every device on your network. `127.0.0.1`: this machine only.       |
| `TZ`         | `UTC`     | Time zone for logs.                                                            |
| FlareSolverr | off       | For sources behind "checking your browser" pages. See the comment in the file. |

## Set up

1. Find the setup code. It is printed in the server log at every start until setup is
   done; use the last one:

   ```sh
   docker compose logs server | grep "setup code" | tail -n 1
   ```

2. Open `http://<server address>:8080/admin/setup` in a browser. On the server itself
   that is `http://localhost:8080/admin/setup`.
3. Follow the steps: create the owner account (this is where the setup code goes),
   choose who can sign up, and add sources if you want to now.
4. Open `http://<server address>:8080/` and sign in to read.

The setup code works only until the owner account exists. Restarting the server before
that prints a new one.

## Use it from other devices

With the default `KOMIQ_BIND=0.0.0.0`, any device on the same network opens komiq at
`http://<server address>:8080`. Find the address with `hostname -I` (Linux),
`ipconfig getifaddr en0` (macOS) or `ipconfig` (Windows).

If a phone cannot connect:

- Allow the port in the server's firewall, for example `sudo ufw allow 8080/tcp`.
- Check the phone is on the same network, not on mobile data or a guest network.

The native reader apps ask for the server address on their "Connect to a server"
screen. Enter the same address, for example `192.168.1.20:8080`.

Plain `http://` is fine at home. Do not open the port to the internet: use the next
section instead.

## Reach it from the internet

Internet access runs through Cloudflare, set up from the admin console: the setup
wizard's Access step, or later **Settings › Access**. It supports a Cloudflare Tunnel
(nothing to open on your router or VPS) and a public IP with DNS. The details are in
[self-host-cloudflare.md](./self-host-cloudflare.md).

- **Tunnel:** the image includes `cloudflared`, which komiq starts for you. Set
  `KOMIQ_BIND=127.0.0.1` afterwards so the plain-HTTP port is closed to the network.
- **Public IP:** komiq serves HTTPS on port 443 inside the container. Uncomment the
  `443:443` line under `ports:` in `docker-compose.yml` and run `docker compose up -d`.

## Update

```sh
git pull
cd deploy/selfhost
docker compose up -d --build
```

Your data stays in the Docker volumes. Updates that change the database migrate it at
start. Take a backup first.

## Back up and restore

Everything komiq keeps is in two Docker volumes:

| Volume              | Contents                                                    |
| ------------------- | ----------------------------------------------------------- |
| `komiq_server-data` | Accounts, libraries, comments, settings, covers, Cloudflare |
| `komiq_engine-data` | Installed extensions and the engine's own data              |

Back up by stopping komiq and copying the volumes into a folder:

```sh
docker compose stop
docker run --rm -v komiq_server-data:/data -v "$PWD":/backup busybox \
  tar czf /backup/komiq-server-data.tgz -C /data .
docker run --rm -v komiq_engine-data:/data -v "$PWD":/backup busybox \
  tar czf /backup/komiq-engine-data.tgz -C /data .
docker compose start
```

Restore onto a fresh install (before its first start, or after `docker compose down`):

```sh
docker compose create
docker run --rm -v komiq_server-data:/data -v "$PWD":/backup busybox \
  sh -c 'tar xzf /backup/komiq-server-data.tgz -C /data && chown -R 10001:10001 /data'
docker run --rm -v komiq_engine-data:/data -v "$PWD":/backup busybox \
  sh -c 'tar xzf /backup/komiq-engine-data.tgz -C /data && chown -R 1000:1000 /data'
docker compose up -d
```

Stop komiq before copying: the database is written all the time, and a copy taken while
it runs may not open. Timed backups are planned for a later release.

## Remove

```sh
docker compose down        # stop, keep your data
docker compose down -v     # stop and delete all data
```

## Troubleshooting

**No setup code in the log.** The server prints it only while no owner exists. If setup
was finished, sign in at `/admin` instead. If the log is empty, the server did not start:
`docker compose logs server`.

**"Invalid setup code".** The code changes at every restart. Take the last one from the
log and type it with its dashes.

**`server` is not healthy.** `docker compose logs server` shows why. A port already in
use shows as an error from `docker compose up`; pick another `KOMIQ_PORT`.

**The catalogue is empty.** A new install has no sources. Add an extension repo and
install a source in the admin console under **Sources**. If adding fails, the engine may
still be starting: `docker compose ps` shows `suwayomi` as `healthy` once it is ready.

**Covers or pages do not load.** Check the engine is healthy, then open the source in the
admin console and look at its health. Sources behind Cloudflare challenges need
FlareSolverr (see `.env.example`).

**The build fails or the machine runs out of memory.** Building needs about 4 GB. On a
small VPS add swap, or build on another machine and copy the image over with
`docker save komiq-selfhost:local | ssh <server> docker load`.

**Free disk space after updates.** Old images and build cache stay behind:
`docker image prune` and `docker builder prune`.
