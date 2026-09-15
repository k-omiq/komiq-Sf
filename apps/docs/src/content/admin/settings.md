---
title: Settings and maintenance
description: Configure your server’s access modes, accounts, sources, and maintenance, and set your console preferences.
---

Use the Settings page in the admin app to manage your personal preferences, your server’s access modes, extension repositories, and maintenance jobs. Settings are grouped into clear sections accessible from the sidebar table of contents. This guide walks through every section and explains what each option does.

## Appearance

Settings in the Appearance section are saved in your local browser only. They do not affect other users or devices:

- `Theme`: A segmented control that switches the console theme between `Dark`, `Light`, and `System`. When set to `System`, the console matches your device appearance settings.

## Content and NSFW controls

Settings in the Content section apply to your account profile on your server across both the admin console and reader apps connected to it:

- `Show NSFW content`: Controls whether NSFW (adult) content is visible.
  - When off, your server blocks browsing and ingestion of sources flagged NSFW, and installation or updates of extensions flagged NSFW for your account.
  - The admin console still displays NSFW records in catalog searches and latest updates to allow correcting wrong tags.
  - Turning this switch on allows those NSFW operations in [Sources](/admin/sources).

## Access and network modes

The Access section appears when your server reports its instance settings. It controls how readers and external devices connect to your server.

### Connection status

- `Mode`: `Public on the internet`, `Public without a domain`, `Only my devices` or `Not chosen yet`.
- `Address`: What readers use to reach your server, with a copy button. In a public mode, this is your domain or your public address. Otherwise it is the address this console is open on. If that is `localhost` and your server runs outside a container, it shows your server's home network address.
- `QR code`: Appears when other devices can reach the address. Readers scan it with a phone or in the reader app.
- `Cloudflare`: The connection type (`Cloudflare Tunnel` or `Public IP`), the domain and the zone. `Turned off` when the mode is no longer `Public on the internet`.
- `Connector`: For a tunnel, the connector's status (`Connected`, `Connecting…`, `Restarting`, `cloudflared not found`, or `Stopped`).
- `HTTPS`: For a public IP, whether HTTPS is being served.
- `Visitor addresses`: Indicates how visitor IP addresses are determined (configured `TRUSTED_PROXY_CIDRS`, Cloudflare proxy lists, or per-connection addresses).

### Cloudflare setup and removal

- `Set up Cloudflare`: Asks for a domain, a tunnel or public IP connection, and an API token. The form says whether the tunnel connector is on your server or will be downloaded, or whether port 443 is free.
- `Run setup again`: Runs setup again. It asks for the API token, which is never stored.
- `Remove`: Deletes the DNS record, tunnel, origin certificate and cache rules that setup created. It asks for the API token.
- `Allow other devices`: A switch in the desktop app that runs its own server. On, devices on the same Wi-Fi can open the reader.
- `Only my devices`: Appears in a public mode. After one confirm step, the tunnel or HTTPS on your server is turned off. What setup made at Cloudflare stays until `Remove`.
- `Turn on`: Appears next to a setup that is turned off. It needs no token.
- `I will do it by hand`: Shows the steps for the Cloudflare dashboard. `Save` stores the domain.
- Setup checklist: Displays live step progress during configuration or removal runs.

### Public without a domain

- `Use without a domain`: Appears while no Cloudflare setup is active, and not in the desktop app that runs its own server. Enter your server's public IP address, such as `203.0.113.5`. With `Encrypt the connection` on, the default, your server gets a Let's Encrypt certificate and serves HTTPS, and the form says whether ports 80 and 443 are free. With it off, you enter the address and port, such as `203.0.113.5:8080`, and **Check address** tests whether your browser gets an answer there. Saving sets the mode to `Public without a domain`.
- `Change the address`: The same form, once that mode is active.

