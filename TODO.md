# TODO

## Server performance

- [ ] Library reads in `apps/server/src/suwayomi.rs` (`mangas(condition:{inLibrary:true})`, lines about 1561, 1596, 1617, 1651) cost about L² per page, where L is the library size. Suwayomi's `MangaQuery.mangas` runs `count()`, `firstOrNull()` and `lastOrNull()` before `limit(first)`, so every page walks the whole matching set again. Library sync and the admin `library()` call use them. These calls are rare. (Added 2026-10-17.)

## 504s on the website and the image Worker (do first)

- [ ] Every edge-cache miss on komiq.cc `/read/*` and on img.komiq.cc `/img` returns 504 with no upstream answer. The Workers report no errors and no CPU-limit kills, and api.komiq.cc shows no 504s. (Added 2026-10-17.)
  - Check which paths the live `komika-reader` version runs the Worker on. The repo config says only `/series/*`, but the Worker gets more requests than that.
  - Tail both Workers until a 504 shows up, find the cause and fix it.
  - Find why a few phones request the same `/read` page hundreds of times (a reload or retry loop) and stop it.
  - 2026-10-18: no visitor got a 504 from 2026-10-11 to 2026-10-17. The 504 rows in the logs are Workers cache lookups that missed, and Cloudflare logs every such miss as 504. Remaining: the 282 × 502 on api.komiq.cc in the same week.

## Edge cache and bot protection (added 2026-10-18)

Edge cache: fix these and Cloudflare serves far more traffic, so fewer requests reach the server.

- [x] Chapter pages at `/media/v1/*` are cached at the edge. Page URLs carry the engine generation, so cached pages stay correct after an engine reset. (2026-10-18)
- [x] Anonymous `GET /graphql` is cached at the edge. (2026-10-18)
- [x] Zone Browser Cache TTL set to "Respect Existing Headers". (2026-10-18)
- [x] Public responses send `Access-Control-Allow-Origin: *` without `Vary: Origin`. (2026-10-18)
- [x] Extra query parameters on cover and avatar URLs redirect to the canonical URL. (2026-10-18)
- [x] Series pages no longer cache errors or unknown series. (2026-10-18)
- [x] The image Worker keys MangaDex pages by content and keeps them 30 days, and browsers keep them 7 days. (2026-10-18)
- [x] Strong ETags on covers and media, so the edge revalidates with a 304. (2026-10-18)
- [x] Worker observability on for `komika-reader` and `komika-img`. (2026-10-18)
- [x] Removed the dead "cache public ssr html" rule and the duplicate cover rule. (2026-10-18)
- [x] Series ids of the wrong shape give "not found" instead of an internal error. (2026-10-19)

Bots: keep cached GETs free of challenges, because a challenge breaks fetch and image loads.

- [x] Bot Fight Mode is off. It challenged search engines and our apps. (2026-10-18)
- [x] AI training crawlers are blocked at the edge. AI search and assistant fetches are allowed. (2026-10-18)
- [ ] WAF custom rules (scripting clients on `/graphql`, datacentre traffic on HTML pages only, odd methods) run in watch-only mode since 2026-10-18. Switch them on one at a time after 48 hours.
- [x] One rate-limit rule on path `/graphql`. The reader's server-side calls use their own path, which only the reader can call. (2026-10-19)
- [x] Accept GET `/graphql` only for known persisted queries, and let signed-in readers use the cache for public data. (2026-10-19)
- [x] Add Turnstile to sign-up, sign-in, password changes and posting, checked by the server. Optional for self-hosted servers. (2026-10-19)
- [x] Keep the website's GraphQL direct to `api.komiq.cc`. A same-origin Worker proxy would add about 3.5 billed Worker requests per page view, even for cache hits.

## Open api.komiq.cc to our own apps only

Goal: the Android app and the PC reader may use api.komiq.cc; users type the address themselves. Other apps and scripts are refused. iOS is on hold. (Added 2026-10-17.)

- [ ] **Server gate**, off by default (`KOMIKA_CLIENT_GATE=on`), so self-hosted servers are unaffected. When on, the server answers only:
  - our Workers (a secret header shared with the server), and
  - apps holding a fresh client token (15 min). Signed-in calls also need a session bound to the same app install.
  - Everything else gets 401 `CLIENT_UNVERIFIED`. `/health` tells the app that the gate is on.
- [ ] **Android proof**: Android key attestation, not Play Integrity (the APK ships outside Play). The phone's secure chip proves our package name and release signing certificate. Rooted or unlocked phones are allowed with a lower quota.
- [ ] **Desktop proof**: a per-release key built in at CI, plus proof-of-work on install. This is a speed bump only, so desktop gets the lower quota. Old release keys are retired to force updates.
- [ ] **Sign-in required** for apps on a gated server. At most one new account per install per day. Per-install and per-account rate limits, by tier.
- [ ] **Website through the Worker**: komiq.cc (and admin.komiq.cc) call `/graphql` and `/api/*` on their own origin; the Worker adds the secret header. The image Worker adds it only on calls to our API. Measured cost today: about 5 more Worker requests per visit, inside the free limits.
- [ ] Admin: list and revoke app installs.
- [ ] Optional free Cloudflare WAF rule on api.komiq.cc that drops requests with neither header.
- [ ] Rollout, in order: server code with the gate off; Workers with the proxy; app releases with proof support; then turn the gate on. Update the docs site (connect guide, self-host configuration).

## Android app size

- [ ] Our arm64 APK is 189 MB; Mihon's is 27.7 MB. After install ours also unpacks about 193 MB of engine and about 29 MB of native libraries. The cause is the bundled HotSpot JVM plus Suwayomi. (Added 2026-10-17.)
- [ ] Replace the Android engine with a Mihon-style Kotlin extension host running on Android's own runtime (ART). The app uses only about 11 engine calls (search, chapters, pages, extension install). Estimate: 12 to 19 engineer days. Start with a 2 to 3 day spike: upgrade Kotlin in the Tauri Android project, port the extension loader, run one extension.
