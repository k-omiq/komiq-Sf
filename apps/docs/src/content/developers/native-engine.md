---
title: The on-device engine
description: Understand how the reader app fetches content on your device while keeping accounts and catalogue data on the server you choose.
---

Use this page when you work on source fetching in the native reader app. Komiq is self-hostable: the admin app runs and manages the server, and the reader app connects to a server. This page covers the reader's bundled Suwayomi engine, its request path and its lifecycle.

The reader uses Tauri to display the web interface and expose Rust commands. Desktop has an engine enabled for normal use, and so does an Android build that carries one. The iOS implementation remains behind an explicit test override.

## Choose where fetching runs

The reader app connects to a server on your local network or a server on the internet. In this page, "the server" means whichever komiq server the reader is connected to. The connect screen accepts an IP address and port, a domain or a pasted link; mobile builds also offer QR scanning. See [Connect to a server](/guide/connect).

On builds that report engine support, you can choose to fetch chapters on your device. The choice is made on the connect screen ("Who gets the chapters?") and in **Settings › Reading** ("Fetch chapters on this device"). The reader keeps it in local storage under `komiq-fetch` as `device` or `server`; the default is server fetching. `apps/reader/src/lib/config.ts` reads it once at load, so a change reloads the app.

A server can remove the choice. When it runs with `KOMIKA_APPS_FETCH_THEMSELVES=on`, its `instance` query returns `appsFetchThemselves: true`. The connect screen then greys out "The server", and the settings switch stays on. An app that still holds `server` stores `device` when it reads that answer, and uses it from its next start. The server still answers `pages`, so the fallback below keeps working.

The Rust command `engine_supported` returns true on desktop. On Android it returns true when `libjvm.so` is in the app and the phone has Android 9 (API 28) or newer. On other platforms, it returns true only when an engine autostart override is enabled. The connect and settings flows use this command to decide whether to offer device fetching.

When device fetching is selected, `apps/reader/src/lib/context.ts` creates a `CompositeBackend`. This is an adapter that divides requests between the komiq server and a local content backend. `LocalSuwayomiBackend` checks support and calls `suwayomi_start`. Rust starts the engine at most once per app run.

The engine does not start automatically by default. You can enable autostart with the process setting `KOMIKA_NATIVE_ENGINE` or the compile-time setting `PUBLIC_KOMIKA_NATIVE_ENGINE`. Starting the engine and selecting it for reader content are separate decisions.

## Bundle the engine and Java

Suwayomi runs from a JAR, a Java application archive. The pin in `apps/reader/src-tauri/suwayomi/VERSION` selects version `v2.3.2243`. `apps/reader/src-tauri/scripts/fetch-suwayomi-jar.sh` downloads that archive and checks its SHA-256 digest, a checksum used to verify the downloaded bytes.

Desktop also bundles a Java runtime environment, or JRE. `apps/reader/src-tauri/scripts/build-jre.sh` uses `jlink`, the Java tool that assembles a runtime from selected modules. It requires Java Development Kit (JDK) 21 and builds for the host operating system and processor. It skips desktop runtime creation when Tauri identifies an iOS or Android build.

The desktop supervisor finds Java under the bundled `jre/<arch>-<os>/bin/` directory and the archive under `suwayomi/`. Development overrides are `KOMIKA_JAVA_BIN` and `KOMIKA_SUWAYOMI_JAR`. Missing assets put the engine into `degraded` state without aborting the app.

On physical iOS devices, the separate implementation links an interpreter-only Java virtual machine, or JVM, into the app. See [iOS build](/developers/ios) for its build recipe and [iOS engine](/developers/ios-engine) for run-time limits.

On Android, the same stock jar runs in-process on a Termux-built OpenJDK 21 that `apps/reader/src-tauri/scripts/build-android-jvm.sh` assembles, and the APK bundles it. See [Android](/developers/android) for the packaging and what has been tested.

## Follow a content request

