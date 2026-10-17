---
title: Architecture
description: Understand the two komiq apps, their shared packages, and the optional image proxy.
---

Komiq is an open-source, self-hostable comic and manga reader with two apps: the admin app and the reader app. This page maps both apps to the code and explains how data moves between them. Read it before changing the server, console or reader.

## The two apps

The admin app is the server and its management portal in one. You can run it on your computer with the desktop app, with Docker on a home server or a virtual private server (VPS), or reach it over the internet through a domain, a Cloudflare Tunnel or a public IP address. The image proxy Worker is optional.

The reader app connects to a komiq server on your local network or on the internet. The global server at `komiq.cc`, the project's free work-in-progress demo, is read in a browser only: its API accepts requests from the `komiq.cc` site alone, so native builds cannot connect to it. In native builds, **Connect to a server** takes an IP address and port such as `192.168.1.20:8080`, a domain such as `komiq.example.com`, a pasted link or a QR code.

The system separates reading and collection metadata from source interaction. The API server stores user accounts, authentication sessions, reviews, comments, and catalog metadata. Upstream catalog exploration and chapter page retrieval use Suwayomi, an open-source content engine, or MangaDex when enabled.

## Admin app

The admin app combines `apps/server`, `apps/admin` and the Suwayomi source engine. The console manages the server you run.

### Admin console

The administration console lives in `apps/admin`. The SvelteKit console lets owners:

- Inspect and curate the manga catalog.
- Review duplicate candidates and merge or separate works.
- Monitor extension status and source health.
- Manage user accounts and registration policies.
- Configure remote network access and Cloudflare integration.

The primary console builds are `embedded`, served by `komika-server` at `/admin` on the same origin, and `desktop`, which starts or picks a server. An origin is the scheme, hostname and port of an address. The desktop shell starts both `komika-server` and Suwayomi when you choose to run komiq on your computer.

The secondary `hosted` target builds the console for a separate origin. Set `PUBLIC_KOMIKA_API` to your own endpoint, for example `https://komiq.example.com/graphql`.

### API server

The API server lives in `apps/server` and compiles to a native binary named `komika-server`. It is written in Rust using the axum web framework and async-graphql.

The server provides a unified GraphQL API implemented in `apps/server/src/graphql/`, with the client schema contract in `packages/api/src/schema/komika.graphql`. It stores user accounts, password hashes, sessions, reviews, comments, and catalog state in PostgreSQL 18 using sqlx, a Rust database library. It federates calls to Suwayomi over HTTP to resolve upstream sources and chapters. When configured with a static web directory, the server serves the reader and admin web applications directly.

