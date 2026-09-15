---
title: Tour of the console
description: Open the management portal of your admin app and use its sidebar and quick search.
---

The admin console is the management portal of the komiq admin app, the server and its portal in one. You self-host the admin app on your own computer, a home server or a virtual private server (VPS). Use the console to manage your catalog, sources, system health, users and server settings.

## Open the console

Open the console at `/admin` on your own server, or in the desktop app. Your server can be on your local network or reached over the internet through a domain, a Cloudflare Tunnel or a public IP address. See [Internet access](/self-host/internet-access).

### In your browser

When you self-host a komiq server with built web apps in `KOMIKA_WEB_DIR`, the directory holding web builds, the server hosts the embedded console build under the `/admin` path.

1. Open your browser and go to `http://192.168.1.20:8080/admin`. On the server machine itself, navigate to `http://localhost:8080/admin`.
2. If the server has no administrator account, the console redirects you to `/admin/setup`. Complete the setup steps to create the administrator account.
3. Creating the owner account signs you in. Once setup finishes, click **Open the catalog**.

The embedded console communicates with the `/graphql` endpoint on the same server origin, the scheme, hostname and port of its address.

### Desktop app

The admin desktop app bundles the console inside a Tauri window, a native app shell. It can run the server and source engine on your computer.

When you run the desktop app for the first time, it displays a start screen with two choices:

- **Run komiq on this computer**: Starts the Suwayomi source engine and the komiq server as local child processes. The app then opens the setup wizard with the local setup code filled in automatically.
- **Connect to a server I already run**: Lets you enter the URL of an existing remote komiq server.

On future launches, the desktop app remembers your choice and starts the local processes or connects to your saved server. Turn on **Allow other devices** to let devices on your network reach the local server.

The reader app connects to the server you run. Its **Connect to a server** screen takes an IP address and port such as `192.168.1.20:8080`, a domain such as `komiq.example.com`, a pasted link or a QR code. See [Connect to a server](/guide/connect).

## Sidebar groups

The sidebar groups console features into three main categories based on what you are looking after: Library, Sources, and Community. System configuration sits at the bottom. The paths below are relative to the console root, `/admin` in the embedded build.

### Library

The Library group manages catalog series, releases, and content health:

- **Catalog** (`/`): Search series, view chapter counts, adjust scan frequencies, add series to the canonical catalog, and initiate merges. See [Catalog and series editor](/admin/catalog).
- **Latest updates** (`/updates`): A chronological feed of newly released chapters from MangaDex.
- **Duplicates** (`/review`): A review queue for candidate pairs that the deduplication matcher could not decide with certainty. See [Duplicates, merge, and split](/admin/duplicates).
- **Cover issues** (`/bugs`): A list of series covers that failed download or conversion in the image processing pipeline. See [Cover issues](/admin/covers).

### Sources

The Sources group manages where comics and manga originate:

- **Extensions** (`/sources/extensions`): View installed source extensions, check for updates, and install new extensions from configured repositories.
- **Browse & add** (`/sources/browse`): Explore titles available through installed extensions and import them into your library.
- **Health** (`/sources/health`): Monitor source scan health. If a source has a detected outage, a red alarm badge appears next to Health in the sidebar.
- **Scanlators** (`/scanlators`): List the scanlation groups that chapters credit, and tag the groups that translate with AI.

For details on source configuration, see [Sources](/admin/sources).

### Community

The Community group manages user accounts and moderation:

- **Users** (`/users`): View registered accounts, create users, assign administrator privileges, and reset passwords. See [Users](/admin/users).
- **Reports** (`/reports`): Review reader-filed problem reports.

### Settings and account controls

The bottom section of the sidebar provides server configuration and session management:

- **Settings** (`/settings`): Configure theme preferences, NSFW content filters, registration toggles, MangaDex access, extension repositories, background auto-updates, network access modes, and bulk maintenance jobs. See [Settings](/admin/settings).
- **Account bar**: Displays your username and role. Includes a theme button that cycles between Dark, Light, and System modes, as well as a sign-out button.
- **NSFW banner**: If your account has NSFW content disabled, a warning banner appears below the navigation. Turning off NSFW blocks source browsing, extension installation, and adding series from sources.

> [!NOTE]
> On screens 900 pixels wide or narrower, the sidebar collapses into a hidden drawer. Click the menu button in the top bar to open it.

## Quick search

While signed in, you can open the quick search palette. The keyboard shortcut does not open it over another dialog:

- Press `Ctrl+K` on Linux or Windows.
- Press `Cmd+K` on macOS.
- Or click the **Search** button near the top of the sidebar.

```flow
Press Ctrl+K -> Search palette opens: type page name or title
Type at least 2 characters -> debounced query hits catalog
Select result with arrow keys -> press Enter to navigate
```

The palette provides two functions:

1. **Navigate to pages and settings**: Type the name of any sidebar page or settings section (such as `Theme`, `NSFW content`, `Extension repos`, `Extension updates`, or `Maintenance jobs`).
2. **Search the catalog**: Type at least two characters of a series title. The console waits 220 milliseconds after your input stops and displays up to six matching series with their covers, titles, and chapter counts.

Use the Up and Down arrow keys to navigate the list, and press Enter to open the selected item. Press Escape or click the backdrop to close the palette.
