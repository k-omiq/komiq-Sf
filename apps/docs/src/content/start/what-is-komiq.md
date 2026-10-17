---
title: What komiq is
description: Understand the two apps, what you can self-host, what komiq does not provide, and its license.
---

Komiq is a self-hostable, open-source reader for manga and comics with two apps. You run the admin app to host a server and manage it. You use the reader app to connect to a server and read.

## What komiq is

The reader app shares one codebase across the web, desktop computers, iOS, and Android. It connects to a server on your local network or a server on the internet. Native apps accept an IP address and port, a domain, or a pasted link. Phones can also scan a QR code.

The software gives you a single place to browse, read, and organize comics from different sources. It keeps track of series information, including titles, authors, genres, and chapter lists. It also provides community features. You can track your reading progress, organize your personal library, write reviews, and post comments on chapters.

Komiq uses Suwayomi as its underlying content engine. Suwayomi connects to sources through modular extensions. Komiq brings those capabilities together into a single reading interface with user accounts and community discussion.

## What komiq is not

Komiq is reading software, not a content provider. Understanding what komiq does not do helps you use and deploy it correctly.

### Not a publisher or content service

Komiq does not produce, publish, or license manga or comics. The project is not affiliated with or endorsed by any publisher, author, artist, or scanlation group.

The global server at `komiq.cc` is the project's free, work-in-progress demo of the reader app. You read on it in a browser. The desktop and mobile apps cannot connect to it. It is not an official content service. Komiq does not own or claim rights to any third-party works that you view through the reader.

### Not a media archive

Komiq does not maintain a central archive of manga or comic files. It does not provide bulk downloads, chapter exports, or file uploads.

When you open a chapter, the reader requests the pages from the third-party source that hosts them. Temporary copies may remain in your browser cache, on-device engine storage, or proxy edge caches so you can turn pages without delay. Once those caches clear, the images are gone. Komiq stores the catalog entry that points to where a chapter exists on the web, not the chapter itself.

### No payments and no advertising

Komiq accepts no payments, subscriptions, or donations of any kind. It carries no advertisements. Nothing in the software is monetized. Because komiq handles no financial transactions, the software contains no payment processors and collects no payment information.

## The two apps

The admin app brings the server, source engine, and management portal together. The reader app is the interface you read with.

### The reader app

The reader app (`apps/reader`) is the client you use to browse and read. It runs as a web application in modern browsers. It also runs as a native application for desktop operating systems, iOS, and Android using a shared Rust core.

### The admin app

You self-host the admin app on your own computer with its desktop app, or with Docker on a home server or a virtual private server (VPS). The desktop app can serve this computer alone or every device on your network with **Allow other devices**. For internet access, you can use a domain, a Cloudflare Tunnel, or a public IP address. A tunnel connects your server to the internet without opening router ports.

The server (`apps/server`) is written in Rust using axum and async-graphql. GraphQL is the interface the reader uses to ask the server for data. The server stores user accounts, authentication sessions, personal libraries, ratings, and comments in PostgreSQL. Covers are cached as files, with their index and new uploads kept in PostgreSQL. It communicates with Suwayomi to manage catalog data and chapter lists.

Komiq embeds Suwayomi-Server as its source engine. The engine manages extensions, communicates with external sources, and retrieves chapter data. With Docker, the engine runs in a container alongside the server. In the admin desktop app, it runs directly on your computer.

The admin console (`apps/admin`) is the management portal. You use it to configure sources, review duplicate entries, manage user accounts, and change server settings. The server serves the embedded console at `/admin`. The admin desktop app starts the server on your computer.

### The optional image proxy

The image proxy (`apps/worker`) is a Cloudflare Worker used by the web reader. Browsers enforce security rules that prevent a web page from loading images from external domains directly. The proxy fetches images on behalf of the browser and caches them at the Cloudflare edge for fast delivery. Native desktop and mobile apps bypass this proxy and fetch images directly.

## The license

Komiq is licensed under the GNU Affero General Public License, version 3.0 only (AGPL-3.0-only).

The project uses the AGPL-3.0 license because it embeds Suwayomi-Server, which is also licensed under AGPL-3.0. The AGPL requires that all modifications and network deployments remain free and open source. Anyone who distributes komiq or runs it as a public network service must make the complete Corresponding Source code available to their users under the same terms.

The software license applies only to the code of komiq. It does not grant any license or rights to third-party comics, covers, or trademarks. You are responsible for ensuring that you have authorization to access the sources you add.

## Next steps

Learn more about the architecture and setup options:

- [How it works](/start/how-it-works): See how data flows through each component.
- [Choose your setup](/start/choose-your-setup): Pick the best way to run or test komiq.
- [Privacy and your data](/guide/privacy): Read how the global server handles user data.
- [Content and copyright](/guide/copyright): Review rules for third-party works and notice procedures.
