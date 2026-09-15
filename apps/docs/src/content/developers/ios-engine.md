---
title: iOS engine
description: Understand how the iOS reader starts its in-process Suwayomi engine, how to turn it on for a test and what a phone test has shown so far.
---

Use this page when you work on the iOS reader app's embedded source engine at run time: how it starts, how to turn it on for a test, what a phone test has shown and what is still open. Komiq is self-hostable: the admin app runs and manages the server, and the reader app connects to it. The physical-device engine is implemented separately from desktop and remains disabled for normal reader use. To build the runtime, read [iOS build](/developers/ios).

## Separate the reader from the engine

The reader uses Tauri, a native shell that displays the web interface and exposes Rust commands. Its native build uses a static SvelteKit frontend, a bundle of browser assets without a web server runtime.

You can connect to a server on your local network or a server on the internet. The app cannot connect to the global server, which accepts requests from the `komiq.cc` website alone. The shared connect flow accepts an IP address and port, a domain or a pasted link. Mobile builds also register the barcode scanner plugin for scanning a server's QR code. In this page, "the server" means whichever komiq server the reader is connected to.

The `engine_supported` Rust command enables device fetching on desktop by default. On iOS, it only reports support when an explicit engine autostart override is enabled. An ordinary iOS build therefore uses the server for content fetching.

Conditional compilation selects `apps/reader/src-tauri/src/suwayomi_inprocess.rs` for the physical-device iOS target with an empty target ABI. Android uses the same file; the iOS-only part is `suwayomi_inprocess_ios.rs`. An ABI is the binary interface for a target platform. Simulator builds select `apps/reader/src-tauri/src/suwayomi_mobile.rs`, whose engine commands report unavailable. A simulator build does not exercise the physical-device engine.

## Run Java inside the app

The iOS implementation calls `JNI_CreateJavaVM` to create a Java virtual machine, or JVM, inside the reader process. JNI is Java's native interface for calling between Java and native code. A dedicated native thread then invokes `suwayomi.tachidesk.MainKt.main` from the bundled Suwayomi archive.

The runtime is OpenJDK Mobile's Zero interpreter. It executes Java instructions without a JIT compiler, which would generate machine code at runtime. Its static libraries become part of the app binary. The reader does not launch a separate `java` process.

Rust proxies GraphQL queries and image requests to the engine over loopback, a connection confined to the device. GraphQL is the engine's query interface. The JavaScript backend uses the same command names as desktop for GraphQL and status: `suwayomi_gql`, `suwayomi_status` and `suwayomi_base_url`.

Page images do not use `suwayomi_image`. On iOS the image provider gives each page an `engine-image://localhost/api/v1/…` address, and the web view loads it by itself. Rust answers it through the `engine-image` scheme registered in `lib.rs`, with the engine's password added on the way. `engine_image_base` returns nothing on iOS, so the provider always builds the scheme address. Nobody has loaded a page through the scheme on an iPhone. It compiles for an iOS device and for the simulator, and nothing more was checked.

The JVM caps its Java heap, memory for Java objects, at 256 MiB. Its Java threads use a 4 MiB stack setting for function calls. Its owner thread has a 16 MiB native stack. The heap cap does not bound the whole app's memory. Temporary files and Java's home-directory setting point into writable app data.

It runs the serial garbage collector (`-XX:+UseSerialGC`, `GC_OPT` in `suwayomi_inprocess.rs`), because the default collector starts several worker threads that a phone does not need. The engine's backup and library-update jobs are off on the phone: `PHONE_SERVER_CONF` sets `server.backupInterval` and `server.globalUpdateInterval` to 0. The memory this saves is not measured.

## Protect the engine with a password

The iOS engine requires HTTP Basic authentication, as the desktop engine does. Both share `apps/reader/src-tauri/src/engine_auth.rs`; [The on-device engine](/developers/native-engine) explains why and how. On iOS, `suwayomi_inprocess.rs` builds one HTTP client with `engine_auth::build_client`, and the readiness poll, the heartbeat, `suwayomi_gql` and the engine image fetch all use it. The password is new on every start of the app. It lives in app memory and in `server.conf` in the app's data container, and in no log.

To probe a debug build over a USB port forward, read the port and the password from `server.conf` with `xcrun devicectl device copy from`. That works only for an app signed for development. Copy the file again after every start, and do not print or paste the password.

## Test the engine

For an explicit device-engine test, build from the repository root:

```sh
PUBLIC_KOMIKA_NATIVE_ENGINE=on pnpm -C apps/reader tauri ios build
```

This compile-time setting enables autostart and makes the support command report true. Select device fetching in the reader to route content through the local backend. The setting is a test override, not a device verification result.

To switch the engine on for one launch of an installed debug build, set the process environment variable `KOMIKA_NATIVE_ENGINE`:

