---
title: Run it locally
description: Run your local admin app server and connect the reader and console to it.
---

Develop komiq against a server you run locally. The admin app consists of the server, its management console and the Suwayomi source engine; the reader app connects to that server. This guide covers the tools and commands to run both apps on your computer.

## Prerequisites

Before starting, install the required development tools:

- Node.js: Version 22.13 or newer. Check your version with `node -v`.
- pnpm: Version 11.9.0. If you have Corepack enabled, run `corepack enable` to use the repository version.
- Rust: Stable toolchain with cargo. Install it through rustup. Check your version with `cargo --version`.
- Java: JDK 21 with `jlink` (`JAVA_HOME` set in your environment). This is required to build the embedded engine's Java runtime.
- System dependencies for Tauri: If you develop desktop builds on Debian or Ubuntu, install `libwebkit2gtk-4.1-dev`, `libayatana-appindicator3-dev`, `librsvg2-dev`, `build-essential`, and `libssl-dev`.

## Install dependencies

Clone the repository and install JavaScript and TypeScript dependencies from the repository root:

```sh
pnpm install
```

The command installs dependencies across all workspaces and establishes internal workspace links. Prebuilt binaries such as esbuild, sharp, and workerd do not run build scripts during install because `allowBuilds` disables them in `pnpm-workspace.yaml`.

## Start a local database

The server needs PostgreSQL 18 and a separate SQLite cover cache. Use native PostgreSQL tools to create a private development cluster from the repository root. Do this once, in a directory that does not already contain a cluster:

```sh
initdb -D .komiq-pg --encoding=UTF8 --locale-provider=builtin \
  --builtin-locale=C.UTF-8 --lc-collate=C --lc-ctype=C \
  --auth-local=peer --auth-host=scram-sha-256 --pwprompt
pg_ctl -D .komiq-pg -o '-h 127.0.0.1' start
createuser --pwprompt --no-superuser --no-createdb --no-createrole komika
createdb --owner=komika --template=template0 --encoding=UTF8 \
  --locale-provider=builtin --builtin-locale=C.UTF-8 \
  --lc-collate=C --lc-ctype=C komika
```

The password prompts do not print the secrets. These commands use local socket access for cluster administration and password authentication for TCP. Give the server the `komika` application role, not the cluster administrator. Set `DATABASE_URL` privately to `postgres://komika:<URL-encoded-password>@127.0.0.1:5432/komika` in your development environment. Do not commit credentials or shell history containing them.

To stop this development cluster, run `pg_ctl -D .komiq-pg stop`. Keep its directory across development sessions. The self-host Compose database publishes no host port, so a native development server cannot use its private container address directly.

## Run the admin app server

The server component of the admin app lives in `apps/server`. It exposes GraphQL, the query language used by the apps, on port 8080.

To start the server with development options and cross-origin resource sharing (CORS) permissions for local web apps, run:

```sh
cd apps/server
PORT=8080 GRAPHIQL_ENABLED=on CATALOGUE_SYNC=on CORS_ORIGINS='http://localhost:5173,http://localhost:5273' cargo run
```

On startup, the server performs these steps:

