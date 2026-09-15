---
title: iOS build
description: Build the iOS reader app with its in-process Java runtime, from the toolchain to the pruned JAR, the zstd decoder and a release build.
---

Use this page when you build the iOS reader app and the Java runtime inside it. Komiq is self-hostable: the admin app runs and manages the server, and the reader app connects to it. For how the in-process engine starts, how to turn it on for a test and what a phone has shown, read [iOS engine](/developers/ios-engine).

## Prepare the toolchain

You need the workspace dependencies, Rust and the physical-device target `aarch64-apple-ios`. The reader crate declares Rust 1.78 as its minimum. The root package requires Node 22.13 or later and selects pnpm 11.9.0.

The JVM build recipe requires Apple Silicon macOS, Xcode with the iPhoneOS software development kit (SDK) and `autoconf`. Its pinned OpenJDK source tree is Java Development Kit (JDK) 27 development code. It accepts a boot JDK, the existing Java toolchain used to compile it, at version 25, 26 or 27. The desktop JDK 21 recipe is separate.

You can set `BOOT_JDK` to your own suitable JDK. Alternatively, `KOMIKA_FETCH_BOOT_JDK=1` fetches the script's pinned Temurin 26 boot JDK. The recipe also downloads verified iOS support libraries and a pinned Temurin 27 early-access JDK for assembling the Java class-library image.

For a release build, also run `rustup component add llvm-tools`. The steps are under "Build a release" below.

Configure `bundle.iOS.developmentTeam` in `apps/reader/src-tauri/tauri.conf.json` for your own signing team. That file sets `minimumSystemVersion` to `15.0`. This is a shell configuration value, not evidence that the embedded JVM works across every iOS version from 15 onward.

## Build and stage the runtime

Run these preparation commands from the repository root:

```sh
bash apps/reader/src-tauri/scripts/fetch-suwayomi-jar.sh
KOMIKA_FETCH_BOOT_JDK=1 bash apps/reader/src-tauri/scripts/build-ios-jvm.sh
bash apps/reader/src-tauri/scripts/build-ios-zstd.sh
bash apps/reader/src-tauri/scripts/prune-ios-jar.sh
pnpm -C apps/reader tauri ios init
```

The archive script verifies the pinned Suwayomi JAR, a Java application archive. The JVM script checks out a pinned OpenJDK Mobile revision and applies the bundled patch. Its output belongs in `apps/reader/src-tauri/jre-ios/aarch64-ios/`. The order of the three scripts matters: the zstd script needs the output of the JVM script, and the prune script needs the zstd loader class. A full run of `build-ios-jvm.sh` makes `jre-ios` again, so run `build-ios-zstd.sh` after it.

The class library ships as one file, `lib/modules`, in the jimage format. The pinned runtime reads jimage 1.0 only. A newer `jlink`, the Java tool that writes this file, writes 1.1, and the JVM then stops the whole app at start with "Unable to load ... lib/lib/modules". So the script pins a linker JDK that writes 1.0. It also copies the trust store and other support files as real files, not symbolic links. It fails when the file is not 1.0 or when a link is left in the output.

The image holds the 31 modules in `IOS_MODULES` in `build-ios-jvm.sh`. The desktop engine runs the same JAR on a similar list (`MODULES` in `build-jre.sh`). Before 2026-10-06 the script linked all 63 modules, including tools a phone never runs. `jdk.localedata` stays on purpose, because extensions read chapter dates with the month names of other languages. If the engine fails with `NoClassDefFoundError` or "module not found" for a JDK class, add the module to `IOS_MODULES`. Then run `build-ios-jvm.sh --relink-modules`, which links `lib/modules` again from the classes of the image already in `jre-ios`, so it needs no JDK build tree.

Before linking, check for `apps/reader/src-tauri/jre-ios/aarch64-ios/lib/zero/libjvm.a` and `apps/reader/src-tauri/jre-ios/aarch64-ios/lib/support/libffi.a`. `libffi` is the native-call support library needed by Zero. `apps/reader/src-tauri/build.rs` expects the latter archive at that exact location; the build recipe does not contain a separate copy step to place it there. Stage the iOS support archive there if it is absent.

Next, merge the link recipe recorded in `apps/reader/src-tauri/scripts/stage-ios-jvm-runtime.sh` into the app target in `apps/reader/src-tauri/gen/apple/project.yml`. It requires:

- A folder reference for `jvm-runtime/lib`.
- CoreFoundation and Foundation framework dependencies.
- `DEAD_CODE_STRIPPING: false`, which keeps runtime-resolved native symbols.
- `STRIP_STYLE: non-global`. A release archive otherwise strips the exported names that the JVM looks up at run time (`Java_*`, `JNI_OnLoad_*`).
- The listed archives loaded with `-force_load`, including `libzstdkmp.a`, plus `-lz`, `-liconv` and `-lc++`.

`apps/reader/src-tauri/build.rs` links the JVM and `libffi` into the Rust library. It also compiles a bundled C++ shim, a small compatibility layer for missing Zero runtime symbols. Xcode must link the remaining JDK archives using the recorded recipe.

After applying those settings, stage the runtime and regenerate the Xcode project:

```sh
bash apps/reader/src-tauri/scripts/stage-ios-jvm-runtime.sh
cd apps/reader/src-tauri/gen/apple
xcodegen generate
```

