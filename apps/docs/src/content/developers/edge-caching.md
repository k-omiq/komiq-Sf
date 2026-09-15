---
title: Edge rendering and caching
description: Learn how a Cloudflare web reader renders and caches data, and which caching paths also apply to your own server.
---

The global server uses a Cloudflare Workers web reader to render public pages at the edge. You can also deploy this web target for your own komiq server. This guide explains server-side rendering, GraphQL caching and entity tags, which let clients check whether data has changed.

## Edge architecture

The web reader is built with SvelteKit and `@sveltejs/adapter-cloudflare`. The build produces a worker script (`_worker.js`) and a directory of static assets.

When deployed to Cloudflare:

- Static assets in `_app/immutable/` are served directly by Cloudflare Workers Assets.
- Dynamic route requests execute inside the edge worker according to the generated `_routes.json` routing configuration.

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

The public routes that render at the edge include:

- Home page (`/`)
- Series details page (`/series/[slug]`)
- Catalog updates feed (`/updates`)
- Catalog browse page (`/browse`); grid rows load client-side.
- Static information pages (`/about` and `/support`)

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

| Surface                            | Path or Route             | Cache-Control Header                  | Lifetime and Behavior                                                                    |
| ---------------------------------- | ------------------------- | ------------------------------------- | ---------------------------------------------------------------------------------------- |
| Home SSR                           | `/`                       | `public, max-age=0, s-maxage=60`      | Shared-cache lifetime: 60 seconds; browser lifetime: 0 seconds.                          |
| Series SSR                         | `/series/[slug]`          | `public, max-age=0, s-maxage=60`      | Shared-cache lifetime: 60 seconds; browser lifetime: 0 seconds.                          |
| Updates SSR                        | `/updates`                | `public, max-age=0, s-maxage=30`      | Shared-cache lifetime: 30 seconds; browser lifetime: 0 seconds.                          |
| Browse SSR                         | `/browse`                 | `public, max-age=0, s-maxage=300`     | Shared-cache lifetime: 5 minutes; browser lifetime: 0 seconds.                           |
| Static SSR                         | `/about`, `/support`      | `public, max-age=0, s-maxage=600`     | Shared-cache lifetime: 10 minutes; browser lifetime: 0 seconds.                          |
| Cacheable GraphQL query            | `GET /graphql`            | `public, max-age=30, s-maxage=60`     | Shared-cache lifetime: 60 seconds; browser lifetime: 30 seconds, with ETag revalidation. |
| Image proxy Worker (global server) | `img.komiq.cc/img`        | `public, max-age=604800, immutable`   | Edge caches for 7 days without revalidation.                                             |
| Server image proxy (self-hosted)   | `/img`                    | `public, max-age=604800, immutable`   | Browser or shared caches can store images for 7 days.                                    |
| Web app pages (self-hosted)        | `/`, `/admin`, SPA routes | `no-cache`                            | The browser checks with the server before it reuses a page.                              |
| Database Covers                    | `/covers/{work_id}.webp`  | `public, max-age=31536000, immutable` | Caches for 1 year without revalidation.                                                  |
| Avatars                            | `/avatars/{user_id}.webp` | `public, max-age=31536000, immutable` | Caches for 1 year; `?v=<timestamp>` busts cache on upload.                               |
| Extension Icons                    | `/ext-icons/{pkg}.png`    | `public, max-age=31536000, immutable` | Caches for 1 year; addressed by package name.                                            |

## Where to look

- `apps/reader/src/routes/+layout.ts`: Declares the default client-side SPA rendering mode.
- `apps/reader/src/lib/ssr.ts`: Defines the `SSR_PUBLIC` constant derived from Vite build flags.
- `apps/reader/src/routes/(app)/+page.ts`: Sets edge SSR cache headers for the home page.
- `apps/reader/src/routes/(app)/series/[slug]/+page.ts`: Sets edge SSR cache headers for series pages.
- `apps/reader/src/lib/data/load-fetch.ts`: Connects SvelteKit edge fetch data serialization to GraphQL backend calls.
- `apps/reader/wrangler.toml`: Configures the Cloudflare Worker deployment and static asset binding for the web reader.
- `apps/reader/vite.config.ts`: Configures adapter selection, stylesheet inlining thresholds, and Content Security Policy directives.
- `apps/reader/src/lib/config.ts`: Connected API origin and image proxy selection.
- `apps/reader/csp.ts`: Connection origins derived from public build settings.
- `apps/server/src/img_proxy.rs`: Self-hosted image proxy and cache headers.
- `apps/server/src/main.rs`: Implements `graphql_get_handler`, weak ETag hashing, 304 revalidation, and public asset routes.
