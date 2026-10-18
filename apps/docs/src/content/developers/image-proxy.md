---
title: The image proxy Worker
description: Use an optional image proxy Worker for your server, with request validation, caching and deployment settings.
---

Komiq can proxy remote images through `/img` on your self-hosted server or an optional Cloudflare Worker. The global server runs its Worker at `img.komiq.cc`. The Worker fetches upstream manga images and serves them with permissive cross-origin headers and long edge-cache lifetimes for the web reader.

## Why the proxy exists

Standard web browsers enforce cross-origin resource sharing (CORS), which prevents scripts and canvas elements from accessing images from third-party domains unless those domains grant permission. Upstream manga content delivery networks (CDNs) such as MangaDex do not send CORS headers and block hotlinking with referrer checks.

In Worker proxy mode, the web reader points remote image tags at the configured Worker instead of fetching directly from upstream CDNs. The global server uses `img.komiq.cc`. The Worker fetches the image bytes on the server side, sets an upstream referrer, caches the response at the Cloudflare edge, and attaches permissive CORS headers.

Native desktop and mobile applications fetch images directly in Rust using native sockets and never touch this Worker.

## Request format

The Worker's image endpoint is:

```text
GET /img?src=<url-encoded-upstream-image-url>
```

Parameters and routes:

- `src`: The absolute HTTP or HTTPS URL of the upstream image. This query parameter is required.
- `GET /` and `GET /healthz`: Return HTTP 200 with the text `ok` for health monitoring.
- `OPTIONS`: Handles CORS preflight requests, returning HTTP 204 No Content with access control headers.

Every successful response includes standard CORS headers:

```text
Access-Control-Allow-Origin: *
Access-Control-Allow-Methods: GET, HEAD, OPTIONS
Access-Control-Allow-Headers: *
Access-Control-Max-Age: 86400
```

## Validation and allow rules

The Worker enforces strict validation before sending any upstream request to prevent open-proxy abuse and server-side request forgery (SSRF).

### Upstream URL validation

The `src` parameter must satisfy three criteria:

1. The scheme must be `http` or `https`.
2. The URL must not contain embedded user credentials (`username` or `password`).
3. The URL must use standard default ports. Custom ports such as `:8080` or `:22` are rejected.

If any check fails, the Worker returns HTTP 400 Bad Request.

### Host allowlist

The Worker checks the destination hostname against the `ALLOWED_SOURCE_HOSTS` variable defined in `apps/worker/wrangler.toml`.

- The allowlist contains comma-separated hostnames.
- Hostnames match exactly or as a dot-delimited suffix. For example, `mangadex.network` permits any subdomain such as `node1.mangadex.network`.
- The committed global server configuration permits `uploads.mangadex.org` (covers), `mangadex.network` (MangaDex@Home reader nodes), and `api.komiq.cc`.
- If the allowlist is empty, the Worker denies all hosts. It fails closed by default.

The `api.komiq.cc` entry is the global server API origin. If you deploy your own Worker, set its route to your domain and configure `ALLOWED_SOURCE_HOSTS` for the upstream hosts it should fetch. Your API hostname belongs there only if the Worker fetches images from it.

### Media routes are refused

The Worker never proxies exact media objects. It refuses any upstream URL whose path is `/media` or starts with `/media/`, on every host, and any host listed in `MEDIA_ROUTE_HOSTS` (comma-separated, matched exactly, empty today). It checks the path after percent-decoding, collapsing repeated slashes and lower-casing; a path it cannot decode counts as media. The check runs on `src` and on every redirect hop, before any fetch, and answers HTTP 403 with `no-store`. Read [Media routes](/developers/media-routes).

### Hotlink protection

The Worker checks browser origin headers against the `ALLOWED_ORIGINS` variable. The global server configuration allows `https://komiq.cc` and `https://www.komiq.cc`. For your own Worker, use your reader origins instead:

- The Worker compares `Origin`, or the origin parsed from `Referer` when `Origin` is absent, against exact allowlist entries. An unparseable referrer is permitted.
- If both headers are missing, the origin check permits the request. This allows non-browser clients, automated cache-fill tasks, and curl requests through that check.
- Leaving `ALLOWED_ORIGINS` empty disables origin validation, which is intended only for local development.

