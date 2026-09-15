---
title: Reach it from the internet
description: Reach your own server from the reader app over the internet, with a domain and Cloudflare or without a domain.
---

Host the admin app on your computer, home server or VPS, and connect the reader over the internet. This page covers your domain with Cloudflare in front, and the way [without a domain](#without-a-domain).

Configure access in [First-run setup](/self-host/first-run), on **Internet**, or in **Settings › Access**.

## Your domain and the reader app

Use your own domain, such as `komiq.example.com`. DNS, the Domain Name System, routes readers to your server. Cloudflare provides HTTPS, encrypted web traffic, and edge caching, image copies near readers.

In the reader app’s **Connect to a server** screen, type `komiq.example.com` or scan the Quick Response (QR) code in **Settings › Access**.

You do three things once at Cloudflare: have a free account, add your domain and point its nameservers at Cloudflare, and create an API token. Komiq does the rest with that token.

## Tunnel versus Public IP

| Feature      | Cloudflare Tunnel (default)             | Public IP with DNS            |
| ------------ | --------------------------------------- | ----------------------------- |
| Router ports | No incoming ports needed                | Port 443 open and forwarded   |
| Public IP    | Works with a changing or shared address | A public IPv4 or IPv6 address |

Under the connection choice, the form says what your server found: whether the connector is there or will be downloaded (tunnel), or whether port 443 is free (public IP).

### Cloudflare Tunnel

A tunnel suits home servers. Your server runs `cloudflared`, Cloudflare's connector program, which connects out to Cloudflare.

You do not install `cloudflared`. The Docker image includes it. Elsewhere, your server looks at `CLOUDFLARED_BIN`, then its `PATH`, then its own tool folder. If it finds none, setup downloads a fixed release, checks its SHA-256 checksum, a fingerprint of the file, and saves it there. Builds exist for Linux and macOS on x86-64 and ARM64, and for Windows on x86-64.

With Docker, keep `KOMIQ_BIND=127.0.0.1` so the plain HTTP port is closed to your network. The installer writes that for `tunnel`. Read [Install with Docker](/self-host/docker).

### Public IP with DNS

Komiq creates a proxied DNS record (A or AAAA) for your server's public IP address. It requests a 15-year Cloudflare Origin Certificate Authority (CA) certificate and serves HTTPS on port 443.

With Docker, port 443 must be published too. Run the installer with `--access public-ip`. Or add this line to `deploy/selfhost/.env` and run `docker compose up -d`:

```sh
COMPOSE_FILE=docker-compose.yml:docker-compose.https.yml
```

## The Cloudflare API token

An application programming interface (API) token lets your server configure Cloudflare. The server uses it for one run and never saves or logs it.

### Create the token

**Open the token page** in the form opens a new Cloudflare token named `komiq`, with some permissions filled in. Add the others on the same page:

| Scope   | Permission group                      | Access level | Filled in |
| ------- | ------------------------------------- | ------------ | --------- |
| Account | Cloudflare Tunnel (Tunnel only)       | Edit         | No        |
| Zone    | SSL and Certificates (Public IP only) | Edit         | Yes       |
| Zone    | Zone                                  | Read         | Yes       |
| Zone    | DNS                                   | Edit         | Yes       |
| Zone    | Zone Settings                         | Edit         | Yes       |
| Zone    | Cache Rules                           | Edit         | No        |
| Account | Account Rulesets                      | Edit         | No        |
| Account | Account Filter Lists                  | Edit         | No        |

Limit **Zone Resources** to your domain and **Account Resources** to your account. Create the token and paste it into the form.

**Check token** verifies the token, finds your domain's zone and lists permissions the token may lack. The list is advice: you can still run setup.

## Steps in the admin console

1. Open **Settings › Access** and click **Set up Cloudflare**, or use the **Internet** setup step.
2. Enter your domain, choose **Cloudflare Tunnel (recommended)** or **Public IP**, paste your API token and click **Set up**.

### What the server configures

A checklist shows each line as it runs:

1. Checks the token and finds your domain's zone.
2. For Tunnel: gets the connector, creates the tunnel and starts it. For Public IP: creates the certificate, serves HTTPS and sets SSL to Full (strict).
3. Sets the trusted proxy ranges, so rate limits see real visitor addresses.
4. Creates the DNS record and the cache rules for covers, page images and static files.
5. Tells Cloudflare to respect your server's cache headers, then tests `https://<domain>/health`.

## What the server stores

The database keeps the IDs of what setup created, and no token or private key. Credential files and a downloaded `cloudflared` sit in `cloudflare/` next to the database.

## Manual configuration

**I will do it by hand** in the form shows the steps for the Cloudflare dashboard instead.

## Optional image proxy Worker

The optional image proxy in `apps/worker` runs on Cloudflare’s network. The bundled web reader and a native reader connected to your server do not need it. Read [Image proxy](/developers/image-proxy).

## Remove or retry setup

If a step fails, click **Retry**. **Run setup again** also repairs the DNS record after your server's address changed.

To pause, choose **Only my devices** in **Settings › Access**. The tunnel, or HTTPS on your server, is turned off. What setup made at Cloudflare stays, and **Turn on** brings it back with no token. To undo setup, click **Remove** and enter your API token again. The server removes what setup created and restores the previous zone settings.

If your server fails to connect, read [Fix server problems](/self-host/troubleshooting).

Next: [Settings reference](/self-host/configuration).

## Without a domain

If your server has a public address, such as a cloud VPS, readers can connect straight to it, with no domain and no Cloudflare account. The connection is HTTPS: komiq gets a free certificate for the IP address from Let's Encrypt, a public certificate authority, and renews it by itself. Using it means you agree to the [Let's Encrypt Subscriber Agreement](https://letsencrypt.org/repository/).

1. Open ports 80 and 443 in your cloud firewall. With Docker, publish them too: run the installer with `--access direct`, or set `COMPOSE_FILE=docker-compose.yml:docker-compose.https.yml:docker-compose.acme.yml` in `.env` and run `docker compose up -d`.
2. In [First-run setup](/self-host/first-run), choose **Public without a domain** on **Access**. Later, use **Use without a domain** in **Settings › Access**.
3. Enter your server's public IP address, such as `203.0.113.5`. The form says whether ports 80 and 443 are free. Readers then use `https://203.0.113.5`.

Keep port 80 open: Let's Encrypt checks it before each certificate. **Settings › Access** shows the certificate's end date and any renewal error.

After the first visit from the internet over HTTPS, komiq stops handling internet requests on its plain HTTP port, `KOMIQ_PORT`. It also does not create the owner account over plain HTTP from the internet: the **Owner** step shows no form there.

> [!NOTE]
> With Docker Desktop or rootless Docker, every connection looks local. There the server goes by the address you typed: a public address counts as the internet.

You can switch **Encrypt the connection** off. Readers then use plain HTTP, such as `http://203.0.113.5:8080`, and people on the network path can see passwords and pages. With Docker, run the installer with `--access direct --encrypt no` so the HTTP port answers every address.
