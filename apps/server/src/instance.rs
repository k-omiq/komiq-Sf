//! Instance settings and first-run setup (self-host).
//!
//! **Settings.** Each [`Setting`] resolves in a fixed order: its env variable (when
//! set and non-empty), then its row in `instance_setting`, then its built-in default.
//! Env always wins, so a deployment that pins a value in its environment (the hosted
//! instance) cannot be changed from the admin console. Keys are a fixed allow-list
//! here; nothing writes a key that is not one of the constants below.
//!
//! **Setup.** An instance with at least one admin account is "set up". Until then the
//! server issues a one-time [`SetupCode`] at boot and prints it to the log once; the
//! first-run screen trades it for the owner account (`createOwner`). The code is held
//! in memory only, as a hash, and is never returned by the API.

use std::sync::Mutex;

use crate::db::Database;
use chrono::Utc;
use rand_core::{OsRng, RngCore};

/// One runtime-changeable instance setting.
pub struct Setting {
    /// Primary key in `instance_setting`.
    pub key: &'static str,
    /// Env variable that overrides the stored value. Empty: no env override (state
    /// that only the server itself writes, e.g. what Cloudflare setup created).
    pub env: &'static str,
    /// Value used when neither env nor table has one.
    pub default: &'static str,
}

/// Display name of the instance (setup screen, page titles).
pub const INSTANCE_NAME: Setting = Setting {
    key: "instance_name",
    env: "KOMIKA_INSTANCE_NAME",
    default: "komiq",
};

/// Whether `register` accepts new accounts. Read it through
/// [`resolve_registration`], not the generic [`resolve`]: the built-in default below
/// (open, how every deployment behaved before this setting existed) holds everywhere
/// except a self-hosted install, which starts closed.
pub const REGISTRATION_OPEN: Setting = Setting {
    key: "registration_open",
    env: "KOMIKA_REGISTRATION_OPEN",
    default: "true",
};

/// The MangaDex USAGE switch: whether this server may talk to MangaDex at all
/// (reading, covers, metadata, the admin MangaDex jobs, `/img` for MangaDex hosts).
/// Resolved by [`resolve_mangadex`], not the generic [`resolve`], because two env
/// variables and the install kind take part. `KOMIKA_MANGADEX` is its own env
/// override; the `instance_setting` row is `mangadex_enabled`.
///
/// The catalogue CRAWL is a separate decision ([`catalogue_crawl`]): `CATALOGUE_SYNC`
/// keeps meaning "run the crawl on this replica", which the hosted fleet runs on
/// exactly one replica while every replica still reads from MangaDex.
pub const MANGADEX_ENABLED: Setting = Setting {
    key: "mangadex_enabled",
    env: "KOMIKA_MANGADEX",
    default: "true",
};

/// The resolved usage switch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MangadexUsage {
    pub enabled: bool,
    pub source: Source,
    /// The env variable that fixes the value (`KOMIKA_MANGADEX` or `CATALOGUE_SYNC`);
    /// the admin console cannot change it then.
    pub pinned_by: Option<&'static str>,
}

/// A self-hosted install: the server serves the web apps itself. The self-host
/// Docker image and the desktop app set `KOMIKA_WEB_DIR`; the hosted API does not.
/// The same signal mounts `/img`.
pub fn is_self_host(env: &impl Fn(&str) -> Option<String>) -> bool {
    env("KOMIKA_WEB_DIR").is_some_and(|v| !v.trim().is_empty())
}

fn env_value(env: &impl Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    env(name).filter(|v| !v.trim().is_empty())
}

