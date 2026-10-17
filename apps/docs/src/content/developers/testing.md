---
title: Testing
description: Verify the admin app, reader app, and optional image proxy with the repository test suites.
---

Verify changes to the two komiq apps against your own local server. The admin app combines the server, management portal and Suwayomi engine; the reader app connects to it. This page covers Rust tests, static type checks, browser tests, desktop process tests, native engine checks and the optional image proxy tests.

## Test suites overview

The suites cover components of both apps and their shared tooling:

- Rust server test suite: Unit and integration tests for the admin app's API server.
- Cloudflare Worker test suite: Security and URL validation tests for the optional image proxy.
- Typecheck suite: SvelteKit and TypeScript type validation in workspaces with check scripts.
- Formatting checks: Prettier style enforcement across files not excluded by `.prettierignore`.
- Playwright end-to-end tests: Browser smoke tests for the reader and admin console.
- Desktop shell tests: Rust unit tests for Tauri process supervision.
- Native read harnesses: Integration scripts verifying on-device engine and offline queue behavior.

## Admin app server tests

The admin app's API server in `apps/server` includes tests for GraphQL resolvers, catalog deduplication, cover hash calculation, authentication, session tokens, and database migrations.

The server tests provision a private PostgreSQL 18.6 cluster automatically and clone isolated databases. They require permission to start local processes and bind private sockets. Cover storage uses files with a PostgreSQL index and upload records. Only the legacy cover copy fixtures retain SQLite.

To run the server test suite:

```sh
cd apps/server
cargo test
```

To run code formatting and lint checks on the Rust codebase:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Continuous integration runs the server formatting and Clippy checks on every pull request. Clippy warnings fail the build. The CI workflow does not run `cargo test`.

## Self-host packaging checks

The installer gate uses fake Docker calls for installation and startup checks:

```sh
bash deploy/selfhost/install.test.sh
```

With prebuilt images and the pinned PostgreSQL image cached, run the opt-in Docker gate:

```sh
bash deploy/selfhost/install.test.sh --real komiq-selfhost:local
sh deploy/image-smoke.test.sh komiq-postgres-server:review komiq-selfhost:local
```

The real installer gate checks a fresh PostgreSQL installation, a restart and a dump restored into another empty installation. Both scripts use disposable resources with no published ports. The image smoke script provisions PostgreSQL before testing readiness, directory exclusion and restart inside both runtime images.

## Cloudflare Worker security tests

The optional image proxy in `apps/worker` contains security tests in `apps/worker/test/security.test.mjs`. These tests use the Node.js native test runner to assert that:

- Only safe image media types (PNG, JPEG, WebP, AVIF, GIF) are accepted.
- Active or dangerous media types such as SVG and HTML are refused with an error.
- Upstream URLs containing credentials or non-standard service ports are rejected.
- Redirects to disallowed destinations fail before a second hop occurs.
- Rate limits remain enforced during manual cover cache bypasses.

To run the worker test suite:

```sh
pnpm --filter @komika/worker test
```

You can also run type checking on the worker without deploying:

```sh
pnpm --filter @komika/worker check
```

## Static type and syntax checks

Before building production bundles, you should verify TypeScript types and Svelte component syntax across the monorepo:

```sh
pnpm check
```

This root command runs `pnpm -r check`, which triggers:

- `svelte-check` for `apps/reader`.
- `svelte-check` for `apps/admin`.
- `svelte-check` for `apps/docs`.
- `tsc --noEmit` for `apps/worker`.

You can also run checks individually for a single application:

```sh
# Typecheck reader app
pnpm --filter @komika/reader check

# Typecheck admin console
pnpm --filter @komika/admin check

# Typecheck image proxy worker
pnpm --filter @komika/worker check
```

## Code formatting checks

Komiq enforces consistent code style with Prettier. Configuration lives in `.prettierrc.json`.

To check whether files conform to formatting standards:

```sh
pnpm lint
```

To automatically format supported files not excluded by `.prettierignore`:

```sh
pnpm format
```

## End-to-end smoke tests

The `e2e/` directory contains an end-to-end test suite using Playwright. This suite validates real browser interactions against the reader single-page application.

> [!NOTE]
> The `e2e/` directory is not part of the pnpm workspace. It has its own `package.json` and must be installed independently using npm.

### Install Playwright dependencies

Navigate to the `e2e/` folder and install dependencies:

```sh
cd e2e
npm install
npm run install:browsers
```

### Run against a running server

First follow [Run it locally](/developers/local-development) to start your local server and point the reader at it. If you already have the reader running on port 5173 (`pnpm dev:reader`):

