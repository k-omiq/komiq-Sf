# N3.1-PROBE — headless acceptance findings

Headless acceptance for **N3.1** in `docs/plans/n3-android-spike.md` §2: prove the **stock
Suwayomi-Server `v2.3.2243`** jar boots and works under a **stock GPL+CE OpenJDK 21 on
Linux-aarch64**, exercising the full stack the Android in-process-JVM plan (path b) depends on,
and answer the spike's headless **Open Question 5**.

Runner: `./run.sh` (Docker container on this aarch64 macOS host). Re-runnable, self-cleaning.
All engine/network responses treated as **data**; no embedded instruction acted upon.

## TL;DR

**RESULT: PASS — 11/11 assertions. The stock `v2.3.2243` jar boots and runs the full content
path on a stock aarch64 OpenJDK 21 with no engine fork and no code changes.** Cold-start **3–4 s**
inside a fresh container. Two findings materially *de-risk* Android beyond the plan's assumptions:

1. **Open Q5 is effectively moot for this pin: the embedded DB is H2 (pure-Java), not SQLite.**
   The jar bundles **no** `xerial/sqlite-jdbc` and needs **no** native DB `.so` on Android.
2. **The AWT-reachability signal is zero** across boot + extension install + dex2jar + browse
   (0 `java.awt` / `sun.font` / `javax.imageio` class-loads) — an early positive indicator for N3.7
   (indicative only; the image-decode/cover-render endpoints were not exercised).

---

## Environment under test