/// Resolve the usage switch, first match wins:
///
/// 1. `KOMIKA_MANGADEX` (on/off) — explicit, pins it either way.
/// 2. `CATALOGUE_SYNC=on` — pins it ON: a replica told to crawl cannot crawl without
///    MangaDex. `CATALOGUE_SYNC=off` says nothing about usage (the hosted replicas
///    that do not crawl still read from MangaDex).
/// 3. The admin's stored choice (`setMangadexEnabled`).
/// 4. The default: OFF on a self-hosted install (plan open point 3), ON everywhere
///    else, so every deployment that predates the switch keeps using MangaDex
///    exactly as it did.
///
/// What else rides on it, as additional env knobs, never a way around it:
/// `METADATA_BACKFILL=on` and `COVER_PHASH=on` act only while usage is on;
/// `COVER_CACHE=off` still stops the whole cover drainer, Suwayomi covers included
/// (with it on and usage off, the drainer still caches Suwayomi covers).
pub async fn resolve_mangadex_with(
    pool: &Database,
    env: impl Fn(&str) -> Option<String>,
) -> MangadexUsage {
    if let Some(v) = env_value(&env, "KOMIKA_MANGADEX") {
        return MangadexUsage {
            enabled: parse_bool(&v),
            source: Source::Env,
            pinned_by: Some("KOMIKA_MANGADEX"),
        };
    }
    if env_value(&env, "CATALOGUE_SYNC").is_some_and(|v| parse_bool(&v)) {
        return MangadexUsage {
            enabled: true,
            source: Source::Env,
            pinned_by: Some("CATALOGUE_SYNC"),
        };
    }
    // The table and the install-kind default; no env variable is consulted here (the
    // generic resolver's env step is the `KOMIKA_MANGADEX` case already handled).
    let r = resolve_with(pool, &MANGADEX_ENABLED, |_| None).await;
    let enabled = match r.source {
        Source::Table => r.as_bool(),
        _ => !is_self_host(&env),
    };
    MangadexUsage {
        enabled,
        source: r.source,
        pinned_by: None,
    }
}

/// [`resolve_mangadex_with`] against the process environment.
pub async fn resolve_mangadex(pool: &Database) -> MangadexUsage {
    resolve_mangadex_with(pool, |name| std::env::var(name).ok()).await
}

/// Resolve the sign-up switch: `KOMIKA_REGISTRATION_OPEN`, then the owner's stored
/// choice, then the default. The default is CLOSED on a self-hosted install ("Only
/// people I add" is the wizard's default, and a fresh server must not take strangers'
/// accounts before its owner has chosen) and open everywhere else.
pub async fn resolve_registration_with(
    pool: &Database,
    env: impl Fn(&str) -> Option<String>,
) -> Resolved {
    let self_host = is_self_host(&env);
    let mut r = resolve_with(pool, &REGISTRATION_OPEN, env).await;
    if r.source == Source::Default && self_host {
        r.value = "false".to_string();
    }
    r
}

/// [`resolve_registration_with`] against the process environment.
pub async fn resolve_registration(pool: &Database) -> Resolved {
    resolve_registration_with(pool, |name| std::env::var(name).ok()).await
}

/// Whether this process runs the MangaDex catalogue crawl (the catalogue/chapter
/// sync and its backfill drains).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogueCrawl {
    /// `CATALOGUE_SYNC=on`: spawned, exactly as before.
    Run,
    /// Self-host with `CATALOGUE_SYNC` unset: spawned, and runs whenever the usage
    /// switch is on — the admin's MangaDex switch is the whole story there.
    FollowSwitch,
    /// `CATALOGUE_SYNC=off`, or unset on a non-self-host server: never spawned, as
    /// before this switch existed.
    Never,
}

/// Decide [`CatalogueCrawl`]. Whatever this says, a spawned loop still waits on the
/// usage switch between passes, so `KOMIKA_MANGADEX=off` stops even a `Run` crawl.
pub fn catalogue_crawl(env: impl Fn(&str) -> Option<String>) -> CatalogueCrawl {
    match env_value(&env, "CATALOGUE_SYNC") {
        Some(v) if parse_bool(&v) => CatalogueCrawl::Run,
        Some(_) => CatalogueCrawl::Never,
        None if is_self_host(&env) => CatalogueCrawl::FollowSwitch,
        None => CatalogueCrawl::Never,
    }
}