## Upstream headers and redirects

When fetching from upstream CDNs, the Worker behaves as a well-formed client:

- `User-Agent`: Sent as `KomikaImageWorker/1.0 (+https://komika.app)`.
- `Referer`: Set to the origin of the target URL with a trailing slash (`scheme://host[:port]/`).
- `Accept`: Set to `image/*,*/*;q=0.8`.
- Timeout: One 25-second deadline covers every redirect hop and the image body. When it passes, the Worker returns HTTP 504.

The Worker follows redirects manually (`redirect: 'manual'`) up to four hops (`MAX_REDIRECTS = 4`). It never relies on automatic runtime redirect following.

On each redirect hop:

1. The Worker extracts the `Location` header and resolves relative paths against the current hop URL.
2. It re-validates the new target URL against the scheme, credential, port, and `ALLOWED_SOURCE_HOSTS` rules.
3. If an upstream redirect attempts to bounce to an unapproved host or an unsafe URL, the request is rejected immediately with HTTP 400.

## Caching and response delivery

The Worker operates as a pure proxy backed by Cloudflare's edge cache (`caches.default`). It does not use object storage or cloud buckets.

### Edge cache lookup

1. The Worker builds a cache key. A MangaDex chapter page (`{baseUrl}/data/{chapter hash}/{file}` or `data-saver`, on a `mangadex.network` node or `uploads.mangadex.org`) uses a content key, `${url.origin}/img/md/{folder}/{chapter hash}/{file}`. The node host is not part of it, so the same file from any MangaDex@Home node shares one entry. The file name must carry the SHA-256 of the bytes. Other URLs use the normalized upstream URL without its fragment: `${url.origin}/img?src=${encodeURIComponent(upstream)}&validated=1`.
2. It queries `caches.default`. If a cached response exists, it returns it immediately. A hit never calls the rate limiter.
3. Adding `&retry=1` to the `/img?src=...` request instructs the Worker to bypass the cache lookup and re-fetch from upstream. This helps readers recover from transient bad cache entries.

### Upstream validation and streaming

On a cache miss, the Worker verifies the upstream response:

- The HTTP status code must be successful (2xx).
- The `Content-Type` header must be a raster image format (`image/jpeg`, `image/png`, `image/webp`, `image/gif`, `image/avif`, `image/bmp`, `image/x-icon`, or `image/vnd.microsoft.icon`).
- Vector images such as SVG are rejected. SVG files can carry executable JavaScript scripts, making them an active document and a stored cross-site scripting risk.
- The declared `Content-Length` must not exceed 32 MiB (`MAX_IMAGE_BYTES`).
- The whole body, at most 32 MiB, must arrive and have complete image framing for its type.
- For a MangaDex page, the SHA-256 of the body must match the hash in the file name. One bad node therefore cannot fill the shared entry.

The Worker reads and checks the whole body before it sends or stores anything.

A MangaDex page miss fetches upstream through the Cloudflare zone cache and its upper tier (`cf.cacheEverything` with `cacheTtlByStatus`): 2xx answers are kept for 30 days and other statuses are not cached. The zone cache keys on the node URL, because custom cache keys need an Enterprise plan.

### Cache storage

When an image is fetched successfully:

- A MangaDex page is stored with `Cache-Control: public, max-age=2592000` (30 days). Browsers get `public, max-age=604800, immutable` (7 days), on a miss and on a hit.
- Other images are stored and served with `public, max-age=300, must-revalidate`, because their URLs can change content.
- Security headers `X-Content-Type-Options: nosniff` and `Content-Disposition: inline` are attached.
- The Worker writes the response into `caches.default` asynchronously using `ctx.waitUntil`. A `HEAD` miss answers with headers only and stores nothing.
- Error responses always carry `Cache-Control: no-store`.

## Limits and rate limiting

To prevent denial of service and bandwidth exhaustion, the Worker uses Cloudflare native rate limiting:

