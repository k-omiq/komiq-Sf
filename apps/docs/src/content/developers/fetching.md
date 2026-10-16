---
title: How fetching works
description: How the reader fetches data from its connected server and routes images across web, desktop and mobile.
---

Komiq is self-hostable, and the reader fetches data from whichever server it is connected to. It separates catalog metadata from chapter images. This guide covers platform fetching, image routing and fallbacks.

## Overview

The reader interface uses two boundaries:

1. `backend`: Handles catalog queries, search, library tracking, user sessions, and reading progress.
2. `images`: Resolves upstream image URLs into displayable image sources.

With device fetching enabled, native apps combine the connected GraphQL server with an on-device content engine. On the web, the browser talks to the connected GraphQL server. It can route remote images through `/img` on that server or an optional image proxy Worker to pass browser security checks.

## Fetching per platform

### Web reader

The web reader runs in browsers. Cross-origin resource sharing (CORS) restricts scripts and canvas access to third-party images unless their server permits it.

Data fetching takes two paths:

- Anonymous reads use `GET /graphql`, falling back to POST on 404 or 405. Error-free responses without non-cacheable fields allow browser and shared caching.
- Authenticated requests, mutations, and user library updates use `POST /graphql`.

Image fetching depends on the image origin:

- Covers cached in the database load directly from `/covers/{work_id}.webp` on the API origin. For card-sized covers, the reader provides responsive variants through a `srcset` attribute at 192 pixels (1x) and 320 pixels (2x) using the `?w=` query parameter, along with the master cover at 3x.
- User avatars load from `/avatars/{user_id}.webp` on the API origin.
- For engine chapter pages, `pages` hands out `/media/v1/c/...` when the server has stored the chapter's source key. Otherwise it returns the older `/api/v1/manga/...` index path, which still works. Both load directly from the API origin with `Cache-Control: public, max-age=300, must-revalidate`. Thumbnails keep their `/api/v1/manga/...` path.
- In proxy or server image mode, MangaDex chapter pages and uncached covers route through the Cloudflare Worker at `img.komiq.cc/img?src=...` on the global server, or through `/img?src=...` on your own server. The embedded reader uses the server path without a Worker. This step adds permissive CORS headers and sets an upstream referrer.

```flow
Web browser -> Image proxy: asks for a page image
Image proxy -> MangaDex CDN: fetches image bytes
Image proxy -> Web browser: returns bytes with CORS headers
```

### Desktop app

The desktop app runs inside Tauri. Its Rust image fetcher uses native HTTP requests.

The desktop app can run Suwayomi as a child process supervised by Rust. With this embedded engine enabled, data fetching splits:

- Account authentication, library states, social features, and search queries always go to the connected server.
- For MangaDex works, `canonicalChapters` retrieves the authoritative server chapter list. It matches local chapters by number, preferring the same scanlator for duplicates or taking the first match. With a match and a ready engine, `canonicalPages` retrieves local pages over Tauri inter-process communication (IPC).
- For Suwayomi works, `pages` retrieves upstream source coordinates from the connected server via `chapterOrigin`. The local engine then attempts to locate the chapter and return its page list within an 8-second budget.

Desktop image resolution uses the native image provider:

- Embedded engine images (`engine:/api/...`) become plain addresses at once, and the web view loads them itself. On desktop the address uses the `engine-image` scheme, which Rust answers from the loopback engine. No bytes cross IPC and no blob URL is made. See [The on-device engine](/developers/native-engine).
- Assets on the configured server origin load directly from that server.
- Remote sources download directly from the source content delivery network using the Rust `fetch_image` command. They never touch the Cloudflare Worker.

The Rust image fetcher enforces strict security rules against server-side request forgery (SSRF), where an application is tricked into requesting internal targets:

1. Scheme must be HTTP or HTTPS.
2. The destination host must resolve to a public IP address. Loopback, private local networks, link-local metadata addresses, and carrier-grade NAT ranges are rejected. Only the configured API server origin is exempt.
3. The connection pins the socket to the validated IP address, preventing domain name system rebinding attacks.
4. Redirects are followed manually up to five hops, re-validating every target address.
5. Image bodies are capped at 32 MiB, with a 30-second request timeout. A 60-second budget is checked before each request attempt and retry delay.
6. A global semaphore limits concurrent image downloads to six across the entire process.

```flow
Desktop reader -> Connected server: asks for the chapter origin
Desktop reader -> Local engine: resolves the chapter via IPC
Local engine -> Source site: fetches the chapter page list
Desktop reader -> Local engine: web view loads each page by its engine-image address
Local engine -> Source site: fetches the page image
Local engine -> Desktop reader: returns the image to the web view
```

### Mobile apps (iOS and Android)

iOS and Android builds run inside Tauri. They bypass browser image proxies and download remote images through Rust using `fetch_image`.

The embedded JVM engine does not start on mobile devices unless it is chosen or enabled:

- On the iOS simulator, the supervisor is replaced by stub implementations (`suwayomi_mobile.rs`) that report an unavailable state.
- Physical iOS builds and Android builds compile the in-process JVM implementation (`suwayomi_inprocess.rs`). On Android, `engine_supported` returns true when `libjvm.so` is in the app and the phone has Android 9 or newer; see [Android](/developers/android). On desktop it is always true, and on iOS only with an engine autostart override.
- Where the engine is not offered or not chosen, mobile clients fetch chapter page lists through the connected server.

### Self-hosted server

A self-hosted komiq server fetches data and images directly from upstream services:

- When enabled, it synchronizes metadata from the MangaDex API and downloads covers to store in SQLite.
- It communicates with Suwayomi at the configured `SUWAYOMI_URL`, defaulting to loopback port 4567, to inspect installed extensions, query sources, and proxy chapter pages.
- With `KOMIKA_IMG_PROXY=on`, the server mounts an `/img` route that mirrors Cloudflare Worker validation rules. Serving the web reader enables this by default unless disabled. Self-hosted readers can proxy images without a Worker.

## Fallback ladders

The composite backend falls back when local operations fail.

### Canonical chapters ladder

When opening a MangaDex work on desktop:

1. The reader requests the chapter list from the connected server. The server list is always authoritative.
2. If the local engine is ready and the work is not marked unusable, the client retrieves source mappings and invokes `refFor` on the local backend.
3. The local engine fetches source chapters. The client matches them by number, preferring the same scanlator for duplicates or taking the first match.
4. If provisioning or resolution fails (such as an unreachable extension repository), the work ID is added to the in-memory `localUnusable` set.
5. `localUnusable` skips future reconciliation for that work. Chapters without local mappings retrieve server pages.
6. If the engine is starting up, the client skips local reconciliation for that request without recording the work in `localUnusable`.

### Suwayomi chapter pages ladder

When opening a Suwayomi chapter on desktop:

1. The client checks whether the chapter ID is in the `localUnusable` set, then whether the local engine is ready. If either check fails, it uses the connected server's `pages`.
2. The client asks the connected server for `chapterOrigin`: the source ID, the source's own keys for the manga and the chapter, and the extension to install. A failure of this request is a server fault, so it is not memoed.
3. The client checks the manga key (source ID plus manga key) in `localUnusable`.
4. The local engine installs the extension if it is missing, then resolves the manga and the chapter with `resolveChapter`.
5. If `resolveChapter` throws (the extension cannot be installed, or the source does not know the manga), the manga key is memoed as unusable. If the source's chapter list lacks that one chapter, only the chapter ID is memoed.
6. The local path has an 8-second budget. Past it, or after an error, the client returns `pages` from the connected server.
7. Local work that passed the budget continues in the background, so the next read of that manga can be served on-device.

### Offline write queue

Native apps with the composite backend queue failed writes in browser local storage under `komika.offlineWrites`:

- Marking a series in the library enqueues a `mark` operation if the server request fails.
- Updating reading progress enqueues a `progress` operation if the server request fails.
- Each operation is tagged with the active user ID.
- When the device regains network access, the queue replays stored operations in order.
- Switching user accounts or logging out purges operations belonging to other users before replay can occur.

## Configuration

Build variables, server settings and local preferences control fetching:

| Setting                      | Source            | Default                                                                           | Description                                                                                                                                                                        |
| ---------------------------- | ----------------- | --------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `PUBLIC_KOMIKA_BACKEND`      | Build environment | `on`                                                                              | Activates live API requests. When off, screens render empty states.                                                                                                                |
| `PUBLIC_KOMIKA_BACKEND_KIND` | Build environment | `komika`                                                                          | Selects `komika` (GraphQL) or `suwayomi` (direct adapter).                                                                                                                         |
| `PUBLIC_KOMIKA_IMG_MODE`     | Build environment | `direct` (dev) / `proxy` (prod)                                                   | Sets image mode: `proxy` (Worker), `server` (API `/img`), or `direct`.                                                                                                             |
| `PUBLIC_KOMIKA_IMG_WORKER`   | Build environment | `http://localhost:8787` (dev) / `https://img.komiq.cc` (global server production) | Base URL for an optional Cloudflare image Worker. Embedded builds ignore it. Native builds with a chosen server clear it.                                                          |
| `PUBLIC_KOMIKA_MEDIA_HOSTS`  | Build environment | empty | Origins a web build may load exact page objects from. Empty keeps every page on `sourceUrl`. Read [Media routes](/developers/media-routes). |
| `komiq-server`               | Local storage     | Unset                                                                             | Server origin chosen on the native connect screen.                                                                                                                                 |
| `komiq-fetch`                | Local storage     | `server`                                                                          | Native fetch execution: `device` (local engine) or `server`.                                                                                                                       |
| `appsFetchThemselves`        | Server instance   | `false`                                                                           | Set with `KOMIKA_APPS_FETCH_THEMSELVES`. Apps with an engine grey out the server choice and store `device`, which applies on the next app start. The server still answers `pages`. |

## Where to look

- `packages/api/src/composite-backend.ts`: Coordinates data and page routing between server and local backends, manages the unusable memo, and replays offline writes.
- `packages/api/src/image-provider.ts`: Implements web and native image providers, cover URL generation, and responsive cover srcset derivation.
- `packages/api/src/local-suwayomi-backend.ts`: Adapts the embedded Suwayomi engine over Tauri IPC for on-device content fetching.
- `apps/reader/src/lib/config.ts`: Evaluates build flags, stored server origins, and device fetch preferences.
- `apps/reader/src/lib/data/source.ts`: Provides data repository getters, chapter version aggregation, and parallel page fetch triggers.
- `apps/reader/src-tauri/src/lib.rs`: Implements `fetch_image` with SSRF guards, manual redirect handling, and concurrency limits.
- `apps/reader/src-tauri/src/suwayomi.rs`: Supervises the desktop JVM child process and handles the loopback image fetch (`engine_image`, behind the `engine-image` scheme and `suwayomi_image`).
- `apps/server/src/img_proxy.rs`: Implements the self-hosted `/img` proxy route in the API server.