1. Connects to the PostgreSQL database supplied in `DATABASE_URL` and initializes the separate SQLite covers cache.
2. Applies migrations from `apps/server/migrations_pg/`. An old `komika.sqlite3` blocks startup when PostgreSQL is empty. Read [Old SQLite install](/self-host/update-and-backup#old-sqlite-install) to start fresh.
3. If no admin account exists, prints a one-time setup code to the terminal log.
4. Binds to port 8080.

You can verify that the server is active:

- GraphQL endpoint and GraphiQL interface: `http://localhost:8080/graphql`
- Health check endpoint: `http://localhost:8080/health`

## Run the source engine

The API server federates catalog and chapter data from Suwayomi. By default, the server expects Suwayomi to listen on `http://localhost:4567`.

You can run Suwayomi directly using Java or inside Docker.

### Run Suwayomi with Java

Download the pinned Suwayomi-Server jar using the helper script:

```sh
bash apps/reader/src-tauri/scripts/fetch-suwayomi-jar.sh
```

Then start the engine:

```sh
java -Djava.awt.headless=true -Xmx512m -jar apps/reader/src-tauri/suwayomi/Suwayomi-Server.jar
```

### Run Suwayomi with Docker

Alternatively, start a Suwayomi container:

```sh
docker run -d --name suwayomi -p 4567:4567 -e BIND_IP=0.0.0.0 -e BIND_PORT=4567 -v suwayomi-data:/home/suwayomi/.local/share/Tachidesk ghcr.io/suwayomi/tachidesk@sha256:e59f212ccf91b26de8676a063a2db90256c148c6261535c7c477d47a43d9751b
```

> [!NOTE]
> When Suwayomi is not running, `komika-server` still answers authentication and local library queries. Suwayomi queries that need an engine fetch will fail; cached results can still be returned.

## Run the reader app

The reader app lives in `apps/reader`. Run these commands from the repository root in a separate terminal.

### Web development server

Before starting the reader, point it at your local server. Create or edit `apps/reader/.env`:

```sh
PUBLIC_KOMIKA_BACKEND=on
PUBLIC_KOMIKA_BACKEND_KIND=komika
PUBLIC_KOMIKA_API=http://localhost:8080/graphql
PUBLIC_KOMIKA_IMG_MODE=direct
```

Setting `PUBLIC_KOMIKA_IMG_MODE=direct` fetches images directly without passing through an external image proxy.

Then start the reader with hot-module reloading, which updates the browser as you edit:

```sh
pnpm dev:reader
```

The Vite development server opens at `http://localhost:5173`.

### Desktop development build

To run the reader as a native desktop application using the Tauri shell:

```sh
pnpm --filter @komika/reader desktop:dev
```

## Run the admin app console

The management portal lives in `apps/admin`. Its development server runs separately from your local API server. Run these commands from the repository root.

### Web development server

To start the admin console web client, run:

```sh
PUBLIC_KOMIKA_API=http://localhost:8080/graphql pnpm dev:admin
```

The Vite development server opens at `http://localhost:5273`.

If your local server is fresh, open `http://localhost:5273/setup` to complete initial setup using the setup code printed in the server logs.

### Desktop development build

To run the admin console as a native desktop application:

```sh
bash apps/admin/src-tauri/scripts/prepare-bundle.sh
pnpm --filter @komika/admin desktop:dev
```

The `prepare-bundle.sh` script prepares the Suwayomi engine and Java runtime, compiles the server binary, and stages the embedded reader and console web bundles. Tauri, the native desktop shell, can then launch the server as a child process. In the admin desktop app, choose **Run komiq on this computer**, or connect to your existing local server at `localhost:8080`.

## Run the image proxy

The image proxy is an optional component, separate from the two apps. Running it is optional during local development. It is needed when testing proxy mode (`PUBLIC_KOMIKA_IMG_MODE=proxy`).

To start the worker locally with Wrangler:

```sh
pnpm --filter @komika/worker dev
```

The worker listens at `http://localhost:8787`. You can verify it with:

```sh
curl http://localhost:8787/healthz
```

## Local port reference

The following table lists the ports used by these local development configurations:

| Port | Service                   | What it serves                           |
| ---- | ------------------------- | ---------------------------------------- |
| 4567 | Suwayomi-Server           | Content engine REST API                  |
| 5173 | `@komika/reader`          | Web reader development server (Vite)     |
| 5273 | `@komika/admin`           | Admin console development server (Vite)  |
| 7580 | `komika-server` (desktop) | Default port when started by desktop app |
| 8080 | `komika-server`           | Standalone API server and GraphiQL UI    |
| 8787 | `@komika/worker`          | Image proxy worker (Wrangler dev)        |

## Where to look

- `.claude/launch.json`: Preconfigured local launch configurations and environment flags.
- `apps/server/src/config.rs`: Environment variable names and defaults for the API server.
- `apps/reader/src/lib/config.ts`: Environment variable parsing for reader backend and image modes.
- `apps/admin/src/lib/config.ts`: Environment variable parsing for admin console API endpoints.
- `apps/worker/wrangler.toml`: Local development variables and upstream host allowlists.
- `apps/reader/src-tauri/scripts/`: Scripts for downloading the engine jar and assembling JRE runtimes.
