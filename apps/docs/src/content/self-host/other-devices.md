---
title: Connect your devices
description: Connect the reader app to your own server on the same network with an IP address and port or QR code.
---

Keep the admin app running your server on your computer or home server, and read on other devices on the same network. In the reader app, open "Connect to a server" and enter your server’s IP address and port, paste its link or scan its QR code. This page covers the local address, connection checks and firewall settings.

## Open komiq on your local network

With Docker, `KOMIQ_BIND=0.0.0.0` publishes the server port on all network interfaces. It is the default, and what the installer writes when you answer yes to its home-network question. Devices on your local area network (LAN) can connect if your firewall allows them.

The desktop admin app starts your server on loopback, an address only that computer can reach. Turn on **Allow other devices** on the **Home network** step of setup, or in **Settings › Access**. Use the address and port it displays; the desktop server starts with port `7580` and chooses another if it is occupied. The step then confirms when a device on your network reached your server.

For Docker’s default port, type `192.168.1.20:8080` in the reader app’s **Connect to a server** screen, using your server’s IP address. To use the web reader, open the same address in a browser:

```text
http://192.168.1.20:8080
```

To find your server's local IP address, run the appropriate command on your server:

- Linux: Run `hostname -I` in your terminal. Use the first IP address in the list.
- macOS: Run `ipconfig getifaddr en0` for Ethernet, or `ipconfig getifaddr en1` for Wi-Fi.
- Windows: Run `ipconfig` in Command Prompt. Look for the IPv4 Address under your active network adapter.

> [!NOTE]
> Your reading device and your server must be connected to the same local network. A phone on cellular data or a guest Wi-Fi network cannot reach a local address.

## The connection QR code

Komiq shows a Quick Response (QR) code of your server address in two places:

- On the final "Done" screen of the first-run setup wizard.
- In the admin console under **Settings › Access**.

With Docker, open the admin console using your server's local network IP address, such as `http://192.168.1.20:8080`, to display a QR code that other devices can use. If you open the console at `http://localhost:8080`, no QR code is shown: other devices cannot reach loopback, and a server in a container cannot see the address of the machine it runs on. A server that runs outside a container finds its home network address and shows that address and its QR code, even when the console is open at `localhost`. The desktop admin app does the same when **Allow other devices** is on.

A phone camera opens the web reader from the code. The reader app scans it on its connection screen.

## The native apps' connect screen

The komiq native reader apps for Android and iOS open to a "Connect to a server" screen on first launch. If you already signed in, you can reach this screen at any time by selecting "Change server" on the sign-in page.

### Enter the address manually

Type your server address: an IP address with a port (such as `192.168.1.20:8080`) or a domain name. You can also paste a server link; the app removes the path.

You do not need to type the protocol prefix:

- The app adds `http://` to an address with a port number, to `localhost` and to `.local` hostnames.
- For an IP address without a port, the app tries `https://` first. For a home network address it then tries `http://`. For a public address it does not: type `http://` or the port yourself.
- The app adds `https://` to domain names.

### Scan the QR code

On phone builds, the screen includes a "Scan a QR code" button below the connection form.

1. Tap "Scan a QR code". If prompted, grant camera permissions to the app.
2. Align the camera viewfinder with the QR code displayed in your admin console.
3. The app scans the code and populates the server address field.
4. Review the address, then tap "Connect".

You confirm the address before the app connects, because connecting replaces your active server session.

### Connection check and chapter fetching

When you tap "Connect", the app verifies the server:

1. It sends a health check request to `/health` at that address with an eight-second timeout.
2. It queries the GraphQL endpoint to verify that the server is running a compatible version of komiq.

If the check succeeds, the app connects and opens the sign-in screen.

The desktop reader app has its own source engine, and an Android app can be built with one. Such an app shows a follow-up screen titled "Who gets the chapters?". Select "This device" to fetch chapter pages on that device, or "The server" to let your server fetch them. For every other app, the server fetches chapters from sources. If you set `KOMIKA_APPS_FETCH_THEMSELVES=on` on your server, "The server" is greyed out for those apps. See [Configuration](/self-host/configuration).

## Firewall fixes

If a device on the same Wi-Fi network cannot open the server address, your server's firewall may block the server’s port. The examples below use Docker’s default port `8080`; use the port the desktop app shows or the one you configured.

Two tools can add the rule for you. The Docker installer offers to open the port when ufw or firewalld is active. In the desktop admin app, the **Home network** step has a help section, **A device cannot connect?**. On Windows, and on Linux with `pkexec` and ufw or firewalld, its **Open the firewall for komiq** button adds the rule after your system asks for permission.

### Linux with UFW

If your server uses Uncomplicated Firewall (UFW), allow incoming TCP traffic on port 8080:

```sh
sudo ufw allow 8080/tcp
```

### Linux with Firewalld

If your server uses Firewalld, open port 8080 and reload the active configuration:

```sh
sudo firewall-cmd --add-port=8080/tcp --permanent
sudo firewall-cmd --reload
```

### macOS

Open System Settings › Network › Firewall. If the firewall is turned on, click Options and ensure that incoming connections are allowed for Docker or your terminal application. For the desktop admin app, macOS asks by itself whether komiq may accept incoming connections. Choose Allow.

### Windows

Open Windows Defender Firewall with Advanced Security. In the left panel, click Inbound Rules, then click New Rule in the right panel. Select Port, choose TCP, specify port `8080`, and select "Allow the connection".

### Router isolation settings

Many Wi-Fi routers have a feature named "AP isolation" or "client isolation". It stops wireless devices from talking to other devices on the same network. If your firewall rules are correct but your phone still cannot reach the server, turn it off in your router settings.

Next: [Reach it from the internet](/self-host/internet-access).
