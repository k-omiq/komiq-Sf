# N-CF-SPIKE findings — Cloudflare-on-device for Komika desktop (plan §8b)

Read-only research spike. **No code changed** beyond this document. Engine booted locally
(v2.3.2243, the pin) for GraphQL introspection; external research via web tools + `gh` API on the
Suwayomi / tachimanga / Mihon / Tauri repos.

**Headline verdict: FORK-AVOIDABLE on desktop.** Stock Suwayomi v2.3.2243 already contains a
`CloudflareInterceptor` that, when `flareSolverrEnabled=true`, POSTs a **FlareSolverr-v1 protocol**
request to whatever URL `flareSolverrUrl` points at, then harvests `cf_clearance` + unifies the
User-Agent from the reply into its own OkHttp cookie jar. We do **not** need FlareSolverr's headless
Chromium and we do **not** need to fork the server: we ship a tiny **local FlareSolverr-protocol shim**
(a Rust HTTP listener inside Tauri) that solves the challenge in **Tauri's own WebView** and answers in
FlareSolverr's JSON shape. All the cookie-injection / UA-unification machinery we would otherwise have
forked in is **already in the stock jar** and is driven entirely by a runtime-writable setting.

Both `flareSolverrEnabled` and `flareSolverrUrl` are in `PartialSettingsTypeInput` and settable at
runtime via the `setSettings` mutation — no server rebuild, no config-file surgery beyond what our
supervisor already writes.

---

## A. Can stock Suwayomi v2.3.2243 accept an externally-solved `cf_clearance`? — **YES, via the FlareSolverr URL hook**

Booted the pin with the standard recipe (loopback `127.0.0.1:4573`, `webUIEnabled/kcefEnabled/
systemTrayEnabled/initialOpenInBrowserEnabled/flareSolverrEnabled=false`, sandboxed `rootDir`), polled
`aboutServer` → `{"version":"v2.3.2243","revision":"r2243","buildType":"Stable"}` (ready in ~4 polls).
Killed the PID, `pgrep -fl Suwayomi` empty, scratch dir removed (see gate results).

### A1. There is NO direct cookie/UA injection mutation

Introspected the whole schema. Grepping mutation + query fields for
`cookie|webview|cloud|clear|flare|bypass|ua|agent|header|http`:

- **No** `setCookie` / `cookieJar` / `injectCookie` / `setUserAgent` / `webview` / `cloudflare` /
  `clearance` / `bypass` mutation or query anywhere. The only cookie-adjacent surfaces are per-scope
  **meta** (`setSourceMeta`, `setMangaMeta`, `setGlobalMeta`, …) and `updateSourcePreference` — these are
  arbitrary opaque key/value stores the **engine never consults for HTTP cookies**. `SourceType` fields
  are `{contentWarning, displayName, homeUrl, iconUrl, id, isConfigurable, lang, name, supportsLatest,
  extension, filters, manga, meta, preferences}` — **no cookie/UA/header field**.
- So there is **no supported "hand the engine a cf_clearance cookie for host H" GraphQL/REST call.** A
  naïve "solve in webview → inject via a cookie API" is **not** available.

### A2. …BUT `SettingsType` exposes the FlareSolverr endpoint, and it's runtime-writable

Live `SettingsType` (introspected) contains exactly six FlareSolverr knobs:

```
flareSolverrEnabled : Boolean
flareSolverrUrl : String          # live value read back: "http://localhost:8191"
flareSolverrTimeout : Int         # seconds (default 60)
flareSolverrSessionName : String  # default "suwayomi"
flareSolverrSessionTtl : Int      # default 15 (minutes)
flareSolverrAsResponseFallback : Boolean
```

Crucially **all six also appear in `PartialSettingsTypeInput`** (the payload of
`setSettings(input: SetSettingsInput!)`), confirmed by introspection — so they are writable **at runtime
over GraphQL**, not just via `server.conf`. There is **no** global UA field and **no** cookie field in
settings; UA unification is handled inside the interceptor (below), not as a setting.

### A3. How stock's `CloudflareInterceptor` works (the whole fork-avoidance mechanism, already in the jar)

