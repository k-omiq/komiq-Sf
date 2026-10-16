---
title: Exact media and signing
description: Learn how a server files chapter pages under exact byte hashes, signs manifests with Ed25519 and serves exact objects.
---

A server can file each chapter page it serves under the SHA-256 hash of its bytes. A signed list of those hashes, a manifest, lets another cache fetch the pages and prove they are the bytes the server meant. This page explains the tables, the ingest pass, the signing key and the `/media/v1` routes. The feature is off by default, and a self-hosted server never needs it.

## Turn it on

Set `KOMIKA_MEDIA_MANIFESTS=on`. Any other value, or no value, leaves it off. The signing key file is `KOMIKA_MEDIA_SIGNING_KEY_FILE`, by default `media-signing.key` in the server's working directory (`/data` in the Docker image). Read [Configuration](/self-host/configuration).

With the feature off, the server makes no key, runs no ingest, and reads or writes none of the media tables. `pages` returns the four `media*` fields as null.

## Copies, objects and revisions

A **copy** is one upload of one chapter by one source: the source id, the source's own manga key and its own chapter key. These keys come from the engine key rows that the server already stores (`engine_manga_key`, `engine_chapter_key`). A copy id looks like `mc_` plus 32 hex characters. It does not depend on the engine's numeric ids, so a reseeded engine keeps the same copies.

Four PostgreSQL tables hold the data:

| Table | Holds |
| --- | --- |
| `media_object` | One row per byte hash: length, MIME type, width and height when known. |
| `media_copy` | One row per copy identity. No foreign key to the catalogue. |
| `media_manifest` | One row per revision of a copy: page count, policy epoch, creation time, `superseded_at`, `stale`. |
| `media_manifest_page` | The ordered page hashes of each revision. |

A **revision** is one ordered list of page hashes. When a page is added, removed, swapped or redrawn, the next ingest writes revision n+1. An unchanged list writes nothing. A revision that a newer one replaced stays for 7 days as a signed record, then a cleanup removes it and any hash no page uses.

All four tables are rebuildable. A reverse export to SQLite leaves them out and lists their row counts in the report. The next ingest refills them.

## The ingest pass

Opening a chapter queues it: the `pages` query adds the chapter id to an in-memory queue. The pass runs 90 seconds later, so it follows the reader's own page fetches. A test showed that the engine then answers every page from its own cache: the pass downloaded under 0.5 % of the page bytes from the network.

```flow
Reader app -> Server: pages (chapter opened)
Server -> Ingest queue: chapter id, run in 90 s
Ingest -> Engine: every page, one after another
Ingest -> PostgreSQL: new revision, only if the list changed
```

The pass has these limits:

- Two chapters at a time, one page at a time.
- Every page goes through the server's page fetch pool. The pass waits while fewer than two pool slots are free, so readers keep the pool.
- A per-source budget of 600 ingest page fetches per hour.
- A chapter that finished stays out of the queue for one hour.
- The queue holds at most 1,024 chapters.

Before and after the pass, the server asks the engine for the chapter again. The source id, manga key and chapter key must still match the stored ones, and the chapter must still be at the same position. A different key counts as a write guard mismatch, as in every other engine write: the server quarantines the engine and checks it again. Read [Suwayomi](/developers/suwayomi).

The pass keeps hashes in memory and writes only a finished revision. The copy row and the revision are written in one transaction, under the engine generation lock. A busy pool, an exhausted budget, a refresh of the series or an engine change abandons the pass, and nothing is written. A later open queues the chapter again. When a reader opens a chapter whose copy has no usable revision, for example because the page count changed, the chapter is queued again even inside the one-hour wait.

## Signing

The server signs with Ed25519 through `ring`. It makes the key only when the feature is on. If the file is missing, it writes a new PKCS#8 key with mode `0600` and links it into place, so the file is never half written. It refuses to start when the file is not a valid key or other users can read it. The log shows the key id and the path, never the key.

The key id (`kid`) is the hex of the first 8 bytes of the SHA-256 of the raw public key.

A signed document is an envelope:

```json
{ "payload": "<canonical JSON text>", "kid": "<key id>", "sig": "<base64url signature of the payload bytes>" }
```

A manifest payload holds `v`, `copy_id`, `copy` (source id, manga key, chapter key), `revision`, `policy_epoch`, `issued_at`, `pages` (`sha256`, `length`, `mime`, `width`, `height`) and `kid`. `issued_at` is the revision's creation time, so a revision always signs to the same bytes. It never holds source URLs, engine ids, cookies or account data.

## Routes

| Route | Answers | Cache-Control |
| --- | --- | --- |
| `/media/v1/o/{copyId}/{sha256}` | the page bytes, after the server checks their hash | `public, max-age=31536000, immutable, s-maxage=600` |
| `/media/v1/manifest/{copyId}[?rev=n]` | the signed manifest | `public, max-age=60` |
| `/media/v1/keys` | the signed key set | `public, max-age=60` |
| `/media/v1/ready` | `204`, for route probes, also with the feature off | `no-store`, `Access-Control-Allow-Origin: *` |

The server keeps no page bytes. For an object request it finds the newest kept revision that names the hash, checks the site-wide block rules for the copy's own series, fetches that page from the engine and hashes it. Bytes that differ are never served: the server marks the revision stale, queues the chapter again and answers `404` with `no-store`. A stale latest revision has no manifest without `?rev`, and `pages` hands out no exact paths for it.

Every error answer has `no-store`. No media route reads `Authorization` or cookies. With the feature off, the object, manifest and key routes answer `404`.

## Page fields

`pages` adds `mediaPath`, `mediaSha256`, `mediaLength` and `mediaMime`. They are set only when the latest revision is not stale and has as many pages as the engine lists. `mediaPath` is a path without an origin. The reader's GraphQL client lists the four names as optional fields, so it removes them for an older server. Readers keep loading `sourceUrl` for now.

## Verifying

`apps/server/src/media_verify.rs` is the verifier for caches that pin the server's public key. It checks the signature over the exact payload bytes before it reads them, then checks length, MIME type and hash of each object. A key set counts only when a pinned key signed it.

## Where to look

- `apps/server/src/media_manifest/ingest.rs`: the queue, the budget and the ingest pass.
- `apps/server/src/media_manifest/store.rs`: copies, revisions, stale marks and cleanup.
- `apps/server/src/media_manifest/signing.rs`: the key file and the signer.
- `apps/server/src/media_verify.rs`: wire types and the verifier.
- `apps/server/src/main.rs`: `serve_media_object`, `serve_media_manifest`, `serve_media_keys`, `serve_media_ready`.
- `apps/server/migrations_pg/0007_media_manifest.sql`: the four tables.
