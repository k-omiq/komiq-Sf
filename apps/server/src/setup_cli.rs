//! `komika-server setup`: the first-run wizard in a terminal, for a server with no
//! screen (a VPS, a NAS, a Docker host reached over SSH).
//!
//! It asks the same questions as the admin console's `/setup` screens, in the same
//! order (owner, access, accounts, sources), and makes the same GraphQL calls against
//! the RUNNING server. Nothing here opens the database or builds server state, so
//! every rule (the setup code, password rules, env-pinned settings, the Cloudflare
//! checklist) is the server's own, and the browser wizard and this one cannot drift
//! apart on behaviour. Each answer is applied when it is given: stopping half-way
//! keeps what was done, and running it again signs in as the owner and carries on.
//!
//! Input is read line by line, so answers can also be piped in. Passwords and the
//! Cloudflare token are read without echo on a terminal. `--setup-code` (or
//! `KOMIKA_SETUP_CODE`) supplies the code; `--access` skips the access choice and
//! `--encrypt` the HTTPS question of `--access direct`.

use std::io::{self, BufRead, IsTerminal, Write};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use rand_core::{OsRng, RngCore};
use serde_json::{json, Value};

const USAGE: &str = "\
Set up this server from a terminal (the same steps as the /admin/setup screens).

Usage: komika-server setup [--server <address>] [--setup-code <code>]
                           [--access <local|internet|tunnel|public-ip|direct>]
                           [--encrypt <yes|no>]

  --server <address>   Where the running server answers.
                       Default: http://127.0.0.1:<PORT>
  --setup-code <code>  Setup code for a first run (default: KOMIKA_SETUP_CODE).
  --access <choice>    Skip the access choice; tunnel/public-ip also choose the connection.
  --encrypt <yes|no>   With --access direct: use HTTPS or not, without the question.

The server must be running. Answers are applied as you give them; run the command
again to continue or to change a choice.";

#[derive(Debug, PartialEq)]
struct Args {
    origin: String,
    chosen_server: bool,
    setup_code: Option<String>,
    access: Option<String>,
    encrypt: Option<bool>,
}

fn parse_args(default_port: u16, args: &[String], setup_code: Option<String>) -> Result<Args> {
    let mut parsed = Args {
        origin: format!("http://127.0.0.1:{default_port}"),
        chosen_server: false,
        setup_code: setup_code.filter(|v| !v.trim().is_empty()),
        access: None,
        encrypt: None,
    };
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        if !matches!(
            flag.as_str(),
            "--server" | "--setup-code" | "--access" | "--encrypt"
        ) {
            bail!("Unknown option: {flag}\n\n{USAGE}");
        }
        let value = args
            .next()
            .filter(|v| !v.starts_with("--") && !v.trim().is_empty())
            .ok_or_else(|| anyhow!("{flag} needs a value\n\n{USAGE}"))?;
        match flag.as_str() {
            "--server" => {
                parsed.origin = value
                    .trim()
                    .trim_end_matches('/')
                    .trim_end_matches("/graphql")
                    .to_string();
                parsed.chosen_server = true;
            }
            "--setup-code" => parsed.setup_code = Some(value.to_string()),
            "--access" => {
                if !matches!(
                    value.as_str(),
                    "local" | "internet" | "tunnel" | "public-ip" | "direct"
                ) {
                    bail!("Unknown access choice: {value}\n\n{USAGE}");
                }
                parsed.access = Some(value.to_string());
            }
            "--encrypt" => {
                parsed.encrypt = Some(match value.as_str() {
                    "yes" => true,
                    "no" => false,
                    _ => bail!("--encrypt must be yes or no\n\n{USAGE}"),
                })
            }
            _ => unreachable!(),
        }
    }
    if parsed.encrypt.is_some() && parsed.access.as_deref() != Some("direct") {
        bail!("--encrypt needs --access direct\n\n{USAGE}");
    }
    Ok(parsed)
}

/// Rows the extension picker prints for one search.
const PICKER_ROWS: usize = 20;

/// The message the server uses when the source engine is down or still starting
/// (the console matches the same text: `ENGINE_UNREACHABLE` in apps/admin).
const ENGINE_UNREACHABLE: &str = "The source engine is not reachable.";

pub async fn run(default_port: u16, args: &[String]) -> Result<()> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let args = parse_args(default_port, args, std::env::var("KOMIKA_SETUP_CODE").ok())?;
    if sends_in_clear(&args.origin).await {
        bail!(
            "{} is on the internet and not encrypted: the owner password would travel in \
             the clear.\nRun `komika-server setup` on the server itself (over SSH, or with \
             `docker compose exec server komika-server setup`), or give an https:// address.",
            args.origin
        );
    }
    let origin = args.origin;
    let chosen_server = args.chosen_server;
    let mut api = Api {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?,
        origin,
        token: None,
    };

    let instance = api
        .call(
            "query Instance { instance { setupRequired name version registrationOpen } }",
            json!({}),
        )
        .await
        .map_err(|e| {
            anyhow!(
                "Could not reach the server at {}: {e}\nStart the server first, then run this \
                 again. Use --server <address> if it answers somewhere else.",
                api.origin
            )
        })?["instance"]
        .clone();
    println!(
        "\n{} server setup\nServer: {} (version {})",
        instance["name"].as_str().unwrap_or("komiq"),
        api.origin,
        instance["version"].as_str().unwrap_or("?"),
    );

    let first_run = instance["setupRequired"].as_bool().unwrap_or(false);
    // A setup code is for a first run. With an owner in place it is an old one (an
    // installer run again reads it from the log): change nothing.
    if !first_run && args.setup_code.is_some() {
        println!("This server already has an owner. Nothing was changed.");
        println!("To change its settings, run `komika-server setup` without a setup code.");
        return Ok(());
    }
    let owner = if first_run {
        step(1, "Owner account");
        println!("The owner manages the server, its sources and its users.");
        create_owner(&mut api, args.setup_code).await?
    } else {
        step(1, "Owner account");
        println!("This server already has an owner. Sign in to continue its setup.");
        sign_in(&mut api).await?
    };

    step(2, "Access");
    let public = access(&api, args.access.as_deref(), args.encrypt).await?;

    step(3, "Accounts");
    // A first run starts closed, like the setup screens. A later run starts from what
    // the server has.
    let open_now = !first_run && instance["registrationOpen"].as_bool().unwrap_or(false);
    let (signup_open, added) = accounts(&api, open_now).await?;

    step(4, "Sources");
    let sources = sources(&api).await?;

    let address = match &public {
        Some(address) => address.clone(),
        None if chosen_server => api.origin.clone(),
        // What a phone on the same network types. The server cannot tell inside a
        // container, or when it only answers on its own computer.
        None => match lan_address(&api).await {
            Some(lan) => lan,
            None if in_container(&api).await => {
                format!("http://<this server's address>:{default_port}")
            }
            // It answers on this computer only.
            None => api.origin.clone(),
        },
    };
    println!("\nYou are all set. Your server is ready to use.\n");
    println!("  Reader    {address}");
    println!("  Console   {address}/admin");
    println!("  Owner     {owner}");
    println!(
        "  Sign-up   {}",
        if signup_open {
            "Anyone can sign up"
        } else {
            "Only people you add"
        }
    );
    println!("  Sources   {sources}");
    println!("  People    {} added", added.len());
    for (name, password) in &added {
        println!("            {name}  temporary password: {password}");
    }
    println!("\nNext: open {address}/admin/sources/extensions to add or change sources.");
    Ok(())
}

