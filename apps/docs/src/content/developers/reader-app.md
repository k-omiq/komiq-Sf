---
title: The reader app
description: Learn how the reader chooses a server and handles build targets, routes, data fetching, rendering and strip reading.
---

The reader app in `apps/reader` connects to a komiq server on your local network or the internet. One codebase supplies the web app, native apps inside [Tauri](/self-host/desktop-app), and the embedded reader served by the server.

## Build targets

The reader builds for four targets from the same source tree. Target selection is controlled in `apps/reader/vite.config.ts`:

- `web`: The default target for `pnpm build`. It uses `@sveltejs/adapter-cloudflare` to output an edge Worker script (`_worker.js`), an `index.html` shell and static assets in `.svelte-kit/cloudflare`. Only `/series/*` reaches the Worker for server-side rendering (SSR), which supplies text and share previews in the HTML. Other documents use the shell and render in the browser.
- `static`: Setting `KOMIKA_TARGET=static` selects `@sveltejs/adapter-static`, with fallback `index.html` in `build`. It uses the same public configuration and runtime behavior as `web`, with SSR disabled for every route. Series links then have no server-rendered share preview. `wrangler.static.toml` contains no Worker script or assets binding. It uses `komika-reader-static`, with no routes and `workers_dev = false`; an operator adds a test hostname when ready.
- `tauri`: Used when building desktop and mobile packages. Setting `KOMIKA_TARGET=tauri` or detecting `TAURI_ENV_PLATFORM` selects `@sveltejs/adapter-static` with fallback `index.html`. This produces a pure client-side single-page application without a Node.js or edge server runtime.
- `embedded`: Used when the backend server hosts the reader directly. Setting `KOMIKA_TARGET=embedded` builds a static single-page application configured to talk to the same host origin at `/graphql`.

Vite defines two compile-time constants during the build: `__KOMIKA_TARGET__` records the runtime target (`static` uses `web`), and `__KOMIKA_SSR__` indicates whether server-side rendering is enabled.

Public configuration is resolved at build time through `$env/static/public` in `apps/reader/src/lib/config.ts`. Inlining these values at build time ensures that edge cold starts on Cloudflare Workers do not rely on dynamic runtime environment variables, which are empty during module evaluation.

## Choosing a server

The reader can use an address built into the app or one chosen on the native connect screen. The web build for the global server at `komiq.cc`, the project's free work-in-progress demo, has its address built in. The native app connects to your own server. It cannot connect to the global server: that server's `CORS_ORIGINS` lists the `komiq.cc` site only, so a request from the app's own origin (`tauri://localhost`) gets no answer the WebView accepts.

In `apps/reader/src/lib/config.ts`, `canChooseServer` is true only when the target is `tauri` and `isTauri()` confirms a native runtime. Web builds and static builds opened in a browser use their configured server. Embedded builds always use `/graphql` on their own origin, ignoring the built-in address.

For native builds, `chosenServer` reads a valid HTTP or HTTPS origin from `localStorage` under `komiq-server`. That choice takes precedence over `PUBLIC_KOMIKA_API`, the build-time endpoint. The app appends `/graphql` to the chosen origin. If neither a saved choice nor a built-in endpoint exists, `needsServer` opens the connection screen.

The connect screen accepts an IP address and port (`192.168.1.20:8080`), a domain (`komiq.example.com`) or a pasted link. On supported phones, it can also scan a QR code. Connection helpers live in `apps/reader/src/lib/server-connect.svelte.ts`:

- `toServerOrigin` trims the input and reduces a link to its origin. Without a scheme, IP addresses, `localhost`, `.local` names and addresses with an explicit port use HTTP. Plain domains use HTTPS. Other schemes are rejected.
- `checkServer` checks `/health` with an 8-second timeout, then queries `instance` at `/graphql` to confirm that the address answers as a usable komiq server.
- `useServer` saves the origin and fetch preference, removes the previous server's token and navigates to `/login`. Reloading rebuilds the clients against the chosen server.

## Routes and rendering choices

Application routes are organized under `apps/reader/src/routes`:

- `(app)/+page.svelte`: The home landing page showing featured items, latest additions, and popular series.
- `(app)/browse/+page.svelte`: The catalog search and filter interface with tag selection and sort controls.
- `(app)/series/[slug]/+page.svelte`: Series details, author information, chapter lists, and scanlation group selection.
- `(app)/updates/+page.svelte`: Chronological list of newly released chapters.
- `(app)/library/+page.svelte`: Authenticated user library tracking reading status.
- `(app)/profile/+page.svelte`: Account settings, user statistics, and user comments.
- `(app)/login/+page.svelte`: User authentication and account registration.
- `read/[slug]/+page.svelte`: The distraction-free reading view for chapter content.

The root layout in `apps/reader/src/routes/+layout.ts` sets `ssr = false` and `prerender = false` as the default. Tauri and static hosts have no server runtime. Authenticated and interactive pages such as the library, user profile, login, and the chapter reader depend on browser storage and state.

