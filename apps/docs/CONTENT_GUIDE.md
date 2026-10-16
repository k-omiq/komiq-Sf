# Writing a docs page

Every page on docs.komiq.cc is one Markdown file in `apps/docs/src/content/<section>/<slug>.md`.
The menu order lives in `apps/docs/src/lib/nav.ts`; add the slug of a new page there. This file is the contract for a page.

## The product model

Frame every page this way. It is the first thing a reader must understand.

komiq is self-hostable. There are two apps:

1. **The admin app** is the server and its management portal in one. It is what a person
   self-hosts. It runs the komiq server and the source engine, and it gives the owner the admin
   console. There are many ways to run it:
   - on your own computer with the desktop app, for this computer only or for every device on your
     network ("Allow other devices");
   - with Docker on a home server or a VPS;
   - reached over the internet through a domain, a Cloudflare Tunnel or a public IP, with the
     optional image proxy Worker.
2. **The reader app** is what people read with. It connects to a komiq server on the local
   network or on the internet. Its "Connect to a server" screen takes an IP address and port
   (`192.168.1.20:8080`), a domain (`komiq.example.com`), a pasted link, or a QR code.

The global server (komiq.cc, the project's free work-in-progress demo) is a website only, for
now. People read on it in a browser. Its API accepts requests from the `komiq.cc` site alone, so
that nobody can point other apps at it and abuse it. The native reader apps cannot connect to it.
The project may open it to the apps later, if enough people ask for it. Until a current doc says
it is open, write it as closed, and write "for now" where a page gives the reason.

In the repository the admin app is `apps/server` (the server) plus `apps/admin` (the console and
the desktop shell that starts the server) plus the Suwayomi engine. The reader app is
`apps/reader`. `apps/worker` is the optional image proxy.

Rules that follow from the model:

- Never tell the reader to build an app against, or point an app at, `api.komiq.cc`. The reader of
  these docs builds for their own server.
- Never list the global server as a choice on the "Connect to a server" screen, and never write
  that the reader app "connects to the global server". The global server is opened at `komiq.cc`
  in a browser. An app connects to a self-hosted server: one on the local network or one on the
  internet.
- `komiq.cc`, `api.komiq.cc`, `admin.komiq.cc` and `img.komiq.cc` are the project's own
  deployment: one example of the model, called "the global server". Name them only when a page
  describes the global server as a place to read in a browser, or in `/developers/deployment`.
- Do not write "the hosted server", "the hosted service" or "the hosted console" for the server an
  app talks to. Write "the server", or "the server the app is connected to". A server is whichever
  one the reader app is connected to.
- The admin console's primary builds are `embedded` (the server serves it at `/admin`, same
  origin) and `desktop` (the desktop app, which starts or picks a server). A console built for a
  separate origin (`hosted` target, `PUBLIC_KOMIKA_API`) is a secondary case: show it with the
  reader's own address, for example `https://komiq.example.com/graphql`.
- Example addresses are `192.168.1.20:8080`, `localhost:8080` and `komiq.example.com`. These are
  the product's own examples and are fine to print. Real machine addresses are not.

## Truth rules

1. Write only what the code or a current doc shows. Read the files before you write.
2. Current sources: the code, `README.md`, `docs/self-host.md`, `docs/self-host-cloudflare.md`,
   `COPYRIGHT.md`, `PRIVACY.md`, and the `README.md` inside each app.
3. History, not truth: `SPEC.md`, `CATALOGUE.md`, `PERFORMANCE_ROADMAP.md`,
   `NATIVE_ENGINE_STATUS_*.md` and everything in `docs/plans/`. Use them to find where to look,
   then confirm in code. When a doc and the code disagree, the code wins.
4. If you cannot confirm a claim, leave it out. Do not guess numbers, flags, paths, defaults or
   command names.
5. Say "planned" only when a current doc says so. Never describe a plan as a shipped feature.

## Never publish