/// Whether the server says it runs in a container, where it cannot know the address of
/// the machine around it. An older server does not say: assume it does.
async fn in_container(api: &Api) -> bool {
    api.call(
        "query InContainer { accessPreflight { inContainer } }",
        json!({}),
    )
    .await
    .ok()
    .and_then(|d| d["accessPreflight"]["inContainer"].as_bool())
    .unwrap_or(true)
}

/// Whether talking to `origin` sends passwords over the internet without encryption:
/// plain HTTP to an address that is not this computer or a home / private network.
async fn sends_in_clear(origin: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(origin) else {
        return false;
    };
    if url.scheme() != "http" {
        return false;
    }
    let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
        return false;
    };
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    // A name that does not resolve: the request fails by itself, with its own message.
    let addresses = tokio::net::lookup_host((host.as_str(), port)).await;
    addresses.is_ok_and(|mut found| found.any(|a| crate::cloudflare::from_internet(a.ip())))
}

/// The server's first address on the home network, if it reports one.
async fn lan_address(api: &Api) -> Option<String> {
    let data = api
        .call(
            "query LanAddresses { accessStatus { lanAddresses } }",
            json!({}),
        )
        .await
        .ok()?;
    data["accessStatus"]["lanAddresses"][0]
        .as_str()
        .map(str::to_string)
}

fn step(n: u8, label: &str) {
    println!("\nStep {n} of 4: {label}");
}

// ---- the server -----------------------------------------------------------------

struct Api {
    http: reqwest::Client,
    /// The server's address without a path (`http://127.0.0.1:8080`).
    origin: String,
    /// The owner's session, once there is one.
    token: Option<String>,
}

impl Api {
    /// Run one GraphQL document and return its `data`. A GraphQL error comes back as
    /// the server's own message, ready to show.
    async fn call(&self, query: &str, variables: Value) -> Result<Value> {
        let mut req = self
            .http
            .post(format!("{}/graphql", self.origin))
            .json(&json!({ "query": query, "variables": variables }));
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let res = req.send().await?;
        let status = res.status();
        let body: Value = res
            .json()
            .await
            .map_err(|_| anyhow!("the server answered {status} without a GraphQL reply"))?;
        if let Some(message) = body["errors"][0]["message"].as_str() {
            bail!("{message}");
        }
        Ok(body["data"].clone())
    }
}

// ---- step 1: owner --------------------------------------------------------------

async fn create_owner(api: &mut Api, mut setup_code: Option<String>) -> Result<String> {
    if setup_code.is_none() {
        println!(
            "The setup code is in the server log. With Docker:\n  \
         docker compose logs server | grep \"setup code\" | tail -n 1"
        );
    }
    loop {
        let username = ask_valid("Username", |v| {
            (v.chars().count() >= 3)
                .then_some(())
                .ok_or("At least 3 characters.")
        })?;
        let email = ask_valid("Email", |v| {
            v.contains('@')
                .then_some(())
                .ok_or("Enter an email address.")
        })?;
        let password = ask_new_password()?;
        let code = match setup_code.as_ref() {
            Some(code) => code.clone(),
            None => ask_valid("Setup code", |v| {
                (!v.is_empty())
                    .then_some(())
                    .ok_or("Enter the setup code from the server log.")
            })?,
        };
        let created = api
            .call(
                "mutation CreateOwner($setupCode: String!, $username: String!, $email: String!, \
                 $password: String!) { createOwner(setupCode: $setupCode, username: $username, \
                 email: $email, password: $password) { token user { username } } }",
                json!({ "setupCode": code, "username": username, "email": email, "password": password }),
            )
            .await;
        match created {
            Ok(data) => {
                api.token = data["createOwner"]["token"].as_str().map(str::to_string);
                println!("Owner account created.");
                return Ok(username);
            }
            // The server says which field is wrong; ask again.
            Err(e) => {
                println!("  {e}");
                // A refused code is asked for by hand; any other field keeps it.
                if e.to_string().to_lowercase().contains("setup code") {
                    setup_code = None;
                }
            }
        }
    }
}

async fn sign_in(api: &mut Api) -> Result<String> {
    loop {
        let username = ask("Owner username")?;
        let password = ask_secret("Password")?;
        let session = api
            .call(
                "mutation Login($username: String!, $password: String!) { login(username: \
                 $username, password: $password) { token user { username isAdmin \
                 mustChangePassword } } }",
                json!({ "username": username, "password": password }),
            )
            .await;
        match session {
            Ok(data) if data["login"]["user"]["isAdmin"].as_bool() == Some(true) => {
                api.token = data["login"]["token"].as_str().map(str::to_string);
                if data["login"]["user"]["mustChangePassword"].as_bool() == Some(true) {
                    choose_new_password(api, &password).await?;
                }
                return Ok(data["login"]["user"]["username"]
                    .as_str()
                    .unwrap_or(&username)
                    .to_string());
            }
            Ok(_) => println!("  That account is not an admin of this server."),
            Err(e) => println!("  {e}"),
        }
    }
}

/// An account with a temporary password can do nothing until it chooses its own
/// (the console shows the same forced step).
async fn choose_new_password(api: &Api, current: &str) -> Result<()> {
    println!("This account has a temporary password. Choose a new one to continue.");
    loop {
        let new = ask_new_password()?;
        let changed = api
            .call(
                "mutation ChangePassword($currentPassword: String!, $newPassword: String!) { \
                 changePassword(currentPassword: $currentPassword, newPassword: $newPassword) \
                 { username } }",
                json!({ "currentPassword": current, "newPassword": new }),
            )
            .await;
        match changed {
            Ok(_) => {
                println!("Password changed.");
                return Ok(());
            }
            Err(e) => println!("  {e}"),
        }
    }
}

