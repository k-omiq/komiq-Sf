# komiq

Komiq is an AGPL-licensed manga and comics reader built from one codebase for the
web, desktop, iOS, and Android. It uses Suwayomi as its content engine and adds a
social layer. [`komiq.cc`](https://komiq.cc) is a free, work-in-progress demo of
the apps; it is not an official publisher or content service.

See [`SPEC.md`](./SPEC.md) for the full architecture.

## How content works

- Eligible community members can add third-party sources. A source added through
  that process becomes available globally without the Komiq operator selecting it
  individually.
- Covers and pages are retrieved on demand when someone uses the reader. The web
  path may proxy image responses through Cloudflare and temporarily store them in
  Cloudflare's edge cache. Native builds may fetch directly from a source or through
  the app's local Suwayomi engine.
- Komiq is not intended to operate a permanent central archive of manga or comic
  image files. Copies can still exist temporarily—or, when a user enables downloads,
  for longer—in Cloudflare edge caches, Suwayomi/app caches, browser caches, or local
  device storage. The operated service also stores accounts, authentication sessions,
  reviews, comments, source and catalog state, and security or operational records.

The exact flow depends on the build and deployment configuration. Self-hosters
control their own sources and infrastructure and are responsible for operating them
lawfully.

## Third-party content

Copyrights and trademarks in third-party works belong to their respective owners.
Komiq does not claim ownership of that material and is not affiliated with or
endorsed by any publisher, author, artist, translator, scanlator, source operator,
Suwayomi, Mihon, or extension author unless expressly stated.

Komiq's software license does not grant anyone a license to third-party content.
Users must add and access only sources and material they are authorized to use and
must follow applicable law and source terms. See the [Copyright Policy](./COPYRIGHT.md)
for reporting and enforcement information.

Voluntary donations support development and maintenance of the open-source software.
They do not purchase content, access, source privileges, or any license to third-party
works.

## Policies

- [Copyright Policy](./COPYRIGHT.md)
- [Privacy Policy](./PRIVACY.md)

These policies describe services operated by the Komiq project, including the WIP
demo. A person who redistributes or self-hosts the software must provide policies and
processes appropriate to their own deployment.

## Prerequisites

- Node ≥ 20, **pnpm** (`corepack enable`)
- Rust (stable) — for the desktop/mobile shell
- Xcode (iOS), Android SDK/NDK (Android)

## Setup

```sh
pnpm install
```

## Develop

```sh
pnpm dev:reader          # reader app (web) at http://localhost:5173
pnpm dev:admin           # admin console at http://localhost:5273

# desktop app (Rust + webview)
pnpm --filter @komika/reader desktop:dev
```

## Build

```sh
pnpm build               # all packages + apps (static SPAs)
pnpm --filter @komika/reader desktop:build   # desktop binaries
```

## Workspace

| Package          | What                                             |
| ---------------- | ------------------------------------------------ |
| `@komika/reader` | user app (SvelteKit SPA + `src-tauri` Rust core) |
| `@komika/admin`  | admin "manga DB" console (web-only SPA)          |
| `@komika/types`  | shared domain types                              |
| `@komika/api`    | data layer — `Backend` + `ImageProvider`         |
| `@komika/ui`     | design tokens + shared components                |

## Status

Work in progress. The hosted web build is a demo of the desktop and mobile apps;
features, data flows, and deployment details may change before a production release.
See [`SPEC.md`](./SPEC.md) for current implementation notes and known limitations.

## License

**AGPL-3.0-only.** See [`LICENSE`](./LICENSE) and [`NOTICE`](./NOTICE).

Komiq embeds [Suwayomi-Server](https://github.com/Suwayomi/Suwayomi-Server)
(AGPL-3.0) as its on-device content engine, so the whole project is released as
AGPL open source with public Corresponding Source.