/// Whether reader apps must fetch chapter pages from the sources themselves instead of
/// through this server ("this server does not fetch for apps"). A statement of policy
/// the apps obey where they can: an app that has its own source engine then offers no
/// way to fetch through the server. Nothing is refused here — the web reader, and an
/// app without an engine, still read through `pages` as before. Off by default; the
/// shared public server turns it on so its readers do not all fetch through it.
pub const APPS_FETCH_THEMSELVES: Setting = Setting {
    key: "apps_fetch_themselves",
    env: "KOMIKA_APPS_FETCH_THEMSELVES",
    default: "false",
};

/// Proxies whose `X-Forwarded-For` / `CF-Connecting-IP` the server believes, as
/// comma-separated CIDRs. A deployment that sets `TRUSTED_PROXY_CIDRS` (the hosted
/// instance) keeps exactly its list; otherwise Cloudflare setup fills the stored value
/// (loopback for a tunnel, Cloudflare's ranges for a public IP). Empty: always key on
/// the socket peer.
pub const TRUSTED_PROXY_CIDRS: Setting = Setting {
    key: "trusted_proxy_cidrs",
    env: "TRUSTED_PROXY_CIDRS",
    default: "",
};

/// The wizard's access choice: `local` (only my devices), `internet` (a domain with
/// Cloudflare in front) or `direct` (public at the server's own address, nothing in
/// front). Display only: it decides what Settings › Access shows, not what the server
/// listens on. Empty until someone chooses.
pub const ACCESS_MODE: Setting = Setting {
    key: "access_mode",
    env: "",
    default: "",
};

/// The public domain readers use in internet mode (`https://<domain>`), set by
/// Cloudflare setup or by an owner who did it by hand. Display only.
pub const PUBLIC_DOMAIN: Setting = Setting {
    key: "public_domain",
    env: "",
    default: "",
};

/// The address readers type in direct mode, as an origin (`https://203.0.113.5`, or
/// `http://203.0.113.5:8080` without encryption): the server's own public address, with
/// no domain and no Cloudflare. Display only.
pub const PUBLIC_ADDRESS: Setting = Setting {
    key: "public_address",
    env: "",
    default: "",
};

/// Direct mode with HTTPS: the IP address or name the server holds a Let's Encrypt
/// certificate for, and keeps renewing (`cloudflare/direct.rs`). Empty: direct mode is
/// plain HTTP, or not in use.
pub const DIRECT_TLS_HOST: Setting = Setting {
    key: "direct_tls_host",
    env: "",
    default: "",
};

/// Direct mode with HTTPS: `1` once a client on the internet has used the HTTPS
/// address. From then on the plain HTTP port refuses clients on the internet
/// (`cloudflare/direct.rs`). Empty before that, so a closed port 443 locks nobody out.
pub const DIRECT_HTTPS_SEEN: Setting = Setting {
    key: "direct_https_seen",
    env: "",
    default: "",
};

/// JSON record of what Cloudflare setup created (ids for "Remove"); see
/// `cloudflare::Stored`. Never holds a token.
pub const CLOUDFLARE_STATE: Setting = Setting {
    key: "cloudflare_state",
    env: "",
    default: "",
};

/// Where a resolved value came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Env,
    Table,
    Default,
}

/// A setting's effective value and its origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub value: String,
    pub source: Source,
}

impl Resolved {
    pub fn as_bool(&self) -> bool {
        parse_bool(&self.value)
    }
}

/// The server's on/off spelling (`on` / `1` / `true`, any case); anything else is
/// off. Matches how `Config::from_env` reads `CATALOGUE_SYNC` and friends.
pub fn parse_bool(v: &str) -> bool {
    let v = v.trim().to_ascii_lowercase();
    v == "on" || v == "1" || v == "true"
}

/// Resolve `s` against the process environment.
pub async fn resolve(pool: &Database, s: &Setting) -> Resolved {
    resolve_with(pool, s, |name| std::env::var(name).ok()).await
}