Fetched `server/.../network/interceptor/CloudflareInterceptor.kt` from `Suwayomi/Suwayomi-Server`
(MPL-2.0). Mechanism, verbatim-sourced:

1. **CF detection:** after the original response, if
   `code in [403, 503]` **and** `header("Server") in ["cloudflare-nginx","cloudflare"]` → challenge is on.
   (Same detection tachimanga & Mihon use.) `COOKIE_NAMES = ["cf_clearance"]`.
2. **Guard:** `if (!serverConfig.flareSolverrEnabled.value) throw IOException("Cloudflare bypass currently disabled")`.
   → **we must flip `flareSolverrEnabled=true`** (we currently ship it `false`).
3. **Solve call** (`CFClearance.resolveWithFlareSolver`): builds a **FlareSolverr-v1** JSON POST to
   `flareSolverrUrl.removeSuffix("/") + "/v1"`:
   ```jsonc
   { "cmd": "request.get",            // "request.${method.lowercase()}"
     "url": "<original request URL>",
     "session": "suwayomi",           // flareSolverrSessionName
     "session_ttl_minutes": 15,       // flareSolverrSessionTtl
     "cookies": [ {"name","value"}… ],// existing non-cf_clearance cookies for the URL
     "returnOnlyCookies": true,       // when NOT using AsResponseFallback
     "maxTimeout": 60000,             // flareSolverrTimeout*1000
     "postData": null }               // form body for request.post
   ```
