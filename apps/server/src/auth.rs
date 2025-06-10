use anyhow::{anyhow, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use chrono::{DateTime, Utc};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

/// A persisted user (row from `users`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
    pub id: String,
    pub username: String,
    #[allow(dead_code)]
    pub email: String,
    pub password_hash: String,
    pub avatar_url: Option<String>,
    pub is_admin: i64,
    pub is_banned: i64,
}

/// Hash a plaintext password with Argon2id + a random salt.
pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
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
    Argon2::default()
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

/// Format a UTC instant as a fixed-width, lexically-sortable timestamp
/// (`YYYY-MM-DDTHH:MM:SSZ`). Session `expires_at` values and the comparison
/// bound both use this, so a plain string comparison agrees with chronological
/// order — unlike `to_rfc3339()`, whose optional `+00:00` offset and variable
/// fractional-second width make lexical comparison unsafe.
pub fn format_ts(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Resolve the user owning a session token, if the token is valid and unexpired.
/// Banned users are treated as anonymous (their existing tokens stop resolving),
/// as are sessions whose `expires_at` is in the past or NULL.
pub async fn user_for_token(pool: &SqlitePool, token: &str) -> Result<Option<User>> {
    let now = format_ts(Utc::now());
    // The column stores `sha256(token)` (hex), never the raw token — hash the
    // presented token the same way before looking it up.
    let token_hash = hash_token(token);
    let user = sqlx::query_as::<_, User>(
        "SELECT u.id, u.username, u.email, u.password_hash, u.avatar_url, u.is_admin, u.is_banned \
         FROM sessions s JOIN users u ON u.id = s.user_id \
         WHERE s.token = ? AND u.is_banned = 0 AND s.expires_at > ?",
    )
    .bind(&token_hash)
    .bind(&now)
    .fetch_optional(pool)
    .await?;
    Ok(user)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO users (id, username, email, password_hash, created_at) \
             VALUES ('u1', 'alice', 'a@e', 'h', 't')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Mimic `new_session`: return plaintext, store the hash.
        let tok = generate_token();
        sqlx::query(
            "INSERT INTO sessions (token, user_id, created_at, expires_at) \
             VALUES (?, 'u1', '2020-01-01T00:00:00Z', '2999-01-01T00:00:00Z')",
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
        assert_eq!(user.id, "u1");

        // The stored column is the HASH, not the raw token.
        let stored: String = sqlx::query_scalar("SELECT token FROM sessions WHERE user_id = 'u1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_ne!(stored, tok, "raw token must never be persisted");
        assert_eq!(stored, hash_token(&tok));

        // A raw-token lookup (i.e. querying by the plaintext directly) finds nothing.
        let none: Option<String> = sqlx::query_scalar("SELECT token FROM sessions WHERE token = ?")
            .bind(&tok)
            .fetch_optional(&pool)
            .await
            .unwrap();
        assert!(none.is_none(), "plaintext token is not in the table");
    }
}
