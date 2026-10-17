//! `komika-server media-cache`: a cache (helper) that serves an owner's exact media
//! objects. It holds no account, database, source access or signing key.
//!
//! - `GET /media/v1/o/{copy}/{sha}`: an object of the copy's signed manifest. A miss
//!   fetches the manifest and the object from the owner; both are checked
//!   (`media_verify`) before they are stored or served, and every hit is checked again.
//! - `GET /media/v1/p/{copy}/{rev}`: an experimental pack, streamed and checked
//!   against that exact signed manifest revision, sharing the object disk budget.
//! - `GET /media/v1/ready`: 204 while the cache may serve (it holds a policy lease, or
//!   is inside the 6 h grace after one expired), else 503.
//! - `GET /health`: 200 while the process runs; `GET /health/ready` answers like
//!   `/media/v1/ready` (the two images' container health checks).
//!
//! Trust comes only from the owner's signing keys, pinned in `pins.json` at the first
//! start. A key change needs `media-cache repin`. The policy lease
//! (`GET /media/v1/policy`) is renewed every minute; denied copies are refused at once,
//! and once the last lease is more than 6 h past its expiry, nothing is served.

pub(crate) mod owner;
mod pack;
pub(crate) mod store;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{anyhow, bail, Context, Result};
use axum::extract::{Path as UrlPath, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;

use crate::media_verify::{
    verify_lease, verify_manifest, verify_object, ManifestBody, ManifestPage, PinnedKeys,
    PolicyGate, VerifyError,
};
use owner::{FetchError, Owner, Pins};
use store::{Key, Store};

/// How often the policy lease is renewed (the owner reissues it each minute).
const LEASE_RENEW_SECS: u64 = 60;
/// A request that needs a newer lease asks the owner at most this often.
const LEASE_RETRY_SECS: i64 = 10;
/// A manifest that lacks a requested object is fetched again at most this often; an
/// owner "not found" for a copy is remembered as long.
const MANIFEST_RECHECK_SECS: i64 = 60;
/// Verified manifests kept in memory (beyond that, the disk copy is read and checked).
const MEMORY_MANIFESTS: usize = 4096;

/// Unix seconds, shifted by an offset that tests move.
#[derive(Clone, Default)]
pub(crate) struct Clock(Arc<AtomicI64>);

impl Clock {
    pub(crate) fn now(&self) -> i64 {
        chrono::Utc::now().timestamp() + self.0.load(Ordering::Relaxed)
    }
    #[cfg(test)]
    pub(crate) fn advance(&self, secs: i64) {
        self.0.fetch_add(secs, Ordering::Relaxed);
    }
}

/// A copy's manifest as last checked: `None` when the owner did not have it.
struct Held {
    body: Option<ManifestBody>,
    checked_at: i64,
}

impl Held {
    fn page(&self, sha: &str) -> Option<&ManifestPage> {
        self.body
            .as_ref()?
            .pages
            .iter()
            .find(|page| page.sha256 == sha)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// Unknown copy or object, or a denied copy.
    NotFound,
    /// No usable lease, or the owner cannot be reached for a miss.
    Unavailable,
    /// The owner's bytes or manifest failed their check.
    BadUpstream,
}

pub(crate) struct Served {
    pub bytes: Vec<u8>,
    pub mime: String,
    pub s_maxage: i64,
    pub hit: bool,
}

type InflightMap = Mutex<HashMap<Key, (Arc<tokio::sync::Mutex<()>>, usize)>>;

/// One key's fetch turn. Dropping it, also when the request is cancelled, ends the turn
/// and forgets the key once nobody else waits for it, so the map holds only keys in
/// flight.
struct Turn {
    map: Arc<InflightMap>,
    key: Key,
    lock: Arc<tokio::sync::Mutex<()>>,
    held: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for Turn {
    fn drop(&mut self) {
        self.held.take();
        let mut map = self.map.lock().unwrap();
        if let Some((_, participants)) = map.get_mut(&self.key) {
            *participants -= 1;
            if *participants == 0 {
                map.remove(&self.key);
            }
        }
    }
}

pub(crate) struct MediaCache {
    owner: Owner,
    pins: PinnedKeys,
    store: Store,
    gate: RwLock<PolicyGate>,
    manifests: Mutex<HashMap<Key, Arc<Held>>>,
    /// One owner fetch per key at a time; later requests wait and then read the disk.
    inflight: Arc<InflightMap>,
    /// When a request last asked the owner for a lease.
    lease_asked: tokio::sync::Mutex<i64>,
    clock: Clock,
}

impl MediaCache {
    pub(crate) fn new(owner: Owner, pins: &Pins, store: Store, clock: Clock) -> Result<Self> {
        Ok(Self {
            owner,
            pins: pins.pinned()?,
            store,
            gate: RwLock::new(PolicyGate::default()),
            manifests: Mutex::new(HashMap::new()),
            inflight: Arc::new(Mutex::new(HashMap::new())),
            lease_asked: tokio::sync::Mutex::new(i64::MIN),
            clock,
        })
    }

    #[cfg(test)]
    pub(crate) fn store(&self) -> &Store {
        &self.store
    }

    #[cfg(test)]
    pub(crate) fn inflight_len(&self) -> usize {
        self.inflight.lock().unwrap().len()
    }

    /// Fetch, check and hold the owner's current lease.
    pub(crate) async fn renew_lease(&self) -> Result<()> {
        let bytes = self.owner.signed("/media/v1/policy").await?;
        let lease = verify_lease(&bytes, &self.pins).map_err(|error| anyhow!("lease: {error}"))?;
        let now = self.clock.now();
        self.gate
            .write()
            .unwrap()
            .accept(lease, now)
            .map_err(|error| anyhow!("lease refused: {error}"))
    }

    /// Renew the lease for a request that needs it, at most once per
    /// `LEASE_RETRY_SECS` (concurrent requests share one attempt).
    async fn renew_lease_for_request(&self) {
        let mut asked = self.lease_asked.lock().await;
        let now = self.clock.now();
        if now.saturating_sub(*asked) < LEASE_RETRY_SECS {
            return;
        }
        *asked = now;
        if let Err(error) = self.renew_lease().await {
            tracing::warn!(%error, "media cache could not renew the policy lease");
        }
    }

    pub(crate) fn serving(&self) -> bool {
        self.gate.read().unwrap().serving(self.clock.now())
    }

    /// May the cache serve an object of `copy` from a manifest signed at `epoch`? A lease
    /// older than the manifest, or no lease at all, is renewed once first.
    async fn allow(&self, copy: &str, epoch: i64) -> Result<i64, Failure> {
        let check = || {
            self.gate
                .read()
                .unwrap()
                .object(copy, epoch, self.clock.now())
        };
        let verdict = match check() {
            Err(VerifyError::EpochBehind | VerifyError::NoLease | VerifyError::Expired) => {
                self.renew_lease_for_request().await;
                check()
            }
            verdict => verdict,
        };
        match verdict {
            Ok(s_maxage) => Ok(s_maxage),
            Err(VerifyError::Denied) => Err(Failure::NotFound),
            Err(_) => Err(Failure::Unavailable),
        }
    }

    fn remember(&self, copy: &str, held: Arc<Held>) {
        self.remember_key(Key::Manifest(copy.to_string()), held);
    }

    fn remember_key(&self, key: Key, held: Arc<Held>) {
        let mut manifests = self.manifests.lock().unwrap();
        if manifests.len() >= MEMORY_MANIFESTS {
            manifests.clear();
        }
        manifests.insert(key, held);
    }

    /// The copy's manifest, checked, from memory, then disk, then the owner. When the
    /// held manifest lacks `sha` and was checked long enough ago, the owner is asked
    /// again (a new revision).
    async fn manifest(&self, copy: &str, sha: &str) -> Result<Arc<Held>, Failure> {
        let now = self.clock.now();
        let fresh = |held: &Held| {
            held.page(sha).is_some() || now.saturating_sub(held.checked_at) < MANIFEST_RECHECK_SECS
        };
        let memory = self
            .manifests
            .lock()
            .unwrap()
            .get(&Key::Manifest(copy.to_string()))
            .cloned();
        if let Some(held) = memory.filter(|held| fresh(held)) {
            return Ok(held);
        }
        let key = Key::Manifest(copy.to_string());
        if let Some(held) = self.manifest_from_disk(copy, &key).await {
            if held.page(sha).is_some() {
                let held = Arc::new(held);
                self.remember(copy, held.clone());
                return Ok(held);
            }
        }
        let turn = self.turn(&key).await;
        // A request that held the lock before may have just fetched it.
        let memory = self.manifests.lock().unwrap().get(&key).cloned();
        let result = match memory.filter(|held| fresh(held)) {
            Some(held) => Ok(held),
            None => self.manifest_from_owner(copy, &key).await.map(|held| {
                let held = Arc::new(held);
                self.remember(copy, held.clone());
                held
            }),
        };
        drop(turn);
        result
    }

    async fn manifest_from_disk(&self, copy: &str, key: &Key) -> Option<Held> {
        let (bytes, generation) = match self.store.get(key).await {
            Ok(Some(stored)) => stored,
            Ok(None) => return None,
            Err(error) => {
                tracing::warn!(%error, "media cache could not read a manifest");
                return None;
            }
        };
        match verify_manifest(&bytes, &self.pins) {
            Ok(body)
                if body.copy_id == copy
                    && match key {
                        Key::Revision(_, revision) => body.revision == i64::from(*revision),
                        _ => true,
                    } =>
            {
                Some(Held {
                    body: Some(body),
                    checked_at: i64::MIN,
                })
            }
            // A file that no longer verifies (damaged, or the pins changed) is dropped.
            _ => {
                self.store.remove(key, generation);
                None
            }
        }
    }

    async fn manifest_from_owner(&self, copy: &str, key: &Key) -> Result<Held, Failure> {
        let now = self.clock.now();
        let bytes = match self
            .owner
            .signed(&format!("/media/v1/manifest/{copy}"))
            .await
        {
            Ok(bytes) => bytes,
            Err(FetchError::Status(StatusCode::NOT_FOUND)) => {
                return Ok(Held {
                    body: None,
                    checked_at: now,
                })
            }
            Err(FetchError::TooLarge) => {
                tracing::warn!(copy, "owner manifest too large; refused");
                return Err(Failure::BadUpstream);
            }
            Err(error) => {
                tracing::warn!(%error, copy, "media cache could not fetch a manifest");
                return Err(Failure::Unavailable);
            }
        };
        let body = match verify_manifest(&bytes, &self.pins) {
            Ok(body) if body.copy_id == copy => body,
            Ok(_) => {
                tracing::warn!(copy, "owner manifest names another copy; refused");
                return Err(Failure::BadUpstream);
            }
            Err(error) => {
                tracing::warn!(%error, copy, "owner manifest failed its check; refused");
                return Err(Failure::BadUpstream);
            }
        };
        if let Err(error) = self.store.put(key, bytes.clone()).await {
            tracing::warn!(%error, "media cache could not store a manifest");
        }
        Ok(Held {
            body: Some(body),
            checked_at: now,
        })
    }

    /// Wait for `key`'s fetch turn: one owner fetch per key at a time.
    async fn turn(&self, key: &Key) -> Turn {
        let lock = {
            let mut map = self.inflight.lock().unwrap();
            let (lock, participants) = map.entry(key.clone()).or_default();
            *participants += 1;
            lock.clone()
        };
        let mut turn = Turn {
            map: self.inflight.clone(),
            key: key.clone(),
            lock,
            held: None,
        };
        turn.held = Some(turn.lock.clone().lock_owned().await);
        turn
    }

    async fn stored_object(&self, key: &Key, page: &ManifestPage) -> Option<Vec<u8>> {
        match self.store.get(key).await {
            Ok(Some((bytes, generation))) => match verify_object(page, &bytes, &page.mime) {
                Ok(()) => Some(bytes),
                Err(error) => {
                    tracing::warn!(%error, sha = page.sha256, "stored object failed its check; dropped");
                    self.store.remove(key, generation);
                    None
                }
            },
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(%error, "media cache could not read an object");
                None
            }
        }
    }

    pub(crate) async fn object(&self, copy: &str, sha: &str) -> Result<Served, Failure> {
        let now = self.clock.now();
        if matches!(
            self.gate.read().unwrap().object(copy, i64::MIN, now),
            Err(VerifyError::Denied)
        ) {
            return Err(Failure::NotFound);
        }
        let held = self.manifest(copy, sha).await?;
        let page = held.page(sha).ok_or(Failure::NotFound)?.clone();
        let epoch = held.body.as_ref().map_or(0, |body| body.policy_epoch);
        self.allow(copy, epoch).await?;
        let key = Key::Object(sha.to_string());
        let mut hit = true;
        let bytes = match self.stored_object(&key, &page).await {
            Some(bytes) => bytes,
            None => {
                hit = false;
                let turn = self.turn(&key).await;
                let bytes = match self.stored_object(&key, &page).await {
                    Some(bytes) => Ok(bytes),
                    None => self.object_from_owner(copy, &key, &page).await,
                };
                drop(turn);
                bytes?
            }
        };
        // Checked again after the fetch: a denial or expiry that landed meanwhile wins.
        let s_maxage = self.allow(copy, epoch).await?;
        Ok(Served {
            bytes,
            mime: page.mime,
            s_maxage,
            hit,
        })
    }

    async fn object_from_owner(
        &self,
        copy: &str,
        key: &Key,
        page: &ManifestPage,
    ) -> Result<Vec<u8>, Failure> {
        let (bytes, mime) = match self
            .owner
            .get(&format!("/media/v1/o/{copy}/{}", page.sha256), page.length)
            .await
        {
            Ok(fetched) => fetched,
            Err(FetchError::Status(StatusCode::NOT_FOUND)) => return Err(Failure::NotFound),
            Err(FetchError::TooLarge) => {
                tracing::warn!(
                    sha = page.sha256,
                    "owner object longer than its manifest; refused"
                );
                return Err(Failure::BadUpstream);
            }
            Err(error) => {
                tracing::warn!(%error, sha = page.sha256, "media cache could not fetch an object");
                return Err(Failure::Unavailable);
            }
        };
        if let Err(error) = verify_object(page, &bytes, &mime) {
            tracing::warn!(%error, sha = page.sha256, "owner object failed its check; refused");
            return Err(Failure::BadUpstream);
        }
        if let Err(error) = self.store.put(key, bytes.clone()).await {
            tracing::warn!(%error, "media cache could not store an object");
        }
        Ok(bytes)
    }
}

fn failure_response(failure: Failure) -> axum::response::Response {
    let status = match failure {
        Failure::NotFound => StatusCode::NOT_FOUND,
        Failure::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        Failure::BadUpstream => StatusCode::BAD_GATEWAY,
    };
    let mut response = crate::image_failure_response(status);
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    response
}

async fn serve_object(
    State(cache): State<Arc<MediaCache>>,
    path: Result<UrlPath<(String, String)>, axum::extract::rejection::PathRejection>,
) -> axum::response::Response {
    let Ok(UrlPath((copy, sha))) = path else {
        return failure_response(Failure::NotFound);
    };
    if !crate::valid_media_copy(&copy) || !store::valid_sha(&sha) {
        return failure_response(Failure::NotFound);
    }
    let served = match cache.object(&copy, &sha).await {
        Ok(served) => served,
        Err(failure) => return failure_response(failure),
    };
    let mut response = crate::raw_image_response_with_cache(
        served.mime,
        served.bytes.into(),
        "public, max-age=31536000, immutable",
    );
    let headers = response.headers_mut();
    // Shared caches keep it no longer than the lease lives, as on the owner.
    if let Ok(value) = HeaderValue::from_str(&format!(
        "public, max-age=31536000, immutable, s-maxage={}",
        served.s_maxage
    )) {
        headers.insert(header::CACHE_CONTROL, value);
    }
    // Readers fetch helper bytes with `fetch` to check them, so any origin may read.
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        "x-komika-cache",
        HeaderValue::from_static(if served.hit { "hit" } else { "miss" }),
    );
    response
}

async fn serve_ready(State(cache): State<Arc<MediaCache>>) -> axum::response::Response {
    let status = if cache.serving() {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
    )
        .into_response()
}

pub(crate) fn router(cache: Arc<MediaCache>) -> Router {
    Router::new()
        .route("/media/v1/o/{copy_id}/{sha256}", get(serve_object))
        .route("/media/v1/p/{copy_id}/{revision}", get(pack::serve))
        .route("/media/v1/ready", get(serve_ready))
        // Container health checks: the server image asks `/health` (the process is up),
        // the self-host image `/health/ready` (the cache may serve).
        .route("/health", get(|| async { "ok" }))
        .route("/health/ready", get(serve_ready))
        .with_state(cache)
}

/// Keep renewing the lease. A failure keeps the held lease (and its grace).
pub(crate) async fn renew_forever(cache: Arc<MediaCache>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(LEASE_RENEW_SECS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Err(error) = cache.renew_lease().await {
            tracing::warn!(%error, "media cache could not renew the policy lease");
        }
    }
}

struct Args {
    repin: bool,
    owner: String,
    dir: PathBuf,
    max_bytes: Option<u64>,
    listen: SocketAddr,
    expect_kid: Option<String>,
}

const USAGE: &str = "usage:
  komika-server media-cache --owner <origin> --dir <path> --max-bytes <n>[K|M|G|T] [--listen <addr:port>] [--expect-kid <kid>]
  komika-server media-cache repin --owner <origin> --dir <path> [--expect-kid <kid>]";

/// A byte count, optionally with a binary K, M, G or T suffix.
fn parse_bytes(raw: &str) -> Result<u64> {
    let raw = raw.trim();
    let (digits, shift) = match raw.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some('K') => (&raw[..raw.len() - 1], 10),
        Some('M') => (&raw[..raw.len() - 1], 20),
        Some('G') => (&raw[..raw.len() - 1], 30),
        Some('T') => (&raw[..raw.len() - 1], 40),
        _ => (raw, 0),
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(1 << shift))
        .filter(|n| *n > 0)
        .ok_or_else(|| anyhow!("--max-bytes {raw:?} is not a positive byte count"))
}

