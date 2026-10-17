---
title: Desktop
description: Run and build the admin desktop app that manages your server and the reader desktop app that connects to it.
---

Komiq is self-hostable and has two desktop apps. Use the admin app to start and manage your server, and use the reader app to connect to a server and read. Both use Tauri 2, a native shell that displays the web interface and exposes Rust commands.

## Choose the app

The admin app is the server and its management portal in one. `apps/admin/src-tauri/` displays the desktop build of the admin console. Its start screen offers running komiq on this computer or connecting to a server you already run. Running locally starts the bundled PostgreSQL database first, then the source engine, then the `komika-server` process. That server also serves bundled reader and admin web builds for other devices when network access is enabled.

The reader app connects to a local server or a server on the internet. It cannot use the global server, which accepts requests from the `komiq.cc` website alone. `apps/reader/src-tauri/` displays the reader. Its "Connect to a server" screen accepts an IP address and port such as `192.168.1.20:8080`, a domain such as `komiq.example.com`, or a pasted link. It saves the server you choose. See [Connect to a server](/guide/connect) for the shared connection flow, including QR scanning on mobile.

On desktop, you can select device fetching to use the reader's embedded Suwayomi engine. The reader starts that engine on demand rather than at every app launch.

The admin crate compiles `apps/reader/src-tauri/src/suwayomi.rs` and `apps/reader/src-tauri/src/cloudflare.rs` by path. It shares the engine implementation instead of maintaining a copy. The admin shell adds its own server supervisor and process cleanup hooks.

## Use the Rust commands

The frontend calls Rust through inter-process communication (IPC). Reader commands:

| Command                 | Purpose                                                    |
| ----------------------- | ---------------------------------------------------------- |
| `engine_supported`      | Reports whether device fetching is supported               |
| `suwayomi_start`        | Starts the engine at most once per app run                 |
| `suwayomi_status`       | Returns state, version and the latest error                |
| `suwayomi_base_url`     | Returns an engine address only while ready                 |
| `suwayomi_gql`          | Proxies a GraphQL query to the engine                      |
| `suwayomi_image`        | Returns image bytes from an engine path over IPC           |
| `engine_image_base`     | Returns the Android image listener's address, else nothing |
| `fetch_image`           | Fetches image bytes from a validated HTTP or HTTPS URL     |
| `set_status_bar_hidden` | Does nothing on desktop                                    |

GraphQL is the query interface used by Suwayomi. `fetch_image` returns binary bytes rather than JSON image data, and the reader turns those bytes into temporary blob URLs for display.

The reader's pages from the embedded engine no longer use `suwayomi_image`. The image provider gives each page an `engine-image://localhost/api/v1/…` address, and Rust answers it from a custom address scheme registered in `lib.rs` (on Windows the web view writes it as `http://engine-image.localhost/api/v1/…`). `suwayomi_image` stays as a thin wrapper over the same Rust function, for callers that want the bytes in JavaScript. The `native-read` test harness calls it directly. Nobody has loaded a page through the scheme on macOS, Windows or Linux yet: it compiles for desktop, and that is the extent of the check.

The admin shell registers a different bridge. `start_local_server` begins startup or retries a failed step. `local_server_status` reports the startup stage and warnings for the owner. `local_server_log` returns captured process output with those warnings on top. `local_network` reports the network choice, and `set_local_network` saves that choice and restarts the server with the new bind setting. `firewall_info` reports the operating system and whether the shell can open the system firewall for komiq. `open_firewall` adds a firewall rule for the server's port after the system's own permission prompt: an elevated PowerShell on Windows, `pkexec` with firewalld or ufw on Linux. On macOS it changes nothing, because macOS asks by itself. It works only while the server listens for other devices.

The opener plugin sends links to your system browser.

The admin shell does not register the reader's engine query and image commands. Its console talks to the komiq server instead.

## Bundle the engine per operating system

Both shells use the Suwayomi archive pinned in `apps/reader/src-tauri/suwayomi/VERSION`. The fetch script verifies its SHA-256 checksum. The Java runtime environment, or JRE, is built with `jlink`, a tool that selects Java modules for a smaller runtime. It requires Java Development Kit (JDK) 21.

`apps/reader/src-tauri/scripts/build-jre.sh` builds for the machine running it. Set `JAVA_HOME` to your JDK 21 installation. The script accepts ARM64 and x86-64 hosts, two processor architectures, and derives the runtime directory from the processor and operating system. It is not a cross-platform JRE builder.

| Operating system | Runtime directory name                | Launcher       |
| ---------------- | ------------------------------------- | -------------- |
| macOS            | `aarch64-macos` or `x86_64-macos`     | `bin/java`     |
| Linux            | `aarch64-linux` or `x86_64-linux`     | `bin/java`     |
| Windows          | `aarch64-windows` or `x86_64-windows` | `bin/java.exe` |

The native-sidecar workflow lists Apple Silicon macOS, Intel macOS, x86-64 Linux and x86-64 Windows.

The reader's base `apps/reader/src-tauri/tauri.conf.json` has an empty resource list. Its `apps/reader/src-tauri/tauri.macos.conf.json`, `apps/reader/src-tauri/tauri.linux.conf.json` and `apps/reader/src-tauri/tauri.windows.conf.json` each list `suwayomi/` and `jre/` as resources. Tauri bundles every file under a resource directory. Keep only the pinned archive and `VERSION` in `apps/reader/src-tauri/suwayomi/`: any other file there ships in the desktop apps. The iOS override includes only its own copy of the archive, which `prune-ios-jar.sh` writes to `apps/reader/src-tauri/suwayomi-ios/`.

