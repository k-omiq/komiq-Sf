---
title: The admin app
description: Build the admin console for your server with embedded or desktop targets, or use a separate-origin console.
---

The admin app is the komiq server and its management portal in one. You self-host it on your own computer, a home server or a VPS, and can make it reachable through a domain, a Cloudflare Tunnel or a public IP. In this repository, `apps/server` runs the server with the Suwayomi source engine, while `apps/admin` provides the console and desktop shell.

The console lets you manage the catalog, inspect sources, resolve duplicate entries, review covers, configure system settings, and manage user accounts.

The admin console is built with SvelteKit as a static Single-Page Application (SPA). Its primary build targets are `embedded`, served by your server at `/admin`, and `desktop`, packaged inside the [desktop app](/self-host/desktop-app). The separate-origin `hosted` target is a secondary case.

## The three build targets

The build target is chosen using the `ADMIN_TARGET` environment variable in `apps/admin/vite.config.ts`. If unset, it defaults to `hosted`, or `desktop` when the Tauri CLI environment variable `TAURI_ENV_PLATFORM` is present.

Vite defines `__ADMIN_TARGET__` at compile time, which is read by `apps/admin/src/lib/config.ts` to adjust runtime behavior for each target:

### Embedded target

The embedded console ships inside self-hosted container images and binary releases. Build it with:

```sh
ADMIN_TARGET=embedded ADMIN_BASE_PATH=/admin pnpm -C apps/admin build
```

This target is designed to be served by the backend server from the directory specified by `KOMIKA_WEB_DIR`.

The embedded console talks to `/graphql` on its own origin. It requires no environment overrides and no external domain configuration. Because the server mounts the console under `/admin`, all internal links and asset references use the `/admin` base path.

### Desktop target

The desktop target runs within the desktop application shell. The build is triggered automatically when Tauri builds the desktop app:

```sh
pnpm -C apps/admin tauri build
```

The desktop console connects to a server chosen by the user at runtime. When first launched without a configured server, `needsServer` evaluates to true, directing the user to `/start`.

Users can choose either a remote server URL or run the server on the local machine. The selection is stored in browser storage:

- Remote servers: Stored in `localStorage` under `komiq-server`.
- Local servers: Stored in `localStorage` as `local`, with the active session address kept in `sessionStorage` under `komiq-local-address`.

The desktop build configures its Content Security Policy in `apps/admin/vite.config.ts` to allow Tauri Inter-Process Communication (IPC) schemes (`ipc:` and `http://ipc.localhost`) in `connect-src`.

### Hosted target

Use the `hosted` target when you serve the console from a different origin than your server. Build it against your own server:

```sh
ADMIN_TARGET=hosted PUBLIC_KOMIKA_API=https://komiq.example.com/graphql pnpm -C apps/admin build
```

This build uses `@sveltejs/adapter-static` with fallback `index.html`. It can deploy to Cloudflare Workers as static assets with no server runtime.

Because the console and server have different origins, the target API endpoint must be known when building. Vite reads `PUBLIC_KOMIKA_API` from `$env/static/public` and inlines the URL into the bundle. Dynamic environment variables are not available because Cloudflare Workers Static Assets serves static files without executing SvelteKit server hooks.

## Base path handling

Self-hosted deployments serve the admin console from a subpath (`/admin`) on the same domain as the reader and API. A separate-origin console can serve from the root of its own domain.

Base path configuration is governed by `ADMIN_BASE_PATH` in `apps/admin/vite.config.ts`. The helper function `adminBasePath()` validates that the value is either empty or begins with a forward slash without trailing slashes. This value is passed to SvelteKit's configuration:

```ts
paths: {
	base: adminBasePath();
}
```

In the application code, internal links must not use hardcoded root-relative strings like `<a href="/users">`. Instead, route generation goes through `apps/admin/src/lib/paths.ts`:

- `href(path)`: Wraps SvelteKit's `resolve` function. In an embedded build, `href('/users')` returns `/admin/users`. In a hosted build, it returns `/users`.
- `basePath`: Returns the normalized prefix without a trailing slash (`/admin` or empty string).
- `routePath(pathname)`: Strips the active base path from the browser location pathname, allowing route comparisons to remain uniform across all build targets.

## Asset URL resolution

The backend returns relative image paths for series covers, such as `/covers/{workId}.webp`.

In a separate-origin console, resolving `/covers/...` against the current browser origin would target the console instead of your server. Because Cloudflare Workers Static Assets does not store cover images, and because `not_found_handling = "single-page-application"` answers unknown paths with `index.html`, relative URLs would receive an HTML response with status 200 instead of image bytes.

To solve this, `assetSrc` in `apps/admin/src/lib/config.ts` resolves relative paths against `apiOrigin`:

```ts
export function assetSrc(url: string | null | undefined): string {
	if (!url) return '';
	return /^([a-z][a-z0-9+.-]*:)?\/\//i.test(url) || url.startsWith('data:')
		? url
		: `${apiOrigin}${url}`;
}
```

In `hosted` builds, `apiOrigin` comes from the configured API endpoint, such as `https://komiq.example.com`. In embedded builds, `apiOrigin` is an empty string, allowing the browser to resolve relative asset paths directly against the current server origin.

## Serving the embedded build

When self-hosting with `KOMIKA_WEB_DIR`, the backend server (`komika-server`) serves both the reader and admin web applications. The implementation is in `apps/server/src/web_dir.rs`.

The directory pointed to by `KOMIKA_WEB_DIR` must contain two subdirectories:

- `reader/`: Built static files for the [reader app](/developers/reader-app), served at `/`.
- `admin/`: Built static files for the admin app, served at `/admin`.

The server checks for the existence of `reader/index.html` and `admin/index.html` on startup. If either file is missing, the server logs a warning.

The routing layout is mounted in Axum using `mount`:

```rust
pub fn mount<S>(app: Router<S>, dir: &Path) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    for sub in ["reader", "admin"] {
        if !dir.join(sub).join("index.html").is_file() {
            tracing::warn!(
                dir = %dir.display(),
                "KOMIKA_WEB_DIR has no {sub}/index.html; that app will not load"
            );
        }
    }
    app.nest_service("/admin", spa(dir.join("admin")))
        .fallback_service(spa(dir.join("reader")))
}
```

The `spa` helper uses Tower HTTP's `ServeDir` configured with a file fallback:

```rust
fn spa(root: PathBuf) -> ServeDir<ServeFile> {
    let index = root.join("index.html");
    ServeDir::new(root).fallback(ServeFile::new(index))
}
```

If an incoming request matches a real static file, the server returns that file with its correct content type. If the requested path does not match any file, the service returns `index.html` with an HTTP 200 status code. This allows client-side routing to handle deep links like `/admin/users` or `/admin/settings` when a user refreshes the page.

API routes take precedence over static files. Endpoints like `/graphql`, `/health`, and `/admin/cover/{work_id}` are registered on the Axum router before `mount` runs. Incoming requests are matched against API endpoints first. Static files and SPA fallbacks only execute if no API route matches.

## Where to look

- `apps/admin/vite.config.ts`: Target selection, base path validation, adapter configuration, and CSP settings.
- `apps/admin/src/lib/config.ts`: Target constant inspection, server address persistence, and asset URL rewriting.
- `apps/admin/src/lib/paths.ts`: Helpers for generating base-aware links and normalizing route pathnames.
- `apps/admin/src/lib/auth.svelte.ts`: Admin session state management and admin-privilege verification.
- `apps/server/src/web_dir.rs`: Static directory mounting, SPA fallback handling, and route priority in Axum.
- `apps/admin/wrangler.toml`: Cloudflare Workers Static Assets configuration for a separate-origin console.