| Item | Value |
|---|---|
| JDK image | `eclipse-temurin:21` (GPLv2 **with Classpath Exception**), platform `linux/arm64` |
| `java -version` | `openjdk 21.0.11 2026-04-21 LTS` / `Temurin-21.0.11+10-LTS` / `64-Bit Server VM (mixed mode, sharing)` |
| Host | aarch64 macOS, Docker Server 29.5.2 (container runs native aarch64 Linux) |
| Engine jar | `Suwayomi-Server.jar` `v2.3.2243`, SHA-256 `821141b3…e98dd5` — **matches** `suwayomi/VERSION` |
| Boot recipe | desktop recipe verbatim except `server.ip="0.0.0.0"` (so Docker's published port reaches the process; the port is published only to host `127.0.0.1`). `kcefEnabled=false`, `flareSolverrEnabled=false`, `webUIEnabled=false`, headless. `-Xmx512m`. |

### Jar/runtime compatibility notes
- **Base bytecode targets Java 21** (major version 65 on `MainKt.class` / `App.class`), so a stock
  JDK 21 runs it directly. The jar is a **Multi-Release** jar with override dirs
  `META-INF/versions/{9,11,15,17,21,23,25}/`; a JDK 21 runtime only consults `≤21` overrides and
  ignores 23/25. Manifest advertises `X-JBR-Release: jbr-release-25.0.3b508.4` (built *with*
  JetBrains Runtime 25) but that does not raise the runtime floor above 21. **JDK 21 is sufficient;
  no need to chase a 25 runtime.**
- Fat jar carries desktop natives for many platforms incl. Linux/aarch64 (`webp-imageio`,
  `zstd-kmp`, JNA `libjnidispatch`) plus optional `libtruffleattach` — relevant to N3.8 stripping,
  not to boot.

---

## Cold-start

**3 s** (final run; observed **3–4 s** across runs) from container process start to first successful
`aboutServer` — comfortably under the generous 120 s ceiling. This is a *container* cold start on a
stock JDK; on-device latency remains a device-only unknown (Open Q2 / N3.2).

## Gate results (per assertion)

| # | Assertion | Result | Evidence |
|---|---|---|---|
| a | jar SHA-256 == VERSION pin | **PASS** | `821141b3…e98dd5` |
| b | eclipse-temurin:21 aarch64 container boots the jar | **PASS** | container started, engine process alive |
| c | engine reaches ready | **PASS** | `aboutServer` responded in 3 s |
| c′ | version == `v2.3.2243` | **PASS** | `{"version":"v2.3.2243","revision":"r2243","buildType":"Stable"}` |
| e | Keiyoushi store added | **PASS** | `{"name":"Keiyoushi","badgeLabel":"KEI"}` |
| e | fetchExtensions refreshes list | **PASS** | 1359 extensions visible |
| e1 | MangaDex installed via store (`updateExtension install:true`) | **PASS** | `isInstalled:true`, v`1.4.211` (jarUrl path — see below) |
| e2 | MangaDex installed via apk (`installExternalExtension`) | **PASS** | `isInstalled:true` from uploaded `.apk` |
| e2 | **dex2jar APK→JAR ran on aarch64 JDK 21** | **PASS** | **235** `com.googlecode.d2j`/`dex2jar` class-loads; produced `extensions/mangadex.jar` |
| f | EN MangaDex source id resolved | **PASS** | `2499283573021220255` (lang `en`) |
| f | `fetchSourceManga(POPULAR,1)` non-empty | **PASS** | **20** mangas, `hasNextPage:true` (OkHttp fetch + Exposed/H2 persist) |

Network egress from the container (Keiyoushi index, GitHub-raw/jsDelivr apk+jar, MangaDex API) all
reachable.

### dex2jar: important correction to the plan's premise
The modern **Keiyoushi `repo` index publishes a pre-built `jarUrl`**
(`…/repo/jar/tachiyomi-all.mangadex-v1.4.211.jar`) alongside `apkUrl`. The normal store-install path
(`updateExtension install:true`) downloads that **`.jar` and does NOT run dex2jar** (0
`com.googlecode.d2j` class-loads; the engine even has migration `AddJarUrlToExtensionTable`). So the
plan's statement that installing the MangaDex extension "exercises dex2jar APK→JAR conversion" is
**false for the store path** on this pin. To genuinely exercise dex2jar this probe **also** uploads
the raw `.apk` via `installExternalExtension`, which converts it (**235** `com.googlecode.d2j`
class-loads → `mangadex.jar`). Net: **the ASM-based DEX→JVM converter runs correctly on a stock
aarch64 JDK 21**, and separately, the common provisioning path can skip it entirely.

---

## Open Question 5 — sqlite-jdbc version + Android natives availability

**Answer: moot for the pin, and fully solvable if it ever applies.**

1. **Stock `v2.3.2243` does not use SQLite.** The embedded DB it creates under `rootDir` is
   **H2** — a single `database.mv.db` (MVStore), pure-Java, **no native lib**. Evidence:
   - `META-INF/services/java.sql.Driver` in the fat jar lists exactly **`org.postgresql.Driver`**
     and **`org.h2.Driver`** — no `org.sqlite.JDBC`.
   - **Zero** `org/sqlite/*` classes and **zero** `libsqlitejdbc.so` in the jar. (Exposed ships
     `SQLiteDialect*` *metadata* classes, but those are inert without the driver.)
   - The probe's created DB file is `<rootDir>/database.mv.db` (H2).

   ⇒ The plan's §0/Q2/Q5 premise that "`xerial/sqlite-jdbc` [is] the one remaining mandatory native
   lib" is **incorrect for this pin**. **No native database `.so` is required on Android** for the
   default (H2) config; the optional Postgres path is also pure-Java JDBC. **N3.5 (SQLite Android
   native) is unnecessary as written** unless a future engine bump switches the default store to
   SQLite.

2. **Contingency (if Suwayomi ever adopts xerial sqlite-jdbc):** Android aarch64 natives **are**
   available, but **not** via a separate `natives-android` classifier — the plan's phrasing is
   imprecise. xerial ships **one fat main jar** that embeds every platform's `.so` internally.
   Verified against `org.xerial:sqlite-jdbc:3.50.1.0` on Maven Central: the main
   `sqlite-jdbc-3.50.1.0.jar` contains
   `org/sqlite/native/Linux-Android/{aarch64,arm,x86,x86_64}/libsqlitejdbc.so`
   (aarch64 = 1.19 MB, dated 2025-06-09). Latest published line is `3.53.x`. So supplying the Android
   `.so` would be a matter of extracting it from the ordinary jar and setting `org.sqlite.lib.path`
   — no special artifact needed.

---

## AWT / font / imageio reachability (N3.7 early signal — indicative only)

With `-verbose:class`, across the **whole probe** (boot + store install + dex2jar apk convert +
browse; **16,832** total class-loads):

| Package | class-loads |
|---|---|
| `java.awt.*` | **0** |
| `sun.font.*` | **0** |
| `javax.imageio.*` | **0** |

**No AWT/Java2D/font/ImageIO code was reached.** This is a positive early indicator that the AWT
blocker (R1) may not bite for the endpoints we exercise. **Caveat (do not over-read):** this probe
did **not** exercise a cover/thumbnail render or the page-byte image-decode path — it browsed
metadata and installed extensions. N3.7 must still instrument the *actual* image endpoints on
device/emulator. (Note: the jar does bundle a `native/Linux/aarch64/libwebp-imageio.so`, so *if* a
webp cover-encode path is hit it has a native available — but AWT/Java2D was not touched here.)

## GraalJS

- **Bundled `js-community` 25.0.3** (jar contains `js-community-25.0.3.pom` +
  `com/oracle/truffle/js/*`). This is the **pure-Java** GraalJS — **no native `.so`** (the only
  truffle native, `libtruffleattach`, is optional JMX-attach glue). Because it's pure bytecode,
  running it on aarch64 is architecture-independent by construction.
- `eu.kanade.tachiyomi.network.JavaScriptEngine` wrapper **loads at boot** (wired in), but the
  **Truffle polyglot `Context` is lazily created on first JS eval** — a MangaDex POPULAR browse does
  not evaluate JS, so 0 `com.oracle.truffle`/`org.graalvm.polyglot` class-loads here (expected).
  **GraalJS interpreted perf/size (Open Q4) remains a device/JS-source measurement** this headless
  boot probe does not settle.
- **No GraalJS init errors or stack traces.**

### Non-fatal warnings observed (none blocked the run)
- `CEFManager … CEF is disabled` (ERROR-logged but caught) — expected consequence of
  `kcefEnabled=false`; the critical setting that avoids the 226 MB Chromium download + SIGTRAP.
- `android.util.Log [Bundle]: Key tachiyomix.contentWarning expected Integer but value was a
  java.lang.String … ClassCastException … default value 0 returned` — benign AndroidCompat `Bundle`
  metadata-parse warning while reading extension metadata; the extension installs and functions.

---

## Reproduce

```
apps/reader/src-tauri/e2e/android-jdk-probe/run.sh
```
Requires: Docker daemon able to run `linux/arm64`, the gitignored `suwayomi/Suwayomi-Server.jar`,
and network egress. Skips cleanly with a message if Docker is unavailable. Prints PASS/FAIL per
assertion; force-removes its container and temp dataDir on exit. Artifacts (gitignored) land in
`build/`: `engine.log` (full `-verbose:class` capture), `java-version.txt`, `cold-start.txt`,
`db-files.txt`, `awt-signal.txt`.