- The binding `IMG_RATE_LIMITER` is configured in `apps/worker/wrangler.toml`.
- The limit is set to 600 requests per 60 seconds per client IP address.
- The rate limiter executes only on a cache miss. Requests served directly from the edge cache are never throttled.
- If a client exceeds 600 cache misses in 60 seconds, the Worker responds with HTTP 429 Too Many Requests and `Retry-After: 10`.

## Status codes and errors

The Worker uses standard HTTP status codes:

| Code | Reason             | Cause                                                                                                                                                                                                        |
| ---- | ------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| 200  | OK                 | Successful image delivery.                                                                                                                                                                                   |
| 204  | No Content         | Successful CORS `OPTIONS` preflight.                                                                                                                                                                         |
| 400  | Bad Request        | Missing `?src`, invalid URL, forbidden host, unsafe port, or redirect to unapproved host.                                                                                                                    |
| 403  | Forbidden          | Disallowed browser `Origin` or `Referer`, or a media route in `src` or a redirect hop.                                                                                                                       |
| 404  | Not Found          | Route other than `/img`, `/`, or `/healthz`.                                                                                                                                                                 |
| 405  | Method Not Allowed | Request method other than GET, HEAD, or OPTIONS.                                                                                                                                                             |
| 429  | Too Many Requests  | Rate limit exceeded on upstream fetch misses.                                                                                                                                                                |
| 502  | Bad Gateway        | Upstream network failure, unsuccessful upstream status, missing body, non-image content type, oversized or incomplete body, page hash mismatch, invalid or missing redirect location, or too many redirects. |
| 504  | Gateway Timeout    | The 25-second upstream deadline passed.                                                                                                                                                                      |

An oversized or incomplete body returns 502 before any image byte is sent.

## Self-hosting without a Worker

Your server can serve `/img?src=...` itself. `KOMIKA_IMG_PROXY=on` enables the route, and setting `KOMIKA_WEB_DIR` enables it by default unless you disable it. The embedded reader uses this same-origin proxy. A reader built with `PUBLIC_KOMIKA_IMG_MODE=server` uses `/img` on its configured API origin.

The Rust implementation in `apps/server/src/img_proxy.rs` checks URLs, hosts, redirects, content types and image sizes. It sends a 7-day immutable cache header for every image. Its host allowlist currently matches the Worker list, including `api.komiq.cc`; it does not substitute your server hostname. The reader loads your server assets directly from the API origin instead of routing them through the Worker.

The server's `/img` refuses media routes like the Worker. It refuses a `/media` path on every host, with the same decoding rules, and the host of every regional or helper `base` in `KOMIKA_MEDIA_ROUTES`. It checks `src` and every redirect hop before it fetches, and answers HTTP 403 with `no-store`.

## How to run and deploy

The local development commands use Node.js and pnpm. The proxy needs no runtime secrets or cloud storage.

Run the development server locally:

```sh
pnpm --filter @komika/worker dev
```

This starts a local instance at `http://localhost:8787`. You can test an image fetch by passing an encoded URL:

```sh
curl -i "http://localhost:8787/img?src=https%3A%2F%2Fuploads.mangadex.org%2Fcovers%2Fexample.jpg"
```

Type-check the Worker TypeScript code without deploying:

```sh
pnpm --filter @komika/worker check
```

Deploy the Worker to Cloudflare:

```sh
pnpm --filter @komika/worker deploy
```

Configuration variables live in `apps/worker/wrangler.toml` under `[vars]`. For your own deployment, change the Worker route from the global server domain to your domain. Edit `ALLOWED_SOURCE_HOSTS` and `ALLOWED_ORIGINS` for your permitted upstream hosts and reader origins. Set `PUBLIC_KOMIKA_IMG_WORKER` when building a web reader that uses this Worker.

## Where to look

- `apps/worker/src/index.ts`: The complete Cloudflare Worker implementation, including route handling, stream byte capping, redirect loops, and edge caching.
- `apps/worker/wrangler.toml`: Worker configuration, routes, environment variables, and the `IMG_RATE_LIMITER` binding.
- `packages/api/src/image-provider.ts`: Client-side URL builder that routes web image requests through the Worker proxy.
- `apps/server/src/img_proxy.rs`: The self-hosted Rust implementation of the same URL, host, content-type, size, and redirect validation rules inside the API server.
