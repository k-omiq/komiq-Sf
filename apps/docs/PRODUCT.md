# Product

<!-- impeccable:product-schema 1 -->

Scope: the docs site at docs.komiq.cc (`apps/docs`). Facts marked (inferred) come from the
repository and the request, not from a confirmed interview answer.

## Platform

web

## Stack

delegated: SvelteKit with `adapter-static`, fully prerendered, served as Cloudflare Workers
static assets. Same stack and deploy shape as `apps/admin`. Pages are Markdown files rendered at
build time.

## Users

- Self-hosters (inferred primary): people who install their own komiq server at home or on a
  VPS. They arrive with a terminal open and want the next command.
- Readers: people who use komiq.cc or the apps and have a problem or a question.
- Developers: people who read the code or contribute. They want to know how fetching, the image
  proxy, the on-device engine and each platform work.

## Product Purpose

One public place that explains komiq: how to set it up, how to use it, how to run the admin
console, and how the system works inside. Success: a reader finds the right page in one search
or two clicks and can act on it without reading the source.

## Positioning

komiq is a self-hostable, AGPL manga and comics reader. There are two apps (confirmed by the
owner, 2026-10-05):

- The admin app is the server and its management portal in one. A person self-hosts it: on their
  own computer for local or network-wide reading, on a VPS, behind a domain, with Cloudflare.
- The reader app connects to a server: a local server, or a server on the internet. It takes an
  IP address and port, a domain, and so on.
- The global server (komiq.cc) is a website only, for now. Its API accepts requests from the
  komiq.cc site alone, so the native apps cannot connect to it. This may open later.

The docs describe the real system, with file paths, and never describe plans as shipped. They
never tell a reader to build against the project's own komiq.cc API.

## Operating Context

Read on a desktop beside a terminal or editor, and on a phone when a device will not connect.
Content lives in the repository as Markdown and ships with the code.

## Capabilities and Constraints

- All content must be confirmed against code or current docs (`apps/docs/CONTENT_GUIDE.md`).
- No private infrastructure details. No names of third-party extension repos or source sites.
- No monetization claims: komiq takes no payments and carries no ads.
- Static output only. No server runtime.

## Brand Commitments

- Name: `komiq`, lowercase in running text. The coral "k" mark is the first letter of the
  wordmark (`apps/admin/src/lib/components/Brand.svelte`).
- Visual system: the admin console's. Tokens from `@komika/ui`, Bricolage Grotesque for display,
  Manrope for body, dark default with a light theme. Binding per the request.

## Evidence on Hand

`README.md`, `docs/self-host.md`, `docs/self-host-cloudflare.md`, `COPYRIGHT.md`, `PRIVACY.md`,
per-app READMEs, and the code. No testimonials, benchmarks or user counts exist; do not invent
them.

## Product Principles

1. True before complete: a missing page is better than a wrong one.
2. The next action first: commands and steps before background.
3. One system: the docs look and read like the product they describe.
4. One door per reader: five handbooks (start, self-host, use, admin, developers), so nobody wades through another reader's pages.

## Accessibility & Inclusion

WCAG 2.2 AA. Keyboard reachable navigation and search. Honors reduced motion. Readable at 200%
text zoom and at phone width.