fn ask_new_password() -> Result<String> {
    loop {
        let password = ask_secret("Password (at least 8 characters)")?;
        if password.chars().count() < 8 {
            println!("  At least 8 characters.");
            continue;
        }
        if ask_secret("Confirm password")? != password {
            println!("  The passwords do not match.");
            continue;
        }
        return Ok(password);
    }
}

// ---- step 2: access -------------------------------------------------------------

const ACCESS_STATUS: &str = "query AccessStatus { accessStatus { mode domain address \
     job { kind running failed steps { id label state message } } } }";

/// Returns the public address when the server is (to be) reached from the internet.
async fn access(api: &Api, preset: Option<&str>, encrypt: Option<bool>) -> Result<Option<String>> {
    let current = api.call(ACCESS_STATUS, json!({})).await.ok();
    let status = current.as_ref().map(|d| &d["accessStatus"]);
    let mode = status.and_then(|s| s["mode"].as_str()).unwrap_or_default();
    let old_address = status
        .filter(|_| mode == "direct")
        .and_then(|s| s["address"].as_str())
        .unwrap_or_default()
        .to_string();
    let old_domain = status
        .and_then(|s| s["domain"].as_str())
        .unwrap_or_default()
        .to_string();

    let picked = match preset {
        Some("local") => 0,
        Some("direct") => 2,
        Some(_) => 1,
        None => choose(
            "Who should reach this server? You can change this later in Settings.",
            &[
                (
                    "Only my devices",
                    "This computer and devices on your home network.",
                ),
                (
                    "Public on the internet",
                    "A domain of yours, with Cloudflare in front for speed and caching.",
                ),
                (
                    "Public without a domain",
                    "Readers type this server's address. No Cloudflare. Encrypted with a free \
                 certificate.",
                ),
            ],
            match mode {
                "internet" => 1,
                "direct" => 2,
                _ => 0,
            },
        )?,
    };
    if picked == 0 {
        if preset.is_some() {
            println!("Only your devices: this computer and your home network.");
        }
        match set_access_mode(api, "local", None).await {
            Err(e) => println!("  The choice was not saved: {e}"),
            Ok(_) => {
                println!("From the internet, the server now refuses plain HTTP.");
                if mode == "internet" {
                    println!(
                        "Cloudflare is turned off on this server. What setup made at Cloudflare"
                    );
                    println!("stays there: remove it in the console, Settings › Access.");
                }
            }
        }
        return Ok(None);
    }
    if picked == 2 {
        return direct(api, &old_address, encrypt).await;
    }

    println!("The domain, or its parent, must already be on Cloudflare.");
    let domain = loop {
        let d = clean_domain(&ask_default("Domain (like read.example.com)", &old_domain)?);
        if d.contains('.') && !d.chars().any(char::is_whitespace) {
            break d;
        }
        println!("  Enter a domain like read.example.com.");
    };
    let port = local_port(&api.origin);
    let mut preset_connection = match preset {
        Some("tunnel") => Some(0),
        Some("public-ip") => Some(1),
        _ => None,
    };
    loop {
        let how = match preset_connection.take() {
            Some(how) => how,
            None => choose(
                "How should Cloudflare connect to this server?",
                &[
                    (
                        "Cloudflare Tunnel",
                        "No open ports. This server connects out to Cloudflare. Recommended.",
                    ),
                    (
                        "Public IP",
                        "Cloudflare connects to this server on port 443 over HTTPS.",
                    ),
                    (
                        "I will do it by hand",
                        "Show the steps for the Cloudflare dashboard.",
                    ),
                    (
                        "Set up later",
                        "Change nothing now. Finish in the console: Settings › Access.",
                    ),
                ],
                0,
            )?,
        };
        if how == 3 {
            println!("Internet access is not set up. Finish it in the console: Settings › Access.");
            return Ok(None);
        }
        let tunnel = how == 0;
        if how == 2 {
            let tunnel = choose(
                "Which connection will you make?",
                &[("Cloudflare Tunnel", ""), ("Public IP", "")],
                0,
            )? == 0;
            println!("\nDo these steps in the Cloudflare dashboard:");
            for (i, line) in manual_steps(tunnel, &domain, &port).iter().enumerate() {
                println!("  {}. {line}", i + 1);
            }
            set_access_mode(api, "internet", Some(&domain)).await?;
            println!("Saved: readers use https://{domain}.");
            return Ok(Some(format!("https://{domain}")));
        }
        if cloudflare_setup(api, &domain, tunnel).await? {
            return Ok(Some(format!("https://{domain}")));
        }
        // Not set up: back to the connection choice (which offers "by hand" and "later").
    }
}

/// A domain as the server stores it: `https://Read.Example.com/` is `read.example.com`.
/// The summary lines print it, so it must not carry a scheme twice.
fn clean_domain(typed: &str) -> String {
    let d = typed.trim();
    let d = d
        .strip_prefix("https://")
        .or_else(|| d.strip_prefix("http://"))
        .unwrap_or(d);
    d.split('/')
        .next()
        .unwrap_or_default()
        .trim_end_matches('.')
        .to_lowercase()
}

/// What the address question of the direct mode offers: the bare host for HTTPS, the
/// host and the HTTP port for plain HTTP. An address kept from the other choice would
/// be saved as it is and lead nowhere (`https://203.0.113.5` with HTTPS just turned
/// off, or `203.0.113.5` read as port 80).
fn direct_default(old_address: &str, public_ip: &str, http_port: &str, encrypt: bool) -> String {
    let old = reqwest::Url::parse(old_address).ok();
    if !encrypt && old.as_ref().is_some_and(|u| u.scheme() == "http") {
        // Plain before and plain now: what the owner had.
        return old_address
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
    }
    let host = match old.as_ref().and_then(|u| u.host_str()) {
        Some(host) => host.trim_matches(['[', ']']).to_string(),
        None => public_ip.to_string(),
    };
    if host.is_empty() || encrypt {
        return host;
    }
    if host.contains(':') {
        format!("[{host}]:{http_port}")
    } else {
        format!("{host}:{http_port}")
    }
}

/// What the server found about its own machine. An older server does not know the
/// query: nothing.
async fn fetch_preflight(api: &Api) -> Option<Value> {
    let data = api
        .call(
            "query AccessPreflight { accessPreflight { cloudflared cloudflaredDetail httpPort \
             httpsPort httpsPortState acmePort acmePortState inContainer publicIp } }",
            json!({}),
        )
        .await
        .ok()?;
    Some(data["accessPreflight"].clone())
}

/// Print what the server found about `path` ("tunnel", "public-ip" or "direct")
/// before the owner commits to it.
async fn access_preflight(api: &Api, path: &str) -> Option<Value> {
    let preflight = fetch_preflight(api).await?;
    print_preflight(&preflight, path);
    Some(preflight)
}

