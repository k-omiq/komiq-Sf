---
title: Repository layout
description: Map repository folders to the admin app, reader app, and shared tooling.
---

Komiq is self-hostable and has two apps. The admin app combines `apps/server`, `apps/admin` and the Suwayomi source engine; the reader app lives in `apps/reader`. This page maps the folders and scripts for contributors.

## Monorepo overview

Komiq uses a monorepo, one repository containing both apps and their shared libraries, managed with pnpm workspaces. The workspace configuration lives in `pnpm-workspace.yaml` and globs two folders:

- `apps/*`: The two apps' components, optional image proxy and documentation site.
- `packages/*`: Shared client libraries, domain type definitions, and styling packages.

Top-level configuration files manage project-wide tooling:

- `package.json`: Defines workspace scripts, dev dependencies, Node.js engine requirements (Node >= 22.13), and pnpm version (11.9.0).
- `pnpm-workspace.yaml`: Declares workspace package globs and sets `allowBuilds` for prebuilt binaries.
- `.prettierrc.json`: Configures code formatting rules for TypeScript, JavaScript, and Svelte files.
- `.prettierignore`: Lists build outputs, lockfiles, GraphQL schema files, and caches ignored by Prettier.
- `LICENSE`: Full text of the GNU Affero General Public License, version 3.0 (AGPL-3.0-only).
- `NOTICE`: Legal notices and attributions for third-party components, including Suwayomi-Server.

## App components

The five directories under `apps/` hold the product components and supporting tools. `apps/admin` and `apps/server` belong to the admin app. `apps/reader` is the reader app. `apps/worker` is optional infrastructure, and `apps/docs` documents both apps.

### apps/admin

The management portal and desktop shell of the admin app. It is a SvelteKit application built with Vite and CSS tokens from `@komika/ui`.

Key parts of `apps/admin`:

- `apps/admin/src/routes/`: Console pages for catalog browsing, duplicate review, source management, user management, and settings.
- `apps/admin/src/lib/`: API clients, state stores, navigation helpers, and Svelte components.
- `apps/admin/src-tauri/`: Tauri 2 desktop shell for the admin app, a native wrapper for its web interface. Bundles `komika-server` as a sidecar process and hosts the administration interface.

The console supports three build targets controlled by `ADMIN_TARGET`:

- `embedded`: Primary browser build, served directly by `komika-server` from `KOMIKA_WEB_DIR` under `/admin`. It uses the same origin as the server.
- `desktop`: Primary desktop build, runs inside Tauri and starts or picks a server. When running locally, it starts both `komika-server` and Suwayomi.
- `hosted`: Secondary build for a separate origin. It connects to your server over HTTPS through `PUBLIC_KOMIKA_API`, for example `https://komiq.example.com/graphql`.

### apps/server

The server component of the admin app, written in Rust. It compiles to the `komika-server` binary.

Key parts of `apps/server`:

- `apps/server/src/main.rs`: Axum web server configuration, route mounting, CORS middleware, and GraphiQL handler.
- `apps/server/src/graphql/`: Async-graphql schema definitions and resolver functions.
- `apps/server/src/db.rs`: SQLite database connection pool and migration runner using sqlx.
- `apps/server/src/auth.rs`: User password hashing with argon2id and session token verification.
- `apps/server/src/suwayomi.rs`: HTTP client federating requests to the Suwayomi source engine.
- `apps/server/migrations/`: SQL migration files defining tables for users, sessions, reviews, comments, works, and catalog state.
- `apps/server/bench/`: Load testing scripts and baseline performance benchmarks.

### apps/reader

The reader app, which connects to a server on the local network or on the internet. It is a SvelteKit application with edge rendering on the web and single-page application (SPA) builds for native and embedded targets.

Key parts of `apps/reader`:

- `apps/reader/src/routes/`: User-facing views for the home screen, catalog search, library collections, and chapter reading pager.
- `apps/reader/src/lib/`: Reader configuration, backend context, state management, and UI components.
- `apps/reader/src-tauri/`: Tauri 2 native desktop and mobile shell. Contains Rust wrappers for platform features and supervisor code for running an embedded Suwayomi engine.
- `apps/reader/src-tauri/scripts/`: Helper scripts to fetch the pinned Suwayomi jar and build tailored Java runtimes using `jlink`.

The reader supports three build targets controlled by `KOMIKA_TARGET`:

- Unset (default): Web build configured for Cloudflare Pages edge rendering.
- `tauri`: Static SPA build used inside native desktop and mobile wrappers.
- `embedded`: Static SPA served directly by `komika-server` at `/`.

### apps/worker

The optional image proxy for web readers, implemented as a Cloudflare Worker in TypeScript.

Key parts of `apps/worker`:

- `apps/worker/src/index.ts`: Worker fetch handler. Validates upstream image URLs against `ALLOWED_SOURCE_HOSTS`, fetches images, and caches responses in `caches.default`.
- `apps/worker/test/`: Security tests covering media type validation and upstream host filtering.
- `apps/worker/wrangler.toml`: Cloudflare deployment configuration and environment variables.

### apps/docs

The documentation site published at docs.komiq.cc. It is built with SvelteKit and `@sveltejs/adapter-static`.

Key parts of `apps/docs`:

- `apps/docs/src/content/`: Documentation pages written in Markdown, structured by section (`start/`, `self-host/`, `guide/`, `admin/`, `developers/`).
- `apps/docs/CONTENT_GUIDE.md`: Guidelines defining truth rules, writing voice, formatting standards, and allowed internal links.
- `apps/docs/PRODUCT.md`: Product requirements and design principles for the documentation site.

## Packages

The `packages/` directory holds libraries shared by the reader app and the admin app's console.

### packages/api

The client data access layer. It provides TypeScript abstractions and backend adapters:

- `packages/api/src/backend.ts`: The `Backend` interface defining operations for fetching titles, chapters, pages, reviews, and reading progress.
- `packages/api/src/graphql-backend.ts`: Implementation of `Backend` communicating with `komika-server` over GraphQL.
- `packages/api/src/suwayomi-backend.ts`: Implementation of `Backend` communicating directly with Suwayomi GraphQL.
- `packages/api/src/composite-backend.ts`: Hybrid implementation that coordinates between remote server state and local engine content.
- `packages/api/src/image-provider.ts`: Manages image URL routing across direct, server proxy, and Cloudflare Worker proxy modes.
- `packages/api/src/offline-queue.ts`: Queues library and reading progress writes when offline for replay when connectivity returns.

### packages/types

TypeScript domain models and contracts shared across all workspaces:

- `packages/types/src/index.ts`: Interfaces and types representing work sources, series metadata, credits, chapters, pages, user references, and reading progress.

### packages/ui

Design tokens and styling rules:

- `packages/ui/tokens.css`: CSS custom properties for color palettes, spacing scales, and typography styles.
- `packages/ui/reset.css`: Standard modern CSS reset stylesheet.
- `packages/ui/console.css`: Shared styles for administrative screens and data tables.

## Supporting directories

Beyond `apps/` and `packages/`, supporting directories include:

- `deploy/`: Container and deployment tooling. `deploy/server.Dockerfile` packages the admin app's server, `deploy/reader.Dockerfile` packages the reader, and `deploy/selfhost/` contains the Docker setup for your own admin app with embedded web apps.
- `e2e/`: Standalone browser smoke test suite for the reader app using Playwright. This project has its own `package.json` and is not part of the pnpm workspace.

## Root scripts

The root `package.json` provides scripts to manage the repository:

| Script            | Command                            | Purpose                                                    |
| ----------------- | ---------------------------------- | ---------------------------------------------------------- |
| `pnpm dev:reader` | `pnpm --filter @komika/reader dev` | Starts the reader web application dev server.              |
| `pnpm dev:admin`  | `pnpm --filter @komika/admin dev`  | Starts the admin console dev server.                       |
| `pnpm build`      | `pnpm -r build`                    | Runs workspace build scripts where defined.                |
| `pnpm check`      | `pnpm -r check`                    | Runs workspace check scripts where defined.                |
| `pnpm lint`       | `prettier --check .`               | Checks formatting across the repository.                   |
| `pnpm format`     | `prettier --write .`               | Formats supported files not excluded by `.prettierignore`. |

## Workspace commands

You can run commands in specific workspaces using the `--filter` flag:

```sh
# Start the reader web application
pnpm --filter @komika/reader dev

# Start the admin console
pnpm --filter @komika/admin dev

# Build the reader web target for your own server
PUBLIC_KOMIKA_API=https://komiq.example.com/graphql PUBLIC_KOMIKA_IMG_MODE=server pnpm --filter @komika/reader build

# Build the primary embedded console for /admin
ADMIN_TARGET=embedded ADMIN_BASE_PATH=/admin pnpm --filter @komika/admin build

# Run the image proxy worker locally
pnpm --filter @komika/worker dev

# Run Rust server tests
cd apps/server && cargo test

# Run Rust desktop tests
cd apps/admin/src-tauri && cargo test
```

## Where to look

- `package.json`: Monorepo scripts, package manager declarations, and dependencies.
- `pnpm-workspace.yaml`: Workspace definitions and build policy.
- `apps/`: Components of the two apps, optional image proxy and documentation site.
- `packages/`: Source code for shared libraries.
- `e2e/`: Standalone Playwright smoke test suite.
- `deploy/`: Container specifications and deployment compose templates.