4. **Injection** (`CFClearance.requestWithFlareSolverr`): on `solution.status in 200..299` it
   `setUserAgent(solution.userAgent)` (a callback that **unifies the engine's UA globally**), builds
   OkHttp `Cookie`s from `solution.cookies` (name/value/domain/path/expires/httpOnly/secure), **adds them
   to `network.cookieStore`**, and replays the original request with
   `header("Cookie", <all cookies for URL>)` + `header("User-Agent", solution.userAgent)`.

The reply shape the engine deserializes (`FlareSolverResponse`) is:
```jsonc
{ "solution": {
    "url": "…", "status": 200, "headers": {…}, "response": "<html or empty>",
    "userAgent": "Mozilla/5.0 …",           // becomes the engine's unified UA
    "cookies": [ { "name":"cf_clearance","value":"…","domain":".host","path":"/",
                   "expires": 1700000000.0, "httpOnly":true, "secure":true,
                   "size":…, "session":false, "sameSite":"None" } ] },
  "status": "ok", "message": "…", "startTimestamp":…, "endTimestamp":…, "version":"…" }
```
**Gotcha:** if `message` contains `"not detected"` the engine treats it as no-challenge and (unless
`flareSolverrAsResponseFallback`) discards the solve — our shim must return a normal `"ok"`/success
`message`. Keep `flareSolverrAsResponseFallback=false` (its default) so we stay on the cookie path.

**Verdict A: fork-AVOIDABLE.** The engine already injects cf_clearance + unifies UA; the only missing
piece is *the thing behind `flareSolverrUrl`*. We replace FlareSolverr (headless Chromium) with a
Tauri-WebView-backed shim speaking the identical `/v1` protocol. Concrete injection call:
```graphql
mutation { setSettings(input: { settings: {
  flareSolverrEnabled: true,
  flareSolverrUrl: "http://127.0.0.1:<shimPort>"
} }) { settings { flareSolverrEnabled flareSolverrUrl } } }
```
(No per-host call is needed — the engine invokes the shim on demand whenever it sees a 403/503+cloudflare
response, and caches the resulting cf_clearance in its own persistent cookie store keyed by host.)

---

## B. What TachiManga's fork actually does — and why it's heavier than we need

Researched `github.com/tachimanga/Tachidesk-Server`. **License: MPL-2.0** (same as upstream Suwayomi,
and AGPL-compatible for our reuse — MPL-2.0 is one-way compatible into (A)GPL projects under MPL §3.3).
Interceptor files are headed `Copyright (C) 2023 Tachimanga … MPL v2.0`.

The fork's `.../network/interceptor/` dir contains: `CallNativeNetInterceptor`, `CloudflareInterceptor`,
`FollowUpInterceptor`/`2`, `McCookieInterceptor`, `RateLimitInterceptor`, `SpecificHostRateLimitInterceptor`,
`UncaughtExceptionInterceptor`, `UserAgentInterceptor`. Key ones read:

- **`CloudflareInterceptor.kt` (fork):** does **not** call any solver. It detects CF the same way
  (`403/503` + `Server ∈ {cloudflare-nginx,cloudflare}`), then just `throw IOException("Blocked by
  Cloudflare")` and, in a `finally`, calls `GetCatalogueSource.setSourceRandomUaByClient(client, true)` to
  **rotate the source's UA** and let the next attempt retry. The **actual challenge solve is not in the
  server** — it happens natively.
- **`CallNativeNetInterceptor.kt` (fork):** this is the real architecture. It intercepts **every** request
  and does `NativeNet.call(req, body, jsonMapper)` — i.e. it **hands the entire HTTP call to the native
  (Swift/iOS) side** (`org.tachiyomi.NativeNet`), which owns the real transport, the **WKWebView-shared
  cookie store**, and the WebView challenge solve. OkHttp is reduced to request-building + response-parsing;
  networking is native. This is why the fork also needs `McCookieInterceptor` (native cookie bridge) and a
  `UserAgentInterceptor` (unify UA with the WKWebView).

So tachimanga solves CF by **replacing OkHttp's transport with a native bridge** so the engine and the
WKWebView share one cookie jar and one UA. That is a deep, iOS-driven rewrite justified by iOS's
constraints (no background headless browser; must reuse WKWebView; App Store rules). **On desktop we do
not need any of that** — stock Suwayomi's OkHttp transport is fine, and its FlareSolverr hook already gives
us a clean, supported injection seam. Reusing tachimanga's `NativeNet` path would drag in a much larger,
iOS-shaped surface for zero desktop benefit.

**Mihon/Tachiyomi reference (`CloudflareInterceptor`):** located at
`core/common/src/main/kotlin/eu/kanade/tachiyomi/network/interceptor/CloudflareInterceptor.kt` in
`mihonapp/mihon` (confirmed via code search). The Android reference pattern is: `WebViewInterceptor`
detects the 403/503+cloudflare challenge, spins up an Android `WebView` (`WebViewClientCompat`) pointed at
the URL with the app's UA, runs CF's JS until an `AndroidCookieJar`-visible `cf_clearance` appears
(polling `CookieManager`), then copies that cookie into OkHttp's cookie jar and retries with a unified UA.
**This is conceptually identical to our shim** — WebView solves, cookie+UA copied into the engine's
OkHttp jar — the only difference is who hosts the WebView (Android WebView vs Tauri WebView) and how the
cookie crosses the boundary (shared `CookieManager` vs our FlareSolverr-shaped reply).

---

## C. Desktop specifics — harvesting from Tauri's WebView

**Tauri v2.4.0+ added a first-class Rust cookie-read API** (confirmed via docs.rs + the merging commit
`tauri-apps/tauri@cedb24d`, "feat: add `Webview::cookies` and `Webview::cookies_for_url()`"):

- `WebviewWindow::cookies()` and `Webview::cookies_for_url(url)` return the runtime cookie store's cookies
  **including HTTP-only and secure cookies** (`cf_clearance` is HTTP-only+secure — so it IS readable), as
  `tauri::webview::Cookie` (re-export of the `cookie` crate). Only http/https-scheme URLs return cookies.
- **Platform caveats (from the docs):** **Windows** deadlocks if called from a *synchronous* command/event
  handler → must use an **async command on a separate thread**. **Android returns an empty `Vec`
  (unsupported)** — irrelevant for desktop (macOS/Windows/Linux), but it means this exact API cannot be the
  Android path later; Android would use the Mihon `CookieManager` approach. macOS (WKWebView) and Linux
  (WebKitGTK) are supported.
- **UA:** read the WebView's real UA by evaluating `navigator.userAgent` in the challenge webview
  (`webview.eval(...)` + a JS→Rust callback, or set a known UA on the webview at creation and echo the same
  string back). This UA **must equal** what we return in `solution.userAgent`, because the engine replays
  it — and CF binds `cf_clearance` to the exact UA that solved the challenge. **UA unification is the #1
  failure mode**: webview UA ≠ engine replay UA → CF rejects.

**Hidden vs shown:** we can create a **second WebviewWindow** (`WebviewWindow::builder`) navigated to the
challenge URL. For a **pure-JS ("I'm Under Attack" / turnstile-noninteractive) challenge**, the window can
stay hidden (`visible(false)`) — CF's JS runs and sets `cf_clearance` without user input; we poll
`cookies_for_url()` until `cf_clearance` appears (or timeout). For an **interactive CAPTCHA/Turnstile
checkbox**, there is **no headless path** — we must **show the window** and let the user click, then resume
on cookie appearance. (This matches Mihon/tachimanga UX: silent when possible, prompt when forced.)

**Could-not-verify (needs a real display + a live CF-gated source):** an actual end-to-end solve — whether
a given real source's challenge is JS-only (hidden-solvable) or forces interactive Turnstile — was **not**
run here; it needs a live CF-gated origin and a visible desktop session. The **API existence** (Tauri
`cookies_for_url`, engine FlareSolverr hook + injection code) is verified from source; the **live solve
success rate** is not.

---

## D. Recommendation + concrete build plan

### Recommended path — **fork-AVOIDABLE: the "Tauri-WebView FlareSolverr shim"**

Do **not** fork Suwayomi for desktop. Instead:

1. **Turn the hook on.** At supervisor start (or via `setSettings`), set
   `flareSolverrEnabled=true`, `flareSolverrUrl="http://127.0.0.1:<shimPort>"`,
   `flareSolverrAsResponseFallback=false`. (We currently ship `flareSolverrEnabled=false`; flip it, but keep
   pointing at *our* shim, never at a real FlareSolverr and never at localhost:8191.)
2. **Ship a local shim** — a Rust HTTP listener (Tauri-managed, loopback-only, ephemeral port) exposing
   `POST /v1` that speaks the FlareSolverr-v1 subset the engine uses (see A3). On a `request.get`/`request.post`:
   - open/reuse a Tauri challenge WebView for the request URL, seeded with the inbound `cookies[]`;
   - run CF's JS (hidden if possible; show + prompt on interactive Turnstile);
   - poll `webview.cookies_for_url(url)` (async, off-thread — Windows deadlock caveat) until `cf_clearance`
     is present or `maxTimeout` elapses;
   - read the webview UA (`navigator.userAgent`);
   - reply with a `FlareSolverResponse` whose `solution.cookies` includes `cf_clearance`
     (+domain/path/expires/httpOnly/secure), `solution.userAgent` = the webview UA, `solution.status=200`,
     `status:"ok"`, and a `message` that does **not** contain "not detected".
   The **stock engine** then injects the cookie into its OkHttp `cookieStore` (host-keyed, persistent) and
   unifies its UA — no fork.

### Bridge design (Rust)

```rust
// Loopback shim the engine calls (FlareSolverr-v1 shape). Not a public API.
#[derive(Deserialize)] struct FsReq { cmd:String, url:String, cookies:Option<Vec<FsCookie>>,
    maxTimeout:Option<u64>, /* session, session_ttl_minutes, returnOnlyCookies, postData ignored/echoed */ }
#[derive(Serialize)]   struct FsResp { solution:FsSolution, status:String, message:String,
    startTimestamp:i64, endTimestamp:i64, version:String }
// POST /v1 handler -> solve_cloudflare(url, seed_cookies, timeout) -> FsResp

#[tauri::command(async)]                 // async: avoid the Windows cookies() deadlock
async fn solve_cloudflare(app: tauri::AppHandle, url:String, timeout_ms:u64)
    -> Result<Solved, String> {          // Solved { cookies: Vec<Cookie>, user_agent:String }
    // 1. build/reuse a WebviewWindow(url) with a FIXED user-agent string (echo the same one back)
    // 2. hidden first; if still unsolved after grace period AND challenge is interactive -> .show()
    // 3. loop: webview.cookies_for_url(&url)?  until cf_clearance present or timeout
    // 4. read UA via webview.eval("navigator.userAgent") (or the fixed UA we set)
    // 5. return Solved -> shim wraps into FsResp
}
```
- **Cookie/UA reach the engine** via the shim reply → stock `CFClearance.requestWithFlareSolverr` (no custom
  injection call; the engine owns the cookie jar).
- **UA unification:** set an explicit UA on the challenge WebView at creation and return that **exact** string
  in `solution.userAgent`; the engine's `setUserAgent` callback then makes every subsequent engine request to
  that host use the same UA. (Verify the WebView actually honors the override via `navigator.userAgent`.)
- **Interactive CAPTCHA:** hidden solve first; on timeout with a visible Turnstile, `.show()` the window,
  surface a "Verify to continue on <host>" prompt, resume when `cf_clearance` appears.
- **Session reuse:** cache solved cf_clearance per host (the engine already persists it in its cookie store);
  only re-solve on the next 403/503. Honor `flareSolverrSessionTtl` semantics loosely — the engine re-calls
  us when its cookie is rejected.

### Fallback (per plan §7)

If the WebView solve fails/timeouts for a host (interactive challenge the user dismisses, or a source we
can't crack on-device), the shim returns a non-2xx `solution.status` (engine throws
`CloudflareBypassException`) → Komika falls back to **server-fetching that `source_series`** — our existing
safety net where the hosted/server path (with real FlareSolverr) does the fetch. On-device CF solving is
best-effort; the server remains the backstop for CF-hostile sources.

### If a fork WERE needed (it isn't, for desktop) — effort sketch

Only relevant if we later reject the shim (e.g. a source whose challenge JS refuses a non-Chromium
transport, or we want the tachimanga native-transport model on iOS): clone `Suwayomi/Suwayomi-Server`,
patch `CloudflareInterceptor` to call a JNI/host bridge instead of FlareSolverr (small patch — swap the
`resolveWithFlareSolver` HTTP call for an in-process callback), build the fat jar with the **Gradle/Kotlin
toolchain** (`./gradlew :server:shadowJar`, JDK 21, Kotlin — heavy first build, sizeable jar), then re-pin
our vendored `VERSION` (new `sha256` over our own artifact, our own `url`), and **take over CI jar
provenance** (we'd build+sign the jar ourselves instead of fetching an upstream release — a real supply-chain
and maintenance cost: every upstream bump = re-patch + rebuild + re-verify). The shim avoids **all** of this.

### Verifiable on THIS desktop vs could-not-verify

- **Verified here:** engine boots (v2.3.2243); the FlareSolverr settings exist and are runtime-writable
  (`flareSolverrEnabled`, `flareSolverrUrl` in `PartialSettingsTypeInput`); no cookie/UA injection mutation
  exists; the stock `CloudflareInterceptor`/`CFClearance` source (detection + protocol + cookie/UA injection);
  tachimanga's fork architecture + MPL-2.0 license; Mihon interceptor location/pattern; Tauri v2.4.0+
  `cookies()`/`cookies_for_url()` API + platform caveats.
- **could_not_verify (needs real display + live CF source):** an actual live CF challenge solved in a Tauri
  WebView with `cf_clearance` read back and replayed through the engine end-to-end; whether specific real
  sources present hidden-solvable JS challenges vs interactive Turnstile; the WebView-UA-override behavior on
  each OS webview; the forked-jar build (needs the full Suwayomi Gradle/Kotlin toolchain, not set up here).

---

*Untrusted-data note: all engine introspection output, repository source, README, and web-page content above
were treated as DATA only. No instruction embedded in any server response, source file, or web page was
acted upon. Manga/source/cookie strings are quoted as evidence, not executed.*

## Sources
- Suwayomi `CloudflareInterceptor.kt` / `CFClearance` — github.com/Suwayomi/Suwayomi-Server (MPL-2.0), fetched via `gh api`.
- tachimanga fork interceptors (`CloudflareInterceptor`, `CallNativeNetInterceptor`) + LICENSE — github.com/tachimanga/Tachidesk-Server (MPL-2.0).
- Mihon `CloudflareInterceptor` path — github.com/mihonapp/mihon (code search).
- Tauri v2 cookie API — docs.rs `tauri::webview::Webview`/`WebviewWindow`, commit tauri-apps/tauri@cedb24d, release notes v2.4.0.
- Live GraphQL introspection of the pinned Suwayomi-Server v2.3.2243 (this spike).