```sh
xcrun devicectl device process launch --device <udid> --terminate-existing \
  --environment-variables '{"KOMIKA_NATIVE_ENGINE":"on"}' app.komika.reader
```

The phone must be unlocked. A launch from the home screen has the engine off. The app log is `Library/Application Support/app.komika.reader/logs/komika.log` in the app data container. Copy it with `xcrun devicectl device copy from` and look for "engine ready".

## Know the lifecycle limits

The implementation allows one JVM boot per app process. Readiness polling has a 300-second budget. After readiness, a heartbeat, a periodic HTTP check, tests the engine every 30 seconds. Three consecutive failures mark it degraded. Terminal errors tell you to restart the app.

The shutdown command marks the engine stopped without waiting for JVM destruction. The JVM ends with the process. There is no app background or foreground lifecycle handler in the reader's engine wiring.

The iOS engine is off by default, and `engine_supported` is unchanged. It is not shipped as an on-by-default feature.

On 2026-10-06 the pruned release build ran on an iPhone 13 Pro with the override on. The engine reported ready after about 20 seconds. Driven over a USB port forward, with the password, it added an extension repo (1413 extensions listed) and installed one extension. One source then returned 20 series, 119 chapters for one of them and 21 pages for one chapter. Page images came back as WebP, and the same request without the password got 401. Before the zstd fix, the first call to that source failed.

Not tested yet:

- The reader's own device-fetch path. Nobody has connected the app on a phone to a server, chosen "This device" and read a chapter. The app itself has not called `suwayomi_gql` or loaded a page image through the `engine-image` scheme on a phone.
- Memory use. The heap cap is set, but app memory is not measured.
- A second source, and zstd compression itself (only the decoder ran).
- A build from a clean checkout with the script order on [iOS build](/developers/ios).

Open defect: each test run logged one "engine heartbeat missed (1/3)" line. It never reached 2/3, and the engine answered afterwards. The cause is not known. iOS also suspends an app that is off screen, and the heartbeat then misses checks. Treat the iOS engine as unproven for reading until the first item is done.

The desktop Cloudflare challenge integration is excluded from the iOS build. The iOS engine configuration disables `flareSolverrEnabled`. The desktop native-read harness uses a substituted transport, so it cannot establish iOS device behavior. No iOS job appears in the native-sidecar workflow.

## Java preferences on iOS

The JVM gets one option that the desktop engine does not need: `-Djava.util.prefs.PreferencesFactory=java.util.prefs.MacOSXPreferencesFactory`. It is `PREFS_FACTORY_OPT` in `apps/reader/src-tauri/src/suwayomi_inprocess_ios.rs`. Do not remove it.

Java has several stores for its preferences API and picks one by operating system. On iOS it picks the file store. But the iOS runtime build compiles the native half of the macOS store only, so the native functions of the file store do not exist in the app. Suwayomi reads Java preferences once, in its first migration. Without the option, that read ended the engine on the first two launches of a new install, and a timer then logged an `UnsatisfiedLinkError` every 30 seconds. With the option the first migration passes on a phone, and the store writes to `Library/Preferences` in the app data container.

Suwayomi's own settings are not in this store. They are XML files in the `settings` folder of the engine data folder.

## Read a Java error from the phone

When Java code throws during start, the Rust side writes the exception class, its message and its stack trace to `komika.log`, under the `suwayomi` target. The first line is also in the engine status the frontend reads (`lastError`). Nothing reads the app's standard error stream on a phone, so the log file is the place to look.

To log every exception the JVM throws, also the ones Java code catches, add `JAVA_TOOL_OPTIONS` to the launch environment with `-Xlog:exceptions=info:file=<data container>/Documents/jvm-exc.log::filecount=0`. The data container path is in the "launching in-process engine" line of `komika.log`. To override a Java option the app sets, use `_JAVA_OPTIONS`: the JVM reads it last.

## Know what hides the device choice

`engine_supported` is false in an app started from the home screen. The reader then shows neither the "Who gets the chapters?" step on the connect screen nor the "Fetch chapters on this device" switch in Profile, Settings. The line "engine autostart disabled; waiting for suwayomi_start" in `komika.log` marks such a start. iOS closes an app that is left alone, and the next tap on its icon is a start from the home screen. Start the app with the `devicectl` command above right before you test.

## Where to look

- `apps/reader/src-tauri/src/lib.rs`: target selection and the support gate.
- `apps/reader/src-tauri/src/suwayomi_inprocess.rs`: JNI startup, shared Java options and heartbeat (also used by Android).
- `apps/reader/src-tauri/src/suwayomi_inprocess_ios.rs`: the iOS-only Java options and bundle checks.
- `apps/reader/src-tauri/src/engine_auth.rs`: engine user, per-run password, authenticated client and `server.conf` template, shared with desktop.
- `apps/reader/src-tauri/src/suwayomi_mobile.rs`: simulator engine stub.
