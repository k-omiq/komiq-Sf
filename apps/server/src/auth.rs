use crate::db::Database;
use anyhow::{anyhow, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use chrono::{DateTime, Utc};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};

/// A persisted user (row from `users`).
#[derive(Debug, Clone)]
pub struct User {
    pub id: String,
    pub username: String,
    #[allow(dead_code)]
    pub email: String,
    pub password_hash: String,
    pub avatar_url: Option<String>,
    pub is_admin: i64,
    pub is_banned: i64,
    /// Set while the account holds a temporary password (`createUser`,
    /// `resetUserPassword`); cleared by `changePassword`.
    pub must_change_password: i64,
}

impl sqlx::FromRow<'_, sqlx::postgres::PgRow> for User {
    fn from_row(row: &sqlx::postgres::PgRow) -> sqlx::Result<Self> {
        use crate::db_types::Row;
        Ok(Self {
            id: row.try_get("id")?,
            username: row.try_get("username")?,
            email: row.try_get("email")?,
            password_hash: row.try_get("password_hash")?,
            avatar_url: row.try_get("avatar_url")?,
            is_admin: row.try_get("is_admin")?,
            is_banned: row.try_get("is_banned")?,
            must_change_password: row.try_get("must_change_password")?,
        })
    }
}

/// Argon2id parameters pinned by the 2026-09 security review: 64 MiB memory,
/// t=3, p=1 — deliberately above the crate defaults (19 MiB, t=2), which were
/// too low for a public-facing login endpoint. NOTE: existing hashes keep
/// verifying after a parameter change because `verify_password` recomputes with
/// the parameters embedded in each stored PHC string, not the ones configured
/// here (password-hash 0.5 `PasswordVerifier`), so this hardening is additive.
fn argon2_instance() -> Argon2<'static> {
    // Hardcoded literals are always in-range, so `Params::new` can't fail —
    // unwrap with context rather than threading a Result through every caller.
    let params = Params::new(65536, 3, 1, None).expect("fixed Argon2 params are valid");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Hash a plaintext password with Argon2id + a random salt.
pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = argon2_instance()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow!("hash error: {e}"))?
        .to_string();
    Ok(hash)
}

