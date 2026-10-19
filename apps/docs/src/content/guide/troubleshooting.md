---
title: Fix reader problems
description: Fix reader problems on the server you connect to, including pages, connections, accounts, and library items.
---

The reader connects to a komiq server on your network or the internet. The admin app runs it. This covers loading, connections, accounts and libraries. You can also open the global server at `komiq.cc` in a browser.

## Pages do not load

### The app itself fails to load

If an app file is unavailable, the reader reloads once in that tab. Another failure shows **Couldn’t load the app** and a **Reload** button. A successful start resets this automatic retry, so the reader can recover again after a later update. Check your connection, then choose Reload. If your browser blocks session storage, it shows the button immediately.

### Retry an individual page

If an image fails, komiq replaces it with an alert button showing the page number. Tap or click it to request the image URL again.

### Chapter has no available pages

**No pages available for this chapter** means the reader received no readable pages, possibly because a fetch failed. Try Previous chapter or Next chapter, or choose Back to series. Read only material you are authorised to access. See [Reading](/guide/reading).

### The chapter is hosted off-site

**This chapter is read on**, followed by a host name, means the publisher hosts the pages off-site. Komiq has no pages to show. Click **Read on** to open the publisher's chapter in a new tab.

### Extension chapters stay unavailable

If MangaDex series open but chapters from extension sources say they are unavailable, the server may be checking its source engine after a restore. Try again later. If it lasts, tell the server owner. The owner fixes it in the admin console, see [Source engine state](/admin/engine).

After such a change, reading progress your app saved while offline can be dropped. The app never puts it on a different chapter.

### Upstream source is offline or protected

Komiq retrieves comic pages from sources on demand. Source outages, slow responses, or browser-verification challenges can prevent pages from loading.

If you run the admin app with Docker:

- From `deploy/selfhost`, check that the Suwayomi content engine container is running and healthy by running `docker compose ps`.
- For sources that need FlareSolverr to handle browser-verification pages, set `COMPOSE_PROFILES=flaresolverr` and `FLARESOLVERR_ENABLED=true` in `deploy/selfhost/.env`, then run `docker compose up -d` from `deploy/selfhost`. For setup instructions, see [Docker install](/self-host/docker).

## Cannot connect

If the native reader app cannot connect, start with [Connect to a server](/guide/connect). It covers local network and internet servers, address formats, HTTP and HTTPS selection, and every connection error.

### Verify the server address

On the Connect to a server screen:

- Enter the server IP and port, such as `192.168.1.20:8080`. Docker defaults to `8080`; desktop defaults to `7580`. On desktop, turn on **Allow other devices** and use its displayed address.
- For an internet server, enter its domain, such as `komiq.example.com`, or paste its link.
- For a server on the same computer, an address such as `localhost:8080` works. On a phone, `localhost` refers to the phone.
- On phones, tap Scan a QR code to read the connection code displayed at the end of setup or in the admin console under Settings, then Access.

### The global server address does not connect

For now, the global server accepts requests from its website alone. Native apps show "No komiq server answered at this address." for its address. Open `komiq.cc` in a browser, or connect the app to a self-hosted server. See [Choose your setup](/start/choose-your-setup).

### Check network and firewall settings

Check network isolation:

- For a server reachable only on your home network, check that your phone or reading device is connected to the same local Wi-Fi network as the server. Mobile data or an isolated guest Wi-Fi network may not reach it.
- Check the firewall on the host machine. If you run Linux with UFW, allow incoming traffic on your configured port. For the default port:

```sh
sudo ufw allow 8080/tcp
```

- The app accepts HTTP and HTTPS. Without a prefix, IP addresses and addresses with a port use HTTP; plain domains use HTTPS. If your server uses HTTPS on an explicit port, include `https://`. For internet setup, follow [Internet access](/self-host/internet-access).

### Check server health

The native app checks `/health` with an 8-second timeout, then queries instance information. If your Docker server does not answer, inspect it from `deploy/selfhost`:

```sh
docker compose ps
docker compose logs server
```

Check both `server` and `suwayomi`. Setup can work while the engine is still starting.

### The server does not allow the apps

A server answers the apps only when `CORS_ORIGINS` lists the origins the apps send: `tauri://localhost`, `http://tauri.localhost` and `https://tauri.localhost`. The self-host Docker image and the admin desktop app set these. If you set `CORS_ORIGINS` yourself, keep them in the list, or the app reports that no komiq server answered. See [Configuration](/self-host/configuration).

## Updates do not load

The Updates page shows two different messages:

- "Couldn't load updates": the server did not answer. Check your connection and select **Retry**.
- "No updates here": the server answered, and nothing matches the format or tab you chose. Select **Reset filters**.

## Cannot sign in

### Setup code is not a login password

The temporary setup code in the server log registers the initial owner at `/admin/setup`. It is not a password for `/login`. Sign in with the account you created during setup.

### Public registration is closed

When the server owner closes registration, the login page hides Create account. Ask the owner to create an account in the admin console.

### Check username and password requirements

Passwords are case-sensitive. When creating an account, passwords must be at least 8 characters long.

### Required password change

If an administrator created your account with a temporary password or initiated a credential reset, komiq presents a Choose a new password prompt upon sign-in. You must choose a new password before you can proceed to the reader.

### The server asks for a security check

Some servers ask for a security check when you sign in, create an account, change your password, comment, rate or report. The desktop and phone apps cannot show it. They say "This server asks for a security check that only its website can show." Do these things on the server's website in a browser.

If the website says "The security check could not be confirmed right now", try again in a moment. If it lasts, tell the server owner: the server's Turnstile secret, the key it uses to confirm the check with Cloudflare, may be wrong, or the server cannot reach Cloudflare.

### Expired session tokens

Komiq stores a session token in browser local storage. This credential keeps you signed in. If it expires or the server invalidates it, authenticated actions fail. Open `/login` and sign in again. See [Accounts](/guide/accounts).

## Library looks empty

### New server catalogue has no sources

A fresh server has no series until its owner configures sources. The owner can enable MangaDex under Settings, then Sources, or add an extension repository and install a source. Series appear after fetching or scanning.

### Active shelf and search filters

Filters may hide library titles:

- Check the active shelf tab. If you have selected Plan to Read or Completed, the view hides series assigned to other shelves. Click All to view every series.
- Clear the Filter by title search box.
- Reset Format to All formats. Clear filters resets title and format, but keeps the selected shelf.

### Check your server and user account

Libraries and progress belong to an account on one server. Check the server address and account icon. Switching servers clears the previous sign-in session. Guests and other accounts cannot see your library. See [Library](/guide/library).
