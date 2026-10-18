---
title: Edge rendering and caching
description: Learn how a Cloudflare web reader renders and caches data, and which caching paths also apply to your own server.
---

The global server uses a Cloudflare Workers web reader to render public pages at the edge. You can also deploy this web target for your own komiq server. This guide explains server-side rendering, GraphQL caching and entity tags, which let clients check whether data has changed.

## Edge architecture

The web reader is built with SvelteKit and `@sveltejs/adapter-cloudflare`. The build produces a worker script (`_worker.js`) and a directory of static assets.

When deployed to Cloudflare:

- Static assets in `_app/immutable/` are served directly by Cloudflare Workers Assets.
- Only `/series/*` requests run the edge Worker (`run_worker_first` in `apps/reader/wrangler.toml`). Every other path gets the static SPA shell from Workers Assets.

Embedded self-hosted readers and native desktop and mobile builds use `@sveltejs/adapter-static` instead. In those builds, `__KOMIKA_SSR__` is false, and SvelteKit generates a single-page application with an `index.html` fallback. The web build enables edge rendering specifically for public pages so their content appears in the HTML.

The hostnames in the committed deployment configuration belong to the global server: `komiq.cc` serves the reader, `api.komiq.cc` serves the API and `img.komiq.cc` serves the image Worker. For your own web deployment, the reader uses the API endpoint and optional Worker address from its public build settings. An embedded reader uses `/graphql` and `/img` on your server instead. Its static build has no edge rendering, while the server still supplies GraphQL and asset cache headers.

## Server-side rendering on public routes

The root layout in `apps/reader/src/routes/+layout.ts` sets:

```ts
export const ssr = false;
```

This defaults the entire application to client-side rendering. Public catalog routes explicitly opt into edge server-side rendering by importing `SSR_PUBLIC` from `apps/reader/src/lib/ssr.ts`:

```ts
import { SSR_PUBLIC } from '$lib/ssr';

export const ssr = SSR_PUBLIC;
```

The public routes that opt in are the home page (`/`), the series page (`/series/[slug]`), `/updates`, `/browse`, `/about` and `/support`. Only the series page reaches the edge Worker, so it is the only page rendered and cached at the edge. The others get the static shell and load their data in the browser.

The series page sets its cache header after its data loads:

- A found series gets `public, max-age=0, s-maxage=60`.
- An unknown series answers `404` with `no-store`.
- When the API cannot be reached, the page shows its retry state with `no-store`.

The adapter's Worker keeps a response in its edge cache only when the status is below 400 and the header allows it, so the last two are never cached. A series URL with query parameters the page does not read gets a `301` redirect to its clean form. The page reads `order=oldest` and `cpage`. The redirect is kept for an hour in the browser and for 60 seconds at the edge.

Interactive and authenticated routes remain client-only. The library (`/library`), user profile, login screen, and the chapter reader run entirely in the browser.

### Data serialization during hydration

SvelteKit provides a scoped `fetch` function to universal `load` functions. The reader uses this function through `api(fetch)` in `apps/reader/src/lib/data/load-fetch.ts`.

When an edge worker renders a page:

1. The `load` function issues an anonymous `GET /graphql` query using SvelteKit's `fetch`.
2. SvelteKit serializes the query response directly into the rendered HTML inside a `<script data-sveltekit-fetched>` tag.
3. The browser downloads the HTML and begins client-side hydration.
4. During hydration, the universal `load` executes again in the browser. Instead of making another round trip over the network to the GraphQL API, SvelteKit resolves the data immediately from the inlined script tag.

For signed-in users, the browser client sends an authenticated `POST /graphql` request with a bearer token. SvelteKit matches serialized responses by URL and request headers. Because the authenticated request includes authorization headers, it bypasses the serialized anonymous response and fetches personalized library marks and content settings.

### Server-side CORS simulation

SvelteKit simulates cross-origin resource sharing checks on server-side `fetch` calls. It throws an error if an upstream response lacks an `Access-Control-Allow-Origin` header that is `*` or matches the page origin.

To prevent failed edge renders:

