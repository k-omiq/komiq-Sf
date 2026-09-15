# Cloudflare in front of a self-hosted komiq

The setup wizard (step "Internet") and Settings › Access can put your server on a domain
with Cloudflare in front of it. You give a domain, a connection type and a Cloudflare API
token; the server runs a checklist and shows each line as it goes. Nothing runs until an
owner starts it.

## What you do and what komiq does

You do three things once, at Cloudflare. The setup form and the terminal setup list them:

1. Have a Cloudflare account. It is free.
2. Add your domain to Cloudflare and point its nameservers there. You change the
   nameservers where you bought the domain; Cloudflare shows the steps.
3. Create an API token and paste it into setup. See [The API token](#the-api-token).

komiq does the rest: it gets the connector if the server has none, creates the tunnel or
the certificate, the DNS record and the cache rules, and tests the address. Each line is
in [What setup does](#what-setup-does-and-what-remove-undoes).

Before you start, the form says what the server found on its own machine: whether
`cloudflared` is there or will be downloaded (Tunnel), and whether port 443 is free,
used by another program, or one the server has no permission to open (Public IP). In a
container it adds that port 443 must be published too, and names the installer that does
it. The terminal setup prints the same checks.

## Connection types

| Type                 | How traffic arrives                                                                                                        | Needs                                                                                                         |
| -------------------- | -------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| **Tunnel** (default) | The server runs `cloudflared`, which connects out to Cloudflare. No open ports.                                            | Nothing to install on the systems listed under [The connector](#the-connector-cloudflared).                   |
| **Public IP**        | A proxied A/AAAA record points at the server. The server serves HTTPS on port 443 with a Cloudflare Origin CA certificate. | Port 443 reachable from Cloudflare (`KOMIKA_HTTPS_PORT` to change what it binds). With Docker, published too. |

With a tunnel, set `BIND_ADDR=127.0.0.1` if nothing else on the network should reach the
plain HTTP port. With a public IP, firewall the plain HTTP port. With Docker, the
installer (`deploy/selfhost/install.sh`) covers this: for `tunnel` and `public-ip` it
keeps the plain HTTP port on the machine itself, unless you answer yes to its
home-network question, and for `public-ip` it publishes port 443. See
[self-host.md](./self-host.md).

The desktop app that runs its own server has no connection choice. It uses a tunnel: that
server answers on its own computer, and cannot take port 443 from the internet.

## The connector (cloudflared)

A tunnel needs the `cloudflared` program. The server looks for it in this order:

1. `CLOUDFLARED_BIN`, when it is set. A path that does not exist is an error, and the
   server never downloads anything.
2. The server's `PATH`. The Docker image has `cloudflared` there.
3. Its own tool folder: `bin/` inside the Cloudflare folder (`cloudflare/` next to the
   database, or `KOMIKA_CLOUDFLARE_DIR`).

When it finds none, tunnel setup downloads `cloudflared` 2026.9.3 from Cloudflare's
releases on GitHub, checks the file's SHA-256 against the value built into the server,
and saves it in the tool folder. A file that fails the check is not saved. This is the
checklist line "Get the connector (cloudflared)", the first line after the token and
zone checks. The desktop app gets its connector this way when the computer has no
`cloudflared` installed.

There is a build for Linux (x86-64 and ARM64), macOS (x86-64 and ARM64) and Windows
(x86-64). On another system the line fails and says to install `cloudflared` and set
`CLOUDFLARED_BIN`.

## The API token

The form's **Open the token page** button, and the link the terminal setup prints, open
Cloudflare's **Create Custom Token** page with the token named `komiq` and some rows
filled in. You add the other rows on the same page:

| Scope   | Permission                                                    | Level | Filled in by the link |
| ------- | ------------------------------------------------------------- | ----- | --------------------- |
| Account | Cloudflare Tunnel _(Tunnel only)_                             | Edit  | no, add it            |
| Zone    | SSL and Certificates _(Public IP only)_                       | Edit  | yes                   |
| Zone    | Zone                                                          | Read  | yes                   |
| Zone    | DNS                                                           | Edit  | yes                   |
| Zone    | Zone Settings                                                 | Edit  | yes                   |
| Zone    | Cache Rules                                                   | Edit  | no, add it            |
| Account | Account Rulesets _(listed by Cloudflare's Cache Rules guide)_ | Edit  | no, add it            |
| Account | Account Filter Lists _(listed by the same guide)_             | Edit  | no, add it            |

Zone Resources: Include › Specific zone › your domain. Account Resources: your account.

Without the link, create the same token at **My Profile › API Tokens › Create Token ›
Create Custom Token**.

**Check token** verifies the token and finds the zone for the domain. It also reads a
few things with the token and lists the rows the token seems to lack ("This token may
lack: …"). It looks at DNS, Zone Settings, Cache Rules, and Cloudflare Tunnel or SSL and
Certificates. This is advice only. A read cannot prove an Edit permission, and the list
never blocks setup. If a step fails later, add the row to the token and retry.

The token is used for one run and then dropped. It is never written to the database, a
file or the log. Re-running or removing the setup asks for it again.

## What setup does (and what "Remove" undoes)

| Step                       | Cloudflare API                                                                                                                                                                                                     | Undone by "Remove"                                                                      |
| -------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------- |
| Check the token            | `GET /user/tokens/verify`                                                                                                                                                                                          | nothing                                                                                 |
| Find the zone              | `GET /zones?name=` (the domain, then each parent)                                                                                                                                                                  | nothing                                                                                 |
| Get the connector (Tunnel) | none. The server finds `cloudflared`, or downloads the pinned release and checks its SHA-256 (see [The connector](#the-connector-cloudflared))                                                                     | nothing. A downloaded file stays in the tool folder                                     |
| Tunnel                     | `POST /accounts/{a}/cfd_tunnel` (`config_src: cloudflare`), `PUT …/{t}/configurations` (domain → `http://127.0.0.1:<PORT>`), `GET …/{t}/token`                                                                     | `DELETE …/{t}/connections`, `DELETE /accounts/{a}/cfd_tunnel/{t}`                       |
| Connector (Tunnel)         | runs `cloudflared tunnel --no-autoupdate run` with `TUNNEL_TOKEN` in its environment, restarted if it exits, started again at every boot unless setup is turned off                                                | stopped, token file deleted                                                             |
| Origin certificate (IP)    | `POST /certificates` (`origin-ecc`, 15 years, CSR made on the server)                                                                                                                                              | `DELETE /certificates/{id}`, key and certificate files deleted                          |
| SSL mode (IP)              | `PATCH /zones/{z}/settings/ssl` → `strict` (zone-wide)                                                                                                                                                             | the previous value is put back                                                          |
| Visitor addresses          | `GET /ips` (IP). The server trusts loopback (Tunnel) or Cloudflare's ranges (IP) for `CF-Connecting-IP`; `TRUSTED_PROXY_CIDRS` in env wins                                                                         | cleared                                                                                 |
| DNS record                 | `GET /zones/{z}/dns_records?name.exact=`, `POST /zones/{z}/dns_records` (proxied CNAME to `<tunnel>.cfargotunnel.com`, or A/AAAA). On a later run, `PATCH /zones/{z}/dns_records/{id}` corrects setup's own record | `DELETE /zones/{z}/dns_records/{id}`                                                    |
| Cache rules                | `GET /zones/{z}/rulesets/phases/http_request_cache_settings/entrypoint`, then `POST /zones/{z}/rulesets/{r}/rules` (or `POST /zones/{z}/rulesets` when the zone has none)                                          | `DELETE …/rulesets/{r}/rules/{id}`; the ruleset too if setup created it and it is empty |
| Respect cache headers      | `PATCH /zones/{z}/settings/browser_cache_ttl` → `0` ("Respect Existing Headers")                                                                                                                                   | the previous value is put back                                                          |
| Test                       | `GET https://<domain>/health`                                                                                                                                                                                      | nothing                                                                                 |

The four cache rules apply to your hostname only (`http.host eq "<domain>"`), mark the
response eligible for cache and keep the server's own `Cache-Control` for edge and browser:

- covers: path starts with `/covers/`
- page images: path starts with `/api/v1/manga/`
- proxied images: path is `/img`
- static files: path starts with `/_app/immutable/` or `/admin/_app/immutable/`

Each rule's reference ends in a short code made from the domain (`komiq_covers_<code>`),
so two komiq servers in one Cloudflare zone keep separate rules.

Records you made yourself, other cache rules, and the records and rules of other
hostnames are never touched. The SSL mode and the Browser Cache TTL are settings of the
whole zone.

## Turned off, run again, removed

What setup made stays until you choose "Remove". Two other things can happen to it.

**Turned off.** Switching the access mode to "Only my devices" or "Public without a
domain" turns a finished setup off on the server. The connector stops (Tunnel), or the
HTTPS listener that setup started stops (Public IP), and a restart does not start them.
Nothing at Cloudflare changes: the DNS record, the cache rules and the tunnel or
certificate stay, and so do the files the server keeps. Readers can no longer open the
domain. Settings › Access shows "Turned off", with "Turn on" and "Remove". The wizard's
Internet step shows "Turn on" too.

"Turn on" sets the mode back to "Public on the internet" and starts the connector or the
listener from the kept files. It asks for no token. Coming from "Public without a
domain" it asks you to confirm first, because the domain then takes the place of the
address readers use. "Remove" asks for the token, turned off or not.

**Run again.** "Run setup again" in Settings › Access goes through the whole checklist
with a token you paste. Steps whose result is in place pass quickly. It repairs the DNS
record: an edited record is corrected, a deleted one is made again, and a public IP that
setup detected is detected again. A record somebody else put on the name is left alone:
the step fails and names it.

## What the server keeps

- In the database (`instance_setting`, key `cloudflare_state`): the domain, the zone,
  account, tunnel, DNS record, certificate, ruleset and rule ids, and the two zone
  settings as they were. Only what "Remove" needs.
- Next to the database, in `cloudflare/` (mode 0600; override with `KOMIKA_CLOUDFLARE_DIR`):
  `tunnel-token` (the connector's run token) or `origin-key.der` and `origin-cert.pem`.
  These are credentials the server needs after a restart. They stay out of the database
  so database backups do not carry them.
- In `cloudflare/bin/`: `cloudflared` (`cloudflared.exe` on Windows), when setup
  downloaded it. "Remove" leaves it there.

Each id is saved the moment the thing exists, so a failed step can be retried, or the
partial setup removed, without leaving anything behind.

## Doing it by hand

The wizard's "I will do it by hand" shows the same steps for the dashboard:

1. Tunnel: Zero Trust › Networks › Tunnels › create a Cloudflared tunnel, install the
   connector on the server, add a public hostname for your domain to
   `http://localhost:<PORT>`. Set `TRUSTED_PROXY_CIDRS=127.0.0.1/32,::1/128`.
   Public IP: a proxied A record, an Origin certificate served on 443 by a reverse proxy,
   SSL mode Full (strict), `TRUSTED_PROXY_CIDRS` set to the ranges at cloudflare.com/ips.
2. Caching › Cache Rules: one rule for your hostname with the paths above, eligible for
   cache, edge and browser TTL from the origin.
3. Caching › Configuration: Browser Cache TTL › Respect Existing Headers.
4. Open `https://<domain>/health`; it answers `ok`.

Then record the domain, so the console shows it as the reader address: "Continue" in the
wizard, "Save" in Settings › Access. The terminal setup saves it after it prints the
steps.

## Settings for testing

`KOMIKA_CLOUDFLARE_API` (API base), `KOMIKA_CLOUDFLARE_TRACE_URL` (public address
detection), `KOMIKA_CLOUDFLARE_HEALTH_BASE` (replaces `https://<domain>` in the last
step) and `KOMIKA_CLOUDFLARED_DOWNLOAD_BASE` (where the connector is downloaded from)
point setup at a local mock. Do not set them in production.
