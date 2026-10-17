# TODO

## Server performance

- [ ] Library reads in `apps/server/src/suwayomi.rs` (`mangas(condition:{inLibrary:true})`, lines about 1561, 1596, 1617, 1651) cost about L² per page, where L is the library size. Suwayomi's `MangaQuery.mangas` runs `count()`, `firstOrNull()` and `lastOrNull()` before `limit(first)`, so every page walks the whole matching set again. Library sync and the admin `library()` call use them. These calls are rare. (Added 2026-10-17.)

## 504s on the website and the image Worker (do first)

- [ ] Every edge-cache miss on komiq.cc `/read/*` and on img.komiq.cc `/img` returns 504 with no upstream answer. The Workers report no errors and no CPU-limit kills, and api.komiq.cc shows no 504s. (Added 2026-10-17.)
  - Check which paths the live `komika-reader` version runs the Worker on. The repo config says only `/series/*`, but the Worker gets more requests than that.
  - Tail both Workers until a 504 shows up, find the cause and fix it.
  - Find why a few phones request the same `/read` page hundreds of times (a reload or retry loop) and stop it.

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
