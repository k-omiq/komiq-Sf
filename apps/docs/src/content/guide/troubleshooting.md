---
title: Fix reader problems
description: Fix reader problems on the server you connect to, including pages, connections, accounts, and library items.
---

The reader app reads from the komiq server you connect to: a local network server or one on the internet. In a browser, you can also read on the global server at `komiq.cc`. The admin app runs and manages that server. This guide helps you fix pages, connections, sign-in, and library problems.

## Pages do not load

### Retry an individual page

If an image fails, komiq replaces it with an alert button showing the page number. Tap or click it to request the image URL again.

### Chapter has no available pages

**No pages available for this chapter** means the reader received no readable pages. A page-fetch failure can also cause it.

To continue reading:

1. Click Previous chapter or Next chapter when available to open an adjacent chapter.
2. Click Back to series to return to the chapter list.
3. Read only material you are authorised to access. For reader controls, see [Reading](/guide/reading).

### The chapter is hosted off-site

**This chapter is read on**, followed by a host name, means the publisher hosts the pages off-site. Komiq has no pages to show. Click **Read on** to open the publisher's chapter in a new tab.

### Upstream source is offline or protected

Komiq retrieves comic pages from sources on demand. Source outages, slow responses, or browser-verification challenges can prevent pages from loading.

If you run the admin app with Docker:

- From `deploy/selfhost`, check that the Suwayomi content engine container is running and healthy by running `docker compose ps`.
- For sources that need FlareSolverr to handle browser-verification pages, set `COMPOSE_PROFILES=flaresolverr` and `FLARESOLVERR_ENABLED=true` in `deploy/selfhost/.env`, then run `docker compose up -d` from `deploy/selfhost`. For setup instructions, see [Docker install](/self-host/docker).

## Cannot connect

If the native reader app cannot connect, start with [Connect to a server](/guide/connect). It covers local network and internet servers, address formats, HTTP and HTTPS selection, and every connection error.

### Verify the server address

On the Connect to a server screen:

- For a server on your local network, enter its IP address and configured port, such as `192.168.1.20:8080`. Docker defaults to `8080`; the admin desktop app defaults to `7580`. Turn on **Allow other devices** in the admin desktop app and use the address it shows.
- For a server on the internet, enter its domain without spaces, such as `komiq.example.com`, or paste a link to it.
- For a server on the same computer, an address such as `localhost:8080` works. On a phone, `localhost` refers to the phone.
- On phones, tap Scan a QR code to read the connection code displayed at the end of setup or in the admin console under Settings, then Access.

### The global server address does not connect

For now, the apps cannot connect to the global server. Its API accepts requests from the `komiq.cc` website alone, so the app shows "No komiq server answered at this address." for `komiq.cc` and `api.komiq.cc`. This is on purpose: an API open to every app is open to abuse. The project may open the global server to the apps later, if enough people ask for it.

- To read on the global server, open `komiq.cc` in a browser.
- To use the app, connect it to a server you or someone else self-hosts. See [Choose your setup](/start/choose-your-setup).

### Check network and firewall settings

Connection errors frequently stem from network isolation:

- For a server reachable only on your home network, check that your phone or reading device is connected to the same local Wi-Fi network as the server. Mobile data or an isolated guest Wi-Fi network may not reach it.
- Check the firewall on the host machine. If you run Linux with UFW, allow incoming traffic on your configured port. For the default port:

```sh
sudo ufw allow 8080/tcp
```

- The app accepts HTTP and HTTPS. Without a prefix, IP addresses and addresses with a port use HTTP; plain domains use HTTPS. If your server uses HTTPS on an explicit port, include `https://`. For internet setup, follow [Internet access](/self-host/internet-access).

### Check server health

The native app sends a request to the server `/health` endpoint with an 8-second timeout, then checks the server's instance information. If you run the admin app with Docker and the health request fails or returns a response outside the successful 200 to 299 range, inspect the containers from `deploy/selfhost`:

```sh
docker compose ps
docker compose logs server
```

For chapter reading through Suwayomi, check that the `server` and `suwayomi` services are healthy. The setup screens can work while the engine is still starting.

### The server does not allow the apps

A server answers the apps only when `CORS_ORIGINS` lists the origins the apps send: `tauri://localhost`, `http://tauri.localhost` and `https://tauri.localhost`. The self-host Docker image and the admin desktop app set these. If you set `CORS_ORIGINS` yourself, keep them in the list, or the app reports that no komiq server answered. See [Configuration](/self-host/configuration).

## Updates do not load

The Updates page shows two different messages:

- "Couldn't load updates": the server did not answer. Check your connection and select **Retry**.
- "No updates here": the server answered, and nothing matches the format or tab you chose. Select **Reset filters**.

## Cannot sign in

### Setup code is not a login password

During initial setup on a new server, the server log outputs a temporary setup code. This code is used only to register the initial owner account at `/admin/setup`.

The setup code cannot be entered on the `/login` screen. Sign in using the username and password you created during the setup wizard.

### Public registration is closed

Self-hosted servers allow the owner to disable public sign-ups. When registration is closed, the login page displays only the Sign in form and hides the Create account tab.

If you need an account on a private server, ask the server owner to create an account for you in the admin console.

### Check username and password requirements

Passwords are case-sensitive. When creating an account, passwords must be at least 8 characters long.

### Required password change

If an administrator created your account with a temporary password or initiated a credential reset, komiq presents a Choose a new password prompt upon sign-in. You must choose a new password before you can proceed to the reader.

### Expired session tokens

Komiq stores a session token in browser local storage. This credential keeps you signed in. If it expires or the server invalidates it, authenticated actions fail. Open `/login` and sign in again. See [Accounts](/guide/accounts).

## Library looks empty

### New server catalogue has no sources

On a fresh self-hosted installation, the catalogue contains no series until the owner configures sources. The library displays an empty state explaining that series appear once sources are added.

The server owner can enable MangaDex under Settings, then Sources, or add an extension repository and install a source. Once the server fetches or scans the configured sources, series become available.

### Active shelf and search filters

Filters may hide library titles:

- Check the active shelf tab. If you have selected Plan to Read or Completed, the view hides series assigned to other shelves. Click All to view every series.
- Clear the Filter by title search box.
- Check the Format dropdown and reset it to All formats. Click Clear filters to reset the title and format filters. This does not change the selected shelf.

### Check your server and user account

Library memberships, shelf choices, and reading progress are stored per user account on the server you connected to. Check that you are using the same server as before. Switching servers clears the previous sign-in session.

Guests and different accounts do not see your previous library items. Check the account icon in the header. For shelves, see [Library](/guide/library).
