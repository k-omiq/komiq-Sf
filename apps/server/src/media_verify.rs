//! Portable signed-byte and exact-object verification for regional caches.

use std::collections::HashMap;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ring::signature::{UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CopyIdentity {
    pub source_id: String,
    pub manga_key: String,
    pub chapter_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestPage {
    pub sha256: String,
    pub length: u64,
    pub mime: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestBody {
    pub v: u32,
    pub copy_id: String,
    pub copy: CopyIdentity,
    pub revision: i64,
    pub policy_epoch: i64,
    pub issued_at: i64,
    pub pages: Vec<ManifestPage>,
    pub kid: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicKeyEntry {
    pub kid: String,
    pub alg: String,
    pub public_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeySetBody {
    pub v: u32,
    pub issued_at: i64,
    pub keys: Vec<PublicKeyEntry>,
}

/// The signed policy lease (`GET /media/v1/policy`): what a serving cache may serve
/// until `expires_at`. Lists are sorted.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeaseBody {
    pub v: u32,
    pub epoch: i64,
    pub issued_at: i64,
    pub expires_at: i64,
    pub denied_copies: Vec<String>,
    pub denied_works: Vec<String>,
    pub kid: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Envelope {
    pub payload: String,
    pub kid: String,
    pub sig: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyError {
    UnknownKey,
    BadSignature,
    Malformed,
    Version,
    KeyMismatch,
    Length,
    Mime,
    Hash,
    /// No valid lease has been accepted yet.
    NoLease,
    /// The lease expired, past the grace for content it did not deny.
    Expired,
    /// A lease with an older epoch than the one held (a replay or a rollback).
    StaleLease,
    /// The lease is older than the manifest's policy epoch.
    EpochBehind,
    /// The lease denies this copy or work.
    Denied,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for VerifyError {}

pub fn key_id(public_key: &[u8]) -> String {
    hex::encode(&Sha256::digest(public_key)[..8])
}

pub struct PinnedKeys(HashMap<String, [u8; 32]>);

impl PinnedKeys {
    pub fn new<I, K, V>(pairs: I) -> Result<Self, VerifyError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut keys = HashMap::new();
        for (kid, public_key) in pairs {
            let raw = decode_key(kid.as_ref(), public_key.as_ref())?;
            keys.insert(kid.as_ref().to_owned(), raw);
        }
        Ok(Self(keys))
    }
}

fn decode_key(kid: &str, encoded: &str) -> Result<[u8; 32], VerifyError> {
    let raw: [u8; 32] = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| VerifyError::Malformed)?
        .try_into()
        .map_err(|_| VerifyError::Malformed)?;
    if key_id(&raw) != kid {
        return Err(VerifyError::KeyMismatch);
    }
    Ok(raw)
}

// Verify the exact text before deserializing any payload, including future leases.
fn open_envelope(bytes: &[u8], pins: &PinnedKeys) -> Result<Envelope, VerifyError> {
    let envelope: Envelope = serde_json::from_slice(bytes).map_err(|_| VerifyError::Malformed)?;
    let key = pins.0.get(&envelope.kid).ok_or(VerifyError::UnknownKey)?;
    let signature = URL_SAFE_NO_PAD
        .decode(&envelope.sig)
        .map_err(|_| VerifyError::Malformed)?;
    UnparsedPublicKey::new(&ED25519, key)
        .verify(envelope.payload.as_bytes(), &signature)
        .map_err(|_| VerifyError::BadSignature)?;
    Ok(envelope)
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn verify_manifest(bytes: &[u8], pins: &PinnedKeys) -> Result<ManifestBody, VerifyError> {
    let envelope = open_envelope(bytes, pins)?;
    let body: ManifestBody =
        serde_json::from_str(&envelope.payload).map_err(|_| VerifyError::Malformed)?;
    if body.v != 1 {
        return Err(VerifyError::Version);
    }
    if body.kid != envelope.kid {
        return Err(VerifyError::KeyMismatch);
    }
    if body.pages.iter().any(|page| !valid_hash(&page.sha256)) {
        return Err(VerifyError::Malformed);
    }
    Ok(body)
}

pub fn verify_key_set(bytes: &[u8], pins: &PinnedKeys) -> Result<Vec<PublicKeyEntry>, VerifyError> {
    let envelope = open_envelope(bytes, pins)?;
    let body: KeySetBody =
        serde_json::from_str(&envelope.payload).map_err(|_| VerifyError::Malformed)?;
    if body.v != 1 {
        return Err(VerifyError::Version);
    }
    for key in &body.keys {
        if key.alg != "Ed25519" {
            return Err(VerifyError::Malformed);
        }
        decode_key(&key.kid, &key.public_key)?;
    }
    Ok(body.keys)
}

pub fn verify_lease(bytes: &[u8], pins: &PinnedKeys) -> Result<LeaseBody, VerifyError> {
    let envelope = open_envelope(bytes, pins)?;
    let body: LeaseBody =
        serde_json::from_str(&envelope.payload).map_err(|_| VerifyError::Malformed)?;
    if body.v != 1 {
        return Err(VerifyError::Version);
    }
    if body.kid != envelope.kid {
        return Err(VerifyError::KeyMismatch);
    }
    if !well_formed(&body) {
        return Err(VerifyError::Malformed);
    }
    Ok(body)
}

/// Sorted lists without repeats, so a lookup is a binary search that cannot miss.
fn well_formed(lease: &LeaseBody) -> bool {
    let sorted = |ids: &[String]| ids.windows(2).all(|pair| pair[0] < pair[1]);
    lease.expires_at > lease.issued_at
        && sorted(&lease.denied_copies)
        && sorted(&lease.denied_works)
}

/// After a lease expires, a cache keeps serving what that lease did not deny for this
/// long (owner question Q7, pick B), then fails closed. A takedown made while the owner
/// is unreachable cannot reach a cache anyway.
pub const LEASE_GRACE_SECS: i64 = 6 * 3600;

/// A serving cache's policy state: the last valid lease. Times are unix seconds.
#[derive(Default)]
pub struct PolicyGate {
    lease: Option<LeaseBody>,
}

impl PolicyGate {
    /// Hold a verified lease. A lease that is already expired, or older than the held
    /// one, is refused and the held lease stays.
    pub fn accept(&mut self, lease: LeaseBody, now: i64) -> Result<(), VerifyError> {
        if !well_formed(&lease) {
            return Err(VerifyError::Malformed);
        }
        if lease.expires_at <= now {
            return Err(VerifyError::Expired);
        }
        if let Some(held) = &self.lease {
            if lease.epoch < held.epoch {
                return Err(VerifyError::StaleLease);
            }
            if lease.epoch == held.epoch && lease.expires_at <= held.expires_at {
                return Ok(());
            }
        }
        self.lease = Some(lease);
        Ok(())
    }

    /// May the cache serve an object of `copy_id` from a manifest signed at
    /// `manifest_epoch`? `Ok` carries the `s-maxage` to send: the lease's remaining
    /// life, 0 during the grace.
    pub fn object(&self, copy_id: &str, manifest_epoch: i64, now: i64) -> Result<i64, VerifyError> {
        self.check(now, Some(manifest_epoch), |lease| {
            lease.denied_copies.binary_search_by(|id| id.as_str().cmp(copy_id)).is_ok()
        })
    }

    /// May the cache serve a work's cover (keyed by the id in its URL, which may be an
    /// old merged-away id)?
    pub fn work(&self, work_id: &str, now: i64) -> Result<i64, VerifyError> {
        self.check(now, None, |lease| {
            lease.denied_works.binary_search_by(|id| id.as_str().cmp(work_id)).is_ok()
        })
    }

    fn check(
        &self,
        now: i64,
        manifest_epoch: Option<i64>,
        denied: impl FnOnce(&LeaseBody) -> bool,
    ) -> Result<i64, VerifyError> {
        let lease = self.lease.as_ref().ok_or(VerifyError::NoLease)?;
        if manifest_epoch.is_some_and(|epoch| lease.epoch < epoch) {
            return Err(VerifyError::EpochBehind);
        }
        if denied(lease) {
            return Err(VerifyError::Denied);
        }
        if now < lease.expires_at {
            Ok(lease.expires_at - now)
        } else if now < lease.expires_at + LEASE_GRACE_SECS {
            Ok(0)
        } else {
            Err(VerifyError::Expired)
        }
    }
}

pub fn verify_object(page: &ManifestPage, bytes: &[u8], mime: &str) -> Result<(), VerifyError> {
    if page.length != bytes.len() as u64 {
        return Err(VerifyError::Length);
    }
    let essence = |value: &str| {
        value
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
    };
    if essence(&page.mime) != essence(mime) {
        return Err(VerifyError::Mime);
    }
    if !valid_hash(&page.sha256) {
        return Err(VerifyError::Malformed);
    }
    if hex::encode(Sha256::digest(bytes)) != page.sha256 {
        return Err(VerifyError::Hash);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::{
        rand::SystemRandom,
        signature::{Ed25519KeyPair, KeyPair},
    };

    fn key() -> Ed25519KeyPair {
        let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        Ed25519KeyPair::from_pkcs8(document.as_ref()).unwrap()
    }
    fn pins(key: &Ed25519KeyPair) -> PinnedKeys {
        PinnedKeys::new([(
            key_id(key.public_key().as_ref()),
            URL_SAFE_NO_PAD.encode(key.public_key().as_ref()),
        )])
        .unwrap()
    }
    fn sign(key: &Ed25519KeyPair, body: &impl Serialize) -> Vec<u8> {
        let payload = String::from_utf8(serde_json::to_vec(body).unwrap()).unwrap();
        serde_json::to_vec(&Envelope {
            sig: URL_SAFE_NO_PAD.encode(key.sign(payload.as_bytes()).as_ref()),
            payload,
            kid: key_id(key.public_key().as_ref()),
        })
        .unwrap()
    }
    fn page(bytes: &[u8]) -> ManifestPage {
        ManifestPage {
            sha256: hex::encode(Sha256::digest(bytes)),
            length: bytes.len() as u64,
            mime: "image/png".into(),
            width: None,
            height: None,
        }
    }
    fn body(key: &Ed25519KeyPair) -> ManifestBody {
        ManifestBody {
            v: 1,
            copy_id: format!("mc_{}", "a".repeat(32)),
            copy: CopyIdentity {
                source_id: "source".into(),
                manga_key: "/manga".into(),
                chapter_key: "/chapter".into(),
            },
            revision: 1,
            policy_epoch: 0,
            issued_at: 123,
            pages: vec![page(b"first"), page(b"second")],
            kid: key_id(key.public_key().as_ref()),
        }
    }
    #[test]
    fn media_verify_round_trip_and_exact_payload_bytes() {
        let key = key();
        let original = body(&key);
        assert_eq!(
            verify_manifest(&sign(&key, &original), &pins(&key)).unwrap(),
            original
        );
        assert!(!serde_json::to_string(&original).unwrap().contains("width"));
        let payload = serde_json::to_string_pretty(&original).unwrap();
        let envelope = Envelope {
            sig: URL_SAFE_NO_PAD.encode(key.sign(payload.as_bytes()).as_ref()),
            payload,
            kid: original.kid,
        };
        assert!(verify_manifest(&serde_json::to_vec(&envelope).unwrap(), &pins(&key)).is_ok());
    }
    #[test]
    fn media_verify_refuses_every_unsigned_payload_change() {
        let key = key();
        let original = body(&key);
        for change in 0..7 {
            let mut envelope: Envelope = serde_json::from_slice(&sign(&key, &original)).unwrap();
            let mut changed = original.clone();
            match change {
                0 => changed.pages[0].sha256.replace_range(0..1, "z"),
                1 => changed.pages[0].length += 1,
                2 => changed.pages[0].mime = "image/jpeg".into(),
                3 => changed.pages.swap(0, 1),
                4 => changed.policy_epoch += 1,
                5 => changed.copy_id.push('b'),
                6 => changed.kid.push('b'),
                _ => unreachable!(),
            }
            envelope.payload = serde_json::to_string(&changed).unwrap();
            assert_eq!(
                verify_manifest(&serde_json::to_vec(&envelope).unwrap(), &pins(&key)),
                Err(VerifyError::BadSignature)
            );
        }
        let mut envelope: Envelope = serde_json::from_slice(&sign(&key, &original)).unwrap();
        envelope.payload.push(' ');
        assert_eq!(
            verify_manifest(&serde_json::to_vec(&envelope).unwrap(), &pins(&key)),
            Err(VerifyError::BadSignature)
        );
    }
    #[test]
    fn media_verify_key_identity_versions_and_malformed_hashes() {
        let signer = key();
        let other = key();
        assert_eq!(
            verify_manifest(&sign(&other, &body(&other)), &pins(&signer)),
            Err(VerifyError::UnknownKey)
        );
        assert!(matches!(
            PinnedKeys::new([(
                "wrong",
                URL_SAFE_NO_PAD.encode(signer.public_key().as_ref())
            )]),
            Err(VerifyError::KeyMismatch)
        ));
        let mut manifest = body(&signer);
        manifest.kid = key_id(other.public_key().as_ref());
        assert_eq!(
            verify_manifest(&sign(&signer, &manifest), &pins(&signer)),
            Err(VerifyError::KeyMismatch)
        );
        manifest = body(&signer);
        manifest.v = 2;
        assert_eq!(
            verify_manifest(&sign(&signer, &manifest), &pins(&signer)),
            Err(VerifyError::Version)
        );
        for bad in ["A".repeat(64), "g".repeat(64), "a".repeat(63)] {
            manifest = body(&signer);
            manifest.pages[0].sha256 = bad;
            assert_eq!(
                verify_manifest(&sign(&signer, &manifest), &pins(&signer)),
                Err(VerifyError::Malformed)
            );
        }
    }
    #[test]
    fn media_verify_key_sets_require_a_pinned_signer() {
        let signer = key();
        let new_key = key();
        let entries = vec![PublicKeyEntry {
            kid: key_id(new_key.public_key().as_ref()),
            alg: "Ed25519".into(),
            public_key: URL_SAFE_NO_PAD.encode(new_key.public_key().as_ref()),
        }];
        let set = KeySetBody {
            v: 1,
            issued_at: 123,
            keys: entries.clone(),
        };
        assert_eq!(
            verify_key_set(&sign(&signer, &set), &pins(&signer)).unwrap(),
            entries
        );
        assert_eq!(
            verify_key_set(&sign(&new_key, &set), &pins(&signer)),
            Err(VerifyError::UnknownKey)
        );
    }
    #[test]
    fn media_verify_objects_check_length_then_mime_then_hash() {
        let expected = page(b"first");
        assert_eq!(
            verify_object(&expected, b"first", "IMAGE/PNG; charset=binary"),
            Ok(())
        );
        assert_eq!(
            verify_object(&expected, b"bad", "image/jpeg"),
            Err(VerifyError::Length)
        );
        assert_eq!(
            verify_object(&expected, b"first", "image/jpeg"),
            Err(VerifyError::Mime)
        );
        assert_eq!(
            verify_object(&expected, b"other", "image/png"),
            Err(VerifyError::Hash)
        );
    }

    fn lease(epoch: i64, issued_at: i64, copies: &[&str], works: &[&str]) -> LeaseBody {
        LeaseBody {
            v: 1,
            epoch,
            issued_at,
            expires_at: issued_at + 600,
            denied_copies: copies.iter().map(|id| id.to_string()).collect(),
            denied_works: works.iter().map(|id| id.to_string()).collect(),
            kid: String::new(),
        }
    }
    #[test]
    fn media_verify_leases_are_signed_and_well_formed() {
        let key = key();
        let mut body = lease(5, 1000, &["mc_a", "mc_b"], &["w_a"]);
        body.kid = key_id(key.public_key().as_ref());
        assert_eq!(verify_lease(&sign(&key, &body), &pins(&key)).unwrap(), body);
        let mut envelope: Envelope = serde_json::from_slice(&sign(&key, &body)).unwrap();
        let mut changed = body.clone();
        changed.denied_copies.pop();
        envelope.payload = serde_json::to_string(&changed).unwrap();
        assert_eq!(
            verify_lease(&serde_json::to_vec(&envelope).unwrap(), &pins(&key)),
            Err(VerifyError::BadSignature)
        );
        assert_eq!(
            verify_lease(&sign(&key, &body), &pins(&self::key())),
            Err(VerifyError::UnknownKey)
        );
        // An unsorted list could hide a denial from the binary search.
        let mut unsorted = body.clone();
        unsorted.denied_copies.reverse();
        assert_eq!(
            verify_lease(&sign(&key, &unsorted), &pins(&key)),
            Err(VerifyError::Malformed)
        );
        let mut empty_life = body.clone();
        empty_life.expires_at = empty_life.issued_at;
        assert_eq!(
            verify_lease(&sign(&key, &empty_life), &pins(&key)),
            Err(VerifyError::Malformed)
        );
        assert_eq!(
            PolicyGate::default().accept(unsorted, 1000),
            Err(VerifyError::Malformed)
        );
    }
    #[test]
    fn media_verify_policy_expiry_follows_the_grace_rule() {
        let mut gate = PolicyGate::default();
        assert_eq!(gate.object("mc_b", 0, 1000), Err(VerifyError::NoLease));
        assert_eq!(gate.work("w_b", 1000), Err(VerifyError::NoLease));
        gate.accept(lease(5, 1000, &["mc_a"], &["w_a"]), 1000).unwrap();
        // Valid lease: s-maxage is the remaining life, down to the last second.
        assert_eq!(gate.object("mc_b", 5, 1000), Ok(600));
        assert_eq!(gate.object("mc_b", 5, 1599), Ok(1));
        assert_eq!(gate.work("w_b", 1300), Ok(300));
        assert_eq!(gate.object("mc_a", 5, 1000), Err(VerifyError::Denied));
        assert_eq!(gate.work("w_a", 1000), Err(VerifyError::Denied));
        // A lease older than the manifest cannot vouch for it.
        assert_eq!(gate.object("mc_b", 6, 1000), Err(VerifyError::EpochBehind));
        // Expired: what the last lease did not deny serves for 6 h with s-maxage 0,
        // then the cache fails closed. Denied stays denied.
        assert_eq!(gate.object("mc_b", 5, 1600), Ok(0));
        assert_eq!(gate.work("w_b", 1600 + LEASE_GRACE_SECS - 1), Ok(0));
        assert_eq!(gate.object("mc_a", 5, 1600), Err(VerifyError::Denied));
        assert_eq!(gate.object("mc_b", 6, 1600), Err(VerifyError::EpochBehind));
        assert_eq!(
            gate.object("mc_b", 5, 1600 + LEASE_GRACE_SECS),
            Err(VerifyError::Expired)
        );
        assert_eq!(
            gate.work("w_b", 1600 + LEASE_GRACE_SECS),
            Err(VerifyError::Expired)
        );
        // An expired or replayed lease is refused and the held one stays.
        let late = 1600 + LEASE_GRACE_SECS;
        assert_eq!(
            gate.accept(lease(7, late - 700, &[], &[]), late),
            Err(VerifyError::Expired)
        );
        assert_eq!(
            gate.accept(lease(4, late, &[], &[]), late),
            Err(VerifyError::StaleLease)
        );
        assert_eq!(gate.object("mc_b", 5, late), Err(VerifyError::Expired));
        // Renewal restores serving, under the new lease's denials.
        gate.accept(lease(6, late, &["mc_b"], &[]), late).unwrap();
        assert_eq!(gate.object("mc_a", 5, late), Ok(600));
        assert_eq!(gate.object("mc_b", 6, late), Err(VerifyError::Denied));
        // The same epoch reissued later extends the life; an earlier copy does not.
        gate.accept(lease(6, late + 60, &["mc_b"], &[]), late + 60).unwrap();
        assert_eq!(gate.object("mc_a", 6, late + 60), Ok(600));
        gate.accept(lease(6, late, &["mc_b"], &[]), late + 60).unwrap();
        assert_eq!(gate.object("mc_a", 6, late + 60), Ok(600));
    }
}
