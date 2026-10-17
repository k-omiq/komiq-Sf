---
title: Media packs
description: Learn how the experimental chapter pack route, its wire format and the reader's opt-in pack fetch work.
---

A media pack is one response that carries every exact page object of one chapter revision. This page covers the owner route, the wire format, the pack path in cache mode and the reader's opt-in fetch. It is for developers working on media. Packs are an experiment: the reader never fetches one unless you turn on its developer flag, and nothing else depends on them.

Read [Exact media and signing](/developers/media-manifests) first for copies, revisions and signed manifests.

## The owner route

The server answers `GET /media/v1/p/{copyId}/{rev}`. `rev` is a revision number above zero. The route has the same eligibility as the object route: with signed manifests off, it answers `404`.

| Status | When |
| --- | --- |
| `200` | The revision exists, is not stale, and the copy is allowed. |
| `404` | Bad path, unknown or stale revision, unknown copy, a denied copy, a copy of a denied series, or a taken-down chapter. |
| `500` | The manifest store cannot answer. |
| `503` | The engine is restarting. |

Every error answer has `no-store`.

A `200` answer has:

| Header | Value |
| --- | --- |
| `Content-Type` | `application/vnd.komika.pack` |
| `Content-Length` | The exact pack length, computed from the manifest. |
| `Cache-Control` | The object route's value, `s-maxage` tied to the policy lease. |
| `Access-Control-Allow-Origin` | `*`, on every answer, errors too. |

The response is never compressed: the server's compression layer skips this content type. `HEAD` answers from the manifest alone and fetches no pages.

The body streams. Before each page fetch, and again after it, the server checks the copy's policy. It fetches each unique page from the engine and compares its length and SHA-256 hash with the manifest. Bytes that differ mark the revision stale and queue the chapter again, as on the object route.

A failed check after the headers are sent cannot change the status. The server ends the body early instead, so it is shorter than `Content-Length`. A client must treat a short body as a failure for every page it has not yet verified.

## The v1 wire format

```text
"KPK1" | index length (u32, big endian) | JSON index | data
```

- The index is at most 1 MiB.
- The JSON index has `v` (always `1`), `copy_id`, `revision` and `pages`. Each page entry has `sha256`, `length`, `mime` and `offset`, in manifest page order.
- The data holds each unique object once, in order of first appearance. `offset` counts from the start of the data.
- A page whose `sha256` appeared earlier shares the first one's offset. It must have the same length and MIME type.

The pack is not signed. Trust comes from elsewhere: a cache checks it against the signed manifest of that exact revision, and the reader checks it against the `pages` answer from the API. A client never takes an offset, a length or a MIME type from the index alone. It computes the expected offsets from trusted metadata and refuses an index that differs.

## Packs in cache mode

[Media cache mode](/developers/media-cache) serves the same route, `GET /media/v1/p/{copy_id}/{rev}`:

1. It refuses a denied copy or an unusable lease before anything else.
2. It loads the signed manifest of that exact revision, from memory, disk or `/media/v1/manifest/{copyId}?rev={rev}` on the owner, and verifies it with the pinned keys.
3. It builds the expected header from that manifest. The pack's header must match it byte for byte.
4. It checks each object's length, MIME type and SHA-256 hash with the same check it uses for single objects, and checks policy again before sending it.

A manifest that fails verification, or a header that does not match, answers `502`. A stored pack is checked the same way on every hit. A damaged one is deleted.

The cache stores revision manifests in `revisions/{copy}-{rev}` and complete packs in `packs/{copy}-{rev}`. Both share the `--max-bytes` budget and LRU order with objects and manifests. A pack is written to `tmp/` and moves into place only after every object passed and the length is exact. A pack larger than the whole budget streams but is not stored.

One fill runs per pack at a time. Another request for the same pack waits up to 30 seconds, then answers `503`. If the client cancels a fill, the cache drops the owner download and its temporary file, and stores nothing.

## The GraphQL field

`Page.mediaPackPath` is an optional string, `/media/v1/p/{copyId}/{rev}`. The server sets it on every page where it sets `mediaPath`, and leaves it null otherwise.

The field is in the client's list of optional fields. An older server answers `Unknown field "mediaPackPath"`, and the client strips the field and retries, like the other `media*` fields.

## The reader flag

The flag lives in the `komiq-reader` object in `localStorage`, the same object that holds the reader's display choices:

```json
{ "packs": true }
```

Add the key to the existing object; do not replace it. The default is `false`, and only the exact value `true` turns it on. No screen shows it, but the settings screen keeps it when it saves. The reader reads it when the reader page loads, so reload after you change it.

With the flag on, the reader also asks the server for `mediaRoutes` when `PUBLIC_KOMIKA_MEDIA_HOSTS` is empty. Pages with an exact object then load it from the owner, as in [Media routes](/developers/media-routes).

## What the reader does with the flag on

For each chapter, `ChapterPackPrefetch` makes at most one pack request:

- It runs only when every page has a valid pack path for the same copy, revision and route, and every page has a hash, length and MIME type.
- It fetches from the page's chosen media route, with no credentials, no referrer, and redirects refused.
- The unique page bytes must fit in 32 MiB. The reader checks this before the request and sends none if the chapter is larger.
- Each range is checked for length, MIME type and SHA-256 hash before it shows, from a `blob:` URL.
- An idle timeout of 30 seconds restarts on every chunk, so a slow body that keeps moving is not cut.

Single objects never wait for the pack. Pages the pack does not deliver keep loading as normal single objects, with their own checks and owner fallback. A failed pack is not a page failure: it never marks a route down and never starts the page refresh. Pages it already delivered stay.

Leaving the chapter, changing the page list or closing the tab aborts the download and revokes every `blob:` URL the pack made.

## Turning it off

Set `"packs"` to `false`, or remove the key, and reload the reader. Nothing else needs removing: the reader stores no pack data, and the server route stays unused.

## Where to look

- `apps/server/src/media_pack.rs`: the wire format, `build_index`, `verify_pack` and the owner route.
- `apps/server/src/media_cache/pack.rs`: the cache-mode route, revision manifests and pack storage.
- `apps/server/src/media_cache/store.rs`: the `revisions/` and `packs/` layout and the shared budget.
- `apps/server/src/main.rs`: the route, `media_object_cors` and the compression exclusion.
- `apps/server/src/graphql/mod.rs`: `mediaPackPath` in the `pages` resolver.
- `packages/api/src/pack.ts`: `verifyPackStream`, `packFits` and the 32 MiB cap.
- `packages/api/src/media-routes.ts`: `MediaRouter.pagePack` and `ChapterPackPrefetch`.
- `packages/api/src/operations.ts`: the optional `mediaPackPath` field.
- `apps/reader/src/lib/reader-prefs.ts`: the `packs` flag.
- `apps/reader/src/routes/read/[slug]/+page.svelte`: the per-chapter fetch and cleanup.