The staging script checks the required project entries, `libzstdkmp.a` among them, and prints the recipe if they are missing. Reapply the recipe and rerun staging after regenerating the Apple project. It also checks the staged runtime: jimage 1.0, no symbolic links, security files not empty.

The class-library image must reach the app bundle at `lib/lib/modules`, relative to the app executable. The statically linked JVM derives its runtime location before processing Java options. The Suwayomi archive is a separate Tauri resource selected by `apps/reader/src-tauri/tauri.ios.conf.json`. Desktop JRE resources must not enter this bundle; staging rejects an existing `apps/reader/src-tauri/gen/apple/assets/jre` directory.

### Prune the JAR and add zstd

`prune-ios-jar.sh` copies the pinned JAR to `apps/reader/src-tauri/suwayomi-ios/Suwayomi-Server.jar` without `font/`, `native/`, `jni/` and `WebUI.zip`. A phone cannot use them, and no class is removed. `tauri.ios.conf.json` bundles the copy under the usual name and runs the script before a build. The desktop apps ship the pinned JAR unchanged. The copy stays outside `suwayomi/` because the desktop apps bundle that whole directory.

Extension sites can answer with zstd, a compression format. The engine's decoder is zstd-kmp, Kotlin classes over a JNI library. Its loader throws "Unsupported OS: darwin" on a phone, so the first call to a source that answered with zstd failed. `build-ios-zstd.sh` builds the fix from the pinned zstd-kmp 0.4.0 and zstd sources, both checked by SHA-256:

- `jre-ios/aarch64-ios/lib/libzstdkmp.a`: the decoder for iOS arm64. Only the JNI functions are global, so its names cannot clash with anything in the app.
- A one-line loader class from `apps/reader/src-tauri/scripts/ios-zstd/` that calls `System.loadLibrary("zstdkmp")`. `prune-ios-jar.sh` puts it into the iOS JAR in place of the shipped loader. It is the one class the script replaces.

### Build a release

A release build for iOS links when three things are in place:

- `rustup component add llvm-tools`. Xcode 27 turns the Swift plugin functions of a release package into local symbols, and `swift-rs` needs `llvm-objcopy` from this component to make them global again. Without it the link fails late with "Undefined symbols". `apps/reader/src-tauri/build.rs` prints a warning for an iOS release build without the tool.
- The profile lines in `apps/reader/src-tauri/Cargo.toml`: `[profile.release.package.tauri]` with `debug = "line-tables-only"`, and `strip = "debuginfo"` for `[profile.release]`. They are already in the file. The first makes `swift-rs` build tauri's Swift package in its debug configuration, which keeps the symbols the linker needs. The second keeps the line tables out of the released binary. Remove both when `swift-rs` makes its own exports global.
- `STRIP_STYLE: non-global` in `gen/apple/project.yml`, as above. `stage-ios-jvm-runtime.sh` asserts it.

The test run used `pnpm tauri ios build --export-method debugging --ci` in `apps/reader`. A release build does not turn the engine on. It still starts only with the override; see [iOS engine](/developers/ios-engine).

## Make the WebView feel native

The reader runs in a WebView that covers the full screen, under the notch and the home indicator (`viewport-fit=cover`). The frontend keeps content clear of them with the safe-area tokens described in [The reader app](/developers/reader-app).

- Zoom: `apps/reader/static/theme-init.js` adds `maximum-scale=1, user-scalable=no` to the viewport when it runs inside Tauri. The chapter reader lifts the limit for zooming in while it is open.
- Status bar: the `set_status_bar_hidden` command in `apps/reader/src-tauri/src/lib.rs` sends `setPrefersStatusBarHidden:` to the root view controller. The chapter reader calls it when its top bar hides or shows, and once more when it closes. The command does nothing on other platforms. The one Objective-C message uses the `objc2` crate, declared for iOS only in `apps/reader/src-tauri/Cargo.toml`.

The connect screen checks a server with a request from the WebView's own origin, `tauri://localhost`. The server must list that origin in `CORS_ORIGINS`. See [Auth and security](/developers/auth-security).

## Where to look

- `apps/reader/src-tauri/src/lib.rs`: mobile plugins and the status bar command.
- `apps/reader/src-tauri/build.rs`: static linking, C++ shim compilation and the `llvm-tools` warning.
- `apps/reader/src-tauri/scripts/build-ios-jvm.sh`: pinned runtime build, toolchain requirements, `IOS_MODULES` and `--relink-modules`.
- `apps/reader/src-tauri/scripts/build-ios-zstd.sh`: zstd decoder library and iOS loader class.
- `apps/reader/src-tauri/scripts/ios-zstd/`: the C entry point and Java loader source that script compiles.
- `apps/reader/src-tauri/scripts/prune-ios-jar.sh`: the iOS copy of the Suwayomi JAR.
- `apps/reader/src-tauri/scripts/stage-ios-jvm-runtime.sh`: bundle layout and Xcode link recipe.
- `apps/reader/src-tauri/scripts/ios-jvm-wx-shim.cpp`: Zero link shim.
- `apps/reader/src-tauri/tauri.ios.conf.json`: iOS archive resource and the prune step.
- `apps/reader/src-tauri/Info.ios.plist`: QR scanning camera usage text.
- `apps/reader/vite.config.ts`: native frontend build selection.
