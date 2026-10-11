---
title: Run it on your computer
description: Host your own komiq server on your computer with the desktop admin app.
---

Host the komiq admin app on your own desktop or laptop without Docker. The admin app is your server and its management portal, with the source engine and web reader bundled. Read on this computer, or turn on "Allow other devices" for your home network.

## What the desktop app does

The desktop admin app uses Tauri, a framework for native apps, to package these components:

- The admin console runs inside a native desktop window.
- Bundled PostgreSQL runs the accounts, settings, catalogue and library database. It accepts connections from this computer only. You install no separate database.
- The `komika-server` binary runs as a sidecar, a separate process next to the app. It serves the web reader at `/`, the admin console at `/admin`, the API and the image proxy.
- The Suwayomi source engine runs with a bundled Java runtime. It executes extensions and fetches chapter data.
- An on-device challenge solver uses a hidden web view to solve browser checks and returns cookies to the engine.

The app starts and stops its database, server and engine with your session. Keep it open while other devices use your server.

Your data stays in the application data folder under `app.komika.desktop`. PostgreSQL holds accounts, settings and library history in `server/postgres/18/data`. Its passwords are in `server/postgres/18/credentials`: without that file PostgreSQL cannot open its data, and the app never makes new passwords for an existing database. Covers stay in a separate SQLite cache, `server/covers.sqlite3`. `suwayomi/` holds installed extensions and engine data.

When you update an old SQLite installation, the first start converts `server/komika.sqlite3`. The start screen shows "Checking the old database…", "Converting the database…" with the current step, then "Database converted. Starting…". The server verifies the copy before serving, then renames the old file to `komika.sqlite3.imported-<UTC time>`. It never deletes that file or a kept copy, `komika.sqlite3.import-kept-<UTC time>`. [Update and back up](/self-host/update-and-backup) explains the two warnings about kept copies. The start screen lists warnings as notes, kept-copy ones first, and waits for **Continue**.

A conversion with no progress for 30 minutes, or longer than 6 hours, stops; **Retry** continues it. Do not close the app meanwhile. It needs free space of about 8 times the old database plus 256 MiB, in the folder that holds it. When the server refuses to convert, the screen shows its reason and the exact renames for files in `server/`: see [Fix server problems](/self-host/troubleshooting).

The app saves a PostgreSQL dump, a consistent database export without the cover cache, when none exists or the newest is at least 20 hours old. Dumps are `server/backups/komika-<UTC time>-<random>.dump`; the seven newest stay. The app has no restore button, and this guide has no restore steps for it yet. Keep dump copies elsewhere too.

To build from source, read [Desktop](/developers/desktop).

## Server selection on the start screen

The start screen has two options:

- **Run komiq on this computer**: Starts the bundled source engine and server on your machine.
- **Connect to a server I already run**: Opens the management portal for a komiq server you already run.

### Run komiq on this computer

Choose "Run komiq on this computer" to start your own server:

```flow
Desktop app -> PostgreSQL: starts the bundled local database
Desktop app -> Source engine: starts background process
Source engine -> Desktop app: reports ready state
Desktop app -> Server process: starts server sidecar
Server process -> Desktop app: reports health and setup code
Desktop app -> Setup wizard: loads first-run setup
```

By default, the server binds to the loopback address (`127.0.0.1`) on port `7580`. If another program holds that port, the app tries the next ones. It saves the chosen port in `server/desktop.json` so bookmarks keep working.

The server makes a one-time setup code on its first start. The app captures it from process output in memory and hands it to the setup wizard, so you read no logs. Once the server answers its health check, the app moves to [First-run setup](/self-host/first-run).

### Connect to a server you already run

Enter your server address, such as `192.168.1.20:8080` or `https://komiq.example.com`. The app checks the `/health` endpoint and queries the GraphQL API to verify compatibility, saves the address and opens the sign-in screen or first-run setup.

## Allow other devices

The server first listens only on `127.0.0.1`, which other devices cannot reach. To let phones, tablets or other computers connect:

1. In [First-run setup](/self-host/first-run), continue from **Access** to the **Home network** step.
2. Turn on the "Allow other devices" switch.

The same switch is in **Settings › Access**, in every access mode.

The app then restarts the server bound to `0.0.0.0` and shows the network addresses other devices can open, such as `http://192.168.1.20:7580`, with a Quick Response (QR) code. Read [Connect your devices](/self-host/other-devices).

The step waits and shows "A device on your network reached komiq." when one connects. If none does, open **A device cannot connect?** on the same step. It covers Wi-Fi and the firewall, the program that blocks incoming connections.

## Put it on the internet

The desktop app's server can use a domain of yours through Cloudflare. Choose **Public on the internet** on the **Access** step. The app uses a Cloudflare Tunnel, which needs no open ports, and downloads `cloudflared` when this computer has none. Readers reach it only while the app is open. For a public IP, or without a domain, run komiq on a server. Read [Reach it from the internet](/self-host/internet-access).

For this installation, continue with [First-run setup](/self-host/first-run), then [Add sources](/self-host/sources).

## If the database does not start

The start screen names the cause. Your data stays. Choose **Retry** after you fix it:

- **An earlier database process is still running.** A force quit, a crash, or renaming the data folder or moving the app while the database ran can leave one. On macOS and Linux the app recognises its own earlier process and asks it to stop. If it does not stop, the screen names it, tells you to wait a minute and gives a `kill -QUIT` command. The app stops no process that is not its own database.
- **The app has administrator rights.** PostgreSQL does not run with them. If you started the app with "Run as administrator", or this Windows account always has them, the screen says so: close the app and start it from an ordinary account. On macOS and Linux the same applies to root.
- **A database from another PostgreSQL version.** The app runs PostgreSQL 18 and has no upgrade step. The screen says the data is kept and no empty database was made; you need a newer app version.

## Status

An automated check of the database handling is set up for Apple-silicon macOS, x86_64 Linux and x86_64 Windows. Intel Macs are not part of it, and the real installers are not yet verified. Conversion duration, installed size and dump duration have not been measured for this release.

Next: [Install with Docker](/self-host/docker).