fn print_preflight(preflight: &Value, path: &str) {
    if path == "tunnel" {
        match preflight["cloudflared"].as_str() {
            Some("found") => println!("The connector (cloudflared) is on this server."),
            Some("download") => {
                println!("The connector (cloudflared) is not on this server. Setup downloads it.")
            }
            Some("unsupported") => println!(
                "{}",
                preflight["cloudflaredDetail"]
                    .as_str()
                    .unwrap_or("Install cloudflared and set CLOUDFLARED_BIN.")
            ),
            _ => {}
        }
        return;
    }
    print_port(preflight, "httpsPort", "httpsPortState");
    if path == "direct" {
        print_port(preflight, "acmePort", "acmePortState");
    }
    if preflight["inContainer"].as_bool() == Some(true) {
        println!(
            "This server runs in a container. The ports must be published too \
             (deploy/selfhost/install.sh does this)."
        );
    }
}

fn print_port(preflight: &Value, port: &str, state: &str) {
    let port = &preflight[port];
    match preflight[state].as_str() {
        Some("free") => println!("Port {port} is free."),
        Some("serving") => println!("Port {port} is already served by komiq."),
        Some("in_use") => println!("Port {port} is used by another program on this server."),
        Some("denied") => {
            println!("This server does not have permission to open port {port}.");
            println!("Start the server with permission for ports below 1024.");
        }
        // Not known: say nothing rather than guess.
        _ => {}
    }
}

/// Public at the server's own address: no domain, nothing in front. Returns the
/// address as the server stored it, or nothing when the owner put it off. `preset`
/// fixes the HTTPS question (`--encrypt`): the installer opened the ports for it.
async fn direct(api: &Api, old_address: &str, preset: Option<bool>) -> Result<Option<String>> {
    println!("Readers connect straight to this server. Nothing sits in front of it.");
    let preflight = fetch_preflight(api).await;
    let found = |field: &str| preflight.as_ref().map(|p| &p[field]);
    let public_ip = found("publicIp")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let http_port = found("httpPort")
        .and_then(Value::as_u64)
        .map_or_else(|| local_port(&api.origin), |p| p.to_string());
    loop {
        if preset != Some(false) {
            println!("With encryption, komiq gets a free certificate for the address from Let's");
            println!(
                "Encrypt and renews it by itself. Ports 80 and 443 must be open. You agree to"
            );
            println!("the Let's Encrypt Subscriber Agreement: https://letsencrypt.org/repository/");
        }
        let encrypt = match preset {
            Some(choice) => choice,
            None => yes_no("Encrypt the connection (HTTPS)?", true)?,
        };
        match &preflight {
            // Only HTTPS needs ports 443 and 80.
            Some(preflight) if encrypt => print_preflight(preflight, "direct"),
            _ => {}
        }
        if !encrypt {
            println!("Warning: this connection is not encrypted. People on the network path can");
            println!("read passwords and pages.");
        }
        let typed = ask_default(
            if encrypt {
                "This server's public IP address (like 203.0.113.5)"
            } else {
                "This server's public address and port (like 203.0.113.5:8080)"
            },
            &direct_default(old_address, public_ip, &http_port, encrypt),
        )?;
        if encrypt {
            println!("Getting a certificate. This can take a minute.");
        }
        match set_direct_access(api, &typed, encrypt).await {
            Ok(Some(address)) => {
                println!("Saved: readers use {address}.");
                if encrypt {
                    println!("Keep ports 80 and 443 open in your cloud firewall.");
                    println!("Open the console at {address}/admin from now on. After the first");
                    println!(
                        "visit over HTTPS, the plain HTTP port stops answering from the internet."
                    );
                } else {
                    let port = local_port(&address);
                    println!("Open port {port} in your cloud firewall, or nobody can connect.");
                    let container = preflight.as_ref().and_then(|p| p["inContainer"].as_bool());
                    if container == Some(true) {
                        println!("With Docker, the port must also answer other machines: set");
                        println!("KOMIQ_BIND=0.0.0.0 in deploy/selfhost/.env and run");
                        println!("`docker compose up -d` (install.sh --access direct --encrypt no");
                        println!("does this).");
                    }
                }
                return Ok(Some(address));
            }
            Ok(None) => bail!("this server does not know the direct mode; update it first"),
            Err(e) => {
                println!("  {e}");
                if !yes_no("Try again?", true)? {
                    println!(
                        "The address was not saved. Finish in the console: Settings › Access."
                    );
                    return Ok(None);
                }
            }
        }
    }
}

/// Returns the address readers use, as the server reports it for the new mode.
async fn set_access_mode(api: &Api, mode: &str, domain: Option<&str>) -> Result<Option<String>> {
    let data = api
        .call(
            "mutation SetAccessMode($mode: String!, $domain: String) { setAccessMode(mode: \
             $mode, domain: $domain) { mode address } }",
            json!({ "mode": mode, "domain": domain }),
        )
        .await?;
    Ok(data["setAccessMode"]["address"]
        .as_str()
        .map(str::to_string))
}

/// Direct mode at `address`. With `encrypt` the server answers once it has a
/// certificate and serves HTTPS.
async fn set_direct_access(api: &Api, address: &str, encrypt: bool) -> Result<Option<String>> {
    let data = api
        .call(
            "mutation SetDirectAccess($address: String!, $encrypt: Boolean!) { \
             setAccessMode(mode: \"direct\", domain: $address, encrypt: $encrypt) { address } }",
            json!({ "address": address, "encrypt": encrypt }),
        )
        .await?;
    Ok(data["setAccessMode"]["address"]
        .as_str()
        .map(str::to_string))
}

