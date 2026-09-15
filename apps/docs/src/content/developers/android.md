---
title: Android
description: See how the Android reader runs its on-device engine, how the APK is built and what has been tested on a phone.
---

This page covers the Android reader app and its on-device source engine. Komiq is self-hostable: the admin app runs and manages the server, and the reader app connects to it. The Android reader can carry the engine inside the app. When it does, the phone fetches chapter pages from the sources itself, and the server still keeps accounts, the catalogue and chapter lists.

## What the Android app does

The reader uses Tauri, a native shell that displays the web interface and exposes Rust commands. `apps/reader/src-tauri/gen/android` holds the generated Android project, and `apps/reader/package.json` has `android:dev` and `android:build` scripts. The build script targets `aarch64` only.

The reader connects to a komiq server on your local network or on the internet, by address or by QR code. See [Connect to a server](/guide/connect). In this page, "the server" means the server the reader is connected to.

An APK built without the engine is the plain reader. It is 5.5 MB and does not offer device fetching. An APK built with the engine offers it, as described below.

## The engine in the app

The reader runs the pinned `Suwayomi-Server` jar, version `v2.3.2243`, on a Java virtual machine (JVM) inside its own process. Desktop runs the engine as a child process, and real iOS devices run it in-process too. See [The on-device engine](/developers/native-engine) for the request path shared by every platform.

`engine_supported` in `apps/reader/src-tauri/src/lib.rs` is true on Android when two things hold: `libjvm.so` is in the app, and the phone has Android 9 (API 28) or newer. The connect screen then shows the step "Who gets the chapters?", and Settings shows the switch "Fetch chapters on this device". The app itself still installs from Android 7 (API 24).

### Where the JVM comes from

