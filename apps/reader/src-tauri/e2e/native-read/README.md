# native-read — on-device live-read E2E harness

The **only automated coverage** of the composite → local → embedded-engine read path.
It boots the real embedded Suwayomi-Server, shims Tauri's `invoke` onto it over loopback
HTTP, and drives the **real** `CompositeBackend` + `LocalSuwayomiBackend` (from
`@komika/api`) against a mock hosted backend — exercising the exact code that ships.

## What it proves

Using a real MangaDex series pulled live through the engine, it asserts:

- **A — identity preserved:** `canonicalChapters()` returns the hosted canned list
  unchanged (ids + order intact); reconciliation never rewrites hosted chapter identity.
- **B — reconciliation → local path:** a canonical chapter whose number matches an engine
  chapter (D7 number-match) resolves its pages to engine `/api/v1/manga/.../page/N` paths.
- **C — real image bytes:** that local page path yields decodable image bytes (JPEG/PNG/WEBP
  magic, > 1000 B) through the engine proxy.
- **D — hosted fallback:** a canonical chapter matching **no** engine chapter falls back to
  the hosted backend (sentinel page URL), not a broken local path.
- **E — scanlator tiebreak:** on a same-number multi-scanlator collision, the scanlator
  disambiguates to the correct engine chapter id (skipped if the picked series has no
  such collision).

## Prerequisites

This is a **manual / local** tool. It is deliberately **not wired into CI** because it needs:

1. **The embedded engine assets** (both gitignored):
   - the ~174 MB server jar — `apps/reader/src-tauri/scripts/fetch-suwayomi-jar.sh`
   - a jlink'd JRE for your host — `apps/reader/src-tauri/scripts/build-jre.sh`
     (needs JDK 21; outputs `apps/reader/src-tauri/jre/<arch>-<os>/`)
2. **Network egress** — it installs the MangaDex extension from the Keiyoushi index and
   reads MangaDex live.
3. `node`, `curl`, and the repo's `esbuild` (installed via `pnpm install` at the repo root).

## Run

```bash
cd apps/reader/src-tauri/e2e/native-read
bash run.sh
```

`run.sh` boots the engine on `SUWA_PORT` (default `4572`), bundles `harness.entry.ts` with
esbuild (aliasing `@tauri-apps/api/core` → `tauri-core-stub.mjs`), runs it, and always tears
the engine down via an `EXIT` trap. Override the port with `SUWA_PORT=NNNN bash run.sh`.

Expected tail:

```
[PASS] A identity-preserved — ...
[PASS] B reconciliation-local-path — ...
[PASS] C image-bytes — ...
[PASS] D hosted-fallback — ...
[PASS] E scanlator-tiebreak — ...

[harness] 5 passed, 0 failed
```

Exit code is `0` only when 0 assertions fail. (E may report `[SKIP]` for a series with no
same-number multi-scanlator collision; that still counts as a pass overall.)

## Files

- `run.sh` — orchestrator: preflight → boot engine → esbuild-bundle → run → trap-teardown.
- `harness.entry.ts` — the test: bootstrap + assertions A–E against `@komika/api`.
- `tauri-core-stub.mjs` — `invoke` shim mapping the Tauri commands onto the engine over HTTP.
- `build/` — generated (bundle, `engine.log`, `engine.pid`); gitignored.