/// Run the server's Cloudflare checklist. False: the owner gave up on this path.
async fn cloudflare_setup(api: &Api, domain: &str, tunnel: bool) -> Result<bool> {
    let preflight = access_preflight(api, if tunnel { "tunnel" } else { "public-ip" }).await;
    let https_port = preflight
        .as_ref()
        .and_then(|p| p["httpsPort"].as_u64())
        .unwrap_or(443);
    println!("\nYou do these 3 things once:");
    println!("  1. Have a Cloudflare account (free).");
    println!(
        "  2. Add your domain to Cloudflare and point its nameservers there (Cloudflare \
         shows the steps)."
    );
    println!("  3. Create an API token (link below) and paste it here.");
    println!("\nkomiq does the rest:");
    if tunnel {
        println!("  Gets the connector (cloudflared), if this server does not have it.");
        println!("  Creates the tunnel and routes it to this server.");
        println!("  Starts the connector.");
    } else {
        println!("  Creates an origin certificate and serves HTTPS on port {https_port}.");
        println!("  Sets SSL to Full (strict).");
    }
    println!("  Creates the DNS record.");
    if !tunnel {
        println!("  Sets the trusted proxies to read visitor addresses from Cloudflare.");
    }
    println!("  Creates cache rules for images and static files.");
    println!("  Sets Cloudflare to respect this server's cache headers.");
    println!("  Tests the public address.");
    println!("\nOpen this link. It fills in part of the form:");
    println!("{}", token_template_url(tunnel));
    println!("Add these permission rows by hand:");
    for (scope, group, level) in token_permissions(tunnel)
        .into_iter()
        .filter(|(_, group, _)| {
            matches!(
                *group,
                "Cloudflare Tunnel" | "Cache Rules" | "Account Rulesets" | "Account Filter Lists"
            )
        })
    {
        println!("  {scope} › {group} › {level}");
    }
    println!("Choose your account and limit Zone Resources to your domain.");
    println!("The token is used for this setup only. The server does not keep it.");

    let token = loop {
        let token = ask_secret("Cloudflare API token (empty to go back)")?
            .trim()
            .to_string();
        if token.is_empty() {
            return Ok(false);
        }
        let checked = api
            .call(
                "mutation CheckCloudflareToken($token: String!, $domain: String!, \
                 $connection: Connection) { checkCloudflareToken(token: $token, domain: $domain, \
                 connection: $connection) { zoneName accountName missing } }",
                json!({
                    "token": token,
                    "domain": domain,
                    "connection": if tunnel { "TUNNEL" } else { "PUBLIC_IP" },
                }),
            )
            .await;
        match checked {
            Ok(data) => {
                let found = &data["checkCloudflareToken"];
                println!(
                    "Found the zone {} in {}.",
                    found["zoneName"].as_str().unwrap_or(domain),
                    found["accountName"]
                        .as_str()
                        .filter(|a| !a.is_empty())
                        .unwrap_or("your account"),
                );
                let missing: Vec<&str> = found["missing"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                if !missing.is_empty() {
                    println!("This token may lack: {}", missing.join(", "));
                    if !yes_no("Continue anyway?", false)? {
                        continue;
                    }
                }
                break token;
            }
            Err(e) => println!("  {e}"),
        }
    };
    let public_ip = if tunnel {
        None
    } else {
        Some(ask_default(
            "This server's public IP address (empty to detect it)",
            preflight
                .as_ref()
                .and_then(|p| p["publicIp"].as_str())
                .unwrap_or_default(),
        )?)
        .filter(|v| !v.is_empty())
    };

    // A start the server refuses (a setup for another domain or connection exists)
    // made no job: there is no failed step to retry, and "retry" would run the OLD
    // setup again. Only an accepted job is retried.
    let mut accepted = false;
    loop {
        let sent = if accepted {
            api.call(
                "mutation RetryCloudflareSetup($token: String!) { retryCloudflareSetup(token: \
                 $token) { mode } }",
                json!({ "token": token }),
            )
            .await
        } else {
            api.call(
                "mutation StartCloudflareSetup($input: CloudflareSetupInput!) { \
                 startCloudflareSetup(input: $input) { mode } }",
                json!({ "input": {
                    "token": token,
                    "domain": domain,
                    "connection": if tunnel { "TUNNEL" } else { "PUBLIC_IP" },
                    "publicIp": public_ip,
                }}),
            )
            .await
        };
        match sent {
            Err(e) => println!("  {e}"),
            Ok(_) => {
                accepted = true;
                if follow_job(api).await? {
                    println!("Your server is online at https://{domain}.");
                    return Ok(true);
                }
            }
        }
        let again = if accepted {
            "Try the failed step again?"
        } else {
            "Try again?"
        };
        if !yes_no(again, true)? {
            return Ok(false);
        }
    }
}

/// Print the checklist as it moves. True: every step is done.
async fn follow_job(api: &Api) -> Result<bool> {
    let mut seen: Vec<String> = Vec::new();
    loop {
        let data = api.call(ACCESS_STATUS, json!({})).await?;
        let job = &data["accessStatus"]["job"];
        let steps = job["steps"].as_array().cloned().unwrap_or_default();
        seen.resize(steps.len(), String::new());
        for (i, s) in steps.iter().enumerate() {
            let state = s["state"].as_str().unwrap_or_default();
            if state == seen[i] || state == "waiting" {
                continue;
            }
            seen[i] = state.to_string();
            let label = s["label"].as_str().unwrap_or_default();
            match (state, s["message"].as_str()) {
                ("failed", Some(why)) => println!("  [failed]  {label}: {why}"),
                ("done", _) => println!("  [done]    {label}"),
                _ => println!("  [{state}] {label}"),
            }
        }
        if job["running"].as_bool() != Some(true) {
            return Ok(job["failed"].as_bool() == Some(false) && !steps.is_empty());
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
}

// The two texts below say what the console's Cloudflare screens say
// (apps/admin/src/lib/cloudflare.ts) and `docs/self-host-cloudflare.md`. Change them
// together.

/// The console builds the same link in apps/admin/src/lib/cloudflare.ts; change them together.
fn token_template_url(tunnel: bool) -> String {
    let mut permissions = vec![
        json!({ "key": "zone", "type": "read" }),
        json!({ "key": "dns", "type": "edit" }),
        json!({ "key": "zone_settings", "type": "edit" }),
    ];
    if !tunnel {
        permissions.push(json!({ "key": "ssl_and_certificates", "type": "edit" }));
    }
    let mut url = reqwest::Url::parse("https://dash.cloudflare.com/profile/api-tokens").unwrap();
    url.query_pairs_mut()
        .append_pair("permissionGroupKeys", &json!(permissions).to_string())
        .append_pair("accountId", "*")
        .append_pair("zoneId", "all")
        .append_pair("name", "komiq");
    url.to_string()
}

/// Rows of Cloudflare's "Create Custom Token" form: scope, group, level.
fn token_permissions(tunnel: bool) -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        if tunnel {
            ("Account", "Cloudflare Tunnel", "Edit")
        } else {
            ("Zone", "SSL and Certificates", "Edit")
        },
        ("Zone", "Zone", "Read"),
        ("Zone", "DNS", "Edit"),
        ("Zone", "Zone Settings", "Edit"),
        ("Zone", "Cache Rules", "Edit"),
        ("Account", "Account Rulesets", "Edit"),
        ("Account", "Account Filter Lists", "Edit"),
    ]
}

/// Dashboard steps that end in the same state automatic setup creates.
fn manual_steps(tunnel: bool, domain: &str, port: &str) -> Vec<String> {
    let mut steps = if tunnel {
        vec![
            "In Cloudflare, open Zero Trust > Networks > Tunnels and create a tunnel of type \
             Cloudflared."
                .to_string(),
            "On this server, run the install command Cloudflare shows. It installs cloudflared \
             with the tunnel token."
                .to_string(),
            format!(
                "Add a public hostname: {domain}, service HTTP, URL localhost:{port}. Cloudflare \
                 creates the DNS record."
            ),
            "Set TRUSTED_PROXY_CIDRS=127.0.0.1/32,::1/128 on the server and restart it, so \
             sign-in limits see real visitor addresses."
                .to_string(),
        ]
    } else {
        vec![
            format!(
                "In DNS, add an A record for {domain} pointing at this server's public address, \
                 with the proxy on."
            ),
            "In SSL/TLS > Origin Server, create a certificate for the domain. Serve it on port \
             443 with a reverse proxy in front of this server."
                .to_string(),
            "In SSL/TLS > Overview, set the encryption mode to Full (strict).".to_string(),
            "Set TRUSTED_PROXY_CIDRS to the ranges listed at cloudflare.com/ips and restart the \
             server."
                .to_string(),
        ]
    };
    steps.push(format!(
        "In Caching > Cache Rules, add a rule for {domain} where the path starts with /covers/, \
         /api/v1/manga/, /_app/immutable/ or /admin/_app/immutable/, or equals /img. Set it to \
         eligible for cache, with edge and browser TTL taken from the origin."
    ));
    steps.push(
        "In Caching > Configuration, set Browser Cache TTL to Respect Existing Headers."
            .to_string(),
    );
    steps.push(format!("Open https://{domain}/health. It should say ok."));
    steps
}

/// The local port the server listens on, as this tool reaches it.
fn local_port(origin: &str) -> String {
    reqwest::Url::parse(origin)
        .ok()
        .and_then(|u| u.port_or_known_default())
        .map_or_else(|| "8080".to_string(), |p| p.to_string())
}

// ---- step 3: accounts -----------------------------------------------------------

/// Returns the sign-up switch as the server holds it, and the people added now with
/// their temporary passwords.
async fn accounts(api: &Api, open_now: bool) -> Result<(bool, Vec<(String, String)>)> {
    let want_open = choose(
        "Who can sign in? Libraries stay separate for every account.",
        &[
            (
                "Only people I add",
                "You create accounts. Nobody can sign up on their own.",
            ),
            (
                "Anyone can sign up",
                "The reader shows a sign-up form to every visitor.",
            ),
        ],
        usize::from(open_now),
    )? == 1;
    let open = match api
        .call(
            "mutation SetRegistrationOpen($open: Boolean!) { setRegistrationOpen(open: $open) }",
            json!({ "open": want_open }),
        )
        .await
    {
        Ok(data) => data["setRegistrationOpen"].as_bool().unwrap_or(want_open),
        // Pinned by the server's environment: nothing to change here, carry on.
        Err(e) if e.to_string().contains("KOMIKA_REGISTRATION_OPEN") => {
            println!(
                "  Sign-up is fixed by this server's configuration. The choice was not applied."
            );
            open_now
        }
        Err(e) => {
            println!("  The setting was not saved: {e}");
            open_now
        }
    };

    let mut added = Vec::new();
    println!("\nAdd people now (optional). Each one gets a temporary password and picks a");
    println!("new one at the first sign-in.");
    loop {
        let username = ask("Username (empty to continue)")?;
        if username.is_empty() {
            break;
        }
        if username.chars().count() < 3 {
            println!("  Usernames need at least 3 characters.");
            continue;
        }
        let email = ask("Email")?;
        if !email.contains('@') {
            println!("  Enter an email address.");
            continue;
        }
        let password = temp_password();
        match api
            .call(
                "mutation CreateUser($input: CreateUserInput!) { createUser(input: $input) { id } }",
                json!({ "input": { "username": username, "email": email, "password": password } }),
            )
            .await
        {
            Ok(_) => {
                println!("  Added {username}. Temporary password: {password}");
                added.push((username, password));
            }
            Err(e) => println!("  {e}"),
        }
    }
    Ok((open, added))
}

/// A password to hand to a new user once (same shape as the console's).
fn temp_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyzABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut bytes = [0u8; 12];
    OsRng.fill_bytes(&mut bytes);
    let raw: Vec<char> = bytes
        .iter()
        .map(|b| ALPHABET[usize::from(*b) % ALPHABET.len()] as char)
        .collect();
    raw.chunks(4)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

// ---- step 4: sources ------------------------------------------------------------

const EXTENSIONS: &str = "query Extensions($refresh: Boolean!) { extensions(refresh: $refresh) \
     { pkgName name lang versionName isInstalled isNsfw } }";

/// Returns the summary line ("MangaDex off, 2 extensions").
async fn sources(api: &Api) -> Result<String> {
    println!("No sources come with komiq. Pick where your library comes from, or skip");
    println!("and do it later in the console.");

    let mut mangadex = false;
    match api
        .call(
            "query MangadexSetting { mangadexSetting { enabled pinned pinnedBy } }",
            json!({}),
        )
        .await
    {
        Ok(data) => {
            let s = &data["mangadexSetting"];
            mangadex = s["enabled"].as_bool().unwrap_or(false);
            if s["pinned"].as_bool() == Some(true) {
                println!(
                    "\nMangaDex is {} (fixed by {} in this server's configuration).",
                    on_off(mangadex),
                    s["pinnedBy"].as_str().unwrap_or("the environment"),
                );
            } else {
                println!("\nMangaDex: when on, the server fetches the MangaDex catalogue, covers");
                println!("and chapter pages. When off, it never contacts MangaDex.");
                let want = yes_no("Use MangaDex?", mangadex)?;
                match api
                    .call(
                        "mutation SetMangadexEnabled($enabled: Boolean!) { \
                         setMangadexEnabled(enabled: $enabled) }",
                        json!({ "enabled": want }),
                    )
                    .await
                {
                    Ok(d) => mangadex = d["setMangadexEnabled"].as_bool().unwrap_or(want),
                    Err(e) => println!("  The setting was not saved: {e}"),
                }
            }
        }
        Err(e) => println!("  MangaDex could not be read: {e}"),
    }

    println!("\nExtension repo (optional): paste the index address a repo publishes,");
    println!("like https://example.org/index.min.json.");
    loop {
        let url = ask("Repo index address (empty to continue)")?;
        if url.is_empty() {
            break;
        }
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            println!("  Enter the repo's index address, starting with https://");
            continue;
        }
        let added = api
            .call(
                "mutation AddExtensionRepo($indexUrl: String!) { addExtensionRepo(indexUrl: \
                 $indexUrl) }",
                json!({ "indexUrl": url }),
            )
            .await;
        match added {
            Ok(_) => println!("  Repo added."),
            Err(e) if engine_down(&e) => break,
            Err(e) => println!("  {e}"),
        }
    }
    // Everything the server's repos list, including repos added on an earlier run.
    let installed = match api.call(EXTENSIONS, json!({ "refresh": false })).await {
        Ok(data) => {
            let list = data["extensions"].as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                0
            } else {
                pick_extensions(api, list).await?
            }
        }
        Err(e) => {
            if !engine_down(&e) {
                println!("  The extension list could not be loaded: {e}");
            }
            0
        }
    };
    Ok(format!(
        "MangaDex {}, {installed} {}",
        on_off(mangadex),
        if installed == 1 {
            "extension"
        } else {
            "extensions"
        }
    ))
}