/// Resolve `s` with an injected env lookup, so tests can exercise the read order
/// without racing sibling tests over process-global env variables.
pub async fn resolve_with(
    pool: &Database,
    s: &Setting,
    env: impl Fn(&str) -> Option<String>,
) -> Resolved {
    if let Some(v) = Some(s.env)
        .filter(|name| !name.is_empty())
        .and_then(&env)
        .filter(|v| !v.trim().is_empty())
    {
        return Resolved {
            value: v,
            source: Source::Env,
        };
    }
    let stored: Option<String> =
        sqlx::query_scalar("SELECT value FROM instance_setting WHERE key = ?")
            .bind(s.key)
            .fetch_optional(pool)
            .await
            // A read failure must not take the setting down with it: log it and
            // fall through to the default.
            .inspect_err(
                |e| tracing::warn!(error = %e, key = s.key, "instance_setting read failed"),
            )
            .ok()
            .flatten();
    match stored {
        Some(value) => Resolved {
            value,
            source: Source::Table,
        },
        None => Resolved {
            value: s.default.to_string(),
            source: Source::Default,
        },
    }
}

/// Store `value` for `s` (upsert). The caller decides whether an env override
/// makes the write pointless; this only touches the table.
pub async fn store(pool: &Database, s: &Setting, value: &str) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO instance_setting (key, value, updated_at) VALUES (?, ?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(s.key)
    .bind(value)
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}

/// True once any admin account exists — the definition of "set up".
pub async fn admin_exists(pool: &Database) -> sqlx::Result<bool> {
    let exists: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE is_admin = 1)")
        .fetch_one(pool)
        .await?;
    Ok(exists != 0)
}

/// Crockford base32 (no I, L, O, U): easy to read off a log and type back in.
const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// 12 symbols × 5 bits = 60 bits, far beyond what the rate-limited
/// `createOwner` lets anyone guess.
const CODE_LEN: usize = 12;

/// The one-time first-run setup code. Only its SHA-256 is kept.
#[derive(Default)]
pub struct SetupCode(Mutex<Option<String>>);

impl SetupCode {
    /// Generate a fresh code, remember its hash, and return the plaintext
    /// (formatted `XXXX-XXXX-XXXX`) for the one log line that shows it.
    pub fn issue(&self) -> String {
        let mut bytes = [0u8; CODE_LEN];
        OsRng.fill_bytes(&mut bytes);
        let raw: String = bytes
            .iter()
            .map(|b| CODE_ALPHABET[(b & 31) as usize] as char)
            .collect();
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(crate::auth::hash_token(&raw));
        format!("{}-{}-{}", &raw[0..4], &raw[4..8], &raw[8..12])
    }

    /// Does `input` match the live code? Dashes, spaces and case are ignored, so
    /// the code can be typed back loosely. Always false once cleared.
    pub fn verify(&self, input: &str) -> bool {
        let normalized: String = input
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .map(|c| c.to_ascii_uppercase())
            .collect();
        let guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        // Compares digests, so timing says nothing useful about the code itself.
        guard
            .as_deref()
            .is_some_and(|h| h == crate::auth::hash_token(&normalized))
    }

    /// Retire the code (the owner account now exists).
    pub fn clear(&self) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    pub fn is_live(&self) -> bool {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).is_some()
    }
}

