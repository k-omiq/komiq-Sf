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
npx sv@0.16.1 create --template minimal --types ts --no-install apps/admin
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

`ADMIN_TARGET` picks where the build runs (default `hosted`):

| Target     | API                                                  | First run                                     |
| ---------- | ---------------------------------------------------- | --------------------------------------------- |
| `hosted`   | `PUBLIC_KOMIKA_API` (admin.komiq.cc, unchanged)      | none                                          |
| `embedded` | same origin `/graphql`, served from `KOMIKA_WEB_DIR` | a server with no owner opens `/setup`         |
| `desktop`  | the server chosen on `/start`, saved on the device   | `/start` (run here or connect), then `/setup` |

`ADMIN_BASE_PATH=/admin` builds the console for a sub-path; the server serves the
`embedded` build there:

```sh
ADMIN_TARGET=embedded ADMIN_BASE_PATH=/admin pnpm -C apps/admin build
```

Internal links go through `href()` in `src/lib/paths.ts` so they carry the base path.
The desktop bridge the Tauri shell implements is described in `src/lib/desktop.ts`.
