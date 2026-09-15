---
title: The reader app
description: Learn how the reader chooses a server and handles build targets, routes, data fetching, rendering and strip reading.
---

The reader app in `apps/reader` is what you read with. It connects to a komiq server on your local network or one on the internet. It runs across web, desktop and mobile platforms from one codebase.

The app supports public catalog browsing, social discussions, personal library tracking, and chapter reading. Depending on how you run it, the reader executes as an edge-rendered web app on Cloudflare Workers, a static single-page application inside [Tauri](/self-host/desktop-app), or an embedded web interface served directly by the backend server.

## Build targets

The reader builds for three targets from the same source tree. Target selection is controlled in `apps/reader/vite.config.ts`:

- `web`: The default target for `pnpm build`. It uses `@sveltejs/adapter-cloudflare` to output an edge Worker script (`_worker.js`) and static assets. Public catalog pages are server-rendered at the edge for fast delivery.
- `tauri`: Used when building desktop and mobile packages. Setting `KOMIKA_TARGET=tauri` or detecting `TAURI_ENV_PLATFORM` selects `@sveltejs/adapter-static` with fallback `index.html`. This produces a pure client-side single-page application without a Node.js or edge server runtime.
- `embedded`: Used when the backend server hosts the reader directly. Setting `KOMIKA_TARGET=embedded` builds a static single-page application configured to talk to the same host origin at `/graphql`.

Vite defines two compile-time constants during the build: `__KOMIKA_TARGET__` records the build target, and `__KOMIKA_SSR__` indicates whether server-side rendering is enabled.

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

Public catalog pages override this default by exporting `export const ssr = SSR_PUBLIC`. The constant `SSR_PUBLIC` in `apps/reader/src/lib/ssr.ts` reflects `__KOMIKA_SSR__`. In web builds, public routes render at the Cloudflare edge to deliver pre-rendered HTML to browsers and search crawlers. In Tauri and embedded builds, `SSR_PUBLIC` is false, keeping all routes client-side.

Universal `load` functions coordinate data hydration through `apps/reader/src/lib/stream.ts`. SvelteKit server-side rendering only outputs the pending branch of an `{#await}` block. If the server returned an unresolved promise, edge HTML would contain only loading skeletons.

To solve this, public loads await data during server-side rendering and initial client hydration. Once the root layout completes mounting and calls `markHydrated()`, subsequent client navigations stream the data unawaited. This displays skeleton placeholders while new routes load:

```ts
export const load: PageLoad = async ({ fetch, setHeaders }) => {
	setHeaders({ 'cache-control': 'public, max-age=0, s-maxage=60' });
	const home = getHome(fetch);
	return { home: shouldStream() ? home : await home };
};
```

## The data layer

Components interact with the backend through shared singletons defined in `apps/reader/src/lib/context.ts`:

- `backend`: Implements the data client for catalog queries, library status, social comments, and user authentication.
- `images`: Resolves page and cover image URLs, routing them through the Cloudflare [image proxy](/developers/image-proxy) Worker on web, the server proxy on self-hosted installs, or directly from source hosts.

When [the on-device engine](/developers/native-engine) is in use, on desktop or in an Android build that carries it, `context.ts` wraps the connected server API and an on-device Suwayomi instance inside a composite client. An offline write queue backed by local storage records reading progress updates and library marks while disconnected and replays them when connectivity returns.

During server-side rendering, `apps/reader/src/lib/data/load-fetch.ts` passes SvelteKit's scoped `fetch` function into GraphQL requests. SvelteKit serializes responses into `<script data-sveltekit-fetched>` tags inside the generated HTML document. When the browser hydrates the page, it reuses the serialized responses rather than issuing duplicate network requests for the same data.

## The long-strip reader

The chapter reading route in `apps/reader/src/routes/read/[slug]/+page.svelte` supports single-page, double-page, and continuous long-strip reading.

In long-strip mode, all pages remain mounted in the Document Object Model (DOM). DOM virtualization with dynamic mounting was evaluated and rejected because frequent layout mutations caused visual stutter during rapid scrolling.

To manage network transfer and rendering performance, long-strip mode uses a three-tier loading system:

1. Starters: The first three pages load immediately with `fetchpriority="high"`.
2. Background queue: Remaining pages load in sequence with low priority. Concurrency is limited to two simultaneous requests, advancing as images finish loading.
3. Scroll lookahead: Six pages ahead of the furthest scrolled position switch from lazy to eager loading. This prevents delays when readers scroll quickly or jump using the progress bar.

