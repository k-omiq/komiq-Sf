---
title: Install with Docker
description: Host the komiq admin app on a home server or VPS with one install command or with Docker Compose.
---

Host the komiq admin app with Docker Compose on a home server or a virtual private server (VPS), a rented server on the internet. The admin app is your server and its management portal. This page covers the installer, the manual start, health checks and settings before you connect the reader app.

## Requirements

You need:

- Linux, macOS or Windows with Docker Engine 24+ and Docker Compose v2 (`docker compose version`). Docker is a container system: it runs an application and its dependencies in an isolated environment.
- 2 GB of memory to run, and 4 GB to build the image. The first build takes 15 to 30 minutes, because it compiles the server.
- At least 3 GB of free disk space for the images, plus room for your library.
- A copy of the komiq repository: a release archive, or `git clone`.

## Install with one command

The self-hosting files are in `deploy/selfhost`. Run the installer from the repository folder:

```sh
bash deploy/selfhost/install.sh
```

The installer does these steps:

1. It checks Docker, Compose v2, the processor type (amd64 or arm64), memory and free disk space. It warns under 4 GB of memory or 5 GB of free disk. Under 2 GB of memory it stops, unless you pass `--yes`.
2. It asks for the HTTP port. The default is `8080`.
3. It asks how people reach komiq: `local`, `tunnel`, `public-ip` or `direct`. For `direct` it also asks whether to encrypt the connection with a free certificate (HTTPS). The default is yes.
4. It asks whether other devices on your home network may connect over HTTP. It skips this for `direct` without encryption.
5. It checks that the ports it needs are free, and stops with a message when one is not.
6. It writes `.env`. It copies an existing `.env` to `.env.bak` once, and changes only `KOMIQ_PORT`, `KOMIQ_BIND` and `COMPOSE_FILE`.
7. It offers to open the needed ports in the machine's firewall, the program that blocks incoming connections. This works with ufw and firewalld, two Linux firewall tools. It asks first and uses `sudo`. On macOS it changes nothing.
8. It builds and starts the containers, and waits up to 5 minutes until the server is healthy.
9. It reads the setup code from the server log and starts setup in your terminal, with your access choice. Read [First-run setup](/self-host/first-run).

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

| Option                            | What it does                                                                                                            |
| --------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| `--port PORT`                     | Sets the HTTP port. Default: `8080`, or the value in `.env`.                                                            |
| `--access MODE`                   | Sets how people reach komiq. Default: `local`.                                                                          |
| `--encrypt yes` or `--encrypt no` | With `--access direct` only: HTTPS, or plain HTTP on the HTTP port. Default: `yes`.                                     |
| `--lan yes` or `--lan no`         | Opens the HTTP port to your home network, or keeps it on this machine. Default: `yes` for `local`, `no` for the others. |
| `--firewall yes`, `no` or `ask`   | Opens the needed ports in ufw or firewalld. Default: `ask`.                                                             |
| `--yes`                           | Accepts the defaults without questions. Allows a build with under 2 GB of memory.                                       |
| `--no-setup`                      | Does not start setup in the terminal.                                                                                   |
| `--help`                          | Prints the options.                                                                                                     |

A second run takes its defaults from `.env`. Without a terminal, pass `--yes` or give every answer as an option. Setup needs a terminal: without one, the installer prints the command to run later.

On a server that already has an owner, a second run changes no setup choice, and says so. The installer's last lines print the command that changes one later: `docker compose exec server komika-server setup`.

## Start it by hand

The installer is optional. To start the same containers yourself:

```sh
cd deploy/selfhost
docker compose up -d
```

This builds the komiq image (`komiq-selfhost:local`), downloads the pinned source engine image (Suwayomi) and starts two services:

- `server`: Your komiq server and management portal. It serves the web reader at `/`, the admin console at `/admin`, the GraphQL application programming interface (API), and the image proxy on a single port.
- `suwayomi`: The source engine that runs manga extensions. It connects only to the internal Docker network and publishes no ports, because its interface has no authentication.

> [!NOTE]
> A fresh installation has no accounts and no sources. You create the owner account and choose sources during setup.

## Check container health

Check that both containers are healthy:

```sh
docker compose ps
```

- `server` reports `healthy` about a minute after the build. Its health check queries `http://localhost:8080/health/ready`.
- `suwayomi` takes one to two minutes longer on its first run, while it sets up its database. Its health check queries `http://localhost:4567/api/v1/settings/about`.

To read the server log, run:

```sh
docker compose logs server
```

Once both are healthy, go to [First-run setup](/self-host/first-run) to create the owner account. On a server with no screen, do it over SSH with `docker compose exec server komika-server setup`.

## Environment settings

Settings are optional. Without a `.env` file, komiq uses the defaults in `deploy/selfhost/docker-compose.yml`. The installer writes `.env` for you.

To change a setting by hand, copy the example file:

```sh
cp .env.example .env
```

Edit `.env`, then run `docker compose up -d` again.

| Setting                | Default   | Description                                                                                                       |
| ---------------------- | --------- | ----------------------------------------------------------------------------------------------------------------- |
| `KOMIQ_PORT`           | `8080`    | Port komiq listens on the host machine.                                                                           |
| `KOMIQ_BIND`           | `0.0.0.0` | Who can reach the port. `0.0.0.0`: every device that can reach this machine. `127.0.0.1`: this machine only.      |
| `COMPOSE_FILE`         | unset     | Extra Compose files that publish ports 443 and 80. Read [Reach it from the internet](/self-host/internet-access). |
| `TZ`                   | `UTC`     | Time zone used for container log output.                                                                          |
| `COMPOSE_PROFILES`     | empty     | Set to `flaresolverr` to start the FlareSolverr container.                                                        |
| `FLARESOLVERR_ENABLED` | `false`   | Set to `true` with `COMPOSE_PROFILES=flaresolverr` to solve Cloudflare challenges.                                |

Use `KOMIQ_BIND=127.0.0.1` on a VPS that you reach through a Cloudflare Tunnel. Keep the default `0.0.0.0` if devices on your home network connect directly. Read [Connect your devices](/self-host/other-devices).

> [!NOTE]
> Accounts, sign-up, sources and domain names are not set in `.env`. You choose them during setup or in the admin console.

Next: [First-run setup](/self-host/first-run).
