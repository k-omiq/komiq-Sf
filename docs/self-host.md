# Self-host komiq

Run your own komiq server with Docker. One address serves everything: the reader at
`/`, the admin console at `/admin`, the API and the images. A fresh install has no
accounts and no sources; the setup screens walk you through both.

This guide covers a home server or a VPS. To run komiq on your own computer without
Docker, use the desktop app instead.

## Requirements

- Linux, macOS or Windows with Docker Engine 24+ and Docker Compose v2
  (`docker compose version`).
- 2 GB of memory to run, 4 GB to build the image. The first build takes 15 to 30
  minutes; most of it is compiling the server.
- About 3 GB of disk for the images, plus room for your library.
- A copy of this repository (`git clone`, or a downloaded release archive).

## Install

Run the installer from the repository folder:

```sh
bash deploy/selfhost/install.sh
```

It asks for the port and how people will reach komiq, and does the rest:

1. It checks that Docker runs, that Compose is v2 or newer, that the CPU is amd64 or
   arm64, and how much memory and free disk Docker has. It warns under 4 GB of memory or
   5 GB of free disk. Under 2 GB of memory it stops, unless you pass `--yes`.
2. It asks for the HTTP port (default `8080`) and for one of four ways to reach komiq:

   | Choice      | Meaning                                                          |
   | ----------- | ---------------------------------------------------------------- |
   | `local`     | This machine or your home network, over HTTP. The default.       |
   | `tunnel`    | A domain through a Cloudflare Tunnel. No inbound ports.          |
   | `public-ip` | A domain through Cloudflare DNS. Needs HTTPS port 443.           |
   | `direct`    | Your public IP address, no domain. HTTPS needs ports 443 and 80. |

   For `direct` it then asks "Encrypt the connection with a free certificate (HTTPS)?
   Needs ports 443 and 80." The default is yes. With no, readers use plain HTTP on the
   HTTP port, and ports 443 and 80 are not needed.

3. It asks whether other devices on your home network may connect over HTTP on that
   port. The default is yes for `local`. For the other three it is no: the plain HTTP
   port then stays on this machine only (`KOMIQ_BIND=127.0.0.1`). It does not ask for
   `direct` without HTTPS: readers use that port, so it is open to every address
   (`KOMIQ_BIND=0.0.0.0`).
4. It checks that the ports it needs are free, and stops with a message when one is not.
5. It writes `.env`. An existing `.env` is copied to `.env.bak` once, and only
   `KOMIQ_PORT`, `KOMIQ_BIND` and `COMPOSE_FILE` are changed.
6. When ufw or firewalld is active, it offers to open the needed ports in it. It asks
   first and uses `sudo`. On macOS it leaves the firewall alone. On a cloud server, open
   the same ports in your provider's firewall yourself.
