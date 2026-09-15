---
title: Get the reader app
description: Choose a reader app for your platform and understand its server connection, fetching options, and current status.
---

The komiq reader app connects to a komiq server on your local network or on the internet. In a browser, you can also read on the global server at `komiq.cc`. The app shares one codebase across web, desktop, iOS, and Android builds. This page explains each platform's capabilities and current status.

## Web application

The web reader runs on the global server or as an embedded interface served by your own server. Browser builds use their configured server; they do not show the native app's server picker.

### Global server

Open `komiq.cc` to read on the global server, the project's free, work-in-progress demo.

This version is built with SvelteKit and runs on Cloudflare. It uses server-side rendering (SSR): edge servers generate the initial HTML page before sending it to your browser. The global server uses `api.komiq.cc` for data and a Cloudflare Worker at `img.komiq.cc` to fetch images for the browser. The demo operates without payment, advertising, or monetization. For now, the global server is a website only. Its API accepts requests from the `komiq.cc` site alone, so the desktop and mobile apps cannot connect to it. The project may open it to the apps later.

### Embedded self-hosted reader

When you self-host the admin app with Docker, the server delivers the reader at `/` from `KOMIKA_WEB_DIR`. The admin console is the management portal at `/admin`.

This build is a static single-page application (SPA), meaning the browser downloads the application once and updates content dynamically without reloading the whole page. The embedded reader connects directly to `/graphql` on the same host address. It does not use a separate Cloudflare image proxy. Instead, it retrieves images directly from sources or through the server proxy route at `/img`.

To learn how to deploy the server, read [Install with Docker](/self-host/docker).

## Desktop application

The desktop application is built with Tauri, combining a Rust application core with the SvelteKit frontend.

### Supported platforms

Desktop builds run on three operating system families:

- macOS (Apple silicon and Intel)
- Linux
- Windows

### On-device content engine

The desktop application includes an embedded Suwayomi engine packaged as a Java archive (`Suwayomi-Server.jar`) alongside a bundled Java runtime environment (JRE).

Because the engine runs inside the desktop application, the desktop app can fetch chapter pages directly from source sites onto your computer. When connecting to a server, desktop users can choose whether the desktop application or the server fetches chapter pages. The engine starts only when you choose the desktop application. A server owner can remove the server choice; see [Connect to a server](/guide/connect).

The reader desktop app connects to a server; the admin desktop app runs one. To self-host on your computer, see [Run the desktop app](/self-host/desktop-app).

### Release status

The reader desktop app is compiled from source in the repository using `pnpm --filter @komika/reader desktop:build`. Choose your own server's address in the app. The app cannot use the global server.

## Mobile applications

The repository contains native iOS and Android builds using Tauri mobile scaffolding. The build requirements below apply to these source builds.

### iOS

The iOS application requires iOS 15.0 or later. It runs as a native shell wrapping the reader interface.

Features and status on iOS:

- Camera scanner: includes a native barcode scanner plugin to read the server connection QR code shown in the admin console.
- Chapter fetching: by default, chapter pages are fetched by your komiq server rather than on the phone itself. Physical device builds include an embedded engine implementation. In a test build it has started on a real iPhone and fetched a chapter page, but it is still a test feature with open defects, so on-device fetching is not offered by default.
- Building: iOS builds require Xcode.

### Android

The Android application wraps the reader interface for Android mobile devices.

Features and status on Android:

- Camera scanner: includes the native barcode scanner plugin to scan server connection QR codes.
- Chapter fetching: your komiq server fetches chapter pages, unless the app was built with the on-device engine. A build with the engine is about 189 MB instead of about 5.5 MB and needs Android 9 or later for the engine. It lets you choose **This device** when you connect. It has been tested on one phone.
- Building: Android builds require the Android SDK and NDK.

## Feature comparison

| Platform                 | Server address                    | QR code scanning | On-device chapter fetching      |
| ------------------------ | --------------------------------- | ---------------- | ------------------------------- |
| Global server web reader | Global server, fixed in the build | No               | No                              |
| Embedded web             | Same origin                       | No               | No                              |
| Desktop                  | Configurable on setup             | No               | Yes (bundled JRE)               |
| iOS                      | Configurable on setup             | Yes (camera)     | Not offered by default          |
| Android                  | Configurable on setup             | Yes (camera)     | Yes, in a build with the engine |

"Configurable on setup" means a self-hosted server. For now, the global server is not a choice in the apps.

The native reader apps accept an IP address and port, a domain, or a pasted link. Phones also scan a QR code from the admin app. Continue to [Connect to a server](/guide/connect) for the main connection guide. For build instructions and platform details, see [iOS](/developers/ios), [Android](/developers/android), and [Desktop](/developers/desktop).
