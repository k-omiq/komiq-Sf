# Cloudflare in front of a self-hosted komiq

The setup wizard (step "Internet") and Settings › Access can put your server on a domain
with Cloudflare in front of it. You give a domain, a connection type and a Cloudflare API
token; the server runs a checklist and shows each line as it goes. Nothing runs until an
owner starts it.

## Connection types

| Type                 | How traffic arrives                                                                                                        | Needs                                                                             |
| -------------------- | -------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------- |
| **Tunnel** (default) | The server runs `cloudflared`, which connects out to Cloudflare. No open ports.                                            | `cloudflared` on the machine (`CLOUDFLARED_BIN`, or on `PATH`).                   |
| **Public IP**        | A proxied A/AAAA record points at the server. The server serves HTTPS on port 443 with a Cloudflare Origin CA certificate. | Port 443 reachable from Cloudflare (`KOMIKA_HTTPS_PORT` to change what it binds). |

With a tunnel, set `BIND_ADDR=127.0.0.1` if nothing else on the network should reach the
plain HTTP port. With a public IP, firewall the plain HTTP port.

## The API token

Create it at **My Profile › API Tokens › Create Token › Create Custom Token**:

| Scope   | Permission                                                    | Level |
| ------- | ------------------------------------------------------------- | ----- |
| Account | Cloudflare Tunnel _(Tunnel only)_                             | Edit  |
| Zone    | SSL and Certificates _(Public IP only)_                       | Edit  |
| Zone    | Zone                                                          | Read  |
| Zone    | DNS                                                           | Edit  |
| Zone    | Zone Settings                                                 | Edit  |
| Zone    | Cache Rules                                                   | Edit  |
| Account | Account Rulesets _(listed by Cloudflare's Cache Rules guide)_ | Edit  |
| Account | Account Filter Lists _(listed by the same guide)_             | Edit  |

Zone Resources: Include › Specific zone › your domain. Account Resources: your account.

The token is used for one run and then dropped. It is never written to the database, a
file or the log. Re-running or removing the setup asks for it again.

## What setup does (and what "Remove" undoes)

| Step                    | Cloudflare API                                                                                                                                                            | Undone by "Remove"                                                                      |
| ----------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------- |
| Check the token         | `GET /user/tokens/verify`                                                                                                                                                 | —                                                                                       |
| Find the zone           | `GET /zones?name=` (the domain, then each parent)                                                                                                                         | —                                                                                       |
| Tunnel                  | `POST /accounts/{a}/cfd_tunnel` (`config_src: cloudflare`), `PUT …/{t}/configurations` (domain → `http://127.0.0.1:<PORT>`), `GET …/{t}/token`                            | `DELETE …/{t}/connections`, `DELETE /accounts/{a}/cfd_tunnel/{t}`                       |
| Connector (Tunnel)      | runs `cloudflared tunnel --no-autoupdate run` with `TUNNEL_TOKEN` in its environment, restarted if it exits, started again at every boot                                  | stopped, token file deleted                                                             |
| Origin certificate (IP) | `POST /certificates` (`origin-ecc`, 15 years, CSR made on the server)                                                                                                     | `DELETE /certificates/{id}`, key and certificate files deleted                          |
| SSL mode (IP)           | `PATCH /zones/{z}/settings/ssl` → `strict` (zone-wide)                                                                                                                    | the previous value is put back                                                          |
| Visitor addresses       | `GET /ips` (IP) — the server trusts loopback (Tunnel) or Cloudflare's ranges (IP) for `CF-Connecting-IP`; `TRUSTED_PROXY_CIDRS` in env wins                               | cleared                                                                                 |
| DNS record              | `GET /zones/{z}/dns_records?name.exact=`, `POST /zones/{z}/dns_records` (proxied CNAME to `<tunnel>.cfargotunnel.com`, or A/AAAA)                                         | `DELETE /zones/{z}/dns_records/{id}`                                                    |
| Cache rules             | `GET /zones/{z}/rulesets/phases/http_request_cache_settings/entrypoint`, then `POST /zones/{z}/rulesets/{r}/rules` (or `POST /zones/{z}/rulesets` when the zone has none) | `DELETE …/rulesets/{r}/rules/{id}`; the ruleset too if setup created it and it is empty |
| Respect cache headers   | `PATCH /zones/{z}/settings/browser_cache_ttl` → `0` ("Respect Existing Headers")                                                                                          | the previous value is put back                                                          |
| Test                    | `GET https://<domain>/health`                                                                                                                                             | —                                                                                       |

The four cache rules apply to your hostname only (`http.host eq "<domain>"`), mark the
response eligible for cache and keep the server's own `Cache-Control` for edge and browser:

- covers: path starts with `/covers/`
- page images: path starts with `/api/v1/manga/`
- proxied images: path is `/img`
- static files: path starts with `/_app/immutable/` or `/admin/_app/immutable/`

Records you made yourself, other cache rules and other hostnames are never touched.

## What the server keeps

- In the database (`instance_setting`, key `cloudflare_state`): the domain, the zone,
  account, tunnel, DNS record, certificate, ruleset and rule ids, and the two zone
  settings as they were. Only what "Remove" needs.
- Next to the database, in `cloudflare/` (mode 0600; override with `KOMIKA_CLOUDFLARE_DIR`):
  `tunnel-token` (the connector's run token) or `origin-key.der` and `origin-cert.pem`.
  These are credentials the server needs after a restart. They stay out of the database
  so database backups do not carry them.

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

## Settings for testing

`KOMIKA_CLOUDFLARE_API` (API base), `KOMIKA_CLOUDFLARE_TRACE_URL` (public address
detection) and `KOMIKA_CLOUDFLARE_HEALTH_BASE` (replaces `https://<domain>` in the last
step) point setup at a local mock. Do not set them in production.
