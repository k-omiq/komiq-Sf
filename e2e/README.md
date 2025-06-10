# Komika E2E smoke tests

Standalone [Playwright](https://playwright.dev) smoke tests for the reader SPA.

> This project is **not** part of the pnpm workspace (the workspace only globs `apps/*` and `packages/*`). Install it on its own with `npm` inside `e2e/`.

## What it covers

`tests/smoke.spec.ts`:

1. **Home** — brand (`YOMU`) + nav render, and at least one series card row appears.
2. **Browse** — navigating to Browse shows the heading and a grid of results.
3. **Series** — clicking a card opens a `/series/<slug>` page with a title heading.

The reader has a mock-data fallback, so these pass with or without the backend running.

## Run

```sh
cd e2e
npm install                      # installs @playwright/test (standalone)
npm run install:browsers         # one-time: Playwright chromium + deps
```

Then, with the reader served somewhere, run the tests against it.

### Option A — against an already-running reader (recommended)

Start the reader yourself in another terminal:

```sh
pnpm --filter @komika/reader dev            # serves http://localhost:5173
# or serve a static build:
pnpm --filter @komika/reader build && npx serve -s apps/reader/build -l 5173
```

Then:

```sh
cd e2e
npm test                          # PW_BASE_URL defaults to http://localhost:5173
```

Point at any other URL (e.g. the nginx image on :8081, or a deployed preview):

```sh
PW_BASE_URL=http://localhost:8081 npm test
```

### Option B — let Playwright launch the reader

Opt in with `PW_WEB_SERVER=1`; the config will run `pnpm --filter @komika/reader dev` for you. It's guarded behind the env flag so it never collides with a dev server you already have running.

```sh
cd e2e
PW_WEB_SERVER=1 npm test
```

## Reports

```sh
npm run report                    # open the last HTML report
```

## CI

Runs in `.github/workflows/ci.yml` as the **E2E smoke (manual)** job, gated to `workflow_dispatch`. It downloads the reader build artifact, serves it on :5173, and runs these tests. Trigger it from the Actions tab before a release.