The reader calls Rust through inter-process communication, or IPC, using Tauri commands. Rust sends HTTP requests to Suwayomi over loopback, a network connection confined to the same device. The local backend does not fetch the engine's HTTP address from JavaScript.

```flow
Reader -> Local content backend: requests chapter pages
Local content backend -> Rust command: invokes suwayomi_gql
Rust command -> Suwayomi: posts a GraphQL request
Suwayomi -> Local content backend: returns engine page paths
Image provider -> Reader: returns an engine-image address per page, no bytes yet
Reader -> Rust handler: web view loads each page by its address
Rust handler -> Suwayomi: requests the image with the engine password
Rust handler -> Reader: answers the web view with the image
```

GraphQL is the query interface used by the engine. `suwayomi_gql` returns its JSON response. Engine page paths carry the `engine:` prefix so the image provider can distinguish them from paths on the chosen komiq server.

The image provider does not fetch the bytes of an engine page. It gives each page a real address at once, and the web view loads the pages itself. The reader's own loading rules then apply: the first three pages first, then two at a time, and lazy loading for the rest. So the first page shows while later pages still load.

There are two address forms:

- Desktop and iOS use the custom address scheme `engine-image`: `engine-image://localhost/api/v1/…`. `apps/reader/src-tauri/src/lib.rs` registers it. The constant `ENGINE_IMAGE_SCHEME` there must match the one in `packages/api/src/image-provider.ts`.
- Android uses a loopback HTTP address, `http://127.0.0.1:<port>/<token>/api/v1/…`, served by `apps/reader/src-tauri/src/engine_image_server.rs`. On Android a scheme request that waits holds back every other request of the web view. See [Android](/developers/android).

The provider asks the Rust command `engine_image_base` once. On Android it returns the listener's address. Elsewhere, or when the listener cannot start, it returns nothing and the provider builds the `engine-image` address.

Both forms end in `suwayomi::engine_image`. It accepts rooted `/api/` paths only. It rejects path traversal and backslashes, also in percent-encoded form, where characters are written as numeric codes. It caps an image at 32 MiB and shares a six-request limit with `fetch_image`. It adds the engine's password itself, so the page never sees the engine's port or password. A failed image is a 502, and the reader shows its placeholder for that page.

The command `suwayomi_image` returns the same bytes over IPC. The reader's pages no longer use it; the `native-read` harness does.

## Keep identity on the server

The local backend provisions extensions, installing a source plugin when needed. It checks the engine's installed state, adds an extension repo when supplied, refreshes extension metadata and installs the requested package. Concurrent requests for the same package share installation work. MangaDex provisioning uses client-supplied coordinates; other chapter origins can supply their extension package and repo.

The composite backend keeps accounts, catalogue data, library state, progress and social features on the komiq server. Canonical chapter lists, the shared lists used for chapter identity, retain the server's identifiers and ordering. For a MangaDex work, the adapter matches those chapters to local chapters by number, using the scanlator field to break ties when possible.

For a Suwayomi chapter, a chapter from any other extension, the adapter asks the server for the source's manga and chapter keys with the `chapterOrigin` query. The local engine then resolves that chapter in its own database. A server engine's numeric row identifier is not treated as a local identifier, because each engine numbers its own rows.

Only chapter pages move to the device. Chapter lists and covers still come from the server.

If local resolution fails or the engine is unavailable, page-list requests fall back to the server. The Suwayomi chapter path waits up to eight seconds for local pages before returning the server's answer. Certain local failures are remembered for the backend session to avoid repeated failed provisioning. This fallback does not guarantee recovery from every later image-byte failure.

## Manage the lifecycle

Desktop writes `server.conf` into the app-data directory's `suwayomi` folder before each launch. It chooses a free loopback port, disables the engine's web interface and bundled Chromium integration, and configures HTTP Basic authentication with a credential generated for that app run.

