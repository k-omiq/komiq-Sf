---
title: Add a regional engine
description: Fetch pages and covers through a second source engine over a private tunnel.
---

The admin app is your komiq server and its management portal. A regional engine is a second Suwayomi source engine in another region. It fetches pages and covers for sources you route to it; the main engine keeps the catalogue, scanning and chapter lists.

Use one when a source answers slowly or refuses connections from your server's region. Reader apps still connect to your komiq server.

## Set up a private tunnel

Reach the regional engine only through WireGuard or an SSH tunnel. Never expose its port to the internet. Its API can install extensions, which run code, so turn on HTTP Basic authentication too: every request must carry a user name and password.

For WireGuard, this is an address sketch for two peers:

```text
Main host address:          10.8.0.1/32
Main host peer AllowedIPs:  10.8.0.2/32
Regional host address:     10.8.0.2/32
Regional peer AllowedIPs:  10.8.0.1/32
```

Configure peer keys and the tunnel endpoint yourself. Bring up the tunnel before starting the container. Allow the komiq host to reach port 4567 through it. Keep that port closed on public interfaces.

## Start the regional engine

On the regional host, save this as `compose.yaml`. It uses the engine image pinned in `deploy/selfhost/docker-compose.yml`, with its own data volume. The published port binds only to the regional tunnel address.

```yaml
services:
  engine:
    image: ghcr.io/suwayomi/tachidesk@sha256:e59f212ccf91b26de8676a063a2db90256c148c6261535c7c477d47a43d9751b
    restart: unless-stopped
    environment:
      BIND_IP: 0.0.0.0
      BIND_PORT: '4567'
      WEB_UI_ENABLED: 'false'
      AUTH_MODE: basic_auth
      AUTH_USERNAME: komika
      AUTH_PASSWORD_FILE: /run/secrets/engine-password
    ports:
      - '10.8.0.2:4567:4567'
    volumes:
      - regional-engine-data:/home/suwayomi/.local/share/Tachidesk
      - ./engine-password:/run/secrets/engine-password:ro

volumes:
  regional-engine-data:
```

Create `engine-password` beside it before starting. It holds only the password, on one line. Use a long random secret. Keep the file private, but let the container's user read it. The engine image writes these values into its own `server.conf` at every start.

Start it with `docker compose up -d`. Keep its volume separate from the main engine's data.

## Connect your komiq server

Set these variables in the komiq server process:

```sh
KOMIKA_ENGINES=sg=http://10.8.0.2:4567
KOMIKA_ENGINE_SG_AUTH_FILE=/run/secrets/engine-sg
SUWAYOMI_PUBLIC_URL=https://komiq.example.com
```

Replace the public URL with your komiq server's address. You can use `/` for same-origin links. This setting makes page and cover links go through komiq, where routing happens. Keep `SUWAYOMI_URL` pointing to the main engine.

The auth file holds one `user:password` line matching the regional engine: `komika:` followed by the password from `engine-password`. Keep it private. With Docker, pass the variables to the server service and mount the file at the configured path. Restart the komiq server after changing its engine configuration.

Engine ids contain 1 to 32 characters: `a-z`, `0-9` and `-`. Start with a letter or digit. `owner` is reserved for the main engine. Separate multiple `id=url` entries with commas. Auth variable names uppercase the id and replace hyphens with underscores. Credentials belong in files, never in URLs.

## Provision and route a source

Run this in the komiq server environment, replacing `SOURCE_ID` with the source's id. You can pass several ids separated by spaces.

```sh
komika-server engine provision sg SOURCE_ID
```

The command adds the extension repository recorded by the main engine and installs or updates its extension. It reports when versions still differ. It cannot select an older repository version. If the regional engine is newer, update the main engine. Routing stays off until the package and known version match.

Open **Sources › Health** to check **Engines**, then open **Sources › Browse**, choose a source and select `sg` in its **Engine** picker. See [Extensions and source health](/admin/sources) for refusal reasons. Provisioning alone does not create a route.

## Failures and undo

If the regional engine is down, its extension version differs, or a regional fetch is missing or fails, the main engine serves instead. A strict route returns HTTP 503, meaning temporarily unavailable, instead of using the main engine. A full regional page pool, or a spent [request limit](/admin/settings#request-limits) for the source, returns a retryable 503 in either mode.

To undo routing, choose the main engine (`owner`) for each routed source. This removes the routes. Remove engine configuration only after removing its routes, especially strict ones.

The database stores the route label, such as `sg`. Regional manga and chapter numeric ids stay in server memory and are never stored in PostgreSQL or sent to readers.