fn on_off(on: bool) -> &'static str {
    if on {
        "on"
    } else {
        "off"
    }
}

/// The engine may still be starting: say so calmly and move on.
fn engine_down(e: &anyhow::Error) -> bool {
    let down = e.to_string().contains(ENGINE_UNREACHABLE);
    if down {
        println!("  The source engine is not reachable yet. Add sources later in the console.");
    }
    down
}

/// Search-and-install loop over a repo's extensions. Returns how many are installed.
async fn pick_extensions(api: &Api, mut list: Vec<Value>) -> Result<usize> {
    println!(
        "\n{} extensions available. Type part of a name (or a language code like en) to",
        list.len()
    );
    println!("search, a number to install, or press Enter to finish.");
    // Indexes into `list` for the rows printed last.
    let mut shown: Vec<usize> = Vec::new();
    loop {
        let input = ask(">")?;
        if input.is_empty() {
            break;
        }
        if let Ok(n) = input.parse::<usize>() {
            let Some(&i) = n.checked_sub(1).and_then(|n| shown.get(n)) else {
                println!("  Search first, then type a number from the list.");
                continue;
            };
            let name = list[i]["name"].as_str().unwrap_or_default().to_string();
            match api
                .call(
                    "mutation InstallExtension($pkgName: String!) { installExtension(pkgName: \
                     $pkgName) { pkgName isInstalled } }",
                    json!({ "pkgName": list[i]["pkgName"] }),
                )
                .await
            {
                Ok(_) => {
                    list[i]["isInstalled"] = json!(true);
                    println!("  Installed {name}.");
                }
                Err(e) => println!("  {name} was not installed: {e}"),
            }
            continue;
        }
        let all = matching(&list, &input);
        shown = all.iter().copied().take(PICKER_ROWS).collect();
        if shown.is_empty() {
            println!("  Nothing matches \"{input}\".");
            continue;
        }
        for (n, &i) in shown.iter().enumerate() {
            let e = &list[i];
            println!(
                "  {:>2}) {} ({}) {}{}{}",
                n + 1,
                e["name"].as_str().unwrap_or_default(),
                e["lang"].as_str().unwrap_or_default(),
                e["versionName"].as_str().unwrap_or_default(),
                if e["isNsfw"].as_bool() == Some(true) {
                    "  [18+]"
                } else {
                    ""
                },
                if e["isInstalled"].as_bool() == Some(true) {
                    "  [installed]"
                } else {
                    ""
                },
            );
        }
        if all.len() > shown.len() {
            println!(
                "  Showing {} of {}. Type more letters to narrow it.",
                shown.len(),
                all.len()
            );
        }
    }
    Ok(list
        .iter()
        .filter(|e| e["isInstalled"].as_bool() == Some(true))
        .count())
}

