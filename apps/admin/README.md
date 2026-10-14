# sv

Everything you need to build a Svelte project, powered by [`sv`](https://github.com/sveltejs/cli).

## Creating a project

If you're seeing this, you've probably already done this step. Congrats!

```sh
# create a new project
npx sv create my-app
```

To recreate this project with the same configuration:

```sh
# recreate this project
npx sv@0.16.1 create --template minimal --types ts --no-install apps/admin
```

## Developing

Once you've created a project and installed dependencies with `npm install` (or `pnpm install` or `yarn`), start a development server:

```sh
npm run dev

# or start the server and open the app in a new browser tab
npm run dev -- --open
```

## Building

To create a production version of your app:

```sh
npm run build
```

You can preview the production build with `npm run preview`.

> To deploy your app, you may need to install an [adapter](https://svelte.dev/docs/kit/adapters) for your target environment.

## Build targets

`ADMIN_TARGET` picks where the build runs (default `hosted`):

| Target     | API                                                  | First run                                     |
| ---------- | ---------------------------------------------------- | --------------------------------------------- |
| `hosted`   | `PUBLIC_KOMIKA_API` (admin.komiq.cc, unchanged)      | none                                          |
| `embedded` | same origin `/graphql`, served from `KOMIKA_WEB_DIR` | a server with no owner opens `/setup`         |
| `desktop`  | the server chosen on `/start`, saved on the device   | `/start` (run here or connect), then `/setup` |

`ADMIN_BASE_PATH=/admin` builds the console for a sub-path; the server serves the
`embedded` build there:

```sh
ADMIN_TARGET=embedded ADMIN_BASE_PATH=/admin pnpm -C apps/admin build
```

Internal links go through `href()` in `src/lib/paths.ts` so they carry the base path.
The desktop bridge the Tauri shell implements is described in `src/lib/desktop.ts`.

## Desktop app

`src-tauri/` is the komiq desktop app (Tauri 2, same versions and layout as the
reader's shell in `apps/reader/src-tauri`). Its window shows the `desktop` build of
this console. "Run komiq on this computer" starts the source engine and the komiq
server as child processes and opens the setup wizard with the setup code filled in;
"Connect to a server I already run" points the app at another server.

What the app bundles, and from where:

| Part                         | Bundled as                                      | Made by                                                          |
| ---------------------------- | ----------------------------------------------- | ---------------------------------------------------------------- |
| `komika-server`              | sidecar (`externalBin`), next to the app        | `apps/server` (`cargo build`)                                    |
| reader + console for the LAN | resource `web/` (the server's `KOMIKA_WEB_DIR`) | reader `embedded` build, console `embedded` build under `/admin` |
| source engine                | resources `suwayomi/` and `jre/`                | the reader's `fetch-suwayomi-jar.sh` and `build-jre.sh`          |

`src-tauri/scripts/prepare-bundle.sh` builds all of it; `tauri build` runs it first
(`beforeBuildCommand`), then the window's own build. The engine supervisor is the
reader's `src/suwayomi.rs`, compiled into this crate by path, not copied; so is its
on-device Cloudflare shim (`src/cloudflare.rs`), which solves Cloudflare challenges for
the engine in a hidden window of the app.

The wizard's Home network step ("Allow other devices", `HomeNetwork.svelte`, also in
Settings › Access) follows the Access step for either choice. It restarts the server on every
interface through the bridge, then asks the server whether a device on the network has
reached it (`accessStatus.lanClientSeenAt`) and says so when one has. Its help section
can add the firewall rule for the server's port (`src-tauri/src/firewall.rs`, bridge
commands `firewall_info` and `open_firewall`): one elevated PowerShell on Windows,
`pkexec` with firewalld or ufw on Linux, each behind the system's own permission prompt.
macOS asks by itself, so nothing is done there. Links that open a new tab (Cloudflare's
token page, the Let's Encrypt agreement) go to the system browser through
`tauri-plugin-opener`; the window itself never navigates away.

The server and the engine end with the app, even when it is force-killed
(`src-tauri/src/child.rs`): `PR_SET_PDEATHSIG` on Linux, a Job Object with
`KILL_ON_JOB_CLOSE` on Windows. macOS has no such kernel feature, so there the app
records its children's pids and the next launch stops any that were left running
before it starts new ones.

Prerequisites: Rust (1.78+), Node 22 and pnpm, JDK 21 with `jlink` for the engine's
runtime (`JAVA_HOME`), and the [Tauri system dependencies](https://v2.tauri.app/start/prerequisites/)
(on Debian/Ubuntu: `libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev
build-essential libssl-dev`).

```sh
# Release app and installers (engine, release server, web apps, window):
pnpm -C apps/admin desktop:build

# Faster local build without installers: debug server, no engine.
KOMIKA_SKIP_ENGINE=1 KOMIKA_SERVER_PROFILE=debug pnpm -C apps/admin tauri build --debug --no-bundle

# Development window with hot reload. Run prepare-bundle.sh once first: Tauri checks
# that the sidecar exists even for `cargo check`.
bash apps/admin/src-tauri/scripts/prepare-bundle.sh
pnpm -C apps/admin desktop:dev
```

Useful environment for development: `KOMIKA_SERVER_BIN` (server binary to run),
`KOMIKA_WEB_DIR` (web folder to serve), `KOMIKA_DESKTOP_ENGINE=off` (run the server
without the engine; sources are then unavailable), and the reader's `KOMIKA_JAVA_BIN`
and `KOMIKA_SUWAYOMI_JAR`.

On the device the app keeps its data in the OS app-data folder
(`app.komika.desktop`): `server/postgres/18/data` (the server's bundled PostgreSQL
database), `server/covers.sqlite3` (the cover cache; an old `server/komika.sqlite3` is
converted at the first start and kept), `server/desktop.json` (the server's port and the
"Allow other devices" choice) and `suwayomi/` (the engine). The server listens on `127.0.0.1` on port 7580 (or the next
free port, which is then kept) until "Allow other devices" is turned on in setup. The
setup code is read from the server's output and handed to the wizard once; it is never
logged or saved. Both processes' output is kept in memory for the start screen's log
view.

Tests (the process management runs against a fake server, `python3` required):

```sh
cd apps/admin/src-tauri && cargo test
# The real server as the local server (fresh database, owner, second launch):
KOMIKA_TEST_SERVER_BIN=/path/to/komika-server cargo test -- --ignored real_server
```
