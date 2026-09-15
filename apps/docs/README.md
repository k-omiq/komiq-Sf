# @komika/docs

The docs site at [docs.komiq.cc](https://docs.komiq.cc): setup, support, admin and developer
documentation in one place. It uses the admin console's design system (`@komika/ui`).

## How it is built

- SvelteKit with `adapter-static`. Every page is prerendered to HTML; there is no server.
- A page is one Markdown file in `src/content/<handbook>/<slug>.md`.
- `src/lib/nav.ts` lists the five handbooks and the order of their chapters. A chapter listed
  there with no file is left out of the site.
- `src/lib/server/content.ts` renders the Markdown at build time (marked + shiki) and checks
  every internal link. A dead link or a file missing from `nav.ts` fails the build.
- Search is a static index (`/search.json`) scanned in the browser. Open it with Cmd/Ctrl+K or `/`.

## Write a page

Read [`CONTENT_GUIDE.md`](./CONTENT_GUIDE.md) first. It sets the truth rules (code wins over old
docs), what must never be published, the voice and the Markdown format, including `flow` blocks
and callouts.

1. Add the file under `src/content/<handbook>/`.
2. Add its slug to `src/lib/nav.ts`. The menu shows the page's own `title`.
3. Run the site and read your page.

## Develop

```sh
pnpm dev:docs            # http://localhost:5410
pnpm --filter @komika/docs check
pnpm --filter @komika/docs build
```

## Deploy

The build output is served as Cloudflare Workers static assets (see `wrangler.toml`).

```sh
pnpm --filter @komika/docs build
cd apps/docs && pnpm exec wrangler deploy
```