/// Extensions whose name or package contains `query`, or whose language IS `query`.
fn matching(list: &[Value], query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    let has = |e: &Value, key: &str| {
        e[key]
            .as_str()
            .is_some_and(|v| v.to_lowercase().contains(&q))
    };
    list.iter()
        .enumerate()
        .filter(|(_, e)| {
            has(e, "name") || has(e, "pkgName") || e["lang"].as_str() == Some(q.as_str())
        })
        .map(|(i, _)| i)
        .collect()
}

// ---- asking ---------------------------------------------------------------------

/// One trimmed line of input. Input that ends (Ctrl-D, an exhausted pipe) stops setup.
fn read_line() -> Result<String> {
    Ok(read_raw_line()?.trim().to_string())
}

/// One line as typed, without its line ending.
fn read_raw_line() -> Result<String> {
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line)? == 0 {
        bail!("input ended; run `komika-server setup` again to continue");
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

fn ask(label: &str) -> Result<String> {
    print!("{label}{} ", if label.ends_with('>') { "" } else { ":" });
    io::stdout().flush()?;
    read_line()
}

fn ask_default(label: &str, default: &str) -> Result<String> {
    if default.is_empty() {
        return ask(label);
    }
    let answer = ask(&format!("{label} [{default}]"))?;
    Ok(if answer.is_empty() {
        default.to_string()
    } else {
        answer
    })
}

fn ask_valid(label: &str, check: impl Fn(&str) -> Result<(), &'static str>) -> Result<String> {
    loop {
        let answer = ask(label)?;
        match check(&answer) {
            Ok(()) => return Ok(answer),
            Err(why) => println!("  {why}"),
        }
    }
}

/// A secret: not echoed on a terminal, a plain line when input is piped. Kept as
/// typed: a password may begin or end with a space.
fn ask_secret(label: &str) -> Result<String> {
    if io::stdin().is_terminal() {
        Ok(rpassword::prompt_password(format!("{label}: "))?)
    } else {
        print!("{label}: ");
        io::stdout().flush()?;
        read_raw_line()
    }
}

fn yes_no(label: &str, default: bool) -> Result<bool> {
    loop {
        let answer = ask(&format!(
            "{label} [{}]",
            if default { "Y/n" } else { "y/N" }
        ))?;
        match answer.to_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("  Answer y or n."),
        }
    }
}

