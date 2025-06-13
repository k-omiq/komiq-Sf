# N1-SPIKE findings — booting embedded Suwayomi-Server headless (desktop)

Proven on macOS 15 (arm64) with Temurin/Homebrew **OpenJDK 21.0.11** against
**Suwayomi-Server v2.3.2243** (pinned in [`VERSION`](./VERSION)).

## Result

- Cold start to `ready`: **~5 s** (target was <30 s). The slow part in a naive boot is a
  226 MB Chromium (CEF) download — eliminated by disabling kcef (below).
- Bound **`127.0.0.1:<ephemeral>`** loopback and answered
  `POST /api/graphql {"query":"{ aboutServer { version } }"}` →
  `{"name":"Suwayomi-Server","version":"v2.3.2243","revision":"r2243","buildType":"Stable"}`.
- Graceful `kill` left **no orphaned java** process.

## The boot recipe the Rust supervisor (N1.1) must follow

1. **Config is via a persisted `<dataDir>/server.conf` (HOCON), NOT JVM sysprops.**
   `-Dserver.*` system properties do **not** reliably override Suwayomi's reactive
   `ServerConfig`. The supervisor must **write `server.conf` into the app data dir
   before launch** with at least:
   ```
   server.ip = "127.0.0.1"           # loopback only — never 0.0.0.0
   server.port = <ephemeral>          # broker a free port ourselves, then write it here
   server.webUIEnabled = false        # we ship our own UI
   server.initialOpenInBrowserEnabled = false
   server.systemTrayEnabled = false   # dorkbox tray NPEs/crashes in a headless JVM
   server.kcefEnabled = false         # CRITICAL — see below
   server.flareSolverrEnabled = false # we solve Cloudflare via the platform WebView (§8b), not FlareSolverr
   ```
2. **`server.kcefEnabled = false` is essential.** Left on (the default), the server
   downloads ~226 MB of Chromium Embedded Framework and then **SIGTRAPs (`EXC_BREAKPOINT`)
   in a headless CLI JVM on macOS arm64** — faulting library confirmed as
   `Chromium Embedded Framework` in the crash report. This aligns with the plan: on device
   we bridge Cloudflare through the **platform/Tauri WebView (§8b)**, not Suwayomi's CEF.
   If a data dir already contains a downloaded `bin/kcef`, delete it too.
3. **Launch:** `<jre>/bin/java -Djava.awt.headless=true -Xmx512m -jar Suwayomi-Server.jar`.
   The server reads `server.conf` from its data root.
4. **Readiness gate:** poll `aboutServer` every ~250 ms until it returns (≈5 s warm,
   allow ~30 s cold-start slack), then expose readiness to the JS side.
5. **Shutdown:** terminate the child on app exit; verify no orphaned `java` (spike: clean).

## Data dir (§3.2) — SOLVED

Sandbox the engine's data root with the JVM property
**`-Dsuwayomi.tachidesk.config.server.rootDir=<dir>`** (note the `.config.` segment — the
sibling `suwayomi.tachidesk.server.rootDir` does NOT work). Verified end-to-end: the DB,
extensions, and downloads land under the chosen dir and the OS default
(`~/Library/Application Support/Tachidesk`) is never touched. The supervisor should point
this at `app_data_dir()/suwayomi` and pre-write `server.conf` there before launch.

Full launch line (proven):
```
<jre>/bin/java -Djava.awt.headless=true -Xmx512m \
  -Dsuwayomi.tachidesk.config.server.rootDir=<app_data_dir>/suwayomi \
  -jar Suwayomi-Server.jar
```

## Toolchain notes for CI (N1.3)

- `jlink` is available in the JDK 21 install; the minimal-JRE build/size-budget job is
  still TODO (spike booted with the full JDK to isolate the runtime question).
- Jar is **fetched + SHA-256-verified** from the pinned `VERSION`, never committed
  (174 MB) and never floated at `:stable`.