/// Verify a plaintext password against a stored PHC hash.
pub fn verify_password(password: &str, phc: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(phc) else {
        return false;
    };
    argon2_instance()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Generate an opaque 256-bit session token (hex).
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Hash a session token for storage/lookup — SHA-256, lowercase hex (defense in
/// depth). The plaintext token is what the client holds; only this hash is ever
/// persisted in `sessions.token`, so a leaked DB snapshot can't be replayed as a
/// bearer token. SHA-256 (not Argon2) is correct here: the token is a full 256-bit
/// random secret, so it isn't brute-forceable and needs no slow KDF, and lookups
/// must stay fast + deterministic (no per-row salt to iterate).
pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    hex::encode(digest)
}

/// Preserve the existing whole-second session clock while using C10 spelling.
/// PostgreSQL compares the resulting native instants, independently of spelling.
pub fn format_ts(t: DateTime<Utc>) -> String {
    use chrono::Timelike;
    crate::db_types::format_date(t.with_nanosecond(0).unwrap())
}

/// How long an admin session lives without use. Each use moves its end this far
/// ahead again (see [`slide_admin_session`]), so an admin who keeps working stays in.
pub const ADMIN_IDLE_SECS: i64 = 8 * 60 * 60;
/// The longest an admin session lives after sign-in, however often it is used.
pub const ADMIN_MAX_SECS: i64 = 7 * 24 * 60 * 60;
/// A slide writes at most once per this many seconds of use, not on every request.
const ADMIN_SLIDE_STEP_SECS: i64 = 10 * 60;

/// Move a live admin session's end to [`ADMIN_IDLE_SECS`] from now, never past
/// [`ADMIN_MAX_SECS`] after sign-in. A shorter configured `ttl_secs` bounds both, as
/// it bounds the lifetime `new_session` gives. An expired session is never revived,
/// and a session is never shortened.
///
/// Reads on the query lane and takes the writer only when the end is due to move.
/// Best-effort: the request it rides on has already been authenticated, so a failure
/// is counted, logged and dropped, and the next request tries again.
pub async fn slide_admin_session(pool: &Database, token: &str, ttl_secs: i64) {
    let token_hash = hash_token(token);
    let row = crate::db_types::query_tuple::<(DateTime<Utc>, DateTime<Utc>)>(
        "SELECT created_at, expires_at FROM sessions WHERE token = $1",
    )
    .bind(&token_hash)
    .fetch_optional(pool)
    .await;
    let (created, expires) = match row {
        Ok(Some(row)) => row,
        Ok(None) => return,
        Err(error) => {
            crate::db::record_database_error(&error);
            tracing::warn!(%error, "admin session slide skipped: lookup failed");
            return;
        }
    };
    let now = Utc::now();
    let end = (now + chrono::Duration::seconds(ttl_secs.min(ADMIN_IDLE_SECS)))
        .min(created + chrono::Duration::seconds(ttl_secs.min(ADMIN_MAX_SECS)));
    if expires <= now || expires >= end - chrono::Duration::seconds(ADMIN_SLIDE_STEP_SECS) {
        return;
    }
    let Ok(end) = crate::db_types::date(&format_ts(end)) else {
        tracing::warn!("admin session slide skipped: clock value did not parse");
        return;
    };
    // Only the row as it was read: a revoke or another slide in between wins.
    if let Err(error) =
        sqlx::query("UPDATE sessions SET expires_at = $2 WHERE token = $1 AND expires_at = $3")
            .bind(&token_hash)
            .bind(end)
            .bind(expires)
            .execute(pool)
            .await
    {
        crate::db::record_database_error(&error);
        tracing::warn!(%error, "admin session slide failed; the next request retries it");
    }
}

/// Resolve the user a session token may ACT as: [`session_user_for_token`], minus
/// accounts that still hold a temporary password. Those are anonymous to everything
/// until they choose their own, so this is the lookup every handler uses.
pub async fn user_for_token(pool: &Database, token: &str) -> Result<Option<User>> {
    Ok(session_user_for_token(pool, token)
        .await?
        .filter(|u| u.must_change_password == 0))
}

/// Resolve the user owning a session token, if the token is valid and unexpired.
/// Banned users are treated as anonymous (their existing tokens stop resolving),
/// as are sessions whose `expires_at` is in the past or NULL. Includes accounts
/// that must change their password first: only the session query and
/// `changePassword` want those; everything else calls [`user_for_token`].
pub async fn session_user_for_token(pool: &Database, token: &str) -> Result<Option<User>> {
    let now = crate::db_types::date(&format_ts(Utc::now()))?;
    // The column stores `sha256(token)` (hex), never the raw token — hash the
    // presented token the same way before looking it up.
    let token_hash = hash_token(token);
    let user = sqlx::query_as::<_, User>(
        "SELECT u.id, u.username, u.email, u.password_hash, u.avatar_url, u.is_admin, u.is_banned, \
                u.must_change_password \
         FROM sessions s JOIN users u ON u.id = s.user_id \
         WHERE s.token = $1 AND u.is_banned = FALSE AND s.expires_at > $2",
    )
    .bind(&token_hash)
    .bind(now)
    .fetch_optional(pool)
    .await?;
    Ok(user)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn username_guard_closes_races_without_merging_legacy_accounts() {
        let pool = crate::sqlite_database::SqliteDatabase::connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql("CREATE TABLE users (id TEXT PRIMARY KEY, username TEXT UNIQUE); INSERT INTO users VALUES ('legacy-a', 'Alice'), ('legacy-b', 'ALICE');")
            .execute(&pool).await.unwrap();
        sqlx::raw_sql(include_str!("../migrations/0103_username_case_guard.sql"))
            .execute(&pool)
            .await
            .unwrap();
        let (a, b) = tokio::join!(
            sqlx::query("INSERT INTO users VALUES ('new-a', 'Bob')").execute(&pool),
            sqlx::query("INSERT INTO users VALUES ('new-b', 'BOB')").execute(&pool)
        );
        assert_ne!(
            a.is_ok(),
            b.is_ok(),
            "exactly one case variant must be admitted"
        );
        assert!(sqlx::query("INSERT INTO users VALUES ('new-c', 'alice')")
            .execute(&pool)
            .await
            .is_err());
        assert!(
            sqlx::query(r#"UPDATE users SET username = 'Alice' WHERE id = 'legacy-a'"#)
                .execute(&pool)
                .await
                .is_ok()
        );
        assert!(
            sqlx::query(r#"UPDATE users SET username = 'bob' WHERE id = 'legacy-a'"#)
                .execute(&pool)
                .await
                .is_err()
        );
        let legacy: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE id LIKE 'legacy-%'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(legacy, 2, "migration must preserve both legacy identities");
    }

    #[test]
    fn hash_roundtrips_and_rejects_wrong() {
        let h = hash_password("hunter2pw").unwrap();
        assert_ne!(h, "hunter2pw", "stored hash must not be the plaintext");
        assert!(verify_password("hunter2pw", &h));
        assert!(!verify_password("wrong-password", &h));
    }

    #[test]
    fn malformed_hash_is_rejected_not_panicked() {
        assert!(!verify_password("anything", "not-a-valid-phc-string"));
    }

    /// Pins the hardened parameters via the PHC string Argon2 emits (m/t/p are
    /// self-describing there), so a future revert to weaker defaults fails loudly.
    #[test]
    fn hash_pins_the_hardened_argon2_params() {
        let h = hash_password("hunter2pw").unwrap();
        assert!(h.contains("m=65536"), "64 MiB memory cost: {h}");
        assert!(h.contains("t=3"), "t_cost 3: {h}");
        assert!(h.contains("p=1"), "parallelism 1: {h}");
        // Argon2id (the strongest variant) rather than Argon2i/d.
        assert!(h.starts_with("$argon2id$"), "variant: {h}");
    }

    /// A hash produced under the OLD default parameters (19 MiB, t=2) must still
    /// verify — this is what makes the parameter upgrade non-destructive for
    /// already-registered users (verification follows the hash's own params).
    #[test]
    fn legacy_default_params_hash_still_verifies() {
        let legacy_params = Params::new(19456, 2, 1, None).expect("default params are valid");
        let legacy = Argon2::new(Algorithm::Argon2id, Version::V0x13, legacy_params);
        let salt = SaltString::generate(&mut OsRng);
        let h = legacy
            .hash_password("hunter2pw".as_bytes(), &salt)
            .unwrap()
            .to_string();
        assert!(h.contains("m=19456"), "test preconditions: {h}");
        assert!(verify_password("hunter2pw", &h));
    }

    #[test]
    fn tokens_are_unique_256bit_hex() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 64, "256-bit token = 64 hex chars");
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn token_hash_is_deterministic_hex_and_not_the_raw_token() {
        let tok = generate_token();
        let h = hash_token(&tok);
        assert_eq!(h, hash_token(&tok), "same token → same hash");
        assert_ne!(h, tok, "the stored hash must not equal the raw token");
        assert_eq!(h.len(), 64, "sha256 hex = 64 chars");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        // Known-answer vector pins the algorithm (lowercase hex sha256("abc")).
        assert_eq!(
            hash_token("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[tokio::test]
    async fn session_token_round_trips_and_is_stored_hashed() {
        use crate::database::TestPoolOptions;
        let pool = TestPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations_pg")
            .run(pool.write())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO users (id, username, email, password_hash, created_at) \
             VALUES ('00000000-0000-4000-8000-000000000001', 'alice', 'a@e', 'h', '2020-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Mimic `new_session`: return plaintext, store the hash.
        let tok = generate_token();
        sqlx::query(
            "INSERT INTO sessions (token, user_id, created_at, expires_at) \
             VALUES ($1, '00000000-0000-4000-8000-000000000001', '2020-01-01T00:00:00Z', '2999-01-01T00:00:00Z')",
        )
        .bind(hash_token(&tok))
        .execute(&pool)
        .await
        .unwrap();

        // The plaintext token resolves to the user (round-trip through the hash).
        let user = user_for_token(&pool, &tok)
            .await
            .unwrap()
            .expect("resolves");
        assert_eq!(user.id, "00000000-0000-4000-8000-000000000001");

        // The stored column is the HASH, not the raw token.
        let stored: String = crate::db_types::query_scalar(
            "SELECT token FROM sessions WHERE user_id = '00000000-0000-4000-8000-000000000001'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_ne!(stored, tok, "raw token must never be persisted");
        assert_eq!(stored, hash_token(&tok));

        // A raw-token lookup (i.e. querying by the plaintext directly) finds nothing.
        let none: Option<String> =
            crate::db_types::query_scalar("SELECT token FROM sessions WHERE token = $1")
                .bind(&tok)
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert!(none.is_none(), "plaintext token is not in the table");
    }
}
