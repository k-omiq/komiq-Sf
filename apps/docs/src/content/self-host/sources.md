---
title: Add sources
description: Choose sources for your own server in the admin app and manage their extensions and health.
---

Choose reading sources for the server you run, using the admin app’s management portal. A fresh server has no content sources or installed extensions. This page covers MangaDex, extension repositories, source health and the optional FlareSolverr helper; use sources you are allowed to access.

## The MangaDex toggle

MangaDex is an online manga platform. Komiq includes built-in support for searching, indexing, and reading from MangaDex.

You can enable or disable MangaDex on the **Sources** step of [First-run setup](/self-host/first-run), or at any time in the admin console under **Settings › Sources**.

- When enabled, the server searches MangaDex titles, fetches covers, indexes chapters, and proxies images.
- When disabled, the server never contacts MangaDex.

To change the setting in the admin console:

1. Open your desktop admin app, or sign in to your server’s admin console at `/admin`.
2. Open **Settings › Sources**.
3. Toggle the **MangaDex** switch.

> [!NOTE]
> If your server environment sets `KOMIKA_MANGADEX=on` or `KOMIKA_MANGADEX=off`, or runs with `CATALOGUE_SYNC=on`, the setting is pinned by the environment and cannot be changed in the browser. Read [Settings reference](/self-host/configuration) for environment details.

## Add an extension repository

An extension repository is an external web directory that publishes an index of source extensions. Extensions teach the source engine (Suwayomi) how to search and fetch content from specific websites.

To add a repository:

1. In the admin console, open **Settings › Sources**.
2. Scroll to **Extension repos**.
3. Paste the repository index address into the field. The address must begin with `https://`.
4. Click **Add repo**.

The server validates the index address. Once validated, the repository appears in your list with its name and available extension count.

To remove a repository, click the remove button next to it. Removing a repository keeps extensions you already installed from it, but stops future update checks from that index.

## Install and manage extensions

Once you add a repository, its extensions become available to install.

### Install an extension

To browse and install extensions:

1. In the admin console, navigate to **Sources › Extensions** (or open `/admin/sources/extensions`).
2. Use the filter controls to locate extensions:
   - Filter by status: **All**, **Installed**, or **Available**.
   - Filter by language with the **Language** menu.
   - Search by name with the search field.
3. Click **Install** on any extension you want to use.

The source engine downloads the extension package, verifies its structure, and registers its sources.

### Background updates

Each installed extension includes a **Background updates** switch:

- When turned on, the server periodically scans the extension's sources to discover new series and keep library titles updated. Turning this on also runs an immediate scan.
- When turned off, the server only fetches series and chapters when you browse them or add them to your library.

### Extension updates and removal

- The server checks for extension updates at boot and every six hours.
- Click **Refresh** on the Extensions page to re-check repositories manually.
- When an update is available, click **Update** to install it.
- Click **Uninstall** to remove an extension and its sources from your server.

## Monitor source health

External websites can change layouts, encounter outages, or block automated requests. The admin console tracks the health of every source.

To view health statistics, open **Sources › Health** in the admin console.

### Request surfaces

The health monitor tracks requests across four surfaces:

| Surface    | Display name  | Description                                                   |
| ---------- | ------------- | ------------------------------------------------------------- |
| `pages`    | Chapter opens | Requests to fetch page images when opening a chapter.         |
| `image`    | Page images   | Proxy requests to retrieve individual page image files.       |
| `chapters` | Chapter lists | Requests to list available chapters for a title.              |
| `browse`   | Listings      | Requests to browse popular, latest, or searched manga titles. |

### Failure causes

When requests fail, the health monitor categorizes the reason:

| Status           | Severity | Description                                                                 |
| ---------------- | -------- | --------------------------------------------------------------------------- |
| Blocked          | Danger   | The site returned HTTP 403 or presented an unsolved Cloudflare challenge.   |
| Rate limited     | Warning  | The site returned HTTP 429 Too Many Requests.                               |
| Solver down      | Warning  | FlareSolverr failed or did not respond when required.                       |
| Not found        | Danger   | The site returned HTTP 404. Content moved or the extension needs an update. |
| Timeout          | Warning  | The website did not respond in time.                                        |
| Unreachable      | Danger   | Network failure (DNS, TCP connection, or TLS error).                        |
| Site error       | Warning  | The website returned an HTTP 5xx error.                                     |
| Extension broken | Danger   | The extension failed to parse site markup and needs an update.              |
| Engine           | Info     | Suwayomi encountered an internal engine error.                              |

The health page also tracks cached fallback counts. When an upstream fetch fails, the engine can fall back to its internal database, which previously masked broken sources.

## Solve challenges with FlareSolverr

Some manga websites protect their pages with Cloudflare browser checks, presenting a "checking your browser" challenge. Automated requests fail on these sites with a `Blocked` status.

FlareSolverr is an optional helper service that runs a headless web browser. When the engine encounters a challenge, FlareSolverr solves it and passes clearance cookies back to the engine.

### Enable FlareSolverr in Docker Compose

To enable FlareSolverr:

1. Open `deploy/selfhost`.
2. Copy `.env.example` to `.env` if you have not done so:
   ```sh
   cp .env.example .env
   ```
3. Edit `.env` to enable the profile and container:
   ```sh
   COMPOSE_PROFILES=flaresolverr
   FLARESOLVERR_ENABLED=true
   ```
4. Restart the Docker stack:
   ```sh
   docker compose up -d
   ```

Docker Compose starts the `flaresolverr` container and points Suwayomi at `http://flaresolverr:8191`.

> [!TIP]
> FlareSolverr has a memory limit of 2 GB configured in `deploy/selfhost/docker-compose.yml`. Ensure your host has sufficient memory before enabling it.

If you run komiq with the desktop app rather than Docker, you do not need FlareSolverr. The desktop app includes a built-in challenge solver using a hidden web view. Read [Run it on your computer](/self-host/desktop-app) for details.

Next: [Connect your devices](/self-host/other-devices).