The engine uses the `openjdk-21` package from Termux, an Android terminal environment. It is OpenJDK 21.0.12 under GPLv2 with the Classpath Exception, built for bionic (Android's C library). It is a server VM with a just-in-time (JIT) compiler, and its libraries are 16 KB aligned. `apps/reader/src-tauri/scripts/build-android-jvm.sh` pins the packages by SHA-256.

The rejected alternative is the build iOS uses, from `openjdk/mobile`. It needs a Linux build host and hours of build time, and no benefit over the Termux package was found.

### How it runs

The app calls `dlopen`, the C library function that loads a shared library, on `libjvm.so` in its native library directory. It then calls `JNI_CreateJavaVM`. There is no child process and no `execve`, the call that starts another program. No foreground service was added, so the engine lives and stops with the app process.

The engine binds to loopback only and requires HTTP Basic authentication with a password made for each app run. `apps/reader/src-tauri/src/engine_auth.rs` holds that code, shared with desktop and iOS.

### What changes around the jar

No class in the pinned jar is changed. The build only removes resources a phone cannot use: the web interface, fonts and desktop native libraries. Four Android-specific adjustments sit around the jar:

1. A small helper library, packed as `libandroid-spawn.so`, corrects the path HotSpot derives `java.home` from. Its source is `apps/reader/src-tauri/scripts/android-jvm-shim.c`.
2. The app manifest turns off native heap pointer tagging with `android:allowNativeHeapPointerTagging="false"`. This is required. Without it HotSpot ends the process.
3. The engine needs Android 9 (API 28) or newer, because the helper relies on bionic's `posix_spawn`.
4. A 0.3 MB overlay jar, `zstd-android.jar`, sits before the Suwayomi jar on the class path. It supplies the Android build of the zstd native library, because the one inside the Suwayomi jar is built for glibc.

### How the code is split

The in-process engine code is shared with iOS. `apps/reader/src-tauri/src/suwayomi_inprocess.rs` holds start, readiness, the heartbeat and the engine commands. `suwayomi_inprocess_android.rs` holds the Android part: it finds the libraries, unpacks the assets and loads `libjvm.so`. `suwayomi_inprocess_ios.rs` holds the iOS part.

`lib.rs` picks the module named `suwayomi` for each platform. Desktop uses `suwayomi.rs`. `suwayomi_mobile.rs` is the stub for the iOS simulator only.

### Packaging

Everything is bundled in the APK, the Android install package. Nothing is downloaded on first use.

- The native libraries are in `jniLibs/arm64-v8a`: 20.6 MB, 16 KB aligned. `useLegacyPackaging` keeps them as files on disk.
- The engine files are APK assets under `assets/engine/`: 184.5 MB. They are the Java runtime image, the Suwayomi jar without its web interface, fonts, desktop native libraries and `zstd-android.jar`.
- On the first start after each install or update, the app copies the assets to `<app data>/engine/`, because a JVM needs real files.

`apps/reader/src-tauri/gen/android/app/build.gradle.kts` adds both directories to the app when they exist. Without the runtime built first, the APK has no engine.

## Load images through a loopback listener

The web view loads engine page images by address, one by one. [The on-device engine](/developers/native-engine) describes that path. Desktop and iOS use the `engine-image` address scheme for it. Android cannot. Its web view asks the app about every request on one queue (`shouldInterceptRequest`), and wry, the web view library under Tauri, waits on that queue until a scheme request has its answer. A page image that waits for a slow source then holds back every other request of the app.

So on Android the pages get a plain HTTP address, `http://127.0.0.1:<port>/<token>/api/v1/…`, served by `apps/reader/src-tauri/src/engine_image_server.rs`. The web view loads it as a normal network request, and the queue stays free.

- The listener binds `127.0.0.1` on a free port. It starts at the first call of the Rust command `engine_image_base`, so it does not exist while the engine is not used.
- Loopback is not an authorization boundary: another app on the phone can connect. So every request must carry a random 128-bit token as its first path segment, or it gets `403`. The app makes the token once per run and never logs it. The token is not the engine's password. It opens engine image requests only.
- It answers `GET` only, one request per connection, and at most 32 connections at a time. A request head may be 8 KiB at most and must arrive within 5 s.
- It answers with `cache-control: no-store`, because the port and the token change with each app run.
- It hands the path to `suwayomi::engine_image`, which checks the path and adds the engine's password.

If the listener cannot start, `engine_image_base` returns nothing and the page uses the `engine-image` scheme. That works, but other requests wait while pages load. The listener's request rules and its HTTP framing have unit tests that run on the host.

## Build the app

You need JDK 21, the Android SDK (platform 36, build-tools, platform-tools) and an NDK. Set `JAVA_HOME`, `ANDROID_HOME` and `NDK_HOME` in your shell. `build-android-jvm.sh` also needs `curl`, `perl`, `zip` and `tar`.

```sh
bash apps/reader/src-tauri/scripts/fetch-suwayomi-jar.sh
bash apps/reader/src-tauri/scripts/build-android-jvm.sh
pnpm -C apps/reader android:dev
pnpm -C apps/reader android:build
```

The first two commands each run once. The script builds the runtime into `apps/reader/src-tauri/jre-android/`, which git ignores. `android:dev` runs on a connected phone or a running emulator. `android:build` makes an arm64 release APK, size-optimized and not signed. A phone refuses it until you sign it with a keystore.

## Test the runtime alone

`apps/reader/src-tauri/e2e/android-jvm-probe/run.sh` proves the runtime on a device or emulator without any reader code. It builds a small debuggable app that holds only the runtime libraries, installs it, and copies the runtime and the jar into its private storage. It then drives the engine over `adb forward`. It uses the jar the app ships by default; pass `stock` to use the pinned jar as downloaded. Set `ANDROID_SERIAL` when more than one device is attached. Use it first when the Termux pins move.

The older `e2e/android-jdk-probe/run.sh` ran Suwayomi on Linux in Docker. That is not Android, and this probe replaces it as evidence.

## Status

### Measured on a phone

These numbers come from a Samsung Galaxy A73 5G (SM-A736B) with Android 16, arm64, WebView 153 and 4 KB memory pages. The app was a release APK signed with the debug key.

| Measure                            | Without the engine | With the engine                      |
| ---------------------------------- | ------------------ | ------------------------------------ |
| APK                                | 5.5 MB             | 189.1 MB                             |
| On the phone after first use       | not measured       | 475 MB (app 222, data 218, cache 36) |
| Memory (PSS) with one chapter open | 279 MB             | 705 MB                               |

The engine is ready 11 to 12 s after the app starts. The first start takes 14.4 s, of which 3.3 s is unpacking. Until the engine is ready, and whenever it cannot answer, the server gets the pages.

The first page of a chapter the engine did not have yet was on the screen 1 to 3 s after the tap, measured with timed screenshots over 3 chapters. The server got no page-list request and no page-image request for them.

What worked on that phone:

- A clean install, the first start, a restart, an update over an APK without the engine, and an update over an APK with the engine.
- A chapter from a source other than MangaDex. It opened with only a `chapterOrigin` query at the server: no `Pages` query and no page-image request. It also worked with the server's page-list and page-image routes blocked, which a test proxy answered with 503.
- A process kill and a restart. A chapter opened 7 s after the start came from the server, because the engine was still starting. The next chapter came from the device. There was no crash.

The first open of a series whose extension is not installed asks the person to allow the extension. The server serves that first open.

### Proven on the emulator only

On an Android 15 (API 35) emulator, the engine's port was blocked while the app ran. The next chapter came from the server at once. The heartbeat, a check every 30 s, declared the engine dead after 3 misses, about 90 s. There was no crash. After an app restart the engine served chapters again. An engine cannot be started twice in one process, so recovery needs an app restart.

Page loading was timed there with the web view's debugger. A chapter the engine did not have yet showed its reader screen in 0.35 to 0.5 s and its first page at about 1 s. Then about 2 pages arrived every 2 s, which is the speed of that source. A chapter the engine already had was complete in 0.34 s. A request for one of the app's own files took at most 25 ms while pages loaded. With the `engine-image` scheme the same request took 1 to 6 s.

### Not there or not tested

- Not there: a Cloudflare solver, AWT (Java's desktop graphics toolkit), CI, signing and 32-bit ARM. A source that needs the solver or AWT fails on the device, and the server gets the pages.
- Not tested: a phone with 16 KB memory pages, a phone locked or asleep for a long time, low-memory phones, and memory tuning.
- Seen once and not again in 5 more tries: a white screen on the first start right after an update. It was gone after a restart. The cause is unknown.

## Where to look

- `apps/reader/src-tauri/scripts/build-android-jvm.sh`: runtime build, package pins and the reasons for each change.
- `apps/reader/src-tauri/scripts/android-jvm-shim.c`: the `libandroid-spawn.so` helper that fixes the `java.home` path.
- `apps/reader/src-tauri/src/lib.rs`: the `suwayomi` module switch and `engine_supported`.
- `apps/reader/src-tauri/src/suwayomi_inprocess.rs`: the engine code shared by iOS and Android.
- `apps/reader/src-tauri/src/suwayomi_inprocess_android.rs`: finding the libraries, unpacking the assets and loading `libjvm.so`.
- `apps/reader/src-tauri/src/engine_auth.rs`: the engine password and HTTP client.
- `apps/reader/src-tauri/src/engine_image_server.rs`: the loopback image listener and its token check.
- `apps/reader/src-tauri/gen/android/app/build.gradle.kts`: how the two engine directories enter the APK.
- `apps/reader/src-tauri/gen/android/app/src/main/AndroidManifest.xml`: the app manifest and the pointer tagging setting.
- `apps/reader/src-tauri/e2e/android-jvm-probe/run.sh`: device and emulator proof of the runtime.
- `apps/reader/src-tauri/e2e/android-jdk-probe/run.sh`: older Linux container probe.
- `apps/reader/src/lib/server-connect.svelte.ts`: server selection and support check.