- The komiq API server explicitly sets `Access-Control-Allow-Origin: *` for anonymous `GET /graphql` requests.
- The reader's `guarded` fetch wrapper in `apps/reader/src/lib/data/load-fetch.ts` retries errors beginning with `CORS error` through global `fetch`. Other errors propagate.

## The cacheable GET /graphql path

The reader sends anonymous GraphQL queries over HTTP GET so they can use URL-based HTTP caching.

The komiq API server exposes `/graphql` over both POST and GET:

- `POST /graphql`: Handles mutations and queries, including authenticated requests.
- `GET /graphql`: Handles query operations. Only error-free anonymous responses without non-cacheable fields receive public cache headers; authenticated or non-cacheable responses carry `Cache-Control: private, no-store`.

The `GET /graphql` pipeline in `apps/server/src/main.rs` enforces three requirements:

1. **Query operations only**: The server inspects the parsed GraphQL document before execution. If the document contains mutations, the request is rejected with HTTP 405 Method Not Allowed.
2. **Anonymous invariance**: The server checks for bearer tokens. Anonymous responses contain public data, which is invariant across viewers. Error-free anonymous responses without non-cacheable fields receive public cache headers.
3. **Sensitive field exclusion**: Queries that inspect dynamic server state (such as the `instance` field containing runtime registration settings) are marked `private, no-store` to prevent caching configuration changes.

## Entity tags and 304 revalidation

For cacheable `GET /graphql` responses, the server generates a weak entity tag (ETag) based on a 64-bit hash of the response body:

```text
ETag: W/"0123456789abcdef"
```

The server compares the calculated ETag against the client's `If-None-Match` request header:

- If the ETag matches, the server returns HTTP 304 Not Modified with an empty body, retaining the `ETag` and `Cache-Control` headers.
- If the ETag does not match or the header is absent, the server returns HTTP 200 OK with the JSON payload.

This allows Cloudflare edge nodes and user browsers to revalidate catalog queries with minimal bandwidth.

## Cache lifetimes summary

Komiq applies tailored cache durations across each layer of the architecture:

| Surface                                      | Path or Route                                                                           | Cache-Control Header                                | Lifetime and Behavior                                                                                                                                    |
| -------------------------------------------- | --------------------------------------------------------------------------------------- | --------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Series SSR, found                            | `/series/[slug]`                                                                        | `public, max-age=0, s-maxage=60`                    | Shared-cache lifetime: 60 seconds; browser lifetime: 0 seconds.                                                                                          |
| Series SSR, unknown or API error             | `/series/[slug]`                                                                        | `no-store`                                          | Never cached. An unknown series answers `404`.                                                                                                           |
| Series URL with unused parameters            | `/series/[slug]?…`                                                                      | `public, max-age=3600, s-maxage=60`                 | `301` redirect to the clean URL.                                                                                                                         |
| Web reader shell                             | Every other document                                                                    | `no-cache`                                          | The browser checks with Workers Assets before it reuses the shell.                                                                                       |
| Web reader app files                         | `/_app/immutable/*`                                                                     | `public, max-age=604800, immutable`                 | Browser lifetime: 7 days. A missing file can be cached as `index.html` for up to 7 days, so every release goes through `apps/reader/deploy/release.mjs`. |
| Cacheable GraphQL query                      | `GET /graphql`                                                                          | `public, max-age=30, s-maxage=60`                   | Shared-cache lifetime: 60 seconds; browser lifetime: 30 seconds, with ETag revalidation.                                                                 |
| Image proxy Worker (global server)           | `img.komiq.cc/img`                                                                      | `public, max-age=604800, immutable`                 | Edge caches for 7 days without revalidation.                                                                                                             |
| Server image proxy (self-hosted)             | `/img`                                                                                  | `public, max-age=604800, immutable`                 | Browser or shared caches can store images for 7 days.                                                                                                    |
| Web app pages (self-hosted)                  | `/`, `/admin`, SPA routes                                                               | `no-cache`                                          | The browser checks with the server before it reuses a page.                                                                                              |
| Cached covers, current version               | `/covers/{work_id}.webp?v={version}`                                                    | `public, max-age=31536000, immutable, s-maxage=600` | Browser lifetime: 1 year; shared-cache lifetime: one media policy lease (10 minutes).                                                                    |
| Avatars, current version                     | `/avatars/{user_id}.webp?v={version}`                                                   | `public, max-age=31536000, immutable, s-maxage=600` | Browser lifetime: 1 year; shared-cache lifetime: 10 minutes.                                                                                             |
| Covers and avatars, newer requested version  | `/covers/{work_id}.webp?v={version}`, `/avatars/{user_id}.webp?v={version}`             | `no-store`                                          | Serve the stored bytes; never redirect backward.                                                                                                         |
| Covers and avatars, older or invalid version | Same versioned paths                                                                    | `no-store`                                          | 307 redirect forward to the current version.                                                                                                             |
| Engine chapter pages                         | `/media/v1/c/{chapterId}/{page}`, `/api/v1/manga/{mangaId}/chapter/{index}/page/{page}` | `public, max-age=300, must-revalidate`              | Revalidate after 5 minutes.                                                                                                                              |
| Extension icons                              | `/ext-icons/{pkg}.png`                                                                  | `public, max-age=86400, must-revalidate`            | Revalidate after 1 day with a strong ETag; a matching `If-None-Match` returns 304 without a body.                                                        |

