---
title: Choose your setup
description: Choose a server to read on, a way to run the admin app, and how your devices will reach it.
---

Komiq has two apps: the admin app runs and manages a server, and the reader app connects to it. You can read on the global server or self-host the admin app. This page helps you choose where to run it and which devices can reach it.

## Only read

Open the global server in a browser if you want to try komiq without running a server. The global server at `komiq.cc` is the project's free, work-in-progress demo.

You can open its web reader in a modern browser on a computer, tablet, or phone. You do not need to install software, run commands, or configure hardware. For now, the native reader apps cannot connect to the global server: its API accepts requests from the `komiq.cc` website alone. The project may open it to the apps later, if enough people ask for it. The apps connect to a server you or someone else self-hosts. See [Get the reader app](/guide/apps) for platform status and [Connect to a server](/guide/connect) for connection steps.

The global server is a shared testing environment. You cannot add custom private sources, install unapproved extensions, or access administrative settings. Self-host the admin app if you want control over your catalog, sources, and data.

## Run the admin app on your computer

Use the admin desktop app on macOS, Windows, or Linux to run your own server without Docker. It bundles PostgreSQL, the komiq server and Suwayomi source engine as background processes. Suwayomi is the engine that connects to sources through extensions.

When you launch the admin app, it starts these components automatically. It retrieves the initial setup code from the server and opens the setup wizard with the code filled in.

Application data stays in your operating system's user application folder:

- PostgreSQL holds server data and user accounts. Cover images live in the separate `server/covers.sqlite3` cache. Old SQLite installations convert automatically at the first start after the update. The server verifies the copy first, then keeps the original file under a new name and never deletes it.
- Source engine files and installed extensions live in `suwayomi/`.

### For this computer

The server listens locally at `127.0.0.1`, on port `7580` by default. Use this option if you read on the same computer. The admin app runs the server; the reader app is what you read with.

### For every device on your network

Turn on **Allow other devices** in the admin app to read from phones, tablets, and other computers on your home network. Those devices connect to the computer running the server.

Follow [Run it on your computer](/self-host/desktop-app) and [Connect your devices](/self-host/other-devices).

## Run the admin app with Docker

Use Docker on a home server, network-attached storage (NAS), or a virtual private server (VPS) if you want a server that runs continuously in the background.

Docker Compose runs three containers:

- `postgres`: the private accounts, library and catalogue database.
- `server`: the komiq server, embedded reader, and admin console.
- `suwayomi`: the source engine that manages extensions and source connections.

Your accounts, libraries, reading history, and extensions persist across updates in three volumes: `komiq_postgres-data`, `komiq_server-data` and `komiq_engine-data`. One script, `deploy/selfhost/install.sh`, checks your machine, generates a private database password, starts PostgreSQL before the server and runs setup in the terminal. Updates from SQLite convert the old database automatically. Use PostgreSQL dumps to back up your accounts and library.

Devices on your local network can open `http://192.168.1.20:8080` in a browser or enter `192.168.1.20:8080` in the native reader app. Replace the example with your server's address. See [Install with Docker](/self-host/docker).

## Reach your server from the internet

Internet access is a choice for your own server. You can use a domain with Cloudflare in front, or your server's public address without a domain.

The admin setup wizard and **Settings › Access** support Cloudflare setup. A tunnel connects out from your server without opening ports on your router. Public-IP mode uses a domain pointing to your server through Cloudflare. The optional image proxy Worker fetches images for the web reader.

Without a domain, readers type your server's public address, such as `203.0.113.5`. This needs no Cloudflare account, and it suits a cloud VPS. The connection is HTTPS: your server gets a free certificate for the address and renews it by itself.

Follow [Reach it from the internet](/self-host/internet-access) for setup. Connect the reader app with your domain or address, a pasted link, or the admin app's QR code on a phone.

## Setup comparison

| Choice                           | Best for                        | Requirements                  | Where data lives               |
| -------------------------------- | ------------------------------- | ----------------------------- | ------------------------------ |
| Global server                    | Only reading or trying komiq    | Browser only                  | Global server                  |
| Admin desktop app, this computer | Reading on one computer         | macOS, Windows, or Linux      | User application folder        |
| Admin desktop app, local network | Reading across home devices     | **Allow other devices**       | User application folder        |
| Admin app with Docker            | An always-on home server or VPS | Docker Engine and Compose     | Persistent Docker volumes      |
| Internet access for your server  | Reading away from home          | A domain, or a public address | Your server's existing storage |

## Status

Desktop installer validation is planned. While it runs, the desktop app saves a PostgreSQL dump when the newest is at least 20 hours old and keeps seven. Keep copies outside the application data folder to protect against loss of this computer.

## Next steps

Complete [First-run setup](/self-host/first-run) to create your server's owner account. Then use [Connect to a server](/guide/connect) to connect your reader devices.
