# @komika/worker — image proxy

A Cloudflare Worker that fetches upstream manga image bytes server-side and
re-serves them with permissive CORS and long cache headers. The web reader can't
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
  allowlist. The upstream response must be an `image/*` type or it's rejected
  `502`.

Also: `GET /` and `GET /healthz` return `ok`. `OPTIONS` is handled as a CORS
preflight.

## Caching

1. **Cloudflare edge** (`caches.default`) — keyed on the normalized upstream URL,
   so hotlink/query noise shares one entry. Served with a 7-day immutable
   `Cache-Control`.
2. **Upstream** — origin of last resort; the response is written back to the edge
   cache.

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

| var                    | meaning                                                                                                                                                                                                                                                                                                             |
| ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ALLOWED_SOURCE_HOSTS` | Comma-separated upstream host allowlist (prevents open-proxy abuse). **Empty = deny all (fail closed).** Matches host exactly or as a suffix. Ships defaulting to the MangaDex hosts: `uploads.mangadex.org` (covers) **and** `mangadex.network` (the `*.mangadex.network` MangaDex@Home page nodes — one suffix covers every node). Add more hosts as you need them. |
| `ALLOWED_ORIGINS`      | Comma-separated browser Origin/Referer allowlist (hotlink protection). Empty = disabled. Requests with no Origin/Referer are always allowed (native apps, cache-fill jobs). Abuse prevention is carried by `ALLOWED_SOURCE_HOSTS`, not this.                                                                            |

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
