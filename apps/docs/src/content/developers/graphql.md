---
title: GraphQL API
description: Use the GraphQL endpoint on your connected server, with authentication, operations by area and client transport.
---

The komiq project uses a unified GraphQL API to serve comic readers, desktop applications and the administrative console. The API combines Suwayomi comic sources with komiq identity, community, library tracking and catalog features.

Komiq is self-hostable, so the endpoint can belong to your local server or one on the internet. This guide explains how to connect to it, authenticate requests, execute operations by domain area and test queries locally.

## Endpoint and transport

Each komiq server exposes its GraphQL service at `/graphql`. Use that path on whichever server the app is connected to:

- Local development: `http://localhost:8080/graphql`
- Your server on the internet: `https://komiq.example.com/graphql`

The server supports two HTTP methods depending on the operation type and whether the caller is signed in.

### Anonymous read queries over GET

Anonymous queries can use `GET /graphql` with URL query parameters:

```sh
curl -s "https://komiq.example.com/graphql?query=%7Bdiscovery%7Btitle%7D%7D"
```

For anonymous GET requests, the server sends an `ETag` and a `Cache-Control` header. Browser caches and reverse proxy caches use these headers to store and revalidate public catalog responses.

The server rejects mutations sent over GET with HTTP 405 Method Not Allowed.

### Authenticated queries and mutations over POST

All mutations and authenticated queries use `POST /graphql` with a JSON payload:

```json
{
	"query": "query MySession { session { user { username } } }",
	"variables": {}
}
```

Set the request `Content-Type` header to `application/json`. Authenticated queries never use GET because edge caches key only on the URL, which would risk serving private user state to another reader.

## Authentication

The API uses opaque bearer session tokens. Send the token in the `Authorization` header on every authenticated request:

```text
Authorization: Bearer <session-token>
```

### Token lifecycle

1. You obtain a token by calling the `login`, `register` or `createOwner` mutations.
2. The server generates a random 256-bit hexadecimal string.
3. The server computes the SHA-256 hash of the token and stores only that hash in the `sessions` table. A leaked database snapshot cannot expose reusable bearer tokens.
4. On future requests, the server hashes the incoming bearer header with SHA-256 and matches it against `sessions.token`.
5. Tokens expire after `SESSION_TTL_SECS` (default 30 days). Administrator sessions expire after at most 8 hours.

### Password change requirement

When an administrator creates an account or resets an account password, the server flags the account with `must_change_password`.

Until that user calls `changePassword`, the server restricts the session. The user can only query `session` and run `changePassword` or `logout`. Other authenticated calls return an authorization error.

## Operations by area

The schema contract lives in `packages/api/src/schema/komika.graphql`. The Rust server implements this contract with code-first resolvers in `apps/server/src/graphql/mod.rs`.

### Catalog and browse

These operations retrieve comic metadata, chapter lists, page image links and search results:

- `discovery`: curated recommendation feeds displayed on the home screen.
- `updates`: scanner detections for series in the library.
- `updatesFeed`: merged chronological feed of recent chapter releases.
- `canonicalUpdates`: stream of recent chapter updates from the mirror.
- `search`: multi-criteria search and browse over canonical works.
- `tags`: curated tag vocabulary and work counts for catalog filters.
- `genreFacets`: genre counts matching current search criteria.
- `browseSources`: the sources the `source` filter of `search` accepts, each with the number of works the viewer may see.
- `scanlatorTags`: the scanlators an administrator tagged as AI or partial AI. A client matches the names to a chapter copy's `scanlator`.
- `sourceAiTags`: the Suwayomi sources an administrator tagged as AI or partial AI, for a site that translates its own series. A client matches the ids to a chapter copy's source and shows the stronger of the two tags.
- `series`: series details by Suwayomi identifier.
- `canonicalSeries`: canonical work details by work identifier (`w_...`).
- `chapters`: chapter list for a specific source series.
- `canonicalChapters`: ordered chapter list for a canonical work.
- `aggregatedChapters`: chapters deduplicated by number across all sources.
- `pages`: page image URLs for a Suwayomi chapter.
- `canonicalPages`: page image URLs for a mirrored chapter.
- `chapterOrigin`: for a Suwayomi chapter, the source ID, the source's own manga and chapter keys, and the extension that provides the source. A reader app with its own engine uses them to find the same chapter there. The server answers from keys it stored earlier and asks the source engine only when it has none. It follows the same adult-content rule as `pages`.

The `search` query supports both full-text search and structured catalog browsing. When the `query` argument is empty, it queries the materialized `browse_catalogue` table:

```graphql
query BrowseManga($filter: BrowseFilter) {
	search(query: "", filter: $filter, page: 1) {
		items {
			id
			title
			coverUrl
			rating {
				average
			}
		}
		total
		hasNextPage
	}
}
```

A content block rule names tag ids, content ratings, or both, for example `{ tags: ["gore"], ratings: [EROTICA, PORNOGRAPHIC] }`. It matches a work that has all of its tags and, when it names ratings, one of them. An adult work is never rated below `EROTICA` for this test. A work is blocked when any rule matches it.

