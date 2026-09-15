---
title: Questions
description: Understand the two apps, server choices, content sources, accounts, and self-hosting options.
---

Komiq has two apps: the admin app you self-host and the reader app you read with. The reader app connects to a server on your local network or on the internet. In a browser, you can also read on the global server at `komiq.cc`. This page answers common questions about those choices, costs, content, and accounts.

## Is it free?

Yes. Komiq is free to use. The global server at `komiq.cc` is the project's free, work-in-progress demo.

The project does not charge subscription fees, offers no paid tiers, and accepts no donations or payments of any kind. The software carries no advertisements and contains no third-party tracking scripts. For privacy specifics, see [Privacy](/guide/privacy).

## Where does content come from?

Komiq is a catalogue and reading client. It is not an official publisher and does not operate a file-hosting service.

- Third-party sources: komiq connects to sources using Suwayomi as its content engine. Eligible community members and server administrators configure which sources to make available.
- On-demand retrieval: When you open a chapter, the reader requests page images from the original source through a web proxy or a native fetch path, depending on the build and configuration. The server or image proxy may temporarily store images in edge caches to reduce traffic on source sites.
- No media archive: komiq is not intended to operate a permanent central archive of manga or comic files. Page copies can remain in engine, browser, or device caches. The reader provides no bulk download tools and no file export features.
- Intellectual property: Copyrights and trademarks belong to their respective creators, translators, and publishers. Users must add and read only material they are authorized to access. For reporting procedures, see [Copyright](/guide/copyright).

## Is there an app?

Yes. The reader app shares a single codebase across multiple platforms:

- Web: open the global server at `komiq.cc`, or open your own server's address.
- Desktop: Desktop applications for Linux, macOS, and Windows built with Rust and the Tauri framework.
- Mobile: Mobile application builds for Android and iOS.

The native reader app connects to a self-hosted komiq server, on your network or on the internet. It cannot connect to the global server; open `komiq.cc` in a browser for that. Enter an IP address and port, a domain, or a pasted link. Phones can also scan the admin app's QR code. See [Get the reader app](/guide/apps) for platform status and [Connect to a server](/guide/connect) for instructions.

## Can I host my own?

Yes. Komiq is self-hostable, open-source software released under the AGPL-3.0-only license. You self-host the admin app, which includes the server, source engine, and management portal.

You can run the admin desktop app on your own computer. Keep it available to this computer alone, or turn on **Allow other devices** for your local network. See [Run the desktop app](/self-host/desktop-app).

You can also run it with Docker and Docker Compose on a home server or virtual private server (VPS). From `deploy/selfhost`, `docker compose up -d` starts a komiq container serving the web application and API, and a separate Suwayomi engine container.

Running your own server gives you full control over your user accounts, source extensions, and library data. To set up your server, see [Install with Docker](/self-host/docker). To reach it over the internet through a domain, a Cloudflare Tunnel, or a public IP, see [Internet access](/self-host/internet-access). A tunnel connects out from your server without opening router ports.

## Do I need an account to read?

No. You do not need an account to browse the catalogue or read chapters.

Sign in if you want to:

- Track series on your personal library shelves.
- Save reading progress across multiple devices.
- Rate series on the 10-star scale.
- Post comments and replies in chapter and series discussions.
- Receive notification alerts when other readers reply to your comments.

For more details on library features, see [Library](/guide/library).

## Why are some chapters missing or unreadable?

Komiq indexes chapters reported by configured sources.

- Source availability: A chapter may have no readable pages at its source, or source outages may prevent pages from loading.
- Off-site chapters: Some publishers host chapters exclusively on their own external platforms. komiq marks these chapters as off-site and provides a link to read them on the publisher's website.
- Multiple versions: If a series has several sources or translation groups, you can open the reader menu under Group & source to choose an alternative copy. For reading controls, see [Reading](/guide/reading).
- Reporting problems: If an entry has missing metadata or chapters from another series, you can submit a report. For instructions on filing reports, see [Troubleshooting](/guide/troubleshooting).