Covers remain a separate SQLite cache. Docker runs PostgreSQL privately beside the server; the desktop admin app bundles it. For an old main database, read [Old SQLite install](/self-host/update-and-backup#old-sqlite-install) to start fresh. For backups, self-host Docker uses PostgreSQL dumps, the desktop app saves a local dump when the newest is at least 20 hours old, and the global deployment packages pgBackRest backups to object storage.

### Source engine

Suwayomi runs source extensions and resolves upstream content for the server. It is part of the admin app even though its engine code comes from Suwayomi rather than a separate komiq application folder.

## Reader app

The reader lives in `apps/reader`. It is a SvelteKit application that compiles to three targets:

- Web target: Web application that runs in the browser and connects to an API server.
- Tauri target: Native desktop and mobile application shell with local device storage.
- Embedded target: Static build served directly by `komika-server` for self-hosted instances.

The reader provides the user interface for browsing works, reading chapters, managing personal libraries, and participating in discussions. In native builds, the reader can bundle an embedded Suwayomi engine and communicate with it directly on the device.

## Optional image proxy

The image proxy lives in `apps/worker`. It is a Cloudflare Worker that fetches upstream comic page images and re-serves them with permissive cross-origin resource sharing (CORS) headers.

The image proxy fetches images on behalf of the web reader and caches them at the Cloudflare edge using `caches.default`. Native desktop and mobile builds bypass this worker and retrieve images through Rust or directly from the chosen server.

## Documentation site

`apps/docs` is the SvelteKit static documentation site.

## Shared packages

The `packages/` directory holds shared libraries.

### Types package

The types package lives in `packages/types`. It contains TypeScript type definitions for domain models across the system. These include models for work sources, series metadata, credits, chapters, pages, user references, and reading status. Both the reader and admin frontends rely on these shared types.

### API package

The API client package lives in `packages/api`. It provides the client-side data layer for frontend applications. It defines:

- `Backend`: The abstract interface that UI views call to fetch catalog items, series details, chapters, and social features.
- `GraphQLBackend`: An implementation of `Backend` that talks to `komika-server` over GraphQL, the query language for the server API.
- `SuwayomiBackend`: An implementation of `Backend` that talks directly to the Suwayomi GraphQL API.
- `CompositeBackend`: A hybrid implementation used in native apps that merges remote server metadata with local on-device engine content.
- `ImageProvider`: An abstraction that resolves whether an image URL should load directly, through the Cloudflare Worker proxy, or through the server proxy.

### UI package

The UI package lives in `packages/ui`. It contains design tokens and stylesheets shared across the web applications. It exports:

- `tokens.css`: Color variables, typography tokens, spacing units, and radius values.
- `reset.css`: Standard baseline CSS reset rules.
- `console.css`: Design styling rules shared by the administration interfaces.

## Communication flows

Communication pathways differ depending on whether you run the web reader or a native client.

### Web reader with an image proxy

A web reader configured to use the optional image proxy talks to `komika-server` for metadata and uses the Cloudflare Worker image proxy for upstream comic page images. Images on its own API origin load directly. The following flow shows a MangaDex-backed request when MangaDex is enabled.

```flow
Reader app -> API server: requests series and chapter metadata
API server -> PostgreSQL catalog: reads stored series and chapter metadata
API server -> Reader app: returns GraphQL response
Reader app -> Image proxy: requests external page image with upstream URL
Image proxy -> Source site: fetches image bytes
Image proxy -> Reader app: returns image bytes with cache headers
```

1. The reader requests series information from `komika-server` over GraphQL.
2. The server reads stored series and chapter metadata from PostgreSQL.
3. The server returns GraphQL data to the browser. Chapter page URLs come from MangaDex's at-home API.
4. When you open a chapter, the reader requests external page images through the image proxy worker. API-origin images load directly.
5. The worker checks its edge cache. On a cache miss, it downloads the image from the source host.
6. The worker returns image bytes to the browser with caching headers.

### Native application flow

Native reader builds can run an on-device Suwayomi engine where the platform supports it. This allows local page fetching without routing image traffic through external proxies. The following flow shows a Suwayomi chapter resolved on the device.

```flow
Reader app -> Composite backend: requests chapter pages
Composite backend -> Local engine: resolves chapter using source keys
Composite backend -> Reader app: returns engine page addresses at once
Reader app -> Local engine: web view loads each page image by address
Local engine -> Reader app: returns the page image
```

1. The reader UI asks `CompositeBackend` for chapter pages.
2. `CompositeBackend` uses the server's chapter origin to resolve the chapter on the local embedded Suwayomi engine when ready.
3. If local resolution succeeds, the local engine returns page references. Otherwise, the backend falls back to pages from the server the app is connected to.
4. The image provider gives each engine page a loadable address at once, without fetching any bytes. The web view then loads the pages itself, under the reader's own loading rules, and Rust answers each request from the engine. On desktop and iOS the address uses the `engine-image` scheme. On Android it is a loopback HTTP address. See [The on-device engine](/developers/native-engine) for both.

## Deeper reading

To explore individual subsystems in detail, continue to these pages:

- [Repository layout](/developers/repository): Monorepo directories, configuration files, and build scripts.
- [Run it locally](/developers/local-development): Setup steps and commands for running services locally.
- [The API server](/developers/server): Internal design, database migrations, and caching in `komika-server`.
- [The reader app](/developers/reader-app): Architecture of the reader web client and native shells.
- [The admin app](/developers/admin-app): Architecture of the admin console and desktop packaging.
- [The image proxy Worker](/developers/image-proxy): Cloudflare Worker image proxy implementation and rate limits.
- [GraphQL API](/developers/graphql): GraphQL schema, queries, mutations, and resolver architecture.
- [Suwayomi, the source engine](/developers/suwayomi): Content engine federation and extension updates.
- [The on-device engine](/developers/native-engine): Embedded Suwayomi and JRE packaging on desktop and mobile.
- [Testing](/developers/testing): Unit tests, integration harnesses, and end-to-end tests.

## Where to look

- `apps/reader/src/`: Reader frontend routes, UI components, and state management.
- `apps/admin/src/`: Admin console pages, catalog curation tools, and settings.
- `apps/server/src/`: Rust API server entrypoint, axum router, and GraphQL resolvers.
- `apps/worker/src/`: Cloudflare Worker image proxy handler and security guards.
- `packages/api/src/`: Client backend implementations and image provider logic.
- `packages/types/src/index.ts`: Shared domain type definitions.
- `packages/ui/`: Shared design tokens and CSS styles.
