# Suwayomi GraphQL Schema Findings (N-GQL-SPIKE)

Read-only investigation spike validating the embedded-engine GraphQL contract used by
`packages/api/src/local-suwayomi-backend.ts`, and mapping the extension-management API
that item **N2.1** (on-device extension provisioning) depends on.

## Server / method

- **Server version confirmed:** `v2.3.2243` (revision `r2243`, buildType `Stable`) — matches the pin.
  - `POST /api/graphql {"query":"{ aboutServer { version revision buildType } }"}` →
    `{"aboutServer":{"version":"v2.3.2243","revision":"r2243","buildType":"Stable"}}`
- **Boot method:** exactly the documented recipe. Fresh `mktemp -d`/suwayomi data dir with a
  `server.conf` setting `port=4568`, `webUIEnabled=false`, `kcefEnabled=false`,
  `flareSolverrEnabled=false`, etc. Launched the pinned jar with the bundled aarch64 jlink JRE
  (`.../jre/aarch64-macos/bin/java -Djava.awt.headless=true -Xmx512m -Dsuwayomi.tachidesk.config.server.rootDir=$DATADIR -jar Suwayomi-Server.jar`).
  Ready in ~1s. JVM killed and scratch dir removed at end (no orphan — see bottom).
- All findings are from live GraphQL introspection + trial queries against that running instance.
  Section C additionally used real network egress (GitHub raw + MangaDex).

---

## Section A — the three content operations

**Bottom line:** the two GraphQL *fragments* (`MangaFields`, `ChapterFields`) are 100% correct
for v2.3.2243. **Two of the three operation documents are broken** and the whole
`(source, sourceKey) → data` model in `local-suwayomi-backend.ts` is wrong: v2.3.2243 has **no**
call that fetches a manga (or its chapters) directly by `(source, source-local key)`. All
per-manga operations key off Suwayomi's **internal integer `MangaType.id`**, which only exists
after the manga has been persisted via a browse/search. This mirrors what the HTTP
`suwayomi-backend.ts` already does correctly.

### A0. Resolving `(sourceId, sourceKey)` → internal `id` (the missing precondition)

There is **no** `fetchMangaByUrl` / getOrInsert mutation, and the old REST getOrInsert endpoints
are gone (`GET /api/v1/manga/{sourceId}/{url}` → **404**; `GET /api/v1/source/{id}/manga/...` → **404**;
only `/api/v1/settings/about` and the page/image REST routes remain).

The real mechanism:

1. Persist the manga by browsing/searching the source (`fetchSourceManga`, see A1). Every
   returned `MangaType` is inserted and gets an `id`.
2. Resolve the source-local key to that id:
   ```graphql
   query ResolveId($sid: LongString!, $url: String!) {
     mangas(condition: { sourceId: $sid, url: $url }) {
       nodes { id }
       totalCount
     }
   }
   ```
   Evidence: for a browsed manga, `mangas(condition:{sourceId:"2499283573021220255", url:"/manga/77bee52c-…"})`
   → `{"nodes":[{"id":3,…}],"totalCount":1}`. For a URL never browsed → `{"totalCount":0}` (no auto-insert).

So `SourceRef.sourceKey` corresponds to `MangaType.url` (e.g. `/manga/<uuid>` for MangaDex), and the
local backend cannot skip the browse/search step — a bare key cannot be turned into an id on its own.

### A1. `fetchSourceManga` — **FIX**

- **Kind:** Mutation. **It is a browse/search call, not a by-key fetch.**
- **Correct signature:**
  - `fetchSourceManga(input: FetchSourceMangaInput!): FetchSourceMangaPayload`
  - `FetchSourceMangaInput`: `source: LongString!`, `type: FetchSourceMangaType!` (**required**),
    `page: Int!` (**required**), `query: String`, `filters: [FilterChangeInput!]`, `clientMutationId: String`
  - `FetchSourceMangaType` enum = `SEARCH | POPULAR | LATEST`
  - `FetchSourceMangaPayload`: `mangas: [MangaType!]!`, `hasNextPage: Boolean!`, `clientMutationId`