fn parse_args(argv: &[String]) -> Result<Args> {
    let mut rest = argv;
    let repin = rest.first().map(String::as_str) == Some("repin");
    if repin {
        rest = &rest[1..];
    }
    let (mut owner, mut dir, mut max_bytes, mut listen, mut expect_kid) =
        (None, None, None, None, None);
    let mut items = rest.iter();
    while let Some(flag) = items.next() {
        let mut value = || {
            items
                .next()
                .cloned()
                .ok_or_else(|| anyhow!("{flag} needs a value\n{USAGE}"))
        };
        match flag.as_str() {
            "--owner" => owner = Some(owner::parse_origin(&value()?)?),
            "--dir" => dir = Some(PathBuf::from(value()?)),
            "--max-bytes" if !repin => max_bytes = Some(parse_bytes(&value()?)?),
            "--listen" if !repin => {
                listen = Some(
                    value()?
                        .parse::<SocketAddr>()
                        .context("--listen must be an address:port")?,
                )
            }
            "--expect-kid" => expect_kid = Some(value()?),
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    let args = Args {
        repin,
        owner: owner.ok_or_else(|| anyhow!("--owner is required\n{USAGE}"))?,
        dir: dir.ok_or_else(|| anyhow!("--dir is required\n{USAGE}"))?,
        max_bytes,
        listen: listen.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 8080))),
        expect_kid,
    };
    if !repin && args.max_bytes.is_none() {
        bail!("--max-bytes is required\n{USAGE}");
    }
    Ok(args)
}

