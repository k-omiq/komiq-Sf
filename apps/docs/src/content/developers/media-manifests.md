---
title: Exact media and signing
description: Learn how a server files chapter pages under exact byte hashes, signs manifests with Ed25519 and serves exact objects.
---

A server can give each chapter page a SHA-256 hash, a fingerprint of its bytes, and sign a list called a manifest. This page explains exact objects and policy leases, signed permissions for caches to serve content for a limited time. Exact media is off by default and optional for self-hosting.

## Turn it on

Set `KOMIKA_MEDIA_MANIFESTS=on`. Any other value, or no value, leaves it off. The signing key file is `KOMIKA_MEDIA_SIGNING_KEY_FILE`, by default `media-signing.key` in the server's working directory (`/data` in the Docker image). Read [Configuration](/self-host/configuration).

With the feature off, the server makes no signing key and runs no ingest. `pages` returns the four `media*` fields as null. Takedowns and site-wide block rules still apply.

## Copies, objects and revisions

A **copy** is one upload of one chapter by one source, identified by source id, manga key and chapter key from `engine_manga_key` and `engine_chapter_key`. Its id is `mc_` plus 32 lowercase hex characters. It does not depend on numeric engine ids, so a reseeded engine keeps the same copies.

Four PostgreSQL tables hold the data:

| Table | Holds |
| --- | --- |
| `media_object` | One row per byte hash: length, MIME type (content type), width and height when known. |
| `media_copy` | One row per copy identity. No foreign key to the catalogue. |
| `media_manifest` | One row per revision of a copy: page count, policy epoch, creation time, `superseded_at`, `stale`. |
| `media_manifest_page` | The ordered page hashes of each revision. |

A **revision** is one ordered list of page hashes. Changed pages produce revision n+1; an unchanged list writes nothing. A replaced revision stays for 7 days as a signed record. Cleanup then removes it and any unused hashes.

All four tables are rebuildable. The next ingest refills them.

## The ingest pass

Opening a chapter queues it: `pages` adds the chapter id to an in-memory queue. The ingest pass fetches and hashes its pages 90 seconds later, after the reader's own fetches.

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

Before and after ingest, the server checks the chapter's source id, manga key, chapter key and position. A different key makes the server quarantine the engine and check it again. Read [Suwayomi](/developers/suwayomi).

The copy and finished revision are written in one transaction under the engine generation lock. A busy pool, exhausted budget, series refresh or engine change abandons the pass without writing. A later open queues it again. A copy with no usable revision, such as a changed page count, can be queued inside the one-hour wait.

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
| `/media/v1/o/{copyId}/{sha256}` | the page bytes, after hash and policy checks | `public, max-age=31536000, immutable, s-maxage=<remaining lease seconds>` (at most 600) |
| `/media/v1/manifest/{copyId}[?rev=n]` | the signed manifest | `public, max-age=60` |
| `/media/v1/keys` | the signed key set | `public, max-age=60` |
| `/media/v1/policy` | the signed policy lease | `public, max-age=60` |
| `/media/v1/ready` | `204`, for route probes, also with the feature off | `no-store`, `Access-Control-Allow-Origin: *` |

The server keeps no page bytes. For an object request it finds the newest kept revision naming the hash, checks site-wide block rules and takedowns for the copy's own series, fetches the page and hashes it. It checks the current policy again after the fetch, so a takedown noticed during a slow fill also refuses that response. A denied copy, a copy of a denied series or a taken-down chapter answers `404` with `no-store, must-revalidate`, even if another copy has identical bytes. The manifest route also refuses a denied copy.

Bytes that differ mark the revision stale, queue the chapter again and return `404` with `no-store`. A stale latest revision has no manifest without `?rev`, and `pages` gives no exact paths for it.

Every error answer has `no-store`. No media route reads `Authorization` or cookies. With the feature off, the object, manifest, key and policy routes answer `404`.

## Policy epoch and takedowns