The admin has one `apps/admin/src-tauri/tauri.conf.json`, with no separate per-OS override files. It maps the reader's engine directories into its own resources, so the same rule applies to the admin app. It also bundles `apps/admin/src-tauri/web/` and declares `apps/admin/src-tauri/binaries/komika-server` as an external binary, a child executable packaged beside the app. `apps/admin/src-tauri/scripts/prepare-bundle.sh` adds the Rust target suffix and Windows executable extension to the prepared server filename.

## Run and build the reader

Use the Node and pnpm versions declared by the root package. Install the Tauri system dependencies for your operating system. The admin README lists Linux development packages.

For reader development, prepare the engine resources first. The development hook starts the frontend but does not run the resource preparation scripts.

```sh
bash apps/reader/src-tauri/scripts/fetch-suwayomi-jar.sh
bash apps/reader/src-tauri/scripts/build-jre.sh
pnpm -C apps/reader desktop:dev
```

Connect to a server and select device fetching to start the local engine. You can point development at existing assets with `KOMIKA_JAVA_BIN` and `KOMIKA_SUWAYOMI_JAR`.

Build the desktop reader with:

```sh
pnpm -C apps/reader desktop:build
```

Its build hook fetches and verifies the archive, builds the host JRE and builds the frontend. Tauri selects the native static frontend through the platform environment it supplies.

## Run and build the admin app

Prepare the server executable and web resources before opening the development window:

```sh
bash apps/admin/src-tauri/scripts/prepare-bundle.sh
pnpm -C apps/admin desktop:dev
```

For a release build and installers:

```sh
pnpm -C apps/admin desktop:build
```

The preparation script builds the engine, the release server, the embedded reader and the embedded admin console. It places the web builds under `apps/admin/src-tauri/web/reader` and `apps/admin/src-tauri/web/admin`. The window's own desktop console build runs afterward.

For a faster debug build without installers or engine preparation:

```sh
KOMIKA_SKIP_ENGINE=1 KOMIKA_SERVER_PROFILE=debug pnpm -C apps/admin tauri build --debug --no-bundle
```

Skipping preparation does not disable engine startup or remove existing resources. To run the local server without an engine during development, use `KOMIKA_DESKTOP_ENGINE=off`; sources are then unavailable. Other development overrides include `KOMIKA_SERVER_BIN` and `KOMIKA_WEB_DIR`.

## Data and process lifetime

The app-data identifiers are `app.komika.reader` and `app.komika.desktop`. Engine data lives in `suwayomi/`.

The admin app runs bundled PostgreSQL 18.6 for the main database and caches cover images and extension icons as files in `server/covers/`. It sets `KOMIKA_COVERS_DIR` to that folder; PostgreSQL holds the index and uploaded covers. The cluster is `server/postgres/18/data`. A cluster of another major version blocks the start, because the app has no upgrade step. `server/postgres/18/credentials` holds the two role passwords, and the app never makes new ones for an existing cluster. The database listens on `127.0.0.1` only, starts before the engine and server and stops after them. On macOS and Linux the app recognises an earlier database of this folder that is still running and stops it with `pg_ctl`.

Dumps include the cover index and uploads since this update. Cover files are not backed up: missing files refill from sources, and uploads return from PostgreSQL. `COVERS_DATABASE_URL` still places the data-directory lock, default covers directory and old `server/covers.sqlite3` copy source. Read [Data model](/developers/data-model) for the background copy and the backup limit for earlier uploads.

The app gives the server its PostgreSQL connection in `DATABASE_URL`. The app saves a dump in `server/backups/` when none exists or the newest is at least 20 hours old, keeps seven, and has no restore command. Its port and network choice live in `server/desktop.json`. A fresh local server tries port 7580, then alternatives if needed, and remembers the chosen port. It binds only to loopback, a connection confined to the computer, until you enable "Allow other devices."

On Linux, the admin installs a parent-death signal for child processes. On Windows, it assigns children to a Job Object, an operating-system process group configured to end when its handle closes. On macOS and Linux, it records each child's identifier, program file and creation time, and stops only matching orphans at the next launch. A record it cannot verify is kept and reported. These additional hooks belong to the admin shell; the reader does not install them.

An automated gate (`.github/workflows/postgres-desktop-gate.yml`) tests the app's database handling on Apple-silicon macOS, x86_64 Linux and x86_64 Windows. Intel macOS is outside it, and the real installers are not yet verified. The native-sidecar workflow tests and boots desktop engines, but it does not build desktop installers. The native-read harness replaces Tauri IPC with a Node HTTP adapter. Neither check establishes signing or a successful read in a native window.

## Where to look

- `apps/reader/src-tauri/src/lib.rs`: reader commands and on-demand startup.
- `apps/reader/src-tauri/src/suwayomi.rs`: shared engine supervisor and resource lookup.
- `apps/reader/src-tauri/scripts/build-jre.sh`: runtime layout and JDK checks.
- `apps/reader/src-tauri/tauri.conf.json`: reader build hooks.
- `apps/reader/src-tauri/tauri.macos.conf.json`, `tauri.linux.conf.json` and `tauri.windows.conf.json` in the same folder: per-OS resources.
- `apps/admin/src-tauri/src/lib.rs`: shared modules and admin bridge commands.
- `apps/admin/src-tauri/src/postgres.rs`: bundled PostgreSQL supervision and local dumps.
- `apps/admin/src-tauri/src/local.rs`: server startup, storage and network choices.
- `apps/admin/src-tauri/src/firewall.rs`: firewall rule commands per operating system.
- `apps/admin/src-tauri/src/child.rs`: per-OS process cleanup.
- `apps/admin/src-tauri/scripts/prepare-bundle.sh`: server and web resource preparation.
- `apps/admin/src-tauri/tauri.conf.json`: admin executable and resource packaging.
- `apps/admin/README.md`: desktop requirements and development commands.