Public catalog pages retain their `export const ssr = SSR_PUBLIC` options. The constant `SSR_PUBLIC` in `apps/reader/src/lib/ssr.ts` reflects `__KOMIKA_SSR__`. Workers Assets routes only `/series/*` to the server in web builds, with `!/series/` excluding an empty slug. All other paths receive the static SPA shell without a User Worker invocation, including unknown paths, which the client router handles. In static, Tauri and embedded builds, `SSR_PUBLIC` is false for every route.

The adapter wrapper generates `_headers` after the shell is built. Its Content Security Policy (CSP) hashes the exact inline script text using `deploy/static-csp.mjs`. Security headers come from `apps/reader/_headers`. HTML uses `Cache-Control: no-cache` and `/_app/immutable/*` uses `public, max-age=31536000, immutable`. Workers Assets can serve the SPA shell at missing chunk URLs, including non-navigation requests, and that shell would get the same year-long header. So every release keeps all earlier chunks; see [Deployment](/developers/deployment). SvelteKit supplies the dynamic series response's own CSP.

The template registers `vite:preloadError` and a startup-only `unhandledrejection` listener before app imports run. Bare bootstrap entry imports reject without emitting Vite's preload event; both failures use the same `sessionStorage` guard to reload once, then show an error and a Reload button. The guard is kept per build and is never cleared, because a deferred import can still fail after the root mounts. When the root layout mounts, it removes only the bootstrap rejection listener. A new build has its own guard. If storage is unavailable, recovery shows the button immediately.

Universal `load` functions coordinate data hydration through `apps/reader/src/lib/stream.ts`. SvelteKit server-side rendering only outputs the pending branch of an `{#await}` block. If the server returned an unresolved promise, edge HTML would contain only loading skeletons.

To solve this, public loads await data during server-side rendering and initial client hydration. Once the root layout completes mounting and calls `markHydrated()`, subsequent client navigations stream the data unawaited. This displays skeleton placeholders while new routes load:

```ts
export const load: PageLoad = async ({ fetch }) => {
	const home = getHome(fetch);
	return { home: shouldStream() ? home : await home };
};
```

## The data layer

Components interact with the backend through shared singletons defined in `apps/reader/src/lib/context.ts`:

- `backend`: Implements the data client for catalog queries, library status, social comments, and user authentication.
- `images`: Resolves page and cover image URLs, routing them through the Cloudflare [image proxy](/developers/image-proxy) Worker on web, the server proxy on self-hosted installs, or directly from source hosts.

When [the on-device engine](/developers/native-engine) is in use, on desktop or in an Android build that carries it, `context.ts` wraps the connected server API and an on-device Suwayomi instance inside a composite client. An offline write queue backed by local storage records reading progress updates and library marks while disconnected and replays them when connectivity returns. A queued progress write keeps the server's numeric epoch known when it was queued and replays with that epoch, never a newer one. When the server refuses it as stale after an engine change, the queue drops it with a log line and does not retry it.

During server-side rendering, `apps/reader/src/lib/data/load-fetch.ts` passes SvelteKit's scoped `fetch` function into GraphQL requests. SvelteKit serializes responses into `<script data-sveltekit-fetched>` tags inside the generated HTML document. When the browser hydrates the page, it reuses the serialized responses rather than issuing duplicate network requests for the same data.

### Progress saves and offline replay

The reader sends `saveProgress` and `saveChapterRead` with the work ID. A base revision identifies the progress version it last saw in the series bundle's `viewerProgress` or the separate query. `GraphQLBackend` sends one save at a time per work. Only `APPLIED` raises its known revision. See [GraphQL API](/developers/graphql) for results and older-server fallbacks.

The root layout listens to `onProgressConflict` for live saves and queued replays. It calls `dropWorkProgressState(workId)` to clear that work's optimistic checkmarks, the marks shown before a save finishes. It also clears the reader cache and refetches the current page.

Each queued progress save stores a stable `opId`, its `workId`, its base `revision` and `chainedTo`, the ID of the preceding save for that work and account:

- A save adopts its predecessor's revision only after `APPLIED`. Other results release the next save with its original base revision.
- Collapsing duplicate chapter saves passes the removed save's predecessor and the greater known base revision to its successor. The replacement joins the end of the work's chain.
- The save being sent is never replaced. New progress for that chapter waits behind it. Live progress joins an existing queue for the same work and account. A live save also waits for an earlier live save of that work, so it joins the queue that a failed save leaves.
- A chapter checkmark cannot be queued. It first sends the queued saves of its work. If they cannot be sent, the checkmark fails and the reader undoes it.
- Removing an acknowledged save and updating its successor happen in one storage write. Account ownership checks and the captured numeric epoch still apply. The epoch identifies the source engine's generation.

- A save is never sent twice at once. When a new sign-in token cancels a replay while a save is out, the next replay waits for that request. If the server answers, the queue removes the save and passes its revision on as usual.
- The queue's storage also keeps the last base revision the app saw for each work of the signed-in account. After an offline restart, a new save uses it, so the server can still refuse it as stale. Loading the work's chapter list from the server replaces it, or forgets it when that load brings no revision. Another account's sign-in or a sign-out clears it.
- A save carries its `opId`. A live save that fails is queued with the same id. When the server applied a save but its answer never arrived, for example after a timeout or a crash, the resend has its old base revision. The server finds that this id made the reader's last change in the work and answers `APPLIED` with the current revision, so the next save of the chain goes out with that revision. A server that predates `opId` judges the resend by its revision alone and refuses it.

