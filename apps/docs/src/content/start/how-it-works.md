---
title: How it works
description: Follow how the admin app and reader app work together, and where covers and chapter pages come from.
---

Komiq has two apps: the admin app you self-host and the reader app you read with. This page follows how they work together. It explains where covers and pages come from, and how browsers and native apps fetch images.

## The admin app

The admin app is the server and its management portal in one. You can run it with the desktop app on your computer, or with Docker on a home server or virtual private server (VPS). You can keep it on one computer, allow devices on your network, or reach it over the internet.

### The server

The server coordinates the system. It handles user accounts, sign-in sessions, reading history, ratings, and comments. It also maintains an index of series metadata. When you open a series, the server tells the reader app which chapters are available and which sources provide them.

### The source engine (Suwayomi)

The source engine handles communication with third-party sources. It runs Suwayomi-Server, which uses extensions to search catalogs and retrieve chapter information from websites. The server talks to this engine whenever it needs new series data or updated chapter listings.

### The admin console

The admin console is the management portal. You use it to install source extensions, merge duplicate series entries, review user reports, and configure network settings. The embedded console is served at `/admin`. The admin desktop app starts the server and opens the console.

## The reader app

The reader app displays your library, catalog search results, chapter lists, and comic pages. It connects to a komiq server on your local network or on the internet. The native desktop and mobile apps take an IP address and port, a domain, or a pasted link. Phones can also scan a QR code. See [Connect to a server](/guide/connect).

You can also read in a browser. Open your server's address for its embedded web reader, or `komiq.cc` for the global server, the project's free work-in-progress demo. For now, the global server at `komiq.cc` is a website only: its API accepts requests from the `komiq.cc` site alone, so the apps cannot connect to it.

## How a page reaches your screen

When you select a chapter to read, komiq does not open a file from a central server archive. The image is fetched directly from its original source at the moment you ask for it.

A proxy fetches an image on behalf of your browser. Here is the path through an image proxy:

```flow
Reader app -> Image proxy: asks for a page image
Image proxy -> Source site: fetches the image
Source site -> Image proxy: returns image data
Image proxy -> Reader app: delivers page to your screen
```

The process happens in four steps:

1. You turn to a new page in the reader app.
2. The reader app sends an image request to the image proxy.
3. The image proxy downloads the original image bytes from the source site.
4. The proxy returns the image to your browser and saves a temporary copy in the edge cache.

If you return to that page shortly afterward, the edge cache returns the image immediately without contacting the source site again.

## Web versus native apps

The web reader and the native apps handle images differently because of browser security restrictions.

### Reading on the web

Web browsers enforce Cross-Origin Resource Sharing (CORS). CORS prevents a website from downloading images or data from an unrelated domain unless that domain explicitly permits it. Many comic sources do not send CORS headers or they block requests that come from unfamiliar web origins.

To solve this, the web reader routes its image requests through the image proxy. The global server uses a Cloudflare Worker (`apps/worker`). A self-hosted server provides its own `/img` proxy endpoint. The Worker is optional for self-hosting. The proxy retrieves the image, attaches the required headers, and delivers it to your browser.

### Reading in native apps

The desktop, iOS, and Android apps do not run inside a standard browser sandbox. They are not restricted by CORS rules. Because of this, native apps can fetch page images directly from upstream source sites.

The desktop reader can also fetch chapter pages with a Suwayomi engine on the same device. So can an Android reader that was built with the engine. The server then does not fetch those pages. You can choose **This device** or **The server** when connecting, unless the server owner has set apps to fetch for themselves. Other mobile apps use the server. See [Get the reader app](/guide/apps) for platform status.

## Where covers and pages are stored

Komiq is designed to be an index and a reader, not a permanent file storage service. It treats covers and chapter pages differently.

### Cover thumbnails

When you browse titles, the reader displays dozens of cover images at once. To avoid overloading source websites every time someone opens the catalog, the komiq server downloads cover images, resizes them into small thumbnails, and stores them in local server storage. These thumbnails are served directly by the komiq server.

### Chapter pages

Chapter pages are never archived permanently on the server. When you finish reading a chapter, komiq keeps your reading progress and the catalog entry that records where the series lives. It does not store the comic chapter files.

Temporary copies of page images exist only in temporary caches:

- In your browser or device memory while the page is visible.
- In Cloudflare edge cache for a limited time to make re-reading fast.
- In local engine cache on machines running Suwayomi.

Once those temporary cache entries expire or are cleared, no copies remain on komiq infrastructure.

## Next steps

Explore how to get started with komiq:

- [What is komiq](/start/what-is-komiq): Read an overview of the project and license.
- [Choose your setup](/start/choose-your-setup): Pick the right installation path for your needs.
- [Install with Docker](/self-host/docker): Set up your own server at home or on a VPS.
- [Run the desktop app](/self-host/desktop-app): Run the admin app and its server on your computer.
