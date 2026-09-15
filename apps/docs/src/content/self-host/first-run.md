---
title: First-run setup
description: Set up your own server in the admin app, create an owner account and choose access and sources.
---

Set up your own server after you host the admin app on your computer, home server or VPS. A fresh installation has no accounts and no sources. Setup creates your owner account and asks how the reader app connects and which sources your server uses. You do it in the setup wizard or in a terminal.

## The setup code

Your server makes a one-time setup code while it has no owner: 12 characters in three groups, `XXXX-XXXX-XXXX`. It proves that you control the machine.

You rarely type it. The desktop admin app passes it to the wizard when you choose "Run komiq on this computer". The Docker installer, `deploy/selfhost/install.sh`, reads it from the server log and passes it to the terminal setup. If you started Docker by hand, read it from the log:

```sh
docker compose logs server | grep "setup code" | tail -n 1
```

The server prints the code once, at startup. A restart before setup is done makes a new code. Dashes, spaces and letter case do not matter when you type it.

## Open the setup wizard

The desktop admin app opens setup when your new server is ready. With Docker, open a browser:

- On the server itself: `http://localhost:8080/admin/setup`.
- From another device on the same network: `http://192.168.1.20:8080/admin/setup`.

Over plain HTTP from the internet, for example `http://203.0.113.5:8080/admin/setup` on a VPS, the **Owner** step shows no form: the connection is not encrypted, so the server refuses the owner password. Use the terminal setup.

The steps after **Access** depend on your choice. The wizard saves progress in the browser, without passwords or tokens, so you can resume later.

## Set up from a terminal

A server with no screen can do the same setup over SSH, while the server runs. The Docker installer starts this command for you:

```sh
docker compose exec server komika-server setup
```

It asks for the owner account, then **Access**, **Accounts** and **Sources**, as below. Each answer is applied when you give it. Run it again, without a setup code, to continue or to change a choice. It then asks you to sign in as the owner. A setup code given to a server that already has an owner changes nothing, and the command says so.

| Option                  | What it does                                                                       |
| ----------------------- | ---------------------------------------------------------------------------------- |
| `--setup-code <code>`   | Gives the setup code. So does the `KOMIKA_SETUP_CODE` environment variable.        |
| `--access <choice>`     | Skips the access question: `local`, `internet`, `tunnel`, `public-ip` or `direct`. |
| `--encrypt yes` or `no` | With `--access direct`: answers the encryption question.                           |
| `--server <address>`    | Where your server answers. Default: `http://127.0.0.1:<PORT>`.                     |

Outside Docker, run `komika-server setup` on your server's machine. It refuses a plain `http://` address on the internet before it asks for a password.

## Welcome

Read the overview and click "Get started".

## Owner account

- What it asks: A username (at least 3 characters), an email address, a password (at least 8 characters, typed twice) and the setup code.
- Why: The owner manages settings, users and sources. This form creates the first administrator and signs you in.

## Access

- What it asks: "Only my devices", "Public on the internet" or "Public without a domain". The desktop app running its own server offers the first two, then **Home network**.
- Why:
  - "Only my devices" is for reading at home. The server then refuses plain HTTP from the internet. The step warns you if this console is open at a public address.
  - "Public on the internet" uses a domain of yours with Cloudflare in front. It adds **Internet**.
  - "Public without a domain" is for a server with a public address, such as a cloud VPS. It adds **Address**.

## Home network

This step appears when the desktop admin app runs your server. With "Public on the internet", devices at home can then connect without your domain.

- What it asks: Turn on **Allow other devices** so other devices on the same network can connect.
- Why: The desktop server starts on loopback, an address only this computer can reach. The switch restarts it on all network interfaces and shows its addresses and a Quick Response (QR) code.

The step then waits, and shows "A device on your network reached komiq." when one connects. If none does, open **A device cannot connect?** for help with the firewall, the program that blocks incoming connections. Read [Connect your devices](/self-host/other-devices).

## Internet

This step appears if you chose "Public on the internet".

- What it asks: Your domain, the connection (Cloudflare Tunnel or Public IP) and a Cloudflare API token. The desktop app running its own server always uses a tunnel. **I will do it by hand** shows steps for the Cloudflare dashboard instead. **Set up later** moves on without it: the Done step says that internet access is not set up yet, and you finish in **Settings › Access**. A setup that was turned off shows **Turn on**.
- Why: Cloudflare sits in front of your server and caches covers and pages. The step lists what you do once (an account, your domain, a token) and what komiq does. It says whether the tunnel connector is on your server or will be downloaded, or whether port 443 is free. Read [Reach it from the internet](/self-host/internet-access).

## Address

This step appears if you chose "Public without a domain".

- What it asks: Your server's public IP address, such as `203.0.113.5`, and whether to encrypt the connection. Encryption is on by default. **Set up later** works as on **Internet**.
- Why: With encryption, your server gets a free Let's Encrypt certificate and serves HTTPS. Open ports 80 and 443 in your cloud firewall first. The step says whether they are free. Without encryption the field holds the address and port, such as `203.0.113.5:8080`, and people on the network path can see passwords and pages. With Docker, the step names the installer command to run first.

## Accounts

- What it asks: "Only people I add" keeps sign-up closed. "Anyone can sign up" lets visitors register on the web reader. You can also add people now, with a username and an email address.
- Why: Each account has its own library. A person you add gets a temporary password, which you share, and chooses a new one at the first sign-in.

## Sources

- What it asks: A MangaDex switch, an optional extension repo index address, and extensions to install from it. "Skip" leaves sources for later.
- Why: With MangaDex off, the server never contacts MangaDex. If the source engine is still starting, add sources later in the admin console. Read [Add sources](/self-host/sources).

## Done

- What it asks: Review the summary: reader address, owner, sign-up mode, sources and people added.
- Why: If other devices can reach the address, the screen shows a QR code for a phone camera or the reader app. "Open the catalog" or "Add your first source" finishes setup.

Next: [Add sources](/self-host/sources).