/// A numbered choice. Returns the index of the option picked.
fn choose(title: &str, options: &[(&str, &str)], default: usize) -> Result<usize> {
    println!("{title}");
    for (i, (name, about)) in options.iter().enumerate() {
        if about.is_empty() {
            println!("  {}) {name}", i + 1);
        } else {
            println!("  {}) {name}: {about}", i + 1);
        }
    }
    loop {
        let answer = ask(&format!("Choice [{}]", default + 1))?;
        if answer.is_empty() {
            return Ok(default);
        }
        match answer.parse::<usize>() {
            Ok(n) if (1..=options.len()).contains(&n) => return Ok(n - 1),
            _ => println!("  Type a number from 1 to {}.", options.len()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_supply_the_setup_code_and_access_choices() {
        let parse = |args: &[&str]| {
            parse_args(
                7580,
                &args.iter().map(|v| v.to_string()).collect::<Vec<_>>(),
                Some("env-code".into()),
            )
        };
        assert_eq!(parse(&[]).unwrap().setup_code.as_deref(), Some("env-code"));
        for choice in ["local", "internet", "tunnel", "public-ip", "direct"] {
            let args = parse(&[
                "--server",
                "http://localhost:9000/graphql/",
                "--setup-code",
                "flag-code",
                "--access",
                choice,
            ])
            .unwrap();
            assert_eq!(args.origin, "http://localhost:9000");
            assert!(args.chosen_server);
            assert_eq!(args.setup_code.as_deref(), Some("flag-code"));
            assert_eq!(args.access.as_deref(), Some(choice));
            assert_eq!(args.encrypt, None);
        }
        for (word, encrypt) in [("yes", true), ("no", false)] {
            let args = parse(&["--access", "direct", "--encrypt", word]).unwrap();
            assert_eq!(args.encrypt, Some(encrypt));
        }
        for args in [
            // Only the direct mode has the HTTPS question.
            vec!["--encrypt", "yes"],
            vec!["--access", "tunnel", "--encrypt", "no"],
            vec!["--access", "direct", "--encrypt", "maybe"],
            vec!["--access", "wrong"],
            vec!["--setup-code"],
            vec!["--access", "--server"],
            vec!["--unknown"],
            vec!["--server", ""],
        ] {
            assert!(parse(&args).unwrap_err().to_string().contains(USAGE));
        }
    }

    #[test]
    fn a_typed_domain_loses_its_scheme_and_path() {
        for typed in [
            "read.example.com",
            " https://Read.Example.com/ ",
            "http://read.example.com/admin",
            "READ.example.com.",
        ] {
            assert_eq!(clean_domain(typed), "read.example.com", "{typed}");
        }
    }

    /// The offered address fits the HTTPS choice, whatever was stored before.
    #[test]
    fn the_direct_address_default_follows_the_encryption_choice() {
        let default = |old: &str, encrypt| direct_default(old, "203.0.113.5", "8080", encrypt);
        // A first run: the address komiq found.
        assert_eq!(default("", true), "203.0.113.5");
        assert_eq!(default("", false), "203.0.113.5:8080");
        // HTTPS turned off: the HTTPS address would lead nowhere.
        assert_eq!(default("https://198.51.100.7", false), "198.51.100.7:8080");
        // HTTPS turned on: no port, HTTPS has its own.
        assert_eq!(default("http://198.51.100.7:9000", true), "198.51.100.7");
        // Nothing changes: what the owner had.
        assert_eq!(
            default("http://198.51.100.7:9000", false),
            "198.51.100.7:9000"
        );
        assert_eq!(
            default("https://read.example.com", true),
            "read.example.com"
        );
        // IPv6 needs brackets next to a port.
        assert_eq!(default("https://[2001:db8::1]", true), "2001:db8::1");
        assert_eq!(
            default("https://[2001:db8::1]", false),
            "[2001:db8::1]:8080"
        );
        assert_eq!(direct_default("", "", "8080", false), "");
    }

    #[tokio::test]
    async fn a_public_plain_http_server_is_refused_before_any_secret() {
        for clear in ["http://203.0.113.5:8080", "http://[2001:db8::1]:8080"] {
            assert!(sends_in_clear(clear).await, "{clear}");
        }
        for fine in [
            "http://127.0.0.1:8080",
            "http://[::1]:8080",
            "http://192.168.1.20:8080",
            "http://10.0.0.4",
            "http://100.64.1.2:8080",
            "https://203.0.113.5",
            "not an address",
        ] {
            assert!(!sends_in_clear(fine).await, "{fine}");
        }
    }

    #[test]
    fn token_template_uses_only_documented_keys() {
        for tunnel in [true, false] {
            let url = reqwest::Url::parse(&token_template_url(tunnel)).unwrap();
            let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
            assert_eq!(query["accountId"], "*");
            assert_eq!(query["zoneId"], "all");
            assert_eq!(query["name"], "komiq");
            let mut expected = json!([
                {"key": "zone", "type": "read"}, {"key": "dns", "type": "edit"}, {"key": "zone_settings", "type": "edit"}
            ]);
            if !tunnel {
                expected
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"key": "ssl_and_certificates", "type": "edit"}));
            }
            assert_eq!(
                serde_json::from_str::<Value>(&query["permissionGroupKeys"]).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn temp_password_has_the_console_shape() {
        let p = temp_password();
        assert_eq!(p.len(), 14);
        let groups: Vec<&str> = p.split('-').collect();
        assert_eq!(groups.len(), 3);
        assert!(groups
            .iter()
            .all(|g| g.len() == 4 && g.chars().all(|c| c.is_ascii_alphanumeric())));
        assert_ne!(p, temp_password());
    }

    #[test]
    fn local_port_reads_the_address() {
        assert_eq!(local_port("http://127.0.0.1:7580"), "7580");
        assert_eq!(local_port("http://komiq.lan"), "80");
        assert_eq!(local_port("https://read.example.com"), "443");
        assert_eq!(local_port("not an address"), "8080");
    }

    #[test]
    fn picker_matches_name_package_or_exact_language() {
        let list = vec![
            json!({ "name": "Alpha Scans", "pkgName": "ext.en.alpha", "lang": "en" }),
            json!({ "name": "Gamma Manga", "pkgName": "ext.es.gamma", "lang": "es" }),
            json!({ "name": "Delta Toons", "pkgName": "ext.fr.delta", "lang": "fr" }),
        ];
        assert_eq!(matching(&list, "ALPHA"), vec![0]);
        assert_eq!(matching(&list, "ext.fr"), vec![2]);
        // A language code: Gamma's language, and part of its package name.
        assert_eq!(matching(&list, "es"), vec![1]);
        assert_eq!(matching(&list, "en"), vec![0]);
        // Part of two names.
        assert_eq!(matching(&list, "a"), vec![0, 1, 2]);
        assert!(matching(&list, "zzz").is_empty());
    }

    #[test]
    fn manual_steps_name_the_domain_and_port() {
        let tunnel = manual_steps(true, "read.example.com", "7580");
        assert!(tunnel[2].contains("read.example.com") && tunnel[2].contains("localhost:7580"));
        assert_eq!(tunnel.len(), 7);
        let public = manual_steps(false, "read.example.com", "7580");
        assert!(public[0].contains("A record for read.example.com"));
        assert_eq!(public.len(), 7);
        assert_eq!(token_permissions(true)[0].1, "Cloudflare Tunnel");
        assert_eq!(token_permissions(false)[0].1, "SSL and Certificates");
    }
}