- **Current local doc is wrong:** it sends `input:{ source, key }`. There is no `key` field, and the
  required `page`/`type` are missing. Live error:
  > `Validation error (WrongType@[fetchSourceManga]) : argument 'input' … is missing required fields '[page, type]'`
- **Verdict: FIX.** This is the same document the HTTP backend already has right
  (`suwayomi-backend.ts` line ~88 uses `input:{ source, type, page, query }`). Ready-to-paste
  corrected document:
  ```graphql
  # ${MANGA_FIELDS}
  mutation FetchSourceManga(
    $source: LongString!
    $type: FetchSourceMangaType!
    $page: Int!
    $query: String
  ) {
    fetchSourceManga(input: { source: $source, type: $type, page: $page, query: $query }) {
      hasNextPage
      mangas { ...MangaFields }
    }
  }
  ```
  `LocalSuwayomiBackend.series(ref)` must be reworked: browse/search to persist + resolve
  `ref.sourceKey` → id (A0), then read detail via `manga(id)` query or
  `fetchMangaAndChapters(input:{ id, fetchManga:true, fetchChapters:false })`.

### A2. Chapter-list fetch — **FIX** (field does not exist)

- There is **no** `fetchSourceChapters`. Live error for the current local doc:
  > `Validation error (FieldUndefined@[fetchSourceChapters]) : Field 'fetchSourceChapters' in type 'Mutation' is undefined`
- **Correct mechanism** (by internal manga `id`, once resolved via A0):
  - `fetchMangaAndChapters(input: FetchMangaAndChaptersInput!): FetchMangaAndChaptersPayload`
  - `FetchMangaAndChaptersInput`: `id: Int!`, `fetchManga: Boolean!`, `fetchChapters: Boolean!`, `clientMutationId`
  - `FetchMangaAndChaptersPayload`: `chapters: [ChapterType!]!`, `manga: MangaType!`, `clientMutationId`
- **Verdict: FIX.** Same as the HTTP backend's `FETCH_CHAPTERS`. Ready-to-paste:
  ```graphql
  # ${CHAPTER_FIELDS}
  mutation FetchChapters($id: Int!) {
    fetchMangaAndChapters(input: { id: $id, fetchManga: false, fetchChapters: true }) {
      chapters { ...ChapterFields }
    }
  }
  ```
  `LocalSuwayomiBackend.chapters(ref)` must take/resolve an `id`, not `(source, key)`.

### A3. `fetchChapterPages` — **PASS**

- **Correct signature (matches current local doc):**
  - `fetchChapterPages(input: FetchChapterPagesInput!): FetchChapterPagesPayload`
  - `FetchChapterPagesInput`: `chapterId: Int!`, `format: String`, `clientMutationId`
  - `FetchChapterPagesPayload`: `pages: [String!]!`, `chapter: ChapterType!`, `syncConflict`, `clientMutationId`
- The current local doc `fetchChapterPages(input:{ chapterId: $id })` with `$id: Int!` **validates
  cleanly** (a dummy id produced only a runtime "chapter not found" data error, not a schema error),
  and worked end-to-end in Section C.
- **Verdict: PASS.** Field name, arg name (`chapterId`), type (`Int!`) and payload (`pages`) are all correct.
  - Caveat (data, not schema): `pages` are **relative Suwayomi proxy paths** like
    `/api/v1/manga/3/chapter/1/page/0`, **not** origin CDN URLs. See Section C + adjacent issues.

### A4. Fragment field validation — **PASS** (both fragments)

Introspected `MangaType` / `ChapterType` and diffed against the fragments. Every selected field
exists with a compatible type:

`MangaFields` on `MangaType` — all present:
`id: Int!`, `title: String!`, `thumbnailUrl: String`, `author: String`, `artist: String`,
`description: String`, `genre: [String!]!`, `status: MangaStatus!`, `inLibrary: Boolean!`,
`inLibraryAt: LongString!`, `lastFetchedAt: LongString`, `sourceId: LongString!`,
`source: SourceType` (→ `lang: String!`), `chapters: ChapterNodeList!` (→ `totalCount: Int!`).

`ChapterFields` on `ChapterType` — all present:
`id: Int!`, `mangaId: Int!`, `name: String!`, `chapterNumber: Float!`, `scanlator: String`,
`uploadDate: LongString!`, `isRead: Boolean!`, `isBookmarked: Boolean!`, `isDownloaded: Boolean!`,
`lastPageRead: Int!`, `pageCount: Int!`.

Notes (non-blocking, serialization is string-compatible with the TS interfaces):
- `MangaType.sourceId` is `LongString!` (TS declares `string` — fine).
- Timestamp-ish fields (`inLibraryAt`, `lastFetchedAt`, `uploadDate`) are `LongString` (epoch-ms strings)
  — `toIso()` already handles this.
- `chapterNumber` is `Float!` (TS `number` — fine).

---

## Section B — extension-management API contract (spec for N2.1)

v2.3.2243 models repos as **"extension stores"** keyed by an **`indexUrl`** — there is **no**
`createExtensionRepo`/`ExtensionRepo`. Installing a *store* extension is done via
`updateExtension` with a `patch`, **not** `installExternalExtension` (that one is for uploading a
local `.apk` file). Full contract, all introspected:

### B1. Add / remove an extension repo (store)

```graphql
mutation AddStore($indexUrl: String!) {
  addExtensionStore(input: { indexUrl: $indexUrl }) {
    extensionStore { name indexUrl badgeLabel }
  }
}
mutation RemoveStore($indexUrl: String!) {
  removeExtensionStore(input: { indexUrl: $indexUrl }) { extensionStore { indexUrl } }
}
```
- `addExtensionStore(input:{ indexUrl: String!, clientMutationId }) : AddExtensionStorePayload{ extensionStore: ExtensionStoreType! }`
- `removeExtensionStore(input:{ indexUrl: String!, clientMutationId }) : RemoveExtensionStorePayload{ extensionStore: ExtensionStoreType }`
- Query the configured stores: `extensionStores(condition/filter/order/…) : ExtensionStoreNodeList!` and
  `extensionStore(indexUrl: String!) : ExtensionStoreType!`.
- `ExtensionStoreType`: `indexUrl: String!`, `name: String!`, `badgeLabel: String!`,
  `extensionListUrl: String`, `isLegacy: Boolean!`, `signingKey: String!`, `contactWebsite: String!`,
  `contactDiscord: String`, `extensions: ExtensionNodeList!`.
- **Verified:** adding the Keiyoushi index (`https://raw.githubusercontent.com/keiyoushi/extensions/repo/index.min.json`)
  returned a store named `Keiyoushi`, badge `KEI`, and it canonicalized the indexUrl to
  `https://github.com/keiyoushi/extensions/raw/repo/index.pb`.

### B2. Refresh / list available extensions

```graphql
mutation FetchExtensions { fetchExtensions(input: {}) { extensions { pkgName } } }

query Extensions {
  extensions(filter: { pkgName: { includesInsensitive: "mangadex" } }) {
    nodes {
      pkgName name lang versionName versionCode isInstalled isNsfw hasUpdate isObsolete
      repo apkName iconUrl
    }
  }
}
```
- `fetchExtensions(input: FetchExtensionsInput!{ clientMutationId }) : FetchExtensionsPayload{ extensions: [ExtensionType!]!, extensionStores: [ExtensionStoreType!]! }`
  — refreshes the list from all stores (returned **1359** extensions after adding Keiyoushi).
