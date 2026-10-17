---
title: Media cache mode
description: Follow helper fetches, byte checks, policy leases and disk storage in komika-server media-cache mode.
---

`komika-server media-cache` runs a helper that caches a komiq server's exact page objects. This page covers the request path, trust checks, serving policy and disk store for developers changing this mode.

## A miss from reader to owner

The **owner** is the server named by `--owner`. A **copy** identifies one source's upload of one chapter. A signed **manifest** lists that copy's page hashes, lengths and MIME types, the content types used for HTTP responses. A hash is a fingerprint of the page's bytes. Read [Exact media and signing](/developers/media-manifests) for the wire format and [Run a media cache](/self-host/media-cache) for setup.

The helper serves `GET /media/v1/o/{copy_id}/{sha256}`. With no stored manifest or object, the request takes this path:

```flow
Reader -> Helper: asks for a copy's exact object
Helper -> Owner: fetches the copy's signed manifest
Helper -> Helper: verifies signature, copy id and hash membership
Helper -> Helper: checks the policy lease, renewing if needed
Helper -> Owner: fetches the object within the manifest length
Helper -> Store: saves bytes after checking length, type and hash
Helper -> Helper: checks the policy lease again
Helper -> Reader: returns the verified object
```

Manifests come from memory, then disk, then the owner. Verified manifests are held in memory, with the map cleared when adding an entry at its 4,096-entry limit. A missing hash triggers another manifest fetch at most once per 60 seconds. An owner's `404` for a copy is also remembered for 60 seconds. A stored hash never grants access to a different copy by itself.

## Trust and byte checks

On first start, `load_or_pin` fetches `/media/v1/keys`. `first_pins` checks the self-signed key set before saving the owner origin and public keys in `pins.json`. `--expect-kid` requires the named key to sign that first key set. Use HTTPS and a key id obtained from the owner to check this first contact.

Later starts keep the pins. A key set signed by an unpinned key, or listing a new key, produces a warning rather than replacing the pins. `media-cache repin` is the explicit replacement command. A helper loads pins into memory at start, so restart it after repinning. A folder pinned to another owner is refused.

Each manifest loaded from disk or fetched from the owner goes through `verify_manifest`. It checks the Ed25519 signature over the exact payload bytes using the pins, then the version, matching key ids and page hash syntax. The cache separately binds `copy_id` to the requested copy. The requested SHA-256 hash must appear in that copy's manifest before any object fetch or disk hit can serve it.

`verify_object` checks the fetched body's exact length, MIME type without parameters and SHA-256 hash. The HTTP client caps the body at the manifest length, both through `Content-Length` when present and while reading chunks. Signed documents are capped at 8 MiB. The client accepts only status `200` and follows no redirects.

Every disk object hit goes through `verify_object` again. Disk objects have no separate saved MIME header; the helper checks them against the manifest's MIME type and serves that type. A damaged object is deleted and fetched again. A disk manifest that fails verification or names another copy is also deleted before trying the owner.

## The policy lease

A **policy lease** is signed permission to serve under the owner's current denied-copy list and policy epoch, a rising policy version. `renew_lease` fetches `/media/v1/policy`, calls `verify_lease` and passes the result to `PolicyGate::accept`. The gate refuses expired leases, older epochs, issue times more than five minutes ahead and malformed or overlong leases.

The renewal loop runs immediately and every 60 seconds. An object request that gets `NoLease`, `Expired` or `EpochBehind` also attempts renewal, then checks once more. These request-driven attempts share a lock and happen at most once per 10 seconds. A lease older than the manifest's `policy_epoch` cannot authorize the object.

The helper checks permission before reading or fetching an object and again before sending it. A denial accepted during a slow fetch therefore wins. A denied copy answers `404` even if its bytes are stored or shared with an allowed copy.

While the lease is valid, the gate returns its remaining lifetime as `s-maxage`, the time a shared HTTP cache may reuse the response. After expiry, `LEASE_GRACE_SECS` allows six hours of serving copies that the last lease did not deny, with `s-maxage=0`. Denied copies remain denied. At the end of grace, objects need a renewed lease or answer `503`.

The lease lives only in memory. A restart with the owner down serves nothing, even when pins, manifests and objects survive on disk. `GET /media/v1/ready` returns `204` for a valid lease or its grace period, otherwise `503`. It sends `Cache-Control: no-store` and `Access-Control-Allow-Origin: *`. `GET /health` returns `200` for the running process and is the server image's health-check target. `GET /health/ready` answers like `/media/v1/ready` for the self-host image's health check.