/// The pins for `dir`: the stored ones, or (first start) the owner's self-signed key
/// set. A directory pinned to another owner is refused.
pub(crate) async fn load_or_pin(
    owner: &Owner,
    dir: &Path,
    expect_kid: Option<&str>,
) -> Result<Pins> {
    if let Some(pins) = Pins::load(dir)? {
        if pins.owner != owner.origin() {
            bail!(
                "{} is pinned to owner {}, not {}; use another --dir",
                dir.display(),
                pins.owner,
                owner.origin()
            );
        }
        if let Some(kid) = expect_kid {
            if !pins.kids().contains(&kid) {
                bail!(
                    "the pinned keys do not include {kid}; run `media-cache repin` to change them"
                );
            }
        }
        match owner.signed("/media/v1/keys").await {
            Ok(key_set) => {
                if let Err(reason) = owner::key_set_matches(&pins, &key_set) {
                    tracing::warn!(
                        %reason,
                        pinned = ?pins.kids(),
                        "the owner's signing keys changed; objects it signs with other keys are refused until an operator runs `media-cache repin`"
                    );
                }
            }
            Err(error) => tracing::warn!(%error, "could not read the owner's keys at start"),
        }
        return Ok(pins);
    }
    let key_set = owner
        .signed("/media/v1/keys")
        .await
        .context("first start: read the owner's /media/v1/keys")?;
    let pins = owner::first_pins(owner.origin(), &key_set, expect_kid)?;
    pins.save(dir)?;
    tracing::info!(owner = owner.origin(), kids = ?pins.kids(), "pinned the owner's signing keys");
    Ok(pins)
}

