---
title: Connect to a server
description: Connect the reader app to a server on your network or on the internet, choose who fetches chapters, and fix connection errors.
---

The reader app connects to a komiq server; the admin app runs and manages one. Native desktop and mobile reader apps can choose a self-hosted komiq server, on your network or on the internet. This is the main guide to entering or scanning its address, switching servers, and choosing who fetches chapters.

## Choose a server

- **Global server**: the project's free, work-in-progress demo at `komiq.cc`. Open it in a browser. For now, the apps cannot connect to it: its API accepts requests from the `komiq.cc` website alone. Do not enter its address on the **Connect to a server** screen. The project may open it to the apps later.
- **Local network**: an admin app running on your computer or home server. Use its network IP address and port, such as `192.168.1.20:8080`. For an admin desktop app, turn on **Allow other devices** and use the address it shows.
- **Internet**: an admin app reached through a domain, or straight at its public address without a domain. Use the address the owner gives you, such as `komiq.example.com` or `203.0.113.5:8080`. An address without a domain uses plain HTTP, which is not encrypted.

For self-hosting choices, see [Choose your setup](/start/choose-your-setup).

## When the screen appears

The **Connect to a server** screen opens on first launch if no address is saved or built in. It also opens when you select **Change server** on `/login` or the server setup screen.

Browser builds do not display this screen. A separately deployed web reader uses the server address configured at build time. An embedded web reader connects to `/graphql` on the address serving it. Open that server's address in your browser.

## Entering an address

Enter an address in any of these formats:

- An IP address and port, such as `192.168.1.20:8080`.
- A local computer address, such as `localhost:8080`, for a server on this device.
- A public domain name, such as `komiq.example.com`.
- A link copied from your browser, such as `https://komiq.example.com/admin/setup`.

Komiq tests the origin: the protocol, hostname, and port. Without a protocol prefix, it selects one:

- Addresses that specify an explicit port, a numeric IP address, `localhost`, or a `.local` hostname receive `http://`.
- Plain domain names without ports receive `https://`.
- An explicit `http://` or `https://` prefix is kept. If your domain uses HTTPS on an explicit port, include `https://`.
- Any subpage paths, query parameters, or trailing slashes are removed so that only the base origin remains.

After entering your address, select **Connect**.

## Scanning a QR code

Phones show **Scan a QR code**. Desktop builds do not.

### How to scan

1. Open the komiq admin console in a web browser on another computer or tablet.
2. Locate the QR code displayed at the end of the initial setup wizard, or open **Settings › Access** in the admin navigation. If no QR code is shown, open the admin console using the network address that other devices use, rather than `localhost`.
3. In the mobile app, tap **Scan a QR code**.
4. If your phone asks for camera permission, allow it.
5. The application turns the screen transparent and shows the camera view. Point your camera frame at the QR code displayed on the screen.
6. Once recognized, the camera closes and the scanned address appears in the server address field with the note "Scanned. Check that this is your server, then tap Connect."
7. Confirm that the address points to your server, then tap **Connect**.

The app does not connect automatically upon scanning. This lets you inspect the address before connecting, since connecting signs the device out of any existing server session.

## Who gets the chapters

The app checks whether it can fetch chapters itself. The desktop app can. An Android app can when it was built with the on-device engine. If supported, **Who gets the chapters?** offers two choices:

- **This device**: the app uses its embedded Suwayomi engine to download chapter pages directly from sources to your device.
- **The server**: the komiq server you connected to retrieves chapter pages from sources and relays them to the application.

A server owner can tell apps to fetch for themselves, with the server setting `KOMIKA_APPS_FETCH_THEMSELVES` (see [Configuration](/self-host/configuration)). On such a server, **This device** is selected, **The server** is greyed out and cannot be chosen, and the screen shows the note "This server does not fetch chapters for apps."

With **This device**, the server names the source extension that a chapter needs, and the app installs it. An extension is code that runs on your device, so the app asks first: "Install an extension?", with the extension's name and its repository. **Allow** applies to that repository on that server and is remembered. **Not now** applies until you close the app; the server then gets those pages for you. Allow only if you trust the person who runs the server. The app's own MangaDex extension installs without a question.

The iOS app, and an Android app built without the engine, skip this screen. Your server gets the chapters for them.

On a phone the engine needs some seconds to start after you open the app. Until it is ready, and whenever it cannot answer, the server gets the pages.

You can change this choice later by opening **Settings › Reading** and using **Fetch chapters on this device**. The app reloads to apply it. On a server that tells apps to fetch for themselves, that switch stays on.

## Switching servers

1. If you are signed in, open the account menu in the navigation bar and select **Sign out**. Then visit `/login`.
2. On the sign-in screen, select **Change server**.
3. Enter or scan the address of the new server.
4. Select **Connect**.

Switching servers removes your session token, the credential used to stay signed in. This keeps the previous server's credentials from being sent to the new one. The app saves the new fetching choice and restarts at `/login`.

## Connection errors

If the application cannot establish a connection, it displays an error message below the address field:

- **Invalid address:** "Enter an address like 192.168.1.20:8080 or komiq.example.com." The input was empty, could not be parsed as a URL, or used a protocol other than HTTP or HTTPS.
- **Server unreachable:** "No komiq server answered at this address. Check that it is running and that this device can reach it." The `/health` request failed, returned an unsuccessful status, or timed out after 8 seconds. Confirm the server is running, check your network connection, and verify that your firewall allows traffic on the configured port. You also get this message when the server does not accept requests from the apps. The global server is one such server: read on it at `komiq.cc` in a browser. For your own server, see `CORS_ORIGINS` in [Configuration](/self-host/configuration).
- **Not a compatible server:** "This address answers, but not as a komiq server this app can use. Update the server, or check the address." The server answered the health check, but the GraphQL instance facts query failed. Verify the address points to a komiq server and update the server if needed.
- **Check failed:** "The server could not be checked." This is the fallback if the check fails without an `Error` object. Check the address and try again.
- **Camera permission denied:** "komiq is not allowed to use the camera. Allow it in the device settings, or type the address." Camera access was denied. Open device settings to grant permission, or type the address manually.
- **Camera hardware error:** "The camera could not be opened. Type the address instead." The camera failed to start. Enter the address manually.
- **Unrecognized QR code:** "That QR code is not a server address. Scan the one komiq shows in the admin." The scanned code did not contain a valid server address.

For additional network help, see [Connect other devices](/self-host/other-devices) and [Troubleshooting](/guide/troubleshooting). To continue setting up your account, see [Accounts and profiles](/guide/accounts).