7. It builds and starts komiq, and waits up to 5 minutes until the server is healthy.
8. It reads the setup code from the server log and starts the
   [terminal setup](#set-up-from-a-terminal) with it. It passes your access choice, and
   the HTTPS answer for `direct`, so setup does not ask them again.

At the end it prints the reader and console addresses, and the command that changes a
choice later: `docker compose exec server komika-server setup`. For `local` it adds
`Internet: closed when the server is set to "Only my devices". It then refuses plain HTTP from the internet.`
The installer does not change the mode of a server that is set up already.

| Option                            | What it does                                                                                                                                               |
| --------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--port PORT`                     | The HTTP port. Default: `8080`, or the value already in `.env`.                                                                                            |
| `--access MODE`                   | `local`, `tunnel`, `public-ip` or `direct`. Default: `local`.                                                                                              |
| `--encrypt yes` or `--encrypt no` | Only with `--access direct`. `yes`: HTTPS with a free certificate. `no`: plain HTTP on the HTTP port. Default: `yes`.                                      |
| `--lan yes` or `--lan no`         | Open the HTTP port to your home network, or keep it on this machine. Default: `yes` for `local`, `no` for the others. Refused as `no` with `--encrypt no`. |
| `--firewall yes`, `no` or `ask`   | Open the needed ports in the machine's firewall. Default: `ask`.                                                                                           |
| `--yes`                           | Accept the defaults without questions. Allows a build with under 2 GB of memory.                                                                           |
| `--no-setup`                      | Do not start the terminal setup.                                                                                                                           |
| `--help`                          | Print the options.                                                                                                                                         |

A second run takes the port, the bind address and the Compose files in `.env` as its
defaults. Without a terminal (in a script, for example), pass `--yes` or give every answer
as an option. The terminal setup itself needs a terminal: without one, the installer
prints the command to run later.

On a server that already has an owner, a second run changes the ports and leaves setup
alone. The installer prints "The server is already set up.", or the setup command answers
"This server already has an owner. Nothing was changed." when the log still held the old
setup code. To change how people reach the server, use **Settings › Access** in the
console, or run the setup command without a setup code.

### By hand

The installer is optional. The same stack starts with:

```sh
cd deploy/selfhost
docker compose up -d
```

This builds the komiq image, pulls the source engine (Suwayomi), and starts both.
Check that they are up:

```sh
docker compose ps
```

`server` should be `healthy` within a minute of the build finishing. The engine takes
a minute longer on its first start.

To change the port or who can reach it, copy the settings file first, edit it, then run
`docker compose up -d` again:

```sh
cp .env.example .env
```

| Setting        | Default   | What it does                                                                               |
| -------------- | --------- | ------------------------------------------------------------------------------------------ |
| `KOMIQ_PORT`   | `8080`    | Port komiq answers on.                                                                     |
| `KOMIQ_BIND`   | `0.0.0.0` | `0.0.0.0`: every device on your network. `127.0.0.1`: this machine only.                   |
| `COMPOSE_FILE` | unset     | Publishes ports 443 and 80. See [Reach it from the internet](#reach-it-from-the-internet). |
| `TZ`           | `UTC`     | Time zone for logs.                                                                        |
| FlareSolverr   | off       | For sources behind "checking your browser" pages. See the comment in the file.             |

## Set up

The installer runs setup in your terminal. If you started komiq by hand, or passed
`--no-setup`, use a browser:

1. Find the setup code. It is printed in the server log at every start until setup is
   done; use the last one:

   ```sh
   docker compose logs server | grep "setup code" | tail -n 1
   ```

2. Open `http://<server address>:8080/admin/setup` in a browser. On the server itself
   that is `http://localhost:8080/admin/setup`.
3. Follow the steps: create the owner account (this is where the setup code goes),
   choose who can sign up, and add sources if you want to now.
4. Open `http://<server address>:8080/` and sign in to read.

The setup code works only until the owner account exists. Restarting the server before
that prints a new one.

### Set up from a terminal

A server with no screen can do the same steps over SSH, without a browser. The installer
starts this command for you. To run it yourself:

```sh
docker compose exec server komika-server setup
```

It asks the same questions as the setup screens: the owner account (with the setup
code), who can reach the server, who can sign up, and sources. Each answer is applied when
you give it. On a first run the sign-up question starts at "Only people I add", like the
setup screens.

Run the command again, without a setup code, to continue or to change a choice. It then
asks you to sign in as the owner. An account that still has a temporary password must
choose a new one first. A setup code is for a first run only: given to a server that
already has an owner, the command changes nothing and says "This server already has an
owner. Nothing was changed."

| Option                  | What it does                                                                                                                             |
| ----------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| `--setup-code <code>`   | Gives the setup code, so the command does not ask for it. The environment variable `KOMIKA_SETUP_CODE` does the same.                    |
| `--access <choice>`     | Skips the access question. `local`, `internet` and `direct` pick the mode. `tunnel` and `public-ip` also pick the Cloudflare connection. |
| `--encrypt yes` or `no` | Only with `--access direct`: answers the HTTPS question, so the command does not ask it.                                                 |
| `--server <address>`    | Where the running server answers. Default: `http://127.0.0.1:<PORT>`.                                                                    |

Before a public choice, the command says what it found on the server:

- For a tunnel: whether the connector (`cloudflared`) is on the server, or setup will
  download it.
- For a public IP, and for no domain with HTTPS: whether port 443 (and port 80, without
  a domain) is free, already served by komiq, used by another program, or one the server
  has no permission to open. In a container it adds that the ports must be published too.

For Cloudflare it first asks for the domain. A scheme and a path are dropped:
`https://Read.Example.com/` is read as `read.example.com`. The connection question has
four answers: "Cloudflare Tunnel", "Public IP", "I will do it by hand" and "Set up
later". "Set up later" changes nothing, and you finish in **Settings › Access**.

It then prints "You do these 3 things once" and "komiq does the rest", a link that opens
Cloudflare's token page with part of the form filled in, and the permission rows to add
by hand. After you paste the token it names any permission the token may lack and asks
whether to continue. When the server refuses to start the run, the command asks "Try
again?" and sends the start again. "Try the failed step again?" comes only after a step
of a run the server accepted has failed. The details are in
[self-host-cloudflare.md](./self-host-cloudflare.md).

Without a domain, the address the command offers fits the HTTPS answer: the bare address
for HTTPS, the address and the HTTP port for plain HTTP. If the server cannot save the
address, the command asks "Try again?". With no, the address stays unsaved, setup goes
on, and you finish in **Settings › Access**. For plain HTTP in a container it adds that
the port must also answer other machines (`KOMIQ_BIND=0.0.0.0`).

Choosing "Only my devices" prints "From the internet, the server now refuses plain HTTP."
On a server that was public on a domain it also prints that Cloudflare is turned off on
this server, and that what setup made at Cloudflare stays until you remove it in the
console. See [Switch back to "Only my devices"](#switch-back-to-only-my-devices).

Outside Docker, run `komika-server setup` on the machine the server runs on. It talks to
`http://127.0.0.1:<PORT>`; use `--server <address>` when the server answers somewhere
else. It refuses a plain `http://` address on the internet before it asks for any
password: run it on the server, or give an `https://` address.

## Use it from other devices

With `KOMIQ_BIND=0.0.0.0`, any device on the same network opens komiq at
`http://<server address>:8080`. That is the default, and what the installer writes when
you answer yes to its home-network question. Find the address with `hostname -I` (Linux),
`ipconfig getifaddr en0` (macOS) or `ipconfig` (Windows).

If a phone cannot connect:

- Allow the port in the server's firewall, for example `sudo ufw allow 8080/tcp`. The
  installer offers to do this when ufw or firewalld is active.
- Check the phone is on the same network, not on mobile data or a guest network.

The native reader apps ask for the server address on their "Connect to a server"
screen. Enter the same address, for example `192.168.1.20:8080`. On a phone, "Scan a
QR code" reads it from the code shown at the end of setup and in Settings › Access.
With Docker, open the admin at the address other devices use, not `localhost`, to see
the code: a server in a container cannot find the address of the machine it runs on.

Plain `http://` is fine at home. Before you open the port to the internet, read the next
section: both ways to reach komiq from the internet give you HTTPS.

## Reach it from the internet

There are two ways, both chosen in the setup wizard's Access step or later in
**Settings › Access**: with a domain and Cloudflare, or without a domain.

The installer prepares Docker for either. Answer `tunnel`, `public-ip` or `direct` at its
prompt, or pass `--access`. It publishes the ports that way needs, offers to open them in
ufw or firewalld, and keeps the plain HTTP port on this machine only
(`KOMIQ_BIND=127.0.0.1`) unless you answer yes to the home-network question. The
exception is `direct` without HTTPS (`--encrypt no`): readers use the HTTP port itself,
so the installer opens it to every address, publishes no extra port and offers only that
port to the firewall.

By hand, ports 443 and 80 are published by two extra Compose files,
`docker-compose.https.yml` (443) and `docker-compose.acme.yml` (80). Switch them on with
`COMPOSE_FILE` in `.env`, then run `docker compose up -d`. You do not edit
`docker-compose.yml`.

| Way to reach komiq           | `COMPOSE_FILE` in `.env`                                              |
| ---------------------------- | --------------------------------------------------------------------- |
| Local, or Cloudflare Tunnel  | unset                                                                 |
| Cloudflare public IP         | `docker-compose.yml:docker-compose.https.yml`                         |
| Without a domain, HTTPS      | `docker-compose.yml:docker-compose.https.yml:docker-compose.acme.yml` |
| Without a domain, plain HTTP | unset, with `KOMIQ_BIND=0.0.0.0`                                      |

Before you commit to a way, the Internet and Address steps, **Settings › Access** and
the terminal setup say what the server found on its machine: whether `cloudflared` is
there or will be downloaded, whether ports 443 and 80 are free, used by another program
or closed to the server for lack of permission, and whether the server runs in a
container, where the ports must be published too. In a container the console also shows
the installer command that publishes them.

If the server cannot open the HTTPS port when it starts (another program holds it, or it
has no permission), it tries again by itself: after 5 seconds, then after waits that
double, up to once every 5 minutes, until HTTPS is up or the access mode no longer needs
it. This holds for a public IP with Cloudflare and for no domain with HTTPS.
**Settings › Access** shows the reason in red under the HTTPS state (the `HTTPS` row with
a public IP, the `Encryption` row without a domain), followed by "The server tries again
by itself." For example: "Could not listen on port 443 (...). The server needs permission
to use that port (with Docker, publish it), or set KOMIKA_HTTPS_PORT." A tunnel connector
that fails to start does not keep HTTPS from starting.

### With a domain

Cloudflare sits in front of komiq and gives it HTTPS and caching. It supports a
Cloudflare Tunnel (nothing to open on your router or VPS) and a public IP with DNS. The
details are in [self-host-cloudflare.md](./self-host-cloudflare.md).

- **Tunnel:** the image includes `cloudflared`, which komiq starts for you. Keep
  `KOMIQ_BIND=127.0.0.1` so the plain HTTP port is closed to the network. The installer
  writes that for `tunnel`.
- **Public IP:** komiq serves HTTPS on port 443 inside the container. Publish it with
  `COMPOSE_FILE`, as in the table above. The installer does that for `public-ip`. The
  Origin CA certificate lasts 15 years and nothing renews it by itself. See
  [Turned off, run again, removed](./self-host-cloudflare.md#turned-off-run-again-removed).

### Without a domain

For a server with a public address, such as a cloud VPS. Readers type the server's
address, like `203.0.113.5`. Nothing sits in front of komiq: no Cloudflare account, no
DNS, and the server fetches covers and pages itself.

The connection is HTTPS. komiq gets a free certificate for the IP address from Let's
Encrypt and renews it by itself. Such a certificate lasts about six days, and komiq
gets a new one every three. At start and at each renewal check, a certificate on disk
that is for another address than the stored one counts as due: komiq does not serve it
and gets one for the stored address. Using it means you agree to the
[Let's Encrypt Subscriber Agreement](https://letsencrypt.org/repository/).

1. Publish ports 443 and 80: run the installer with `direct`, or set `COMPOSE_FILE` as in
   the table above and run `docker compose up -d`. Open ports 80 and 443 in your cloud
   firewall. Let's Encrypt checks the server on port 80 before each certificate, so keep
   it open.
2. In the Access step, choose **Public without a domain** and enter the address. Saving
   can take a minute. If it fails, the message says why; the usual reason is a closed
   port 80.
3. Readers use `https://203.0.113.5`, and you open the console at
   `https://203.0.113.5/admin`.

The desktop app, when it is the console of a remote server, moves itself to the
`https://` address once that address answers. It tries four times over about half a
minute, and only for the host you typed. The form in Settings stays busy meanwhile, and
the setup wizard waits on that step and shows "Moving to the new address." If the new
address does not answer, the app stays on the old one. **Settings › Access** and the last
setup step then show this notice, with a **Try again** button: "This app still uses the
old address, without encryption. It got no answer at https://203.0.113.5. Check that port
443 is open in your cloud firewall, then try again. After the first visit over HTTPS, the
old address stops answering from the internet." The notice lasts until you close the app.

The plain HTTP port (`KOMIQ_PORT`) keeps answering this machine, for the terminal setup,
and your home network when `KOMIQ_BIND` is `0.0.0.0`. After the first visit from the
internet over HTTPS, komiq no longer handles requests from the internet on it: a page
load is sent to the HTTPS address, and a sign-in is refused with that address. It waits
for that first visit so that a closed port 443 on the server cannot lock you out.

On a VPS, the first-run screens themselves still open over plain HTTP, because the
certificate comes later. The server does not accept the owner account over plain HTTP
from the internet: the Owner step shows no form there, and `createOwner` is refused. Run
the setup in a terminal instead (`komika-server setup`, see above). The home network,
this computer, HTTPS and a trusted proxy are not affected.

For both rules, the server knows a request comes from the internet by the connection's
own address. Behind Docker Desktop or rootless Docker every connection looks local, so
there the server goes by the address you typed: a public IP address, or a name that
leads to one, counts as the internet. `localhost`, a home-network address and a name
such as `komiq.local` do not. A proxy of yours that ends HTTPS in front of the server is
not affected when it sends `X-Forwarded-Proto: https`, as most do.

The address must be public: Let's Encrypt does not sign for home-network addresses. An
IPv6 address works too, typed without brackets, like `2001:db8::1`. If the address of the
server changes, enter the new one in **Settings › Access**.

The server settings `KOMIKA_HTTPS_PORT` and `KOMIKA_ACME_PORT` change the two ports the
server binds. The form then says what to open: another HTTPS port becomes part of the
address, like `https://203.0.113.5:8443`, and public port 80 must be forwarded to the
other port, because Let's Encrypt always connects to port 80. The address komiq shows
names the HTTPS port of the running server, so a changed `KOMIKA_HTTPS_PORT` shows after
a restart.

You can switch **Encrypt the connection** off. Readers then type the address and port,
like `203.0.113.5:8080`, over plain HTTP, and people on the network between a reader and
the server can see passwords and pages. The address field follows the switch: the bare
address for HTTPS, the address and the HTTP port for plain HTTP.

With Docker, plain HTTP needs the HTTP port open to other machines, not only to this one:
`KOMIQ_BIND=0.0.0.0` in `.env`, and no extra Compose files. This command sets both, offers
to open that one port in ufw or firewalld, and prints `http://<public-IP>:<port>/` as the
address:

```sh
bash deploy/selfhost/install.sh --access direct --encrypt no
```

Open the same port in your cloud firewall. In a container, the form and the terminal
setup both say that the port must answer other machines.

Inside the container the server always listens on 8080. The compose file passes your
`KOMIQ_PORT` to it as `KOMIKA_PUBLISHED_PORT`, so the address the form and the terminal
setup suggest names the published port, like `203.0.113.5:9000` with `KOMIQ_PORT=9000`.
The two extra Compose files publish HTTPS and the certificate check as fixed `443:443`
and `80:80`, so those numbers are the same inside and outside.

### Switch back to "Only my devices"

While the server is public, **Settings › Access** has an **Only my devices** action. It
asks first, then turns the public way in off. Nothing is deleted:

- With Cloudflare, the tunnel connector stops, or with a public IP the HTTPS listener
  that setup started. A restart does not start them again. The DNS record, the cache
  rules and the tunnel or certificate stay at Cloudflare until you choose **Remove**.
- Without a domain, HTTPS at the server's address stops, and so do the certificate
  renewals.

In "Only my devices" the server also refuses plain HTTP from the internet. It answers
HTTP 403, and a GraphQL call gets the text as its error message:

```text
This server is set to "Only my devices". It does not answer from the internet. Use its address on your home network. To open it to the internet, run `komika-server setup` on the server.
```

"From the internet" means the client's address is a public one. This computer, your home
network, and private and VPN ranges (including `100.64.0.0/10`) are not the internet. A
proxy listed in `TRUSTED_PROXY_CIDRS` is let through. So is a request from one of those
private addresses for which HTTPS ended in front of the server: your own reverse proxy
on the same machine or network, which sends `X-Forwarded-Proto: https`.
Before you choose an access mode nothing is refused, so a fresh server's setup screens
still open. The other modes are unchanged.

Behind Docker Desktop or rootless Docker the server goes by the address the client typed,
as described under [Without a domain](#without-a-domain). A client can send a false
address or header, so there the sure way to close the port is `KOMIQ_BIND=127.0.0.1` in
`deploy/selfhost/.env` (the installer's `--lan no`), or a firewall.

An owner who opens the console at `http://<public-ip>:<port>` is locked out after the
switch. To get back in, use the server's address on your home network, or run
`komika-server setup` on the server (`docker compose exec server komika-server setup`)
and choose a public mode. An SSH tunnel to the server's port works too, because the
request then arrives from the server's own computer.

The console warns first. In the confirm step, when the console is open at a public
address, it says "This console is open at a public address. After the switch it stops
answering here. Open it on your home network, or run "komika-server setup" on the server
to change the mode." For a server that was public without a domain and without HTTPS, the
same step says "The HTTP port stops answering clients on the internet." The setup
wizard's Access step shows a warning when you select "Only my devices" in a console
opened at a public plain-HTTP address.

Choosing **Public without a domain** turns a finished Cloudflare setup off in the same
way. The Cloudflare line in **Settings › Access** then says "Turned off", with **Turn
on** and **Remove**. **Turn on** needs no API token, because the server kept what it
needs. From "Public without a domain" it asks first ("Turn Cloudflare on?"): the domain
then takes the place of the address readers use.

## Update

```sh
git pull
cd deploy/selfhost
docker compose up -d --build
```

Or run `git pull` and then `bash deploy/selfhost/install.sh` again. It takes its defaults
from your `.env`, rebuilds and restarts. Add `--no-setup` to leave the setup questions
out.

Your data stays in the Docker volumes. Updates that change the database migrate it at
start. Take a backup first.

## Back up and restore

Everything komiq keeps is in two Docker volumes:

| Volume              | Contents                                                    |
| ------------------- | ----------------------------------------------------------- |
| `komiq_server-data` | Accounts, libraries, comments, settings, covers, Cloudflare |
| `komiq_engine-data` | Installed extensions and the engine's own data              |

Back up by stopping komiq and copying the volumes into a folder:

```sh
docker compose stop
docker run --rm -v komiq_server-data:/data -v "$PWD":/backup busybox \
  tar czf /backup/komiq-server-data.tgz -C /data .
docker run --rm -v komiq_engine-data:/data -v "$PWD":/backup busybox \
  tar czf /backup/komiq-engine-data.tgz -C /data .
docker compose start
```

Restore onto a fresh install (before its first start, or after `docker compose down`):

```sh
docker compose create
docker run --rm -v komiq_server-data:/data -v "$PWD":/backup busybox \
  sh -c 'tar xzf /backup/komiq-server-data.tgz -C /data && chown -R 10001:10001 /data'
docker run --rm -v komiq_engine-data:/data -v "$PWD":/backup busybox \
  sh -c 'tar xzf /backup/komiq-engine-data.tgz -C /data && chown -R 1000:1000 /data'
docker compose up -d
```

Stop komiq before copying: the database is written all the time, and a copy taken while
it runs may not open. Timed backups are planned for a later release.

## Remove

```sh
docker compose down        # stop, keep your data
docker compose down -v     # stop and delete all data
```

## Troubleshooting

**The installer stops with "problem:".** The line says what to fix. "Port 8080 is in use.
Try --port 8081." means another program has the HTTP port: run it again with the port it
names. "Port 443 is in use. Free it before continuing." means another program, often a
web server, holds a port komiq needs: stop it, or choose `tunnel`. "Could not check
ports" means none of `ss`, `lsof` and `netstat` is installed. "--lan no does not fit
--encrypt no" means plain HTTP without a domain needs the HTTP port open: leave `--lan`
out.

**"This server already has an owner. Nothing was changed."** The setup command got a
setup code, from you or from the installer, which found an old one in the log. Nothing is
wrong. To change a setting, use the console, or run
`docker compose exec server komika-server setup` without a setup code.

**The installer says "Server did not become healthy".** It waited 5 minutes.
`docker compose logs server`, run from `deploy/selfhost`, shows why.

**No setup code in the log.** The server prints it only while no owner exists. If setup
was finished, sign in at `/admin` instead. If the log is empty, the server did not start:
`docker compose logs server`.

**"Invalid setup code".** The code changes at every restart. Take the last one from the
log and type it with its dashes.

**`server` is not healthy.** `docker compose logs server` shows why. A port already in
use shows as an error from `docker compose up`; pick another `KOMIQ_PORT`.

**Port 443 or 80 does not answer.** komiq answers on them inside the container. Docker
publishes them only when `COMPOSE_FILE` in `.env` names the extra Compose files (see
[Reach it from the internet](#reach-it-from-the-internet)). Then check the machine's
firewall and your cloud provider's firewall.

**HTTPS shows "Not running" after a restart.** The server could not open its HTTPS port.
**Settings › Access** shows why in red, and the server tries again by itself, at least
every 5 minutes. Free the port, give the server permission to use it, or publish it with
Docker. See [Reach it from the internet](#reach-it-from-the-internet).

**Readers cannot open `http://<public-IP>:<port>/`.** Without a domain and without HTTPS,
readers use the HTTP port. With Docker it must be open to every address:
`KOMIQ_BIND=0.0.0.0` in `.env`. Run the installer with `--access direct --encrypt no`,
then open the port in your cloud firewall.

**The domain stopped answering after you changed the access mode.** "Only my devices"
and "Public without a domain" turn Cloudflare off on the server. **Settings › Access**
shows "Turned off": choose **Turn on**.

**The console stopped answering at the server's public address.** You chose "Only my
devices", and the server refuses plain HTTP from the internet. Open the console at the
server's home network address, or run `docker compose exec server komika-server setup`
on the server and choose a public mode. See
[Switch back to "Only my devices"](#switch-back-to-only-my-devices).

**The DNS record was edited or deleted, or the public IP changed.** Choose **Run setup
again** in **Settings › Access** and paste the API token. Setup corrects its record, or
makes it again. It detects the public IP again, unless you typed one during setup.

**"Get the connector (cloudflared)" fails.** Outside Docker, tunnel setup downloads
`cloudflared` when the server has none. Check that the server can reach GitHub, then
retry the step. Or install `cloudflared` yourself and set `CLOUDFLARED_BIN` to its path.

**"This token may lack: …".** The token check found a permission row it could not read.
It is advice: you can still run setup. If a step fails, add the row to the token in
Cloudflare and retry.

**The catalogue is empty.** A new install has no sources. Add an extension repo and
install a source in the admin console under **Sources**. If adding fails, the engine may
still be starting: `docker compose ps` shows `suwayomi` as `healthy` once it is ready.

**Covers or pages do not load.** Check the engine is healthy, then open the source in the
admin console and look at its health. Sources behind Cloudflare challenges need
FlareSolverr (see `.env.example`).

**The build fails or the machine runs out of memory.** Building needs about 4 GB. On a
small VPS add swap, or build on another machine and copy the image over with
`docker save komiq-selfhost:local | ssh <server> docker load`.

**Free disk space after updates.** Old images and build cache stay behind:
`docker image prune` and `docker builder prune`.
