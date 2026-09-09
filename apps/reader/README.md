# sv

Everything you need to build a Svelte project, powered by [`sv`](https://github.com/sveltejs/cli).

## Creating a project

If you're seeing this, you've probably already done this step. Congrats!

```sh
# create a new project
npx sv create my-app
```

To recreate this project with the same configuration:

```sh
# recreate this project
npx sv@0.16.1 create --template minimal --types ts --no-install apps/reader
```

## Developing

Once you've created a project and installed dependencies with `npm install` (or `pnpm install` or `yarn`), start a development server:

```sh
npm run dev

# or start the server and open the app in a new browser tab
npm run dev -- --open
```

## Building

To create a production version of your app:

```sh
npm run build
```

You can preview the production build with `npm run preview`.

> To deploy your app, you may need to install an [adapter](https://svelte.dev/docs/kit/adapters) for your target environment.

## Build targets

`KOMIKA_TARGET` picks the build (default: the hosted web build):

| Target      | Adapter              | API and images                                                                      |
| ----------- | -------------------- | ----------------------------------------------------------------------------------- |
| unset (web) | Cloudflare, edge SSR | `PUBLIC_*` from `.env.production` (komiq.cc, unchanged)                             |
| `tauri`     | static SPA           | `PUBLIC_*` (native apps, the Docker image)                                          |
| `embedded`  | static SPA           | same origin: `/graphql`, own images direct, MangaDex via `/img`; `PUBLIC_*` ignored |

The `embedded` build is the one the API server serves at `/` from `KOMIKA_WEB_DIR`
(next to the embedded admin at `/admin`):

```sh
KOMIKA_TARGET=embedded pnpm -C apps/reader build   # output: apps/reader/build
```

Images: `PUBLIC_KOMIKA_IMG_MODE` is `proxy` (the `img.komiq.cc` Worker, hosted),
`direct`, or `server` — MangaDex images through the Komika server's own `/img` route
on the API origin, everything else direct. The `embedded` build always behaves like
`server` on its own origin. The server mounts `/img` when it serves the web apps
(`KOMIKA_WEB_DIR`) or when `KOMIKA_IMG_PROXY=on`.

Static builds ask the server's `instance` facts: a server without an owner shows a
"not set up" page that links to `/admin/setup`. The sign-in page hides sign-up when
the server has closed it (every build).