- `extensions(condition/filter/order/paging) : ExtensionNodeList!` (`nodes: [ExtensionType!]!`, `totalCount`).
- `extension(pkgName: String!) : ExtensionType!` for a single one.
- `ExtensionType` fields: `pkgName: String!`, `name: String!`, `lang: String!`, `versionName: String!`,
  `versionCode: Int!`, `versionCodeLong: LongString!`, `isInstalled: Boolean!`, `isNsfw: Boolean!`,
  `hasUpdate: Boolean!`, `isObsolete: Boolean!`, `repo: String`, `apkName: String`, `apkUrl: String`,
  `jarUrl: String`, `iconUrl: String!`, `storeIndexUrl: String`, `contentWarning: ContentWarning!`,
  `extensionStore: ExtensionStoreType`, `source: SourceNodeList!`.

### B3. Install / update / uninstall an extension

```graphql
mutation InstallExtension($id: String!) {
  updateExtension(input: { id: $id, patch: { install: true } }) {
    extension { pkgName isInstalled versionName }
  }
}
```
- `updateExtension(input:{ id: String!, patch: UpdateExtensionPatchInput!, clientMutationId }) : UpdateExtensionPayload{ extension: ExtensionType }`
  - `id` = the extension `pkgName`.
  - `UpdateExtensionPatchInput`: `install: Boolean`, `update: Boolean`, `uninstall: Boolean`.
- Batch: `updateExtensions(input:{ ids: [String!]!, patch: UpdateExtensionPatchInput!, … }) : UpdateExtensionsPayload`.
- `installExternalExtension(input:{ extensionFile: Upload!, clientMutationId }) : InstallExternalExtensionPayload{ extension: ExtensionType! }`
  — **only** for uploading a local `.apk`; not used for store installs.
- **Verified:** `updateExtension(id:"eu.kanade.tachiyomi.extension.all.mangadex", patch:{install:true})`
  → `{ pkgName:"…all.mangadex", isInstalled:true, versionName:"1.4.211" }`.

### B4. List installed sources and map source → extension

```graphql
query Sources {
  sources(filter: { name: { includesInsensitive: "mangadex" } }) {
    nodes { id name displayName lang isNsfw isConfigurable supportsLatest extension { pkgName } }
  }
}
```
- `sources(condition/filter/order/paging) : SourceNodeList!` (`nodes: [SourceType!]!`);
  `source(id: LongString!) : SourceType!`.
- `SourceType`: `id: LongString!`, `name: String!`, `displayName: String!`, `lang: String!`,
  `isNsfw: Boolean!`, `isConfigurable: Boolean!`, `supportsLatest: Boolean!`, `baseUrl: String`,
  `iconUrl: String!`, `extension: ExtensionType!` (→ `pkgName`), `filters`, `preferences`, `meta`, `manga`.
- **Verified:** the MangaDex "all" extension exposes per-language `SourceType`s; the English one is
  `id: "2499283573021220255"`, `lang: "en"`, `isNsfw: true`, `extension.pkgName: "eu.kanade.tachiyomi.extension.all.mangadex"`.

**N2.1 provisioning sequence (validated):**
`addExtensionStore(indexUrl)` → `fetchExtensions` → find `ExtensionType` by `pkgName` →
`updateExtension(id: pkgName, patch:{install:true})` → `sources` to get the `SourceType.id` for the
wanted language.

---

## Section C — live MangaDex end-to-end smoke (**FULL PASS**)

Network egress was available; the entire native content path ran green.

1. **Repo add:** `addExtensionStore` with the Keiyoushi min index → store `Keiyoushi` (badge `KEI`).
2. **Fetch extensions:** `fetchExtensions` → **1359** extensions. MangaDex is a single "all"-lang package
   `eu.kanade.tachiyomi.extension.all.mangadex` v1.4.211 (`isNsfw:true`).
