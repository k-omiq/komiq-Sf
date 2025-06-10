<div align="center">

<img src="apps/reader/static/icon-192.png" width="96" height="96" alt="Komika logo" />

# Komika

**Self-hostable manga & comics reader, built on [Suwayomi](https://github.com/Suwayomi/Suwayomi-Server).**

_One catalogue, every source — with a social layer on top. Web, desktop, and mobile from a single codebase._

[![CI](https://github.com/k-omiq/komika/actions/workflows/ci.yml/badge.svg)](https://github.com/k-omiq/komika/actions/workflows/ci.yml)
[![Native sidecar](https://github.com/k-omiq/komika/actions/workflows/native-sidecar.yml/badge.svg)](https://github.com/k-omiq/komika/actions/workflows/native-sidecar.yml)
[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-coral.svg)](LICENSE)

![Rust](https://img.shields.io/badge/Rust-axum%20%2B%20SQLite-orange?logo=rust)
![SvelteKit](https://img.shields.io/badge/SvelteKit-SPA%20%2B%20edge%20SSR-ff3e00?logo=svelte)
![Tauri](https://img.shields.io/badge/Tauri-desktop%20%26%20mobile-24C8DB?logo=tauri)
![Cloudflare](https://img.shields.io/badge/Cloudflare-Workers%20%2B%20edge%20cache-F38020?logo=cloudflare)

</div>

---

## What it is

Komika is "Mihon as a service": a hosted (or self-hosted) reader where the
**server** does the heavy lifting — source scanning, a canonical deduplicated
catalogue, cover caching, update polling — and every client stays thin.

- 📚 **Canonical catalogue** — one entry per series, whatever the source. A
  MangaDex-anchored spine plus a multi-step matcher folds mirrors together.
- 🔌 **Suwayomi under the hood** — the battle-tested extension ecosystem powers
  source fetching; desktop builds embed it as a supervised sidecar.
- 🫂 **Social layer** — accounts, per-user library, reading progress, reviews,
  comments, and view-based trending.
- ⚡ **Built to be cheap to run** — in-process feed caches, one-round-trip
  series pages, batched writes, `ETag`/`Cache-Control` on public reads so a CDN
  absorbs anonymous traffic. A 4-core box goes a very long way.
- 🖥️ **Every platform** — SvelteKit web app (edge-SSR on public pages), Tauri
  desktop shell, and the same UI headed for iOS/Android.
- 💝 **Donation-only** — no paywalls, no gating, no ads.

See [`SPEC.md`](./SPEC.md) for the full architecture and
[`PERFORMANCE_ROADMAP.md`](./PERFORMANCE_ROADMAP.md) for the measured numbers.

## Self-hosting

The whole stack is three containers: the Rust API server, the reader SPA
(nginx), and Suwayomi — orchestrated by
[`deploy/docker-compose.yml`](./deploy/docker-compose.yml).

```sh
git clone https://github.com/k-omiq/komika.git
cd komika/deploy
docker compose up -d
```

Read [`deploy/GO-LIVE.md`](./deploy/GO-LIVE.md) for the full runbook (volumes,
backups via Litestream, Cloudflare tunnel, image Worker) and
[`PRODUCTION.md`](./PRODUCTION.md) for operational notes.

## Developing

Prerequisites: Node ≥ 20 with **pnpm** (`corepack enable`), stable Rust; Xcode /
Android SDK only for the mobile shells.

```sh
pnpm install

pnpm dev:reader          # reader app (web) at http://localhost:5173
pnpm dev:admin           # admin console at http://localhost:5273
pnpm --filter @komika/reader desktop:dev    # desktop app (Rust + webview)

cd apps/server && cargo run                 # API server at :8080
```

Build everything:

```sh
pnpm build               # all packages + apps (static SPAs)
pnpm --filter @komika/reader desktop:build  # desktop binaries
```

## Workspace

| Package          | What                                             |
| ---------------- | ------------------------------------------------ |
| `apps/server`    | Rust API — axum + async-graphql + SQLite (WAL)   |
| `@komika/reader` | user app (SvelteKit SPA + `src-tauri` Rust core) |
| `@komika/admin`  | admin "manga DB" console (web-only SPA)          |
| `@komika/types`  | shared domain types                              |
| `@komika/api`    | data layer — `Backend` + `ImageProvider`         |
| `@komika/ui`     | design tokens + shared components                |

## License

**AGPL-3.0-only.** See [`LICENSE`](./LICENSE) and [`NOTICE`](./NOTICE).

Komika embeds [Suwayomi-Server](https://github.com/Suwayomi/Suwayomi-Server)
(AGPL-3.0) as its content engine, so the whole project is released as AGPL open
source with public Corresponding Source. Distribution is web / sideload /
F-Droid / desktop download.
