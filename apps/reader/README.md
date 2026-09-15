# sv

Everything you need to build a Svelte project, powered by [`sv`](https://github.com/sveltejs/cli).

## Creating a project

If you're seeing this, you've probably already done this step. Congrats!

```sh
# create a new project
npx sv create my-app
```

To recreate this project with the same configuration:

```sh
# recreate this project
npx sv@0.16.1 create --template minimal --types ts --no-install apps/reader
```

## Developing

Once you've created a project and installed dependencies with `npm install` (or `pnpm install` or `yarn`), start a development server:

```sh
npm run dev

# or start the server and open the app in a new browser tab
npm run dev -- --open
```

## Building

To create a production version of your app:

```sh
npm run build
```

You can preview the production build with `npm run preview`.

> To deploy your app, you may need to install an [adapter](https://svelte.dev/docs/kit/adapters) for your target environment.

## Build targets

`KOMIKA_TARGET` picks the build (default: the hosted web build):

| Target      | Adapter              | API and images                                                                      |
| ----------- | -------------------- | ----------------------------------------------------------------------------------- |
| unset (web) | Cloudflare, edge SSR | `PUBLIC_*` from `.env.production` (komiq.cc, unchanged)                             |
| `tauri`     | static SPA           | `PUBLIC_*` (native apps, the Docker image)                                          |
| `embedded`  | static SPA           | same origin: `/graphql`, own images direct, MangaDex via `/img`; `PUBLIC_*` ignored |

The `embedded` build is the one the API server serves at `/` from `KOMIKA_WEB_DIR`
(next to the embedded admin at `/admin`):

```sh
KOMIKA_TARGET=embedded pnpm -C apps/reader build   # output: apps/reader/build
```

Images: `PUBLIC_KOMIKA_IMG_MODE` is `proxy` (the `img.komiq.cc` Worker, hosted),
`direct`, or `server` — MangaDex images through the Komika server's own `/img` route
on the API origin, everything else direct. The `embedded` build always behaves like
`server` on its own origin. The server mounts `/img` when it serves the web apps
(`KOMIKA_WEB_DIR`) or when `KOMIKA_IMG_PROXY=on`.

Static builds ask the server's `instance` facts: a server without an owner shows a
"not set up" page that links to `/admin/setup`. The sign-in page hides sign-up when
the server has closed it (every build).

## Android

The Android app is the reader, and it can carry an on-device engine: a JVM inside the
app's own process that runs the pinned Suwayomi jar, so the phone gets chapter pages from
the sources itself. The engine is in the APK only when its runtime was built first
(`build-android-jvm.sh`, below). An APK built without it is the plain reader (5.5 MB), and
it does not offer "This device".

It needs JDK 21, the Android SDK (platform 36, build-tools, platform-tools) and an NDK,
and these in the shell:

```sh
export JAVA_HOME=/opt/homebrew/opt/openjdk@21/libexec/openjdk.jdk/Contents/Home
export ANDROID_HOME=/opt/homebrew/share/android-commandlinetools
export NDK_HOME=$ANDROID_HOME/ndk/28.2.13676358
export PATH=$ANDROID_HOME/platform-tools:$ANDROID_HOME/emulator:$PATH
```

```sh
bash apps/reader/src-tauri/scripts/fetch-suwayomi-jar.sh    # the pinned jar (once)
bash apps/reader/src-tauri/scripts/build-android-jvm.sh     # the engine runtime (once)
pnpm -C apps/reader android:dev      # run on a connected phone or a running emulator
pnpm -C apps/reader android:build    # release APK: arm64 only, size-optimized, NOT signed
```

The release APK lands in
`src-tauri/gen/android/app/build/outputs/apk/universal/release/`. It is unsigned, so a
phone refuses it until it is signed with a keystore.

### The on-device engine

Where the parts are:

- `src-tauri/scripts/build-android-jvm.sh` makes `src-tauri/jre-android/` (git-ignored).
  The JVM is HotSpot from Termux's `openjdk-21` package (OpenJDK 21.0.12, GPLv2 with the
  Classpath Exception, built for Android's libc, with the JIT). The packages are pinned
  by SHA-256. The script's header says what it changes in them and why.
- `jre-android/jniLibs/arm64-v8a/`: `libjvm.so` and the JDK's native libraries (20.6 MB,
  16 KB aligned). Gradle packs them as the app's native libraries.
- `jre-android/assets/engine/`: the Java runtime image, the Suwayomi jar without WebUI,
  fonts and desktop native libraries, and `zstd-android.jar` (184.5 MB). They are APK
  assets. The app copies them to `<app data>/engine/` on the first start after each
  install or update, because a JVM needs real files.
- `src-tauri/gen/android/app/build.gradle.kts` adds both directories to the app, and
  keeps the native libraries as files on disk (`useLegacyPackaging`).
- `AndroidManifest.xml` sets `android:allowNativeHeapPointerTagging="false"`. HotSpot
  ends the process without it.
- `src-tauri/src/suwayomi_inprocess.rs` is the engine code that iOS and Android share
  (start, readiness, heartbeat, the four commands, the loopback address with a password
  from `engine_auth.rs`). `suwayomi_inprocess_android.rs` holds the Android part: find
  the libraries, unpack the assets, load `libjvm.so`.
- `src-tauri/src/engine_image_server.rs` gives the engine's page images a plain address,
  `http://127.0.0.1:<port>/<token>/api/v1/…`, that the web view loads by itself. The
  other platforms use the `engine-image` address scheme for this (`lib.rs`; it compiles
  there and is not tested at run time). Android
  cannot: its web view asks the app about every request on one queue, and a page image
  that waits for a slow source then holds back every other request (1 to 6 s for a
  request that takes 5 ms, measured on the emulator). The listener starts with the first
  engine image. Each request must carry a random token made for this app run; the token
  opens image requests only, not the engine's API.
- `engine_supported` is true only when `libjvm.so` is in the app and the phone has
  Android 9 (API 28) or newer. The reader then shows "This device" on the connect screen
  and the switch "Fetch chapters on this device" in Settings. The app itself still
  installs from Android 7 (API 24).

Sizes and speed, measured on a Galaxy A73 5G (SM-A736B, Android 16) with a release
build on 2026-10-06:

|                                    | Without the engine | With the engine                      |
| ---------------------------------- | ------------------ | ------------------------------------ |
| APK                                | 5.5 MB             | 189.1 MB                             |
| On the phone after first use       | not measured       | 475 MB (app 222, data 218, cache 36) |
| Memory with one chapter open (PSS) | 279 MB             | 705 MB                               |

The engine is ready 11 to 12 s after the app starts (14.4 s on the first start, which
also unpacks the files in 3.3 s). Until then, and whenever the engine cannot answer, the
server gets the pages.

A new chapter opens in steps: the reader screen in about 0.5 s, the first page about
1 s after the tap, then about 2 pages each 2 s, which is the speed of the test
source. Measured on the emulator on 2026-10-06. On the Galaxy A73 the first page
of a new chapter was on the screen 1 to 3 s after the tap (screenshots, 3 chapters),
and the server got no page request.
A chapter the engine already has opens in full in under 1 s.

What is true about it today:

- Tested on that phone: a clean install, the first start, a restart, an update over an
  older APK with and without the engine, a chapter from a non-MangaDex source with no
  page request to the server, the same with the server's page routes
  blocked, a process kill, and 4 minutes in the background with the screen on (the
  process stayed and the engine answered its check every 30 s).
- An engine that stops answering is not started again in the same run (a process can
  create a JVM only once). The app uses the server until it is restarted. This was
  tested on the emulator only, where the engine's port can be blocked.
- There is no foreground service. The engine lives and stops with the app process.
- Not there: a Cloudflare solver, AWT (a source that needs either one fails on the
  device and the server gets the pages), 32-bit ARM.
- Not tested: a phone with 16 KB memory pages, the phone locked or asleep for a long
  time, low-memory phones (the phone above has 8 GB).
- `src-tauri/e2e/android-jvm-probe/` is a small test app that starts only the JVM and
  the jar on a phone (`run.sh`). Use it first when the Termux pins move.

### What keeps the plain app small

- `src-tauri/tauri.android.conf.json`: the build runs `pnpm build` only, so it never
  fetches the Suwayomi jar or builds a desktop JRE.
- `src-tauri/Cargo.toml`: the sidecar's crates (`tiny_http`, tokio's `process`) are
  desktop-only dependencies; `jni-sys`, `libc` and `zip` are Android-only.
- `vite.config.ts` (`WEB_ONLY_STATIC`): a native app build leaves out the browser-only
  static files (tab icons, web manifest, the link-preview image).
- `android:build` in `package.json`: LTO, one codegen unit, `opt-level=s` and strip for
  the Rust library, set by environment so desktop and iOS release builds do not change.
- `src-tauri/gen/android/app/build.gradle.kts`: R8, resource shrinking, English-only
  library strings.

`src-tauri/gen/android` is tracked in git (the rest of `gen/` is not) because it holds
hand edits: the app icons, plain HTTP allowed in release (a self-hosted server on a home
network has no certificate), the engine lines named above, and `MainActivity.kt`, which
keeps the page clear of the system bars and the keyboard on a WebView older than 139.
Newer WebViews do that themselves through CSS `env(safe-area-inset-*)`.