```sh
cd e2e
npm test
```

To point tests at your local server with an embedded reader, or a different reader port or host:

```sh
PW_BASE_URL=http://localhost:8080 npm test
```

### Run with automatic server launch

Playwright can start the reader development server automatically during test execution. Configure the reader for your local server first; this option launches the reader, not the API server or source engine:

```sh
cd e2e
PW_WEB_SERVER=1 npm test
```

### Inspecting test results

To run tests in a visible browser window:

```sh
npm run test:headed
```

CI generates an HTML report. To view it after a test run:

```sh
npm run report
```

What the smoke tests verify:

- Home page: Navigation renders and series card rows appear.
- Browse page: Navigating to Browse loads the grid of available titles.
- Series page: Selecting a card opens the series route and shows a nonempty title heading.

## Desktop shell tests

Both apps contain Tauri desktop shells, native wrappers for their web interfaces, in their respective `src-tauri` directories. The admin shell starts or picks a server; the reader shell connects to a server and can use an on-device engine.

### Admin desktop tests

The admin desktop tests verify child process management, port allocation, and startup handshakes:

```sh
cd apps/admin/src-tauri
cargo test
```

To test against a real `komika-server` binary rather than a mock server:

```sh
KOMIKA_TEST_SERVER_BIN=/path/to/komika-server cargo test -- --ignored real_server
```

The desktop database gate starts the bundled PostgreSQL runtime, the supervisor and a release `komika-server`, then checks restarts, a left-over database process and dumps. Stage the runtime for your system, build the server in release mode, then run the ignored test. `<target>` is a name such as `aarch64-macos`, and the stage command needs Python 3.12:

```sh
python3 scripts/postgres-desktop-bundle.py --triple <rust triple>
export KOMIKA_POSTGRES_RUNTIME="$PWD/apps/admin/src-tauri/postgres/<target>/18"
export KOMIKA_TEST_SERVER_BIN="$PWD/apps/server/target/release/komika-server"
export KOMIKA_DESKTOP_GATE_ROOT=/absolute/writable/absent/directory
export KOMIKA_DESKTOP_GATE_JSON=/absolute/writable/gate.json
TAURI_CONFIG='{"bundle":{"externalBin":[]}}' cargo test --manifest-path apps/admin/src-tauri/Cargo.toml \
  --features desktop-gate --test desktop_postgres_gate -- --ignored --exact desktop_postgres_gate --nocapture
```

The gate root must not exist yet. The JSON file lists each step and its result.

### Reader desktop tests

The reader desktop tests verify the embedded Suwayomi engine supervisor, platform bridges, and Cloudflare challenge handlers:

```sh
cd apps/reader/src-tauri
cargo test
```

## Native read integration harnesses

For low-level verification of native engine integration, the repository includes shell harnesses in `apps/reader/src-tauri/e2e/`.

### Offline write queue check

The offline queue check validates that library and reading progress writes are queued locally while offline and replayed on recovery. It tests the real `@komika/api` bundle without requiring a running engine:

```sh
bash apps/reader/src-tauri/e2e/offline-queue/run-offline-check.sh
```

### Fallback ladder check

The fallback ladder check verifies chapter and page fallback to the server the reader is connected to after local engine failures, remembers failed local resolutions, and retries an engine that becomes ready:

```sh
bash apps/reader/src-tauri/e2e/fallback-ladder/run-fallback-check.sh
```

### Native live read harness

The `native-read` harness verifies that `CompositeBackend` and `LocalSuwayomiBackend` in `@komika/api` correctly resolve chapters and stream image bytes. It starts two real Suwayomi engines: one plays the server's engine and one, empty at the start, plays the device:

```sh
cd apps/reader/src-tauri/e2e/native-read
bash run.sh
```

This harness requires the Suwayomi jar and a compiled JRE to be present on your machine.

## Where to look

- `apps/server/src/`: Server unit tests colocated with Rust modules.
- `apps/worker/test/security.test.mjs`: Security validation tests for the image proxy.
- `e2e/tests/`: Playwright browser test specifications.
- `e2e/playwright.config.ts`: Playwright runner configuration.
- `apps/admin/src-tauri/src/`: Process supervisor tests for the desktop admin app.
- `apps/reader/src-tauri/src/`: Platform and engine supervisor tests for the desktop reader app.
- `apps/reader/src-tauri/e2e/`: Native engine integration scripts and assertion harnesses.
- `.github/workflows/ci.yml`: Continuous integration test workflows and check definitions.