/// Boot step: when no admin exists, issue a setup code and print it once.
/// Runs after `provision_admins`, so an env-seeded admin means no code at all.
pub async fn issue_setup_code_if_needed(pool: &Database, code: &SetupCode) -> anyhow::Result<()> {
    if admin_exists(pool).await? {
        return Ok(());
    }
    let plain = code.issue();
    tracing::warn!(
        "no admin account yet: open the setup screen and enter setup code {plain} \
         (valid until the owner account is created; a restart issues a new code)"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::TestPoolOptions;

    async fn pool() -> Database {
        let pool = TestPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations")
            .run(pool.write())
            .await
            .unwrap();
        pool
    }

    const TEST: Setting = Setting {
        key: "test_key",
        env: "KOMIKA_TEST_SETTING",
        default: "fallback",
    };

    #[tokio::test]
    async fn settings_read_env_then_table_then_default() {
        let pool = pool().await;
        let no_env = |_: &str| None;
        let env = |name: &str| (name == "KOMIKA_TEST_SETTING").then(|| "from-env".to_string());
        let blank_env = |_: &str| Some("  ".to_string());

        // Nothing anywhere: the default.
        let r = resolve_with(&pool, &TEST, no_env).await;
        assert_eq!((r.value.as_str(), r.source), ("fallback", Source::Default));
        // A blank env value counts as unset.
        let r = resolve_with(&pool, &TEST, blank_env).await;
        assert_eq!(r.source, Source::Default);

        // A stored row beats the default.
        store(&pool, &TEST, "from-table").await.unwrap();
        let r = resolve_with(&pool, &TEST, no_env).await;
        assert_eq!((r.value.as_str(), r.source), ("from-table", Source::Table));

        // Env beats the stored row.
        let r = resolve_with(&pool, &TEST, env).await;
        assert_eq!((r.value.as_str(), r.source), ("from-env", Source::Env));

        // Store is an upsert.
        store(&pool, &TEST, "again").await.unwrap();
        let r = resolve_with(&pool, &TEST, no_env).await;
        assert_eq!(r.value, "again");
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM instance_setting")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn sign_up_starts_closed_only_on_a_self_hosted_install() {
        let pool = pool().await;
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        const WEB: (&str, &str) = ("KOMIKA_WEB_DIR", "/srv/web");

        // Hosted (no web dir): open, as before the setting existed.
        let r = resolve_registration_with(&pool, env(&[])).await;
        assert!(r.as_bool());
        assert_eq!(r.source, Source::Default);
        // Self-host: closed until the owner says otherwise.
        let r = resolve_registration_with(&pool, env(&[WEB])).await;
        assert!(!r.as_bool());
        assert_eq!(r.source, Source::Default);
        // The owner's stored choice beats the default, both ways.
        store(&pool, &REGISTRATION_OPEN, "true").await.unwrap();
        assert!(resolve_registration_with(&pool, env(&[WEB]))
            .await
            .as_bool());
        store(&pool, &REGISTRATION_OPEN, "false").await.unwrap();
        assert!(!resolve_registration_with(&pool, env(&[])).await.as_bool());
        // Env beats both.
        let r =
            resolve_registration_with(&pool, env(&[("KOMIKA_REGISTRATION_OPEN", "on"), WEB])).await;
        assert!(r.as_bool());
        assert_eq!(r.source, Source::Env);
    }

    /// The deployment matrix: what each kind of server uses and crawls.
    #[tokio::test]
    async fn mangadex_usage_and_crawl_across_deployments() {
        let pool = pool().await;
        fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
            move |name| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        }
        const WEB: (&str, &str) = ("KOMIKA_WEB_DIR", "/srv/web");

        // (a) Hosted primary: crawls, so it must use MangaDex; pinned.
        let e = env(&[("CATALOGUE_SYNC", "on")]);
        let u = resolve_mangadex_with(&pool, &e).await;
        assert!(u.enabled);
        assert_eq!(u.pinned_by, Some("CATALOGUE_SYNC"));
        assert_eq!(catalogue_crawl(&e), CatalogueCrawl::Run);

        // (b) Hosted replica: no crawl here, but reading/covers still use MangaDex.
        let e = env(&[("CATALOGUE_SYNC", "off")]);
        let u = resolve_mangadex_with(&pool, &e).await;
        assert!(u.enabled);
        assert_eq!(u.pinned_by, None);
        assert_eq!(catalogue_crawl(&e), CatalogueCrawl::Never);

        // (c) Nothing set, not self-host: uses MangaDex, does not crawl — as before.
        let e = env(&[]);
        let u = resolve_mangadex_with(&pool, &e).await;
        assert_eq!(
            (u.enabled, u.source, u.pinned_by),
            (true, Source::Default, None)
        );
        assert_eq!(catalogue_crawl(&e), CatalogueCrawl::Never);

        // (d) Self-host, nothing else: off by default; the admin's choice applies, and
        // the crawl follows it.
        let e = env(&[WEB]);
        let u = resolve_mangadex_with(&pool, &e).await;
        assert_eq!(
            (u.enabled, u.source, u.pinned_by),
            (false, Source::Default, None)
        );
        assert_eq!(catalogue_crawl(&e), CatalogueCrawl::FollowSwitch);
        store(&pool, &MANGADEX_ENABLED, "true").await.unwrap();
        let u = resolve_mangadex_with(&pool, &e).await;
        assert_eq!((u.enabled, u.source), (true, Source::Table));
        // A stored "off" holds on a hosted server too (the admin turned it off).
        store(&pool, &MANGADEX_ENABLED, "false").await.unwrap();
        assert!(!resolve_mangadex_with(&pool, env(&[])).await.enabled);
        store(&pool, &MANGADEX_ENABLED, "true").await.unwrap();

        // (e) KOMIKA_MANGADEX=off pins it off everywhere — over a stored "on", and
        // over CATALOGUE_SYNC=on (whose crawl then stays parked on the switch).
        for e in [
            env(&[("KOMIKA_MANGADEX", "off")]),
            env(&[("KOMIKA_MANGADEX", "off"), WEB]),
            env(&[("KOMIKA_MANGADEX", "off"), ("CATALOGUE_SYNC", "on")]),
        ] {
            let u = resolve_mangadex_with(&pool, &e).await;
            assert!(!u.enabled);
            assert_eq!(u.pinned_by, Some("KOMIKA_MANGADEX"));
        }
        // And =on pins it on, even on a self-host.
        let u = resolve_mangadex_with(&pool, env(&[("KOMIKA_MANGADEX", "on"), WEB])).await;
        assert!(u.enabled);
        assert_eq!(u.pinned_by, Some("KOMIKA_MANGADEX"));
        // CATALOGUE_SYNC=off on a self-host: never crawls, usage still the admin's.
        assert_eq!(
            catalogue_crawl(env(&[("CATALOGUE_SYNC", "off"), WEB])),
            CatalogueCrawl::Never
        );
        // Blank values count as unset.
        assert_eq!(
            catalogue_crawl(env(&[("CATALOGUE_SYNC", " "), ("KOMIKA_WEB_DIR", "")])),
            CatalogueCrawl::Never
        );
    }

    #[test]
    fn bool_parsing_matches_the_config_spelling() {
        for on in ["on", "1", "true", "TRUE", " On "] {
            assert!(parse_bool(on), "{on}");
        }
        for off in ["off", "0", "false", "yes", ""] {
            assert!(!parse_bool(off), "{off}");
        }
    }

    #[test]
    fn setup_code_verifies_loosely_and_clears() {
        let code = SetupCode::default();
        assert!(!code.verify(""), "no code issued yet");
        let plain = code.issue();
        assert_eq!(plain.len(), 14);
        assert!(code.is_live());
        assert!(code.verify(&plain));
        assert!(code.verify(&plain.replace('-', "").to_ascii_lowercase()));
        assert!(!code.verify("0000-0000-0000"));
        code.clear();
        assert!(!code.verify(&plain));
        assert!(!code.is_live());
    }

    #[tokio::test]
    async fn setup_code_is_issued_only_without_an_admin() {
        let pool = pool().await;
        let code = SetupCode::default();
        issue_setup_code_if_needed(&pool, &code).await.unwrap();
        assert!(code.is_live());

        sqlx::query(
            "INSERT INTO users (id, username, email, password_hash, is_admin, created_at) \
             VALUES ('a', 'admin', 'a@e', 'h', 1, 't')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let code = SetupCode::default();
        issue_setup_code_if_needed(&pool, &code).await.unwrap();
        assert!(!code.is_live());
    }
}