The supervisor launches Java with a 512 MiB heap cap, limiting memory for Java objects, and polls `aboutServer` for readiness. Status exposes `starting`, `ready`, `degraded` or `stopped`, plus a version and `lastError`. Unexpected exits trigger restarts with increasing delays. More than five restart attempts in a minute trigger a 60-second backoff.

A lockfile with a periodic heartbeat, a timestamp refreshed by the running app, limits concurrent use of the same engine data directory. A refused lock is retried, and a stale heartbeat allows takeover. Normal app exit requests engine shutdown. Unix tries a termination signal before forcing a kill; Windows uses process termination.

## Protect the engine

The engine's GraphQL API can install extensions, which runs code inside the app. A bind to `127.0.0.1` keeps out the network, not other software on the same device. So the engine runs behind HTTP Basic authentication on desktop, iOS and Android.

`apps/reader/src-tauri/src/engine_auth.rs` holds the one implementation. `suwayomi.rs` and `suwayomi_inprocess.rs` both include it. It sets the user to `komika`, makes a random password for each app run, builds the client that sends the header and renders the `server.conf` template. The password lives in the app's memory and in `server.conf`. It is not logged, and JavaScript never sees it.

The Android image listener is a second loopback door, so it has its own guard: every request must carry a random 128-bit token of the current app run. The token is not the engine's password. It opens engine image requests only, behind the same path checks. See [Android](/developers/android).

## Current limits

The iOS device engine boots once per process and needs an app relaunch after a terminal failure. On one test iPhone with the override on, it returned a chapter's page images. It stays off in normal builds, and the reader's own path through it is not tested. See [iOS engine](/developers/ios-engine) for open defects. iOS simulators return an unavailable-engine status.

The Android engine has been tested on one phone. See [Android](/developers/android) for what is not tested. The desktop Cloudflare challenge integration is not compiled into either mobile implementation.

The `engine-image` scheme compiles for desktop and iOS, but nobody has loaded a page through it at run time there. The Android listener ran on an emulator and on one phone.

The `native-read` harness runs two real engines but replaces the Tauri commands with a Node HTTP adapter. It does not verify the Rust commands or image painting in a native window. See [Testing](/developers/testing). Read [Desktop](/developers/desktop) for the two desktop apps and their different process cleanup behavior.

## Where to look

- `apps/reader/src-tauri/src/lib.rs`: platform selection, startup commands and image limits.
- `apps/reader/src-tauri/src/suwayomi.rs`: desktop launch, locking and restart policy.
- `apps/reader/src-tauri/src/engine_auth.rs`: engine password, client and `server.conf` template.
- `apps/reader/src-tauri/src/suwayomi_inprocess.rs`: the in-process engine shared by iOS devices and Android: boot, readiness and liveness checks.
- `apps/reader/src-tauri/src/suwayomi_inprocess_ios.rs`: where the JVM and its files come from on a physical iOS device.
- `apps/reader/src-tauri/src/suwayomi_inprocess_android.rs`: where the JVM and its files come from on Android.
- `apps/reader/src-tauri/src/suwayomi_mobile.rs`: unavailable-engine commands for the iOS simulator.
- `apps/reader/src-tauri/scripts/build-jre.sh`: desktop Java runtime recipe.
- `apps/reader/src-tauri/scripts/build-android-jvm.sh`: Android Java runtime recipe.
- `apps/reader/src-tauri/suwayomi/VERSION`: archive version and checksum pin.
- `apps/reader/src/lib/context.ts`: reader backend selection.
- `packages/api/src/local-suwayomi-backend.ts`: local content queries and provisioning.
- `packages/api/src/composite-backend.ts`: chapter mapping and server fallback.
- `apps/reader/src-tauri/src/engine_image_server.rs`: the Android loopback image listener and its token check.
- `packages/api/src/image-provider.ts`: engine image paths and the `engine-image` or loopback addresses built from them.
- `apps/reader/src-tauri/e2e/native-read/README.md`: harness scope and requirements.
