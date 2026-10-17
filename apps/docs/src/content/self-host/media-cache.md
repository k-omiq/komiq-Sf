---
title: Run a media cache
description: Run a helper that stores and serves a komiq server's exact page objects near its readers.
---

Run a media cache to serve a komiq server's page objects closer to its readers. You can also help a server you read on without running another catalogue or source engine.

## What you need

The **owner** is the komiq server you fetch from. Your cache is a **helper**: it stores page objects on demand and serves the owner's exact bytes.

You need the `komika-server` executable or server image, a writable folder and a disk budget. You need no account, database, sources or signing key. The helper must be able to reach the owner, and readers must be able to reach the helper.

Ask the owner to set `KOMIKA_MEDIA_MANIFESTS=on`. This enables signed manifests, lists of page lengths and hashes. A hash is a fingerprint of the bytes that lets your cache check what it receives. Read [Exact media and signing](/developers/media-manifests) for how the owner makes them.

## Start the cache

The command takes an owner origin, a folder and a positive byte budget:

```sh
komika-server media-cache --owner <origin> --dir <path> --max-bytes <n>
```

For example, replace the key id below with the one the owner gives you:

```sh
komika-server media-cache \
  --owner https://komiq.example.com \
  --dir ./media-cache \
  --max-bytes 20G \
  --expect-kid '<owner-key-id>'
```

`--owner` accepts an HTTP or HTTPS origin with no credentials, path, query or fragment. Use HTTPS. `--max-bytes` accepts bytes or the binary suffixes `K`, `M`, `G` and `T`, so `1G` means 1,073,741,824 bytes. Objects and manifests share the budget. The cache removes the least recently used files when it needs room.

`--listen` defaults to `0.0.0.0:8080`. Set it explicitly to choose another address and port. For example, `--listen 127.0.0.1:8080` keeps the listener on this machine behind a reverse proxy, a service that forwards incoming requests. Publish the helper at an HTTPS origin such as `https://media-helper.example.com`.

### Use the server image

Use the server image made from `deploy/server.Dockerfile`. Replace `<image>` with its image name:

```sh
docker run -d --name media-helper \
  -p 127.0.0.1:8080:8080 \
  -v media-cache-data:/data \
  <image> media-cache \
  --owner https://komiq.example.com \
  --dir /data/media-cache \
  --max-bytes 20G \
  --expect-kid '<owner-key-id>'
```

The image's entrypoint passes the arguments after the image name to `komika-server`. This command starts cache mode without PostgreSQL or Suwayomi. The named volume keeps the cache folder across container replacements. Forward your helper's HTTPS origin to the published port.

## Keep the owner's keys pinned

On first start, the helper reads the owner's `/media/v1/keys`, checks its signature and saves the keys and owner origin in `pins.json` inside `--dir`. This is **trust on first use**: without an expected key id, the helper trusts a key set signed by one of its own listed keys.

Use HTTPS and `--expect-kid` with the key id the owner gives you. On first pinning, that key must sign the key set. Later starts keep the saved pins. A folder pinned to a different owner is refused.

After the owner changes its signing key, stop the helper and accept the new key explicitly:

```sh
komika-server media-cache repin \
  --owner https://komiq.example.com \
  --dir ./media-cache \
  --expect-kid '<new-owner-key-id>'
```

Use the same cache folder, or `/data/media-cache` in the mounted Docker volume. Then restart the helper with the new expected key id. `repin` changes the saved keys; a running helper keeps its keys in memory until restart.

## Check whether it can serve

`GET /media/v1/ready` answers `204` when the helper holds a usable policy lease or is within its grace period. A lease is the owner's signed permission to serve copies for a limited time. Without one, readiness answers `503`.

The helper renews the lease every 60 seconds. If renewal fails, it can keep serving copies the last lease did not deny for six hours after that lease expires. Shared caches receive `s-maxage=0` during this grace period. After it ends, the helper stops serving objects and answers `503` until renewal succeeds. Denied copies answer `404`, including during grace.

Leases are not saved to disk. A restart while the owner is down serves nothing, even when the page files remain. The server image's health check asks `/health`, which answers `200` while the process runs, so container health alone does not mean the helper can serve pages. The self-host image's health check asks `/health/ready`, which answers like `/media/v1/ready`.

## Ask the owner to add it

The owner adds a helper entry to `KOMIKA_MEDIA_ROUTES`, alongside its owner entry:

```json
[
	{ "id": "owner", "kind": "owner" },
	{ "id": "helper", "kind": "helper", "base": "https://media-helper.example.com" }
]
```

The web reader's build must also include `https://media-helper.example.com` in `PUBLIC_KOMIKA_MEDIA_HOSTS`. A server entry alone cannot add an origin to a reader build. Helpers join the same probes, route choice and owner fallback as regional routes. The reader checks every helper object's bytes before showing them. Read [Media routes](/developers/media-routes) for those checks and [Media cache mode](/developers/media-cache) for the cache internals.