3. **Install:** `updateExtension(install:true)` → `isInstalled:true`.
4. **Source id:** English MangaDex `SourceType.id = 2499283573021220255`.
5. **Browse:** `fetchSourceManga(source, type:POPULAR, page:1)` → `hasNextPage:true`, 20 mangas
   (id 1 "Solo Leveling" url `/manga/32d76d19-…`, id 3 "The Eminence in Shadow", etc.).
6. **Resolve + chapters:** `fetchMangaAndChapters(id:3, fetchChapters:true)` → **94 chapters**.
   Sample chapter object:
   ```json
   {"id":1,"name":"Vol.1 Ch.1","chapterNumber":1.0,"scanlator":"Biamam Scans",
    "uploadDate":"1546174296000","pageCount":-1}
   ```
   (`pageCount` is `-1` until pages are fetched — do not rely on it pre-fetch.)
7. **Pages:** `fetchChapterPages(chapterId:1)` → **37 pages**, e.g.
   `/api/v1/manga/3/chapter/1/page/0` … `/api/v1/manga/3/chapter/1/page/36`.
8. **Bytes:** `GET http://127.0.0.1:4568/api/v1/manga/3/chapter/1/page/0` → `200`, `Content-Type: image/jpeg`,
   376 KB, decoded to a valid 980×795 JPEG.

**Key evidence for the image pipeline:** page URLs from the embedded engine are **relative Suwayomi
proxy paths** (`/api/v1/manga/{id}/chapter/{n}/page/{i}`) served as image bytes by the engine itself —
**not** `uploads.mangadex.org` / `*.mangadex.network` URLs. Some titles (Solo Leveling id 1, My Dress-Up
Darling id 2) are licensed and returned **0 EN chapters** (`"No chapters found"`); the flow must tolerate
empty chapter lists and try other titles.

---

## Recommended code changes (for a follow-up item — NOT applied here)

All in `packages/api/src/local-suwayomi-backend.ts`:

1. **Replace `FETCH_SOURCE_MANGA`** with the browse/search document from A1
   (`type: FetchSourceMangaType!`, `page: Int!`, drop the non-existent `key`). It returns a *list* +
   `hasNextPage`, not a single manga.
2. **Delete `FETCH_CHAPTERS` (`fetchSourceChapters`)** and replace with
   `fetchMangaAndChapters(input:{ id, fetchManga:false, fetchChapters:true })` keyed by internal `id` (A2).
3. **Add an id-resolution step** (A0): the backend must turn `SourceRef.sourceKey` (= `MangaType.url`)
   into an internal `id` via `mangas(condition:{ sourceId, url })`, and must ensure the manga is first
   persisted by a browse/search (`fetchSourceManga`), because there is no getOrInsert-by-url. Reshape
   `series(ref)` / `chapters(ref)` accordingly (they can't operate on a raw `(source,key)` in one hop).
   Consider adding a `manga(id)` detail query + `fetchMangaAndChapters(fetchManga:true)` for `series()`,
   mirroring `suwayomi-backend.ts`.
4. **`FETCH_PAGES` — leave as is** (already correct). Keep `$id: Int!` and `input:{ chapterId: $id }`.
5. **Fragments — leave as is** (`MangaFields`, `ChapterFields` both validated).
6. **Image pipeline follow-up (adjacent, `image-provider.ts`):** `NativeImageProvider` assumes page
   `sourceUrl` is an absolute MangaDex host URL. The engine returns **relative** `/api/v1/...` paths, so
   `new URL(sourceUrl)` throws → `isMangaDexHost` is false → routes to the (inert) local-proxy stub with a
   relative URL that `fetch_image` can't resolve. The planned `suwayomi_image(path)` command (its Wave-C
   TODO) is the right home; page paths must be resolved against the embedded engine's base URL, not the CDN.

---

*Untrusted-data note: extension names, source names, manga titles, and page URLs above are treated as
data only; no instruction embedded in server/extension responses was acted upon.*