Read [Reach it from the internet](/self-host/internet-access#without-a-domain) before you choose it.

## Accounts and registration

The Accounts section controls how new readers join your server:

- `Anyone can sign up`: A toggle that controls public registration.
  - When on, any visitor can register an account on your server through a connected reader app.
  - When off, reader apps hide registration. New accounts can only be created by an administrator in [Users](/admin/users).
  - If your server configuration sets `KOMIKA_REGISTRATION_OPEN`, your server refuses changes. After a refused attempt, the console displays the reason and disables the switch.

## Sources and extension repositories

Komiq includes no built-in comic sources. You must enable MangaDex, add extension repositories, or both.

### MangaDex integration

- `MangaDex`: Controls your server's direct MangaDex integration.
  - When on, the integration permits catalogue, cover, and chapter-page requests. Catalogue crawling also depends on your server's sync configuration.
  - When off, the direct integration refuses new requests to MangaDex. Suwayomi extensions are managed separately.
  - If configured by server environment variables, the switch is disabled and indicates that it is pinned.

### Extension repositories

Extension repositories provide Suwayomi with indexes of installable Mihon extensions:

- Adding a repository: Enter the repository index URL in the input field and click `Add repo`.
- Engine status: If Suwayomi is starting up or unreachable, a notice explains that the engine is not reachable yet, with a retry button.
- Repository list: Displays the name, index URL, and extension count for each configured repository.
- Removing a repository: Click the trash button on a repository row to delete it. Extensions from the repository will no longer be offered for installation or updates, but titles already added to your catalogue remain intact.
- Manage installed extensions: A footer link opens [Sources](/admin/sources) to manage installed extensions.

### Request limits

Some sites block a server that asks them too often. A request limit makes your server check one source for new chapters slowly.

- Adding a limit: Choose an installed source, enter how many requests are allowed and in how many minutes, then click `Add limit`. For example, 10 requests every 5 minutes.
- What a limit does: The server checks that source one series at a time, evenly spaced. One check of a series uses 2 requests, so 10 requests every 5 minutes means one series each minute.
- When it applies: At once. You do not need to restart the server.
- Changing a limit: Click the edit button on its row, or choose the same source again. The form loads its numbers. Change them and click `Save limit`.
- Removing a limit: Click the trash button on its row. The server then checks that source at full speed again.
- Limits set on the server: A limit from the `SCAN_SOURCE_RATE_LIMITS` environment variable is listed too. You can change it here, and your limit then replaces it. The trash button on that row removes your limit, and the server's limit applies again. You cannot remove the server's limit here.
- What a limit covers: Only the scheduled checks for new chapters. It does not slow down a reader who opens a chapter, or an `Add all` run on the Sources page.

## Extension updates

The Extension updates section keeps installed extensions up to date to prevent source breakages when external sites change:

- `Update all now`: Immediately starts a background pass on your server to update all installed extensions to their latest versions.
- `Automatic updates`: Displays whether scheduled updates are active. Scheduled updates are configured using `EXTENSION_AUTO_UPDATE_INTERVAL_SECONDS` on your server. When active, it displays the update interval.
- Run status and history: Displays the timestamp of the last update pass, how many extensions were updated, and any errors. Expand `Update history` to inspect recent package updates, versions, and failure messages.

## Maintenance jobs

Maintenance jobs operate across your server’s entire catalogue. Each job asks for confirmation before starting:

- `Save the whole catalogue`: Copies English series from the Suwayomi library into the database read-cache used by readers. Series details are persisted during the request, and chapter lists fill in the background.
- `Store covers on this server`: Crawls the catalogue, downloads missing cover images, and stores them locally in the database as WebP images. This allows readers to load covers directly from `/covers/{id}.webp`. It runs slowly in the background to avoid rate limits.
- `Merge duplicate works`: Reviews pending pairs, then scans groups of works sharing a title or alternative title (articles such as "the", "a", and "an" are ignored). The matching rules consider credits, descriptions, covers, native-script titles, years, and how specific a title is. A pair is merged automatically only when the rules approve it and exactly one target qualifies. Pairs that are clearly distinct works are closed. Unclear pairs remain queued in [Duplicates](/admin/duplicates) for manual review. Each batch examines up to 200 pending reviews and, once that queue is exhausted, up to 200 title groups. You can halt the run between batches with `Stop`. Merging permanently deletes the duplicate work record.

## About

The About section displays information about your server:

- `Server`: The backend API endpoint or local origin address.
- `Version`: The running server release version.
- `Signed in as`: The username of the currently authenticated administrator.
