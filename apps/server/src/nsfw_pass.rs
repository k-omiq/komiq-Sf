//! The shared NSFW pass: lets NSFW catalogue reads stay edge-cacheable.
//!
//! A signed-in viewer who opted in to NSFW used to send every catalogue read with their
//! token, so none of it could be cached. Instead, the server signs one pass per UTC day,
//! `HMAC-SHA256(key, "komika-nsfw-pass:<day>")`, and hands it only to opted-in viewers
//! (`Query.nsfwPass`). The apps send it as `&nsfw=<pass>` on the same anonymous persisted
//! GETs. Every opted-in viewer sends the same pass, so they all share one cached copy of
//! each URL, and the gate stays on the server: without a valid pass a GET reads as
//! anonymous, which never shows NSFW works.
//!
//! The pass of today and of yesterday are accepted, so a pass fetched just before UTC
//! midnight keeps working; a leaked pass stops working within two days. The key lives in
//! `instance_setting`, so every API node and every restart signs the same pass.
use ring::hmac;
use tokio::sync::OnceCell;

use crate::db::Database;

/// `instance_setting` key holding the hex-encoded HMAC key.
const KEY_SETTING: &str = "nsfw_pass_key";
const SECS_PER_DAY: i64 = 86_400;

/// The answer to a pass that no longer verifies: the apps fetch a fresh one and retry.
pub(crate) const EXPIRED: &str = "NSFW_PASS_EXPIRED";

static KEY: OnceCell<hmac::Key> = OnceCell::const_new();

/// The UTC day number of `unix_secs`.
pub(crate) fn day(unix_secs: i64) -> i64 {
    unix_secs.div_euclid(SECS_PER_DAY)
}

fn today() -> i64 {
    day(chrono::Utc::now().timestamp())
}

fn message(day: i64) -> String {
    format!("komika-nsfw-pass:{day}")
}

/// The pass of `day` under `key`, lowercase hex.
pub(crate) fn sign(key: &hmac::Key, day: i64) -> String {
    hex::encode(hmac::sign(key, message(day).as_bytes()).as_ref())
}

/// Whether `pass` is the pass of `today`, of the day before, or of the day after (a node
/// whose clock runs a little behind another's at midnight). Constant time per day.
pub(crate) fn verify(key: &hmac::Key, pass: &str, today: i64) -> bool {
    let Ok(tag) = hex::decode(pass) else {
        return false;
    };
    [today, today - 1, today + 1]
        .iter()
        .any(|d| hmac::verify(key, message(*d).as_bytes(), &tag).is_ok())
}

/// The pass of today.
pub(crate) async fn current(pool: &Database) -> sqlx::Result<String> {
    Ok(sign(key(pool).await?, today()))
}

/// Whether `pass` verifies today.
pub(crate) async fn valid(pool: &Database, pass: &str) -> sqlx::Result<bool> {
    Ok(verify(key(pool).await?, pass, today()))
}

/// The signing key, created once per deployment. Two nodes that race to create it both
/// insert-if-absent and then read the winner back, so they always agree.
async fn key(pool: &Database) -> sqlx::Result<&'static hmac::Key> {
    KEY.get_or_try_init(|| async {
        let fresh = {
            let mut bytes = [0u8; 32];
            ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
                .map_err(|_| sqlx::Error::Protocol("system random failed".into()))?;
            hex::encode(bytes)
        };
        sqlx::query(
            "INSERT INTO instance_setting (key, value, updated_at) VALUES ($1, $2, $3) \
             ON CONFLICT(key) DO NOTHING",
        )
        .bind(KEY_SETTING)
        .bind(&fresh)
        .bind(chrono::Utc::now())
        .execute(pool)
        .await?;
        let stored: String =
            crate::db_types::query_scalar("SELECT value FROM instance_setting WHERE key = $1")
                .bind(KEY_SETTING)
                .fetch_one(pool)
                .await?;
        let bytes = hex::decode(stored.trim())
            .map_err(|_| sqlx::Error::Protocol("nsfw_pass_key is not hex".into()))?;
        Ok(hmac::Key::new(hmac::HMAC_SHA256, &bytes))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> hmac::Key {
        hmac::Key::new(hmac::HMAC_SHA256, &[seed; 32])
    }

    #[test]
    fn a_pass_verifies_on_its_day_and_the_next_only() {
        let k = key(1);
        let pass = sign(&k, 100);
        assert!(verify(&k, &pass, 100));
        assert!(
            verify(&k, &pass, 101),
            "a pass from just before midnight still works"
        );
        assert!(!verify(&k, &pass, 102), "two days later it has expired");
        assert!(
            verify(&k, &pass, 99),
            "a clock just behind midnight still accepts it"
        );
        assert!(
            !verify(&k, &pass, 98),
            "a pass is not valid well before its day"
        );
    }

    #[test]
    fn a_pass_from_another_key_or_garbage_never_verifies() {
        let pass = sign(&key(1), 100);
        assert!(!verify(&key(2), &pass, 100));
        assert!(!verify(&key(1), "not hex", 100));
        assert!(!verify(&key(1), "", 100));
        assert!(!verify(&key(1), &pass[..32], 100), "a cut pass is refused");
    }

    #[test]
    fn days_split_at_utc_midnight() {
        assert_eq!(day(0), 0);
        assert_eq!(day(86_399), 0);
        assert_eq!(day(86_400), 1);
        assert_eq!(day(-1), -1);
    }
}