## Store and concurrent requests

The cache folder contains:

```text
pins.json
objects/{first two hash characters}/{sha256}
manifests/{copy_id}
revisions/{copy_id}-{rev}
packs/{copy_id}-{rev}
tmp/
```

`revisions/` and `packs/` hold the experimental chapter packs and their exact revision manifests. Read [Media packs](/developers/media-packs).

Objects, packs and signed manifest envelopes share the `--max-bytes` budget. The least recently used (LRU) files are evicted first. Reads update an in-memory use order. On restart, the store rebuilds that order from modification times and evicts down to the current budget. `pins.json` is outside that accounting. A value larger than the whole budget can be served after verification but is not stored.

Writes use unique temporary files, sync their contents, then rename them into place. Startup clears leftover temporary files and removes invalid entries from the store directories. Renames, index updates and evictions share the index lock. A blocking write finishes even if its request is cancelled. Objects and manifests are checked again when read, so damaged files cannot bypass verification. Pin writes also sync the containing directory after renaming.

One owner fetch per object hash can run at a time. Concurrent misses wait on the same key, then check disk again. Manifest fetches use a lock per copy. Completed or cancelled turns release their locks and remove unused entries from the in-flight map. A failed fetch or a value that cannot be stored can require another request to fetch it again.

## Responses and limits

Successful objects answer `200` with these headers:

| Header                        | Value                                                                                                 |
| ----------------------------- | ----------------------------------------------------------------------------------------------------- |
| `Cache-Control`               | `public, max-age=31536000, immutable, s-maxage=<remaining lease seconds>`; `s-maxage=0` during grace. |
| `Access-Control-Allow-Origin` | `*`, so readers from other origins can fetch and check the bytes.                                     |
| `X-Komika-Cache`              | `hit` for a verified disk hit, `miss` when the request entered the miss path.                         |
| `Content-Security-Policy`     | `sandbox; default-src 'none'`, restricting bytes opened as a document.                                |

The helper also sends the manifest's `Content-Type` and the body's `Content-Length`. A request that waits behind another miss can still report `miss` after using the filled file.

| Status | Object request result                                                                                              |
| ------ | ------------------------------------------------------------------------------------------------------------------ |
| `404`  | Invalid copy or hash, unknown copy or object, hash absent from the copy's manifest, or a denied copy.              |
| `502`  | Owner manifest fails verification or copy binding, or object exceeds its declared length or fails its byte checks. |
| `503`  | No usable lease after renewal, or a needed owner fetch fails for another reason.                                   |

Object errors send `Cache-Control: no-store, must-revalidate` and `Access-Control-Allow-Origin: *`.

Cache mode starts before the main server's database and source state. It never signs content, changes page bytes, fetches sources, accepts arbitrary upstream URLs or exposes the owner's manifests, keys or policy as helper routes. It has no account or database and never silently accepts new signing keys.

The reader keeps a `HELPER` route only when its origin is in `PUBLIC_KOMIKA_MEDIA_HOSTS`. Helpers use the same probes, choice and fallback as regional routes, and the reader always checks their bytes. The server image proxy refuses helper hosts through `MediaRoutes::non_owner_hosts`. Read [Media routes](/developers/media-routes) for the reader path.

## Where to look

- `apps/server/src/media_cache/mod.rs`: CLI, request checks, leases, responses and fetch locks.
- `apps/server/src/media_cache/owner.rs`: owner HTTP client and pinned keys.
- `apps/server/src/media_cache/store.rs`: file layout, byte budget, eviction and writes.
- `apps/server/src/media_cache_tests.rs`: hits, failures, grace, rotation, eviction and concurrent misses.
- `apps/server/src/media_verify.rs`: manifest and object checks, `PolicyGate` and `LEASE_GRACE_SECS`.
- `apps/server/src/main.rs`: cache-mode dispatch and shared image response headers.
- `apps/server/src/media_routes.rs`: helper route configuration and proxy host exclusions.
- `packages/api/src/media-routes.ts`: reader trust, probes, byte checks and fallback.
- `deploy/server.Dockerfile`: server image and `/health` check.
- `deploy/server-entrypoint.sh`: arguments forwarded to `komika-server`.