- A site-wide rule applies to every viewer. The list queries and `library` leave the work out. The queries that take an identifier answer as they do for an unknown work. So do `reviews`, the `comments` of a series and the library mutations. The server also refuses the chapter page images of a blocked series from the engine. Covers stay open, because the admin console still lists the series.
- A user's own rule applies to that user's lists only: `search`, `discovery`, `updates`, `updatesFeed` and `canonicalUpdates`. A work the user opens by its identifier still opens, and `library` keeps it.
- The counts of `tags`, `genreFacets` and `browseSources` do not change with the rules.
- An administrator who sends `includeNsfw: true` to `search` or `canonicalUpdates` gets the list without the rules. The admin console does this.

### Library and reading progress

These operations manage personal reading shelves, chapter progress and view counters:

- `library`: series saved in the signed-in user's personal library.
- `libraryProgress`: batched reading progress for all library series.
- `mark`: add or remove a series from the user's library.
- `setLibraryStatus`: file a series into a shelf (`reading`, `completed`, `onhold`, `plan`).
- `setFavorite`: toggle the favorite status of a series.
- `setProgress`: save last page read and chapter completion state. With `read: true` it also runs the catch-up that marks earlier chapters of the same source read (see [Data model](/developers/data-model)), and it clears the untick record of that chapter.
- `setChapterRead`: mark one chapter read or unread. With `read: false` it records an untick, so catch-up does not mark that chapter read again. With `read: true` it clears the untick record.
- `recordView`: record a chapter open to update trending counts.

The `recordView` mutation requires no authentication. Readers send it when opening a chapter. The server applies an in-memory sliding-window rate limit per IP address and series ID to prevent tally manipulation.

### Social and community

These operations support community discussion, ratings and user notifications:

- `reviews` and `myReview`: paginated user reviews and ratings (1 to 10) for a work.
- `postReview` and `deleteReview`: create, update or remove a review.
- `comments` and `myComments`: comment threads for a series or chapter.
- `postComment` and `voteComment`: post a comment or vote (1 for like, -1 for dislike, 0 to clear).
- `notifications` and `unreadNotificationCount`: user notification inbox and badge counts.
- `markNotificationsRead`: mark notifications as read.
- `myActivity`: activity stream of recent user interactions.

### Identity and settings

These operations manage account registration, login and profile updates:

- `session`: resolve current session user, permissions and flags.
- `instance`: resolve public instance metadata, setup status, registration state and `appsFetchThemselves` (apps with their own engine must fetch chapter pages themselves) without authentication.
- `login`: authenticate with username and password.
- `register`: create a new user account.
- `logout`: invalidate the current session token.
- `createOwner`: complete first-run setup with the server setup code.
- `changePassword`: update user password and clear forced change flag.
- `updateProfile`: update display name and biography.
- `setShowNsfw`: toggle adult content visibility preference.
- `myBlockRules` and `setMyBlockRules`: read and replace the signed-in user's content block rules.

### Administration and operations

Operations in this category require `isAdmin: true` on the authenticated user:

- **Instance settings**: `accessStatus`, `accessPreflight`, `setAccessMode`, `checkCloudflareToken`, `startCloudflareSetup`, `retryCloudflareSetup`, `removeCloudflareSetup`, `setRegistrationOpen`, `siteBlockRules`, `setSiteBlockRules`.
- **User moderation**: `users`, `createUser`, `setUserAdmin`, `resetUserPassword`, `banUser`, `deleteComment`.
- **Sources and extensions**: `extensions`, `extensionRepos`, `sources`, `sourceBrowse`, `installExtension`, `uninstallExtension`, `updateExtension`, `addExtensionRepo`, `removeExtensionRepo`, `mangadexSetting`, `setMangadexEnabled`, `sourceRateLimits`, `setSourceRateLimit`, `removeSourceRateLimit`, `scanlators`, `setScanlatorAi`, `scanlatorSites`, `setSourceAi`, `startSourceIngest`, `cancelSourceIngest`, `startExtensionIngest`, `cancelExtensionIngest`, `setExtensionSubscription`.
- **Catalog curation**: `mergeQueue`, `resolveMergeCandidate`, `mergeWorks`, `splitSourceSeries`, `workSourceRows`, `workReviewDetail`, `consolidateExactDuplicates`, `reconcileCatalogue`, `resyncCatalogue`, `splitProgressReaders` (how many readers' progress a split would move), `addSourceSeries`, `bulkAddSourceSeries`, `coverIssues`, `retryCover`, `materializeCatalogueCovers`, `persistCatalogue`, `backfillMangadexMetadata`.
- **Scanner and series administration**: `scanStatus`, `triggerScan`, `setSeriesPaused`, `rescanWork`, `updateSeriesAdmin`, `seriesAdminMeta`, `updateSeriesMetadata`, `addSeriesAltTitle`, `removeSeriesAltTitle`, `workChaptersAdmin`, `setChapterOverride`.
- **Adult sources**: `markSourceNsfw` flags a whole Suwayomi source as adult, updates the works already added from it, and remembers the choice for series added later. `rederiveSuwayomiNsfw` recomputes the flag for every work added from a Suwayomi source. The admin console has no control for these two; call them through GraphQL.
- **Reader reports**: `reports`, `resolveReport`, `submitReport`. The `submitReport` mutation is open to unauthenticated users so readers can report catalog errors without signing in.

## Client implementation

The TypeScript client in `packages/api/src/graphql-backend.ts` wraps the operations defined in `packages/api/src/operations.ts`.

It provides built-in resilience when client and server versions drift during deployments:

```flow
Client -> Server: sends GraphQL query
Server -> Client: returns Unknown field or Unknown argument error
Client -> Client: strips unknown field or argument from document
Client -> Server: retries query once with stripped document
Server -> Client: returns valid data
```

### Opportunistic field stripping

When a new reader application introduces an optional field from `OPTIONAL_SERIES_FIELDS` (such as `detectedAt` or `firstReleasedAt`), an older deployed server returns an `Unknown field` error.

The `GraphQLBackend` class catches this error, removes the offending line from the query, retries the request, and caches the omission for the rest of the application session.

### Opportunistic argument stripping

Arguments in `OPTIONAL_ARGUMENTS` (such as `filter`, `includeNsfw` and `numericEpoch`) receive similar handling. If an older server rejects an argument, `GraphQLBackend` strips the variable declaration and usage before retrying.

### Numeric epoch

Numeric series and chapter ids come from the Suwayomi engine and mean something only within one engine generation. `setProgress`, `setChapterRead`, `mark`, `setLibraryStatus` and `setFavorite` take an optional `numericEpoch` for them. For a signed-in viewer, `GraphQLBackend` reads the `numericEpoch` query with the first read that returns numeric ids: the epoch request goes out first, and the read returns only after both answer. It keeps the value for the life of the page or app. Anonymous reads skip it, because an anonymous viewer cannot write. It does not refresh it: after an engine change the app may still hold old chapter ids, and a fresh epoch would let their progress land on a chapter that reuses the number. The server refuses a stale epoch with the error code `STALE_NUMERIC_EPOCH` (see `isStaleNumericEpoch` in `packages/api/src/numeric-epoch.ts`). A live write refused this way calls the listeners of `onStaleNumericEpoch` once, and the reader asks the user to reload. A queued offline write refused this way is dropped without a notice. Canonical MangaDex ids carry no epoch.

Starting a new generation advances the epoch to at least the current Unix time in seconds and past its stored value. Epoch 0 remains valid before the first generation change. Numeric library and progress writes without an epoch are refused for 30 days after a generation change. On older servers, the client strips an unsupported library epoch argument for that mutation only, so it still sends epochs on progress writes to servers that support them.

### GET fallback

If an intermediary proxy or older server version returns an HTTP 404 or 405 error on a `GET /graphql` request, `GraphQLBackend` sets an internal flag and retries using `POST /graphql`. Subsequent read queries use POST directly.

## Exploring the API locally

You can test queries and explore the schema interactively on your development machine.

### GraphiQL interactive IDE

The server includes an embedded GraphiQL interface. To enable it, start the server with `GRAPHIQL_ENABLED=on`:

```sh
GRAPHIQL_ENABLED=on cargo run
```

Open `http://localhost:8080/graphiql` in your browser. When `GRAPHIQL_ENABLED=on` is set, the server also enables GraphQL schema introspection, allowing documentation browsers and IDE autocomplete to inspect all types and fields.

> [!WARNING]
> Keep `GRAPHIQL_ENABLED` disabled in production deployments to prevent unauthorized schema introspection and public IDE access.

### Testing with curl

You can execute queries directly from your terminal.

Execute an anonymous query:

```sh
curl -s http://localhost:8080/graphql \
  -H "Content-Type: application/json" \
  -d '{"query": "query { discovery { title kind } }"}'
```

Log in to obtain a bearer token:

```sh
curl -s http://localhost:8080/graphql \
  -H "Content-Type: application/json" \
  -d '{"query": "mutation { login(username: \"owner\", password: \"pass123\") { token } }"}'
```

Execute an authenticated query:

```sh
curl -s http://localhost:8080/graphql \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer <your-token>" \
  -d '{"query": "query { session { user { username isAdmin } } }"}'
```

## Where to look

- `packages/api/src/schema/komika.graphql`: formal GraphQL Schema Definition Language (SDL) contract.
- `packages/api/src/operations.ts`: query and mutation documents used by the web and desktop applications.
- `packages/api/src/graphql-backend.ts`: TypeScript `GraphQLBackend` class with retry and field-stripping logic.
- `apps/server/src/graphql/mod.rs`: Rust query and mutation resolver implementations and caching layers.
- `apps/server/src/graphql/types.rs`: Async-graphql type mappings and converters between Suwayomi and komiq types.
- `apps/server/src/block.rs`: content block rules, their validation and the SQL condition that every gated query shares.
