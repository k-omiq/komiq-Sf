---
title: Fix server problems
description: Fix setup, container and source problems on the komiq server you host with Docker.
---

Use this page when the server you run with the admin app’s Docker installation has a setup, container or source problem. Check your own server’s logs and settings for each symptom below. For problems using the reader app after it connects, read [Reader troubleshooting](/guide/troubleshooting).

## The installer stops with "problem:"

`bash deploy/selfhost/install.sh` prints one line that says what to fix:

- "Port 8080 is in use. Try --port 8081.": another program has the HTTP port. Run the installer again with the port it names.
- "Port 443 is in use. Free it before continuing.": another program, often a web server, holds a port komiq needs. Stop it, or choose `tunnel`.
- "Could not check ports.": install `ss`, `lsof` or `netstat`.
- "Server did not become healthy.": the installer waited 5 minutes. Read the server log, as described below.

## No setup code in the server log

Running `docker compose logs server | grep "setup code"` returns no matching lines.

The server issues a setup code only while no administrator account exists. After setup it prints none. If the container failed before the application started, the log is empty.

If you already created an owner account, open `http://192.168.1.20:8080/admin` and sign in.

If this is a fresh installation, inspect the full container log:

```sh
docker compose logs server
```

Check for initialization errors, such as missing directory permissions or an invalid environment variable.

## Setup displays "Invalid setup code"

The server makes a new setup code each time it starts, and the old one stops working. Retrieve the active code from the running container:

```sh
docker compose logs server | grep "setup code" | tail -n 1
```

Type the code shown in the last log line. The server ignores spaces, dashes and letter case.

## Setup displays "Too many setup attempts"

The server limits setup attempts per client IP address. Wait for the time the message names, confirm the code in the server log, and submit the form again.

## The Owner step cannot check the connection

The step shows "komiq could not check this connection." when your server did not say whether the connection is safe for a password. Click **Try again**. The form appears only after the check answers.

## Server container is not healthy

Running `docker compose ps` shows the `server` container in an `unhealthy` state.

The container health check queries `http://localhost:8080/health/ready` every 30 seconds. It tests that the server answers and that the database accepts queries.

Inspect the server logs for the cause:

```sh
docker compose logs server
```

If the log shows an address binding error, another service on your host is using port 8080. Open `.env`, set `KOMIQ_PORT` to an unused port number (such as `8081`), and restart with `docker compose up -d`.

If the log shows database permission errors, check file ownership in the data directory. The server container runs as user ID `10001`. Ensure the `komiq_server-data` volume permits write access to that user.

## Port 443 or 80 does not answer

With Docker, your server answers on these ports inside the container, and Docker publishes them only when `COMPOSE_FILE` in `.env` names the extra Compose files. Run the installer with `--access public-ip` or `--access direct`, or read [Reach it from the internet](/self-host/internet-access). Then check the firewall on the machine and at your cloud provider.

If **Settings › Access** shows HTTPS as "Not running", it names the reason, and the server tries again by itself.

## The console stops answering at its public address

With **Only my devices**, the server refuses plain HTTP from the internet. Open the console at the server's home network address, or run `docker compose exec server komika-server setup` and choose a public mode.

## Cloudflare setup problems

If the checklist fails at "Get the connector (cloudflared)", your server could not download `cloudflared`, the tunnel's connector program. Check that your server can reach GitHub, then click **Retry**. Or install `cloudflared` yourself and set `CLOUDFLARED_BIN` to its path.

"This token may lack: …" after **Check token** is advice. You can still run setup. If a step fails, add the named permission to the token in Cloudflare and retry.

## Source engine container fails or stays in starting

Running `docker compose ps` shows the `suwayomi` container remaining in a `starting` status or marked `unhealthy`.

On first start, the Suwayomi engine unpacks assets and creates its initial database structure. This initialization takes one to two minutes.

Inspect the engine logs:

```sh
docker compose logs suwayomi
```

Wait two minutes for the initial launch to finish. If the container stops repeatedly, check that the host has enough memory.

## The catalogue is empty

A fresh installation has no sources. Add an extension repo and install extensions to fill your catalogue.

To add sources:

1. Open `http://192.168.1.20:8080/admin` and log in as the owner.
2. To add an extension repository, open **Settings › Sources**, scroll to **Extension repos**, enter the repository index address, and click **Add repo**.
3. Navigate to **Sources › Extensions** to install extensions from your added repository. Read [Add sources](/self-host/sources) for details.
4. If you read from MangaDex, open **Settings › Sources** and toggle the **MangaDex** switch.

If adding a repository fails, check `docker compose ps` to ensure the `suwayomi` container is `healthy`. The server cannot install extensions while the engine is still booting.

## Covers or chapter pages do not load

Cover thumbnails appear blank, or chapter pages fail to load in the reader.

This issue occurs if the source engine is stopped, an extension is down, or the upstream source website uses Cloudflare bot protection.

First, check that both containers are running:

```sh
docker compose ps
```

In the admin console, navigate to **Sources › Health** and inspect the status of the affected extension.

If the upstream source website requires solving a Cloudflare "checking your browser" challenge, enable FlareSolverr. In `.env`, set:

```sh
COMPOSE_PROFILES=flaresolverr
FLARESOLVERR_ENABLED=true
```

Apply the changes by running `docker compose up -d`. FlareSolverr runs a headless browser that solves challenge pages for the engine.

## Build fails or the machine runs out of memory

Running `docker compose up -d` fails during the container build stage with compiler errors or process termination.

Compiling the server requires approximately 4 GB of RAM. The installer warns below that. On a small virtual private server, Linux stops the compiler when memory runs out.

You can resolve this issue in two ways:

- Configure a swap file on your server to provide additional temporary memory. A 2 GB to 4 GB swap file allows low-memory servers to finish compiling.
- Build the image on another computer with sufficient RAM, then transfer the image to your server:

```sh
docker save komiq-selfhost:local | ssh <server> docker load
```

After loading the image onto the server, run `docker compose up -d` without the `--build` flag.

## Disk space runs low after updates

The host system runs low on available disk space after updating komiq.

Docker preserves earlier container images, intermediate build steps, and compiler caches on disk.

To free storage space, remove unused Docker images and build caches:

```sh
docker image prune
docker builder prune
```
