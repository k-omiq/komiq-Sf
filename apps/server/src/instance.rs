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

use chrono::Utc;
use rand_core::{OsRng, RngCore};
use sqlx::SqlitePool;

/// One runtime-changeable instance setting.
pub struct Setting {
    /// Primary key in `instance_setting`.
    pub key: &'static str,
    /// Env variable that overrides the stored value.
    pub env: &'static str,
    /// Value used when neither env nor table has one.
    pub default: &'static str,
}

/// Display name of the instance (setup screen, page titles).
pub const INSTANCE_NAME: Setting = Setting {
    key: "instance_name",
    env: "KOMIKA_INSTANCE_NAME",
    default: "Komika",
};

/// Whether `register` accepts new accounts. Defaults open: that is how every
/// deployment behaved before this setting existed.
pub const REGISTRATION_OPEN: Setting = Setting {
    key: "registration_open",
    env: "KOMIKA_REGISTRATION_OPEN",
    default: "true",
};

/// Whether the direct-MangaDex catalogue sync runs. Shares `CATALOGUE_SYNC` with
/// `Config::catalogue_sync_enabled`, so the env meaning is unchanged.
pub const MANGADEX_ENABLED: Setting = Setting {
    key: "mangadex_enabled",
    env: "CATALOGUE_SYNC",
    default: "false",
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
pub async fn resolve(pool: &SqlitePool, s: &Setting) -> Resolved {
    resolve_with(pool, s, |name| std::env::var(name).ok()).await
}

/// Resolve `s` with an injected env lookup, so tests can exercise the read order
/// without racing sibling tests over process-global env variables.
pub async fn resolve_with(
    pool: &SqlitePool,
    s: &Setting,
    env: impl Fn(&str) -> Option<String>,
) -> Resolved {
    if let Some(v) = env(s.env).filter(|v| !v.trim().is_empty()) {
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
pub async fn store(pool: &SqlitePool, s: &Setting, value: &str) -> sqlx::Result<()> {
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
pub async fn admin_exists(pool: &SqlitePool) -> sqlx::Result<bool> {
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
pub async fn issue_setup_code_if_needed(pool: &SqlitePool, code: &SetupCode) -> anyhow::Result<()> {
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
    use sqlx::sqlite::SqlitePoolOptions;

    async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
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