Unloaded strip cells reserve vertical space using an initial aspect ratio of 2:3. As each image loads, the reader measures its natural dimensions. If an image finishes loading above the visible viewport, its height expansion would shift the active reading view. Because browser scroll anchoring is disabled to prevent conflicts with chapter transitions, the reader calculates the height difference and calls `window.scrollBy` to maintain the user's scroll position.

The active reading position is tracked against a reading line set at 40 percent of the viewport height. A binary search in `stripPageAtLine` identifies which page intersects this threshold. Scroll event processing is coalesced into a single `requestAnimationFrame` callback to avoid layout thrashing.

When a reader completes 40 percent of a chapter, the reader automatically preloads the next chapter's metadata and warms its first three page images into the browser cache. The same warming logic triggers when a reader points at, focuses, or touches chapter navigation buttons.

Reading progress persists automatically when a reader reaches the end of a chapter. To ensure mid-chapter progress is not lost, the reader also records progress when the user navigates away, switches browser tabs, or unmounts the component.

## Phone layout

Base styles are written for a phone. Wider layouts are added with `min-width` media queries. The shared pieces live in `apps/reader/src/lib/app.css`:

- Safe areas: `--k-safe-top`, `--k-safe-right`, `--k-safe-bottom` and `--k-safe-left` wrap `env(safe-area-inset-*)`. `--k-header-h` is the top bar height plus the top inset. `--k-gutter` is the page gutter, never smaller than a side inset. Use these tokens instead of raw `env()` calls.
- The phone shell: a bottom tab bar is the primary navigation and the top bar has no nav links. It is on for screens up to 640 pixels wide, and for a phone on its side (`max-height: 500px` and `pointer: coarse`). `--k-tabbar-h` is the space the bar takes, and `0px` where there is none. `BottomNav.svelte`, `Header.svelte` and `routes/(app)/+layout.svelte` share those two conditions.
- Tap targets: add the `k-hit` class to a control that is smaller than 44 by 44 pixels. On touch screens it adds an invisible touch area around the control and changes nothing you can see. Rows in menus get `min-height: 44px` under `@media (pointer: coarse)`.
- Text fields: on touch screens every text field is at least 16 pixels, because iOS zooms the page when a smaller field takes focus.
- Native app only: `apps/reader/static/theme-init.js` sets `data-native` on `<html>` and locks page zoom before first paint. Rules under `html[data-native]` turn off overscroll, the long-press callout, the tap highlight and text selection on controls. A browser keeps its own behaviour.

`apps/reader/src/lib/native.ts` holds the two calls a screen can make. `allowPageZoom()` lets the chapter reader zoom in and locks zoom again when the reader closes. `setStatusBarHidden()` asks the native shell to hide the status bar; see [iOS](/developers/ios).

A `backdrop-filter` on an element makes it the containing block for `position: fixed` children. A full-screen backdrop inside the blurred top bar therefore sizes itself with viewport units (`100vw`, `100dvh`), not `inset: 0`.

The rating row in `apps/reader/src/lib/components/Stars.svelte` is one touch target: a finger selects the star under it, a slide corrects the choice, and lifting commits. In paged mode the reader turns the page on a sideways swipe, following the reading direction; it ignores the swipe while the page is zoomed in or wider than the screen.

## Where to look

- `apps/reader/vite.config.ts`: Target selection, adapter configuration, style inlining thresholds, and Content Security Policy definitions.
- `apps/reader/src/lib/config.ts`: Build-time environment variable bindings, server address persistence, and runtime target flags.
- `apps/reader/src/lib/server-connect.svelte.ts`: Address normalization, server checks and switching servers.
- `apps/reader/src/lib/components/ConnectServer.svelte`: Connect screen and QR scanning on supported phones.
- `apps/reader/src/lib/ssr.ts`: The `SSR_PUBLIC` flag controlling route-level server rendering.
- `apps/reader/src/lib/stream.ts`: Logic distinguishing edge rendering and hydration from subsequent client navigations.
- `apps/reader/src/lib/context.ts`: Application singletons for the API backend, image provider, and offline write queue.
- `apps/reader/src/lib/data/load-fetch.ts`: SvelteKit fetch integration and serialized response hydration.
- `apps/reader/src/routes/read/[slug]/+page.svelte`: Reader layout, image queuing, aspect ratio stabilization, and prefetch logic.
- `apps/reader/src/lib/app.css`: Safe-area tokens, the phone shell token, `k-hit` and the native-app rules.
- `apps/reader/src/lib/native.ts` and `apps/reader/static/theme-init.js`: Zoom lock and status bar calls for the native app.