The **policy epoch** is a rising number in `media_policy`. It moves in the same transaction as each site-wide block rule change, takedown addition or lift, and merge or split carrying a takedown. A new epoch is at least the current time in milliseconds. Site-wide rules close chapter pages but leave covers open.

`media_takedown` records work and copy takedowns. Denied works include old ids redirecting to them. A work takedown also records its current copies. Merges move takedowns to survivors; splits copy them to new works. See [Takedowns](/admin/takedowns) for effects on lists, pages and covers, manual lifting, the admin API and CLI.

The server holds the denied set in memory. It re-reads the epoch at most once every 5 seconds and rebuilds the set when it moves, and every 60 seconds anyway. This replaces the per-request block-rule read on page routes. Admin console rule changes and admin API takedowns invalidate it at once on that server. CLI changes reach a running server within about 5 seconds.

## Policy lease

`GET /media/v1/policy` uses the same signed envelope and key as manifests. Its payload holds `v`, `epoch`, `issued_at`, `expires_at`, `denied_copies`, `denied_works` and `kid`. Times are Unix seconds. The lease lasts 600 seconds from the current minute's start. The server reuses signed bytes per epoch and per minute.

Exact objects set `s-maxage`, their shared-cache lifetime, to the remaining life of the current minute's lease when the response is built, at most 600 seconds. Its expiry can be up to one minute later than the lease that authorized the response. Shared caches such as a content delivery network (CDN) can reuse the bytes until that expiry. This cannot recall bytes already in a browser, offline chapters or saved copies. Older chapter page and source cover responses keep their 300-second lifetime. Current versioned covers allow 600 seconds in shared caches and a year in browsers. See [Edge rendering and caching](/developers/edge-caching).

## Page fields

`pages` adds `mediaPath`, `mediaSha256`, `mediaLength` and `mediaMime`. They are set only when the latest revision is not stale and has as many pages as the engine lists. `mediaPath` is a path without an origin. The reader's GraphQL client lists the four names as optional fields, so it removes them for an older server. Only a web build with media routes loads `mediaPath`; every other build keeps loading `sourceUrl`. Read [Media routes](/developers/media-routes).

## Verifying

`apps/server/src/media_verify.rs` is the verifier for caches that pin the server's public key. It checks the signature over the exact payload bytes before it reads them, then checks length, MIME type and hash of each object. A key set counts only when a pinned key signed it.

`verify_lease` verifies a signed lease and refuses a lifetime longer than 900 seconds, the owner's 600-second lease plus 5 minutes of clock skew. Pass it to `PolicyGate::accept`, which also refuses an issue time more than 5 minutes ahead of the cache's clock, an expired lease or an older epoch. Without an accepted lease, the gate refuses serving. `PolicyGate::object` refuses a denied copy or a lease older than the manifest's `policy_epoch`. `PolicyGate::work` refuses a denied work, including an old id in a cover URL.

The gate returns the remaining lease life as `s-maxage`. After expiry, it keeps serving only what the last lease did not deny for 6 hours (`LEASE_GRACE_SECS`), with `s-maxage=0`. After that it fails closed, refusing serving until a valid lease is accepted. Denied content stays denied during the grace period.

## Where to look

- `apps/server/src/media_manifest/ingest.rs`: the queue, the budget and the ingest pass.
- `apps/server/src/media_manifest/store.rs`: copies, revisions, stale marks and cleanup.
- `apps/server/src/media_manifest/signing.rs`: the key file and the signer.
- `apps/server/src/media_policy.rs`: takedowns, epochs, the denied set and signed leases.
- `apps/server/src/media_verify.rs`: wire types and the verifier.
- `apps/server/src/main.rs`: page, object, manifest, policy, key and probe routes.
- `apps/server/src/graphql/mod.rs`: admin takedown operations and viewer gates.
- `apps/server/src/block.rs`: block rules and the takedown list gate.
- `apps/server/src/cover.rs`: cover deletion and passes that skip takedowns.
- `apps/server/migrations_pg/0007_media_manifest.sql`: the four tables.
- `apps/server/migrations_pg/0009_media_policy.sql`: policy and takedown tables.
