---
title: Media routes
description: Learn how a web reader picks the origin it loads exact page objects from, and why the image proxy Worker never touches them.
---

A media route is an origin that serves exact page objects, the `/media/v1/o/{copyId}/{sha256}` paths from [Exact media and signing](/developers/media-manifests). This page explains the route list, how a web reader chooses a route, what happens when an object fails to load, and the image proxy rule that keeps these objects out of the Worker. A self-hosted server needs none of it: the default list has one route, the server itself.

## The route list

The server answers a top-level GraphQL query:

```graphql
query MediaRoutes {
	mediaRoutes {
		id
		base
		kind
		region
	}
}
```

`kind` is `OWNER` or `REGIONAL`. `base` is an origin, `scheme://host[:port]` with no path, or an empty string for the server's own origin. The answer is the same for every viewer, so it rides the cacheable anonymous `GET /graphql` like other public reads.

The list comes from `KOMIKA_MEDIA_ROUTES`, a JSON array:

```json
[
	{ "id": "owner", "kind": "owner" },
	{ "id": "sg", "kind": "regional", "base": "https://media-sg.example.com", "region": "sg" }
]
```

Without the setting, the list is one owner entry with an empty `base`. The server refuses to start when the value is not valid. It needs 1 to 16 entries, exactly one owner, unique ids made of letters, digits, `_` and `-`, and a `base` on every regional entry. A `base` must be an `http` or `https` origin with no user name, password, path, query or fragment. The server stores it in normal form: lower case, no default port, no trailing slash.

The query has its own capability flag in the GraphQL client, `mediaRoutesUnsupported`. A server that predates the query answers `Unknown field "mediaRoutes"` once, and the client asks it no more. The line-based field stripper cannot remove a field with a sub-selection, so this works the same way as `chapterOrigin`.

## Who uses routes

Only the web build (`pnpm build` and `KOMIKA_TARGET=static`) uses routes, and only when its build-time list `PUBLIC_KOMIKA_MEDIA_HOSTS` is not empty. The list holds exact origins, separated by commas or spaces. An entry that is not a bare `http` or `https` origin fails the build. Hosts containing `*`, including subdomain wildcards, are rejected. The reader also ignores server route entries with wildcard hosts.

The reader trusts the intersection of that list and the server's list, plus the API origin as the owner. A server cannot add a host to a web build. The owner is the server's owner `base` when it is the API origin or on the list, and the API origin otherwise.

Every other build loads each page's `sourceUrl`, as before: the native apps, the static self-host build, a reader pointed at a server from the connect screen, and the `embedded` build. The `embedded` build asks `mediaRoutes` once at start, only to learn the owner's origin. It never routes pages.

## Choosing a route

A page with an exact object (`mediaPath`) loads from the session's route. The choice holds for the browser tab's session, in `sessionStorage`. Until a probe round picks one, objects load from the owner. Nothing waits for probes.

A probe is `GET {origin}/media/v1/ready`. It sends no credentials (`credentials: 'omit'`) and no headers, and its deadline is 1.5 seconds. The ready route answers `204` with `no-store` and `Access-Control-Allow-Origin: *`. Probes run only when there is more than one route: at start and after a route fails. Each route is probed at most once per 60 seconds, and a session runs at most 3 rounds. A probe never downloads an image.

The first round picks the fastest route that answered. After that, a switch needs another route faster by more than 30 % and by more than 50 ms, in two rounds in a row.

## When an object fails

```flow
Reader -> Regional route: exact object
Reader -> Owner: same path, once (region marked down)
Reader -> Server: pages again by POST, once per chapter
Reader -> Reader: changed pages load again, or a new page list
```

1. A regional object that fails loads the same path once from the owner. The region is down for the rest of the session, and a probe round starts.
2. If the owner fails too, the bytes have likely moved. The reader fetches the chapter's `pages` again, once per chapter, by POST. A POST skips the browser cache and the edge cache that hold anonymous GET answers. Every page whose URL changed loads again, unless it is already on screen.
3. If the refresh returns a different number of pages, the reader replaces the chapter's page list in place, without leaving the chapter. A page number past the new end moves to the new last page.
4. After that, a page that still fails shows the usual retry button. Each page, and the page list, is replaced by the refresh at most once, so a page never loops.

## No tokens on media

An `<img>` never sends an `Authorization` header, and probes use `credentials: 'omit'`. The GraphQL client keeps the token for the API origin. The route choice never touches the API endpoint.

## Classification

| URL | Loaded |
| --- | --- |
| An exact object (`MediaUrl`) | from its route, never through the image provider |
| The API origin, relative `/media/`, `/covers/`, `/avatars/` | direct |
| An origin on `PUBLIC_KOMIKA_MEDIA_HOSTS` | direct, never Worker-wrapped |
| Any other host | through the image proxy Worker in proxy mode |

The image provider compares origins in normal form: host names are lower case and default ports are removed.

An exact object is a typed value, `MediaUrl` (route id and path), not a string. The only way to a URL is `MediaRouter.href`, so an object cannot reach the image provider's `resolve()` by mistake.

## The Worker never proxies media

The [image proxy Worker](/developers/image-proxy) refuses any upstream URL whose path is `/media` or starts with `/media/`, on every host. It checks the path after percent-decoding, collapsing repeated slashes and lower-casing, and a path it cannot decode counts as media. It also refuses every host listed in `MEDIA_ROUTE_HOSTS`, matched exactly. The check runs on `src` and on every redirect hop, before any fetch, and answers `403` with `no-store`.

The rule exists because the Worker's host allowlist includes the global server's API host for legacy page paths. Without it, the Worker would cache exact objects outside the server's policy. Read [Deployment](/developers/deployment) for the order this must ship in.

## Content Security Policy

The web build adds the `PUBLIC_KOMIKA_MEDIA_HOSTS` origins to `connect-src`, for the probes. `img-src` already allows `https:`.

## Where to look

- `apps/server/src/media_routes.rs`: `KOMIKA_MEDIA_ROUTES` parsing and the route types.
- `apps/server/src/graphql/mod.rs`: the `mediaRoutes` resolver.
- `packages/api/src/media-routes.ts`: the trusted list, `MediaRouter`, probes and `ChapterMediaRecovery`.
- `packages/api/src/image-provider.ts`: `mediaOrigins`, which `resolve()` never wraps.
- `apps/reader/src/lib/media.ts`: the reader's router and the `embedded` owner lookup.
- `apps/reader/src/routes/read/[slug]/+page.svelte`: the fallback and refresh on an image error.
- `apps/reader/csp.ts`: `connect-src` for media hosts.
- `apps/worker/src/index.ts`: `isMediaRouteTarget`.
