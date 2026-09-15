---
title: Run it on your computer
description: Host your own komiq server on your computer with the desktop admin app.
---

Host the komiq admin app on your own desktop or laptop without Docker. The admin app is your server and its management portal, with the source engine and web reader bundled alongside it. Use the reader app to read on this computer, or turn on "Allow other devices" to read across your home network.

## What the desktop app does

The desktop admin app uses Tauri, a framework for native apps, to package your server components:

- The admin console runs inside a native desktop window.
- The `komika-server` binary runs as a sidecar, a separate process next to the application. It serves the web reader at `/`, the admin console at `/admin`, the application programming interface (API), and the image proxy.
- The Suwayomi source engine runs as a child process alongside a bundled Java Runtime Environment (JRE) generated with `jlink`. The engine executes manga extensions and fetches chapter data.
- An on-device Cloudflare challenge solver runs in a hidden web view. When a source site presents a Cloudflare verification challenge, the app solves the challenge on your device and returns cookies to the engine.

The desktop app automatically controls process lifecycles. When you exit or close the application, the app terminates its server and engine child processes. On Linux, it registers `PR_SET_PDEATHSIG` with the kernel. On Windows, it assigns child processes to a Job Object configured with `KILL_ON_JOB_CLOSE`. On macOS, it tracks process identifiers (PIDs) on disk and terminates any orphaned processes on the next launch.

Your database and cover files are stored in your operating system application data folder under `app.komika.desktop`. Inside this folder, `server/komika.sqlite3` holds accounts, settings, and library history, while `suwayomi/` holds installed source extensions and engine data.

## Server selection on the start screen

When you launch the desktop app for the first time, it displays a start screen with two options:

- **Run komiq on this computer**: Starts the bundled source engine and server on your machine.
- **Connect to a server I already run**: Opens the management portal for a komiq server you already run.

### Run komiq on this computer

Choose "Run komiq on this computer" to start your own server:

```flow
Desktop app -> Source engine: starts background process
Source engine -> Desktop app: reports ready state
Desktop app -> Server process: starts server sidecar
Server process -> Desktop app: reports health and setup code
Desktop app -> Setup wizard: loads first-run setup
```

The app first launches the source engine and verifies its health. Once the engine responds, the app starts the `komika-server` process.

By default, the server binds to the loopback address (`127.0.0.1`) on port `7580`. If port 7580 is occupied by another program, the app increments the port number until it finds an open port. It saves the chosen port in `server/desktop.json` so your bookmarks continue to work across restarts.

The server produces a one-time security setup code on its first start. The desktop app captures this code directly from process output in memory and forwards it to the setup wizard. You do not need to inspect terminal logs to find the code. Once the server answers its health check, the app transitions to [First-run setup](/self-host/first-run).

### Connect to a server you already run

If you already run a komiq server on a home server or a virtual private server (VPS), select "Connect to a server I already run".

Enter your server address, such as `192.168.1.20:8080` or `https://komiq.example.com`. The app checks the `/health` endpoint and queries the GraphQL API to verify compatibility. Once confirmed, the app saves the target address and opens the sign-in screen or first-run setup.

## Allow other devices

The server first listens only on `127.0.0.1`, which other devices cannot reach.

To allow phones, tablets, or other computers to connect:

1. In [First-run setup](/self-host/first-run), continue from **Access** to the **Home network** step.
2. Turn on the "Allow other devices" switch.

The same switch is in **Settings › Access**, in every access mode.

When enabled, the desktop app restarts the server bound to `0.0.0.0`. It displays the local network addresses that other devices can open, such as `http://192.168.1.20:7580`, along with a connection Quick Response (QR) code. Read [Connect your devices](/self-host/other-devices) for pairing instructions.

The step then waits. When a device on your network reaches your server, it shows "A device on your network reached komiq." If none does, open **A device cannot connect?** on the same step:

- Put the device on the same Wi-Fi as this computer. A guest network or mobile data does not reach it.
- Let komiq through this computer's firewall, the program that blocks incoming connections. macOS asks by itself: choose Allow. Windows asks the first time. On Windows, and on Linux with `pkexec` and ufw or firewalld, **Open the firewall for komiq** adds a rule for the server's port after your system asks for permission.

## Put it on the internet

The desktop app's server can also use a domain of yours through Cloudflare. Choose **Public on the internet** on the **Access** step. The app uses a Cloudflare Tunnel, which needs no open ports. Readers reach it only while the app is open. You do not install the connector program, `cloudflared`: setup downloads it when this computer has none. For a public IP, or without a domain, run komiq on a server. Links that open a new tab, such as Cloudflare's token page, open in your system browser. Read [Reach it from the internet](/self-host/internet-access).

## Build the desktop app

If you want to compile the desktop app from source code, follow these steps.

### Prerequisites

Install the following software on your build machine:

- Rust 1.78 or newer (`rustc` and `cargo`).
- Node.js 22 and the `pnpm` package manager.
- Java Development Kit (JDK) 21 with `jlink` installed, with the `JAVA_HOME` environment variable configured.
- System development libraries required by Tauri. On Debian or Ubuntu systems, install:
  `libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev build-essential libssl-dev`.

### Build release packages

To build the release application and platform installers:

```sh
pnpm -C apps/admin desktop:build
```

This command executes the preparation script `apps/admin/src-tauri/scripts/prepare-bundle.sh`. The script downloads the pinned Suwayomi source engine archive, constructs a minimal runtime environment with `jlink`, compiles `komika-server` in release mode, and builds the web reader and admin console assets. Tauri then compiles the native wrapper and outputs installer packages.

### Build for local testing

To create a fast local build without generating installer bundles or packaging the engine:

```sh
KOMIKA_SKIP_ENGINE=1 KOMIKA_SERVER_PROFILE=debug pnpm -C apps/admin tauri build --debug --no-bundle
```

To run the desktop app in development mode with hot reloading:

```sh
bash apps/admin/src-tauri/scripts/prepare-bundle.sh
pnpm -C apps/admin desktop:dev
```

For this installation, continue with [First-run setup](/self-host/first-run), then [Add sources](/self-host/sources).

The next chapter covers the other way to host the admin app, on a home server or VPS.

Next: [Install with Docker](/self-host/docker).