/// Replace the pins with the owner's current self-signed key set (operator command).
async fn repin(owner: &Owner, dir: &Path, expect_kid: Option<&str>) -> Result<()> {
    let old = Pins::load(dir)?;
    if let Some(old) = &old {
        if old.owner != owner.origin() {
            bail!(
                "{} is pinned to owner {}, not {}",
                dir.display(),
                old.owner,
                owner.origin()
            );
        }
    }
    let key_set = owner
        .signed("/media/v1/keys")
        .await
        .context("read the owner's /media/v1/keys")?;
    let pins = owner::first_pins(owner.origin(), &key_set, expect_kid)?;
    pins.save(dir)?;
    println!(
        "pinned keys for {}: {:?} (was {:?})",
        owner.origin(),
        pins.kids(),
        old.as_ref().map(Pins::kids).unwrap_or_default()
    );
    Ok(())
}

pub(crate) async fn run_cli(argv: &[String]) -> Result<()> {
    let args = parse_args(argv)?;
    std::fs::create_dir_all(&args.dir).with_context(|| format!("create {}", args.dir.display()))?;
    let owner = Owner::new(args.owner.clone())?;
    if args.repin {
        return repin(&owner, &args.dir, args.expect_kid.as_deref()).await;
    }
    let pins = load_or_pin(&owner, &args.dir, args.expect_kid.as_deref()).await?;
    let store = Store::open(&args.dir, args.max_bytes.unwrap_or_default())?;
    tracing::info!(
        owner = owner.origin(),
        dir = %args.dir.display(),
        max_bytes = args.max_bytes,
        stored_bytes = store.total_bytes(),
        "media cache starting"
    );
    let cache = Arc::new(MediaCache::new(owner, &pins, store, Clock::default())?);
    // The first tick runs at once; until a lease arrives the cache serves nothing.
    tokio::spawn(renew_forever(cache.clone()));
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("bind {}", args.listen))?;
    tracing::info!(listen = %args.listen, "media cache listening");
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(crate::shutdown_signal(shutdown_tx));
    axum::serve(listener, router(cache))
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.changed().await;
        })
        .await?;
    Ok(())
}
