---
title: Contributing
description: Contribute code and documentation for the two self-hostable komiq apps.
---

Contribute to komiq as a self-hostable product with two apps. The admin app combines `apps/server`, `apps/admin` and Suwayomi; the reader app lives in `apps/reader`, with `apps/worker` as an optional image proxy. This guide covers licensing, code style and documentation for developers and writers preparing pull requests.

## The AGPL license

All code in this repository is licensed under the GNU Affero General Public License, version 3.0 only (AGPL-3.0-only). The full legal text lives in `LICENSE`, with third-party notices in `NOTICE`.

### Why komiq uses the AGPL

The admin app runs Suwayomi-Server as its source engine. Native reader builds can also embed it on the device. Suwayomi-Server is licensed under AGPL-3.0-only. The project's `NOTICE` explains that the whole project is released under AGPL-3.0 with public Corresponding Source for the software it distributes.

### What the AGPL means for contributors

The license and project notices set out these requirements:

1. Open source grant: All submitted code becomes part of an AGPL-3.0 project. You cannot contribute code that imposes proprietary restrictions or conflicts with copyleft terms.
2. Network service requirement: Under AGPL Section 13, a modified version supporting remote interaction must prominently offer its Corresponding Source to all users interacting with it remotely over a computer network, through a network server at no charge.
3. Complete Corresponding Source: For released binaries, `NOTICE` commits to publishing the build and packaging scripts needed to regenerate them, including the JRE assembly steps and Tauri bundle configuration.
4. Project policy: komiq accepts no donations, subscriptions, or payments of any kind, and carries no advertising. Nothing is monetized.

### Third-party content and copyright

The AGPL license covers the software codebase only. It does not grant anyone a license to third-party comics, manga, artwork, or trademarks.

- Do not check in private API tokens, passwords, or personal server credentials.
- Komiq is reader software; users supply their own sources and are responsible for following applicable laws and source terms.

## Code style and formatting

Komiq enforces automated code style checks to keep changes consistent across the monorepo.

### Prettier configuration

JavaScript, TypeScript, Svelte, JSON, CSS, and Markdown files are formatted with Prettier. Settings are defined in `.prettierrc.json`:

- Tabs for indentation: `useTabs: true`.
- Single quotes: `singleQuote: true`.
- Print width: 100 characters (`printWidth: 100`).
- Svelte formatting: Handled through `prettier-plugin-svelte`.

To check formatting across the repository:

```sh
pnpm lint
```

To automatically reformat code before opening a pull request:

```sh
pnpm format
```

### Rust standards

Code in `apps/server`, `apps/admin/src-tauri`, and `apps/reader/src-tauri` must conform to standard Rust conventions:

- Format code using `rustfmt`: Check with `cargo fmt --check`.
- Lint with `clippy`: Check with `cargo clippy --all-targets -- -D warnings`.
- CI requires zero clippy warnings for `apps/server`.

### TypeScript standards

Applications with a `check` script run static type checking:

- Verify with `pnpm check`.

## How to add a documentation page

Documentation pages are written in Markdown and rendered into the docs website at docs.komiq.cc. The guide contract lives in `apps/docs/CONTENT_GUIDE.md`.

### File location and naming

Every documentation page is stored in `apps/docs/src/content/<section>/<slug>.md`.

Supported sections:

- `start/`: Introduction, overview, and quick installation choices.
- `self-host/`: Docker guides, configuration, and maintenance.
- `guide/`: User reader guides, library management, and account settings.
- `admin/`: Guides to the admin app's management portal, catalog curation and source configuration.
- `developers/`: Architecture, internals, APIs, and development workflows.

The slug forms the public URL path: `apps/docs/src/content/developers/my-page.md` becomes `/developers/my-page`.

### Frontmatter format

Each page begins with YAML frontmatter containing exactly two keys:

```text
---
title: My page title
description: One sentence explaining what the reader can do after reading this page.
---
```

Do not add quotation marks around titles or descriptions unless the string contains a colon.

### Writing style rules

Follow these writing rules to maintain clarity:

- Plain words: Write short sentences in active voice. Address the reader as "you".
- Lead with action: Explain what to do before explaining background reasons.
- Product name: Write `komiq` in lowercase within sentences. Capitalize as `Komiq` only at the start of a sentence.
- Punctuation: Do not use em dashes. Use a comma, colon, or period instead.
- Word choices: Do not use marketing language or buzzwords. Avoid exaggerated adjectives.
- Technical terms: Explain technical words the first time they appear on a page.
- Heading hierarchy: Do not write an H1 heading in markdown. The title comes from the frontmatter. Use `##` for top sections and `###` for subsections. Do not use deeper levels.
- Code blocks: Always specify the programming language tag on code fences (`sh`, `ts`, `rust`, `svelte`, `json`, `yaml`, `sql`, `graphql`).
- Page length: Pages should be between 400 and 1100 words. Developer documentation may extend up to 1600 words.

### Product framing

Describe the admin app as the server and its management portal in one. You can run it on your computer, a home server or a virtual private server (VPS), and reach it over the internet through a domain, a Cloudflare Tunnel or a public IP. The reader app connects to that server. Its native **Connect to a server** screen takes an IP address and port, a domain, a pasted link or a QR code.

Use your own server addresses in development and build examples, such as `localhost:8080` and `komiq.example.com`. Open the console at `/admin` on your server or in the desktop app. Treat `embedded` and `desktop` as the primary console builds; `hosted` is the secondary target for a separate origin. The project's global server is one deployment example. Follow the guide's rules for when to name it.

### Technical truth rules

Documentation must reflect actual code behavior:

1. Confirm against code: Read the implementation files before writing a claim.
2. No guessing: If a default value, port, flag, or path cannot be verified, leave it out.
3. Never publish private information: Do not publish private IP addresses, personal server hostnames, SSH keys, or Cloudflare account IDs.
4. Source names: Do not publish names or URLs of third-party source sites or extension repositories. MangaDex and Suwayomi may be named. For anything else, use generic phrases like "a source" or "an extension repo".
5. Link validation: Internal links must use absolute paths from the allowed page list defined in `apps/docs/CONTENT_GUIDE.md`.
6. Ending section: Developer pages must end with a `## Where to look` section listing relevant source files.

### Registering the page in navigation

After creating your Markdown file, register the slug in the navigation menu file at `apps/docs/src/lib/nav.ts`. Keep it within the page list in `apps/docs/CONTENT_GUIDE.md`. This ensures the page appears in the sidebar navigation and search index.

## Where to look

- `LICENSE`: The AGPL-3.0 license terms.
- `NOTICE`: Third-party attributions and Corresponding Source commitments.
- `.prettierrc.json`: Monorepo formatting rules.
- `apps/docs/CONTENT_GUIDE.md`: Standards, voice, formatting, and allowed link targets for documentation.
- `apps/docs/PRODUCT.md`: Scope and principles for the documentation site.
- `apps/docs/src/content/`: Source markdown files for documentation pages.