- Real IP addresses, hostnames of private machines, SSH users or key paths, Cloudflare account IDs,
  email addresses, tokens, passwords, or anything from `PRODUCTION.md`, `deploy/GO-LIVE.md`,
  `deploy/CLOUDFLARE-RUNBOOK.md` or `.env` files that identifies the operator's own machines.
  Public names are fine: `komiq.cc`, `api.komiq.cc`, `img.komiq.cc`, `admin.komiq.cc`.
- Names or URLs of third-party extension repos or source sites. MangaDex and Suwayomi may be
  named. For anything else say "an extension repo" or "a source".
- Advice that helps someone get content they are not allowed to access. komiq is reader
  software; the reader of these docs brings their own sources and follows the law.

## Voice

- Plain words. Short sentences. Active voice. Talk to the reader as "you".
- One idea per sentence. Lead with what to do, then why.
- The product name is `komiq` in running text and `Komiq` at the start of a sentence.
- No em dashes. Use a comma, a colon or a full stop.
- No marketing words: no "powerful", "seamless", "robust", "simply", "just".
- Explain a technical word the first time you use it on a page.

## Format

```markdown
---
title: Install with Docker
description: One sentence that says what the reader can do after this page.
---

Opening paragraph: two or three sentences. What this page covers and who it is for.

## First section
```

- Frontmatter has exactly two keys, `title` and `description`. No quotes unless the text has a colon.
- The title is the chapter's one name: its heading, its menu entry and its search result. Write it
  in sentence case and keep it to 32 characters or fewer.
- Do not write an H1. The title comes from the frontmatter.
- Use `##` for sections and `###` under them. No deeper levels.
- Code blocks always name a language: `sh`, `ts`, `svelte`, `rust`, `toml`, `yaml`, `json`,
  `graphql`, `sql`, `kotlin`, `swift`, `text`.
- File paths, commands, settings and identifiers go in backticks, relative to the repo root:
  `apps/worker/src/index.ts`.
- Callouts use this exact form, on their own block:

  ```markdown
  > [!NOTE]
  > Text of the note.
  ```

  Kinds: `NOTE`, `TIP`, `WARNING`.

- Tables are fine for settings and comparisons. Keep them to four columns or fewer.
- Link to other pages with absolute paths from the list below: `[Add sources](/self-host/sources)`.
- To show a path that data takes, use a `flow` block. One hop per line, at most eight lines:

  ````markdown
  ```flow
  Reader app -> Image proxy: asks for a page image
  Image proxy -> Source site: fetches the image
  Image proxy -> Reader app: returns the bytes
  ```
  ````

- Developer pages end with a `## Where to look` section: a bullet list of the source files that
  matter, each with a few words on what it holds.
- Length: 400 to 1100 words. Developer pages may go to 1600.

## Pages

```text
/start/what-is-komiq          /start/how-it-works            /start/choose-your-setup

/self-host/docker             /self-host/desktop-app         /self-host/first-run
/self-host/sources            /self-host/other-devices       /self-host/internet-access
/self-host/configuration      /self-host/update-and-backup   /self-host/troubleshooting

/guide/apps                   /guide/connect                 /guide/accounts
/guide/browse                 /guide/reading                 /guide/library
/guide/community              /guide/troubleshooting         /guide/faq
/guide/privacy                /guide/copyright

/admin/overview               /admin/catalog                 /admin/duplicates
/admin/covers                 /admin/sources                 /admin/engine
/admin/users                  /admin/settings

/developers/architecture      /developers/repository         /developers/local-development
/developers/fetching          /developers/image-proxy        /developers/edge-caching
/developers/media-manifests   /developers/media-routes
/developers/server            /developers/graphql            /developers/data-model
/developers/catalogue-sync    /developers/canonical-works    /developers/suwayomi
/developers/native-engine     /developers/ios                /developers/ios-engine
/developers/android           /developers/desktop            /developers/reader-app
/developers/admin-app         /developers/auth-security      /developers/deployment
/developers/testing           /developers/contributing
```