Cover and avatar URLs with an older or invalid version return a 307 redirect to the current version with `Cache-Control: no-store`. A requested version newer than the stored version serves the stored bytes with `no-store`. It never redirects backward or uses immutable caching. URLs without `v` redirect to the current version with `public, max-age=300`. Cover redirects keep an allowlisted `w` width. Avatar uploads always increase the version, even within one millisecond.

A missing cover file is restored from PostgreSQL when an uploaded cover is available. Otherwise the server keeps the lazy MangaDex fetch path. That fetch response has `public, max-age=300, must-revalidate` without `v` and `no-store` with `v`, because the fetched bytes may not be the version the URL names. Suwayomi source covers use that same cache lifetime. If a derivative resize fails, the server serves the master with `no-store`. Missing covers, avatars and comment images also return `no-store`.

`/media/v1/c/{chapterId}/{page}` is a chapter-id locator. The server looks up the chapter's current position at the engine. It keeps that result in a 30-second memo and drops it at the start and end of server refreshes. After a refresh with an unknown outcome, it turns the memo off for 60 seconds. Locator pages fetched during that window carry `no-store`, including fetches that finish after the window ends. Page fetches on both paths wait up to 10 seconds while the server refreshes that series, then return `503` with `no-store`. On the locator path, if a refresh starts or ends while a page is being fetched, the server drops those bytes and fetches the page once more. An engine refresh the server did not start can still cause a mismatch for up to 30 seconds.

## Where to look

- `apps/reader/src/routes/+layout.ts`: Declares the default client-side SPA rendering mode.
- `apps/reader/src/lib/ssr.ts`: Defines the `SSR_PUBLIC` constant derived from Vite build flags.
- `apps/reader/src/routes/(app)/series/[slug]/+page.ts`: Sets the series cache header after the data loads.
- `apps/reader/src/hooks.server.ts` and `apps/reader/src/lib/series-url.ts`: Redirects series URLs to their clean query string.
- `apps/reader/deploy/static-assets.mjs`: Writes the shell and app file headers.
- `apps/reader/src/lib/data/load-fetch.ts`: Connects SvelteKit edge fetch data serialization to GraphQL backend calls.
- `apps/reader/wrangler.toml`: Configures the Cloudflare Worker deployment and static asset binding for the web reader.
- `apps/reader/vite.config.ts`: Configures adapter selection, stylesheet inlining thresholds, and Content Security Policy directives.
- `apps/reader/src/lib/config.ts`: Connected API origin and image proxy selection.
- `apps/reader/csp.ts`: Connection origins derived from public build settings.
- `apps/server/src/img_proxy.rs`: Self-hosted image proxy and cache headers.
- `apps/server/src/main.rs`: Implements `graphql_get_handler`, weak ETag hashing, 304 revalidation, and public asset routes.