### Chapter entries and moved links

A chapter-list entry represents one chapter of the work and links its source copies. The series list joins copies by `entryId` and sorts by `entryOrder` when the work's list is ready. Aggregate rows carry the `special` flag for badges. Reader navigation groups and orders by entry only when every copy in its pool has both entry fields. Otherwise, the whole pool keeps the older chapter-number rules.

If a requested chapter is missing from the resolved work, or a link's `src` names a source outside the work, the reader asks `chapterWork(chapterId)` for its current owner. A different work produces `movedTo`; the route redirects with HTTP 307 to `/read/{workId}?ch={chapterId}`. An unsupported or failed lookup leaves the empty reader.

## The long-strip reader

`apps/reader/src/routes/read/[slug]/+page.svelte` supports single-page, double-page and long-strip reading. Long-strip mode keeps every page mounted:

1. The first three pages load with `fetchpriority="high"`.
2. A low-priority queue loads the rest, at most two requests at once.
3. Six pages ahead of the furthest scroll position switch to eager loading.

Unloaded cells reserve a 2:3 aspect ratio. If a loaded image changes height above the viewport, `window.scrollBy` compensates. A binary search tracks the reading line at 40 percent of viewport height; animation frames coalesce scroll work.

After 40 percent of a chapter, the app preloads the next chapter's metadata and first three images. Pointing at, focusing or touching chapter navigation also warms it. Progress saves at chapter end, navigation away, tab switches and unmounting.

## Phone layout

`apps/reader/src/lib/app.css` holds shared phone styles:

- Safe-area tokens wrap `env(safe-area-inset-*)`. Use `--k-safe-top`, `--k-safe-right`, `--k-safe-bottom`, `--k-safe-left`, `--k-header-h` and `--k-gutter`.
- Bottom navigation appears up to 640 pixels wide, or on a coarse-pointer screen under 500 pixels high. `BottomNav.svelte`, `Header.svelte` and the app layout share those conditions.
- `k-hit` gives small controls a 44-pixel touch target. Touch text fields use at least 16-pixel text to avoid automatic zoom.
- `static/theme-init.js` marks native windows with `data-native`. Native rules disable overscroll, callouts, tap highlights and selection on controls.

`apps/reader/src/lib/native.ts` exposes `allowPageZoom()` and `setStatusBarHidden()`; see [iOS](/developers/ios). A fixed backdrop inside a blurred element uses viewport units, because `backdrop-filter` changes its containing block.

The rating row is one touch target, allowing a slide to correct a selection. Paged reading turns pages on sideways swipes and ignores them when zoomed in or wider than the screen.

## Where to look

- `apps/reader/vite.config.ts`: Target selection, adapter configuration, style inlining thresholds, and Content Security Policy definitions.
- `apps/reader/wrangler.toml` and `apps/reader/wrangler.static.toml`: Series-only Worker routing and the script-free alternative.
- `apps/reader/deploy/static-assets.mjs` and `apps/reader/src/app.html`: Generated shell headers and early preload recovery.
- `apps/reader/deploy/release.mjs` and `apps/reader/deploy/retain-assets.mjs`: Releases a build with every earlier chunk.
- `apps/reader/src/lib/config.ts`: Build-time environment variable bindings, server address persistence, and runtime target flags.
- `apps/reader/src/lib/server-connect.svelte.ts`: Address normalization, server checks and switching servers.
- `apps/reader/src/lib/components/ConnectServer.svelte`: Connect screen and QR scanning on supported phones.
- `apps/reader/src/lib/ssr.ts`: The `SSR_PUBLIC` flag controlling route-level server rendering.
- `apps/reader/src/lib/stream.ts`: Logic distinguishing edge rendering and hydration from subsequent client navigations.
- `apps/reader/src/lib/context.ts`: Application singletons for the API backend, image provider, and offline write queue.
- `apps/reader/src/lib/data/load-fetch.ts`: SvelteKit fetch integration and serialized response hydration.
- `apps/reader/src/lib/data/source.ts` and `apps/reader/src/lib/data/chapter-versions.ts`: Entry joins, reading order, progress conflicts and moved-chapter lookup.
- `packages/api/src/graphql-backend.ts` and `packages/api/src/progress-conflict.ts`: Revisioned saves and conflict listeners.
- `packages/api/src/offline-queue.ts` and `packages/api/src/composite-backend.ts`: Offline save chains, revision handover and account checks.
- `apps/reader/src/routes/read/[slug]/+page.svelte`: Reader layout, image queuing, aspect ratio stabilization, and prefetch logic.
- `apps/reader/src/lib/app.css`: Safe-area tokens, the phone shell token, `k-hit` and the native-app rules.
- `apps/reader/src/lib/native.ts` and `apps/reader/static/theme-init.js`: Zoom lock and status bar calls for the native app.
