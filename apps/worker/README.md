# @komika/worker — image proxy

A Cloudflare Worker that fetches upstream manga image bytes server-side and
re-serves them with permissive CORS and a bounded cache lifetime. The web reader can't
cross-origin-fetch arbitrary source CDNs (CORS), so it points its `<img src>` at
this Worker instead. The Worker is a **pure proxy backed by the Cloudflare edge
cache** — there is no object-storage tier and it needs no secrets.

> Native (desktop/mobile) builds fetch images directly in Rust and never touch
> this Worker; it exists solely for the CORS-bound web build.

## URL scheme

```
GET /img?src=<url-encoded upstream image URL>
```

- **`src`** (required) — absolute `http(s)` URL of the upstream image. Rejected
  with `400` if missing, not `http(s)`, or not in the `ALLOWED_SOURCE_HOSTS`
  allowlist. The upstream response must declare a supported raster MIME type and
  contain a nonempty image with matching container framing. SVG, mislabeled bodies,
  incomplete images and bodies over 32 MiB are rejected with an uncacheable `502`.

Also: `GET /` and `GET /healthz` return `ok`. `OPTIONS` is handled as a CORS
preflight.

## Media routes are never proxied

The Worker refuses `/media` and `/media/…` on every host after percent-decoding,
collapsing repeated slashes and lowercasing the path; undecodable paths also fail
closed. `MEDIA_ROUTE_HOSTS` adds a comma-separated list of regional media hostnames
or origin URLs (none today), matched by exact hostname after lowercasing and
removing a trailing dot. These checks apply to `src` and every redirect hop;
refusals return an uncacheable `403` before fetching the media target.

Deploy this guard **before** `/media/v1/o` is enabled for the hosted owner, because
`api.komiq.cc` remains in `ALLOWED_SOURCE_HOSTS` for legacy page paths. Deployment
requires the owner's OK.

## Caching

1. **Cloudflare edge** (`caches.default`). A MangaDex chapter page
   (`…/data/{chapter hash}/{n}-{sha256}.{ext}` or `data-saver`, on a
   `*.mangadex.network` node or `uploads.mangadex.org`) keys on
   `/img/md/{folder}/{chapter hash}/{file}`, so every MangaDex@Home node URL for
   one file shares an entry. It is stored with `public, max-age=2592000` (30 days)
   and sent to browsers as `public, max-age=604800, immutable` (7 days). Other
   URLs key on the normalized upstream URL plus an internal validation-version
   token, so hotlink/query noise shares one entry. They are mutable and use
   `Cache-Control: public, max-age=300, must-revalidate`. A hit never calls the
   rate limiter.
2. **Upstream** — origin of last resort, with one 25 s deadline across every
   redirect hop and the body (504 when it passes). A page miss goes through the
   zone cache and its upper tier (`cf.cacheEverything`, 2xx kept 30 days, other
   statuses not cached); that tier keys on the node URL, because custom cache keys
   are Enterprise-only. The response is written back to the edge cache only after
   a capped body read and raster-container validation, and for a page only when
   the body's SHA-256 matches the file name, so one bad node cannot fill the shared
   entry. A HEAD miss stores nothing. Errors are `no-store`; a 429 carries
   `Retry-After: 10`.

Container validation checks framing, MIME and recognized signatures; it does not
decode every pixel or authenticate source art. A future signed manifest must bind
exact revisions and digests before regional caches can promise exact bytes.

Validation buffers up to 32 MiB before returning an image, including when the
upstream supplies Content-Length. This changes first-byte latency, CPU and isolate
memory use from the earlier streaming path. Realistic image sizes, concurrent
cold fills, CPU limits and slow clients require an isolated Worker performance
gate before deployment. Previously issued browser `immutable` entries cannot be
recalled by changing these response headers; rollout needs fresh public locators
or an explicit cache-bust plan.

There is no B2 / object-storage tier (removed in favor of edge-cache-only).

## Run locally

```sh
pnpm --filter @komika/worker dev     # wrangler dev on http://localhost:8787
```

Type-check without deploying:

```sh
pnpm --filter @komika/worker check   # tsc --noEmit
```

Deploy:

```sh
pnpm --filter @komika/worker deploy  # wrangler deploy
```

## Configuration

Non-secret config lives in `wrangler.toml` under `[vars]`. The Worker needs no
secrets (`wrangler secret put` / `.dev.vars` are not required).

| var                    | meaning                                                                                                                                                                                                                                                                                                                                                               |
| ---------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ALLOWED_SOURCE_HOSTS` | Comma-separated upstream host allowlist (prevents open-proxy abuse). **Empty = deny all (fail closed).** Matches host exactly or as a suffix. Ships defaulting to the MangaDex hosts: `uploads.mangadex.org` (covers) **and** `mangadex.network` (the `*.mangadex.network` MangaDex@Home page nodes — one suffix covers every node). Add more hosts as you need them. |
| `ALLOWED_ORIGINS`      | Comma-separated browser Origin/Referer allowlist (hotlink protection). Empty = disabled. Requests with no Origin/Referer are always allowed (native apps, cache-fill jobs). Abuse prevention is carried by `ALLOWED_SOURCE_HOSTS`, not this.                                                                                                                          |

## Pointing the reader at it

The reader reads two `PUBLIC_*` env vars (see `apps/reader/src/lib/config.ts`):

```sh
PUBLIC_KOMIKA_IMG_MODE=proxy                    # (default) route through this Worker
PUBLIC_KOMIKA_IMG_WORKER=http://localhost:8787  # Worker base URL (this default)
```

In proxy mode the reader builds `${PUBLIC_KOMIKA_IMG_WORKER}/img?src=<encoded>`,
which matches this Worker's scheme exactly. Set `PUBLIC_KOMIKA_IMG_MODE=direct`
to bypass the Worker and use source URLs unchanged — only for already-CORS-safe
hosts (e.g. a Suwayomi server that proxies images itself). Combining `direct`
with MangaDex hosts breaks images via CORS/hotlink; the reader warns when it
detects this.
