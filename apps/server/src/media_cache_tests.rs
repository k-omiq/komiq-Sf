//! `media-cache` against an in-process owner on a loopback port.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{header, Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use crate::media_cache::owner::{Owner, Pins};
use crate::media_cache::store::{Key, Store};
use crate::media_cache::{self, Clock, Failure, MediaCache};
use crate::media_manifest::signing::Signer;
use crate::media_manifest_tests::TempDir;
use crate::media_verify::{
    CopyIdentity, KeySetBody, LeaseBody, ManifestBody, ManifestPage, LEASE_GRACE_SECS,
};

const COPY: &str = "mc_0123456789abcdef0123456789abcdef";
const OTHER_COPY: &str = "mc_fedcba9876543210fedcba9876543210";

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// An object's bytes and MIME type.
type Page = (Vec<u8>, String);

#[derive(Default)]
struct Counts {
    keys: AtomicUsize,
    policy: AtomicUsize,
    manifest: AtomicUsize,
    object: AtomicUsize,
}

struct OwnerState {
    signer: Mutex<Arc<Signer>>,
    clock: Clock,
    /// copy -> pages (bytes, mime)
    copies: Mutex<HashMap<String, Vec<Page>>>,
    /// Serve this copy's manifest under another copy's path.
    swap_manifest: Mutex<Option<(String, String)>>,
    policy_epoch: Mutex<i64>,
    denied: Mutex<Vec<String>>,
    oversized_manifest: AtomicBool,
    corrupt: AtomicBool,
    wrong_mime: AtomicBool,
    down: AtomicBool,
    slow: AtomicBool,
    counts: Counts,
}

impl OwnerState {
    fn signer(&self) -> Arc<Signer> {
        self.signer.lock().unwrap().clone()
    }
    fn manifest(&self, copy: &str) -> Option<ManifestBody> {
        let pages = self.copies.lock().unwrap().get(copy)?.clone();
        let signer = self.signer();
        Some(ManifestBody {
            v: 1,
            copy_id: copy.to_string(),
            copy: CopyIdentity {
                source_id: "1".into(),
                manga_key: "/m".into(),
                chapter_key: "/c".into(),
            },
            revision: 1,
            policy_epoch: *self.policy_epoch.lock().unwrap(),
            issued_at: self.clock.now(),
            pages: pages
                .iter()
                .map(|(bytes, mime)| ManifestPage {
                    sha256: sha(bytes),
                    length: bytes.len() as u64,
                    mime: mime.clone(),
                    width: None,
                    height: None,
                })
                .collect(),
            kid: signer.kid.clone(),
        })
    }
}

fn down(owner: &OwnerState) -> Option<axum::response::Response> {
    owner
        .down
        .load(Ordering::SeqCst)
        .then(|| StatusCode::SERVICE_UNAVAILABLE.into_response())
}

fn signed(bytes: Vec<u8>) -> axum::response::Response {
    ([(header::CONTENT_TYPE, "application/json")], bytes).into_response()
}

async fn ownerkeys(State(owner): State<Arc<OwnerState>>) -> axum::response::Response {
    owner.counts.keys.fetch_add(1, Ordering::SeqCst);
    if let Some(response) = down(&owner) {
        return response;
    }
    let signer = owner.signer();
    signed(
        signer
            .sign(&KeySetBody {
                v: 1,
                issued_at: owner.clock.now(),
                keys: vec![signer.public_entry()],
            })
            .unwrap(),
    )
}

async fn owner_policy(State(owner): State<Arc<OwnerState>>) -> axum::response::Response {
    owner.counts.policy.fetch_add(1, Ordering::SeqCst);
    if let Some(response) = down(&owner) {
        return response;
    }
    let signer = owner.signer();
    let now = owner.clock.now();
    let mut denied = owner.denied.lock().unwrap().clone();
    denied.sort();
    signed(
        signer
            .sign(&LeaseBody {
                v: 1,
                epoch: *owner.policy_epoch.lock().unwrap(),
                issued_at: now,
                expires_at: now + 600,
                denied_copies: denied,
                denied_works: Vec::new(),
                kid: signer.kid.clone(),
            })
            .unwrap(),
    )
}

async fn owner_manifest(
    State(owner): State<Arc<OwnerState>>,
    UrlPath(copy): UrlPath<String>,
) -> axum::response::Response {
    owner.counts.manifest.fetch_add(1, Ordering::SeqCst);
    if let Some(response) = down(&owner) {
        return response;
    }
    if owner.oversized_manifest.load(Ordering::SeqCst) {
        return signed(vec![b' '; (8 << 20) + 1]);
    }
    let mut target = copy.clone();
    if let Some((asked, served)) = owner.swap_manifest.lock().unwrap().clone() {
        if asked == copy {
            target = served;
        }
    }
    match owner.manifest(&target) {
        Some(body) => signed(owner.signer().sign(&body).unwrap()),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn owner_object(
    State(owner): State<Arc<OwnerState>>,
    UrlPath((copy, hash)): UrlPath<(String, String)>,
) -> axum::response::Response {
    owner.counts.object.fetch_add(1, Ordering::SeqCst);
    if let Some(response) = down(&owner) {
        return response;
    }
    if owner.slow.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    let page = owner
        .copies
        .lock()
        .unwrap()
        .get(&copy)
        .and_then(|pages| pages.iter().find(|(bytes, _)| sha(bytes) == hash).cloned());
    let Some((mut bytes, mut mime)) = page else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if owner.corrupt.load(Ordering::SeqCst) {
        bytes[0] ^= 1;
    }
    if owner.wrong_mime.load(Ordering::SeqCst) {
        mime = "image/svg+xml".into();
    }
    ([(header::CONTENT_TYPE, mime)], bytes).into_response()
}

struct Harness {
    owner: Arc<OwnerState>,
    origin: String,
    dir: TempDir,
    keys: TempDir,
    clock: Clock,
}

async fn harness() -> Harness {
    let keys = TempDir::new();
    let clock = Clock::default();
    let owner = Arc::new(OwnerState {
        signer: Mutex::new(Arc::new(Signer::load(&keys.0.join("a.key")).unwrap())),
        clock: clock.clone(),
        copies: Mutex::new(HashMap::from([
            (
                COPY.to_string(),
                vec![
                    (b"page one bytes".to_vec(), "image/webp".to_string()),
                    (b"page two".to_vec(), "image/png".to_string()),
                ],
            ),
            (
                OTHER_COPY.to_string(),
                vec![(b"another copy".to_vec(), "image/jpeg".to_string())],
            ),
        ])),
        swap_manifest: Mutex::new(None),
        policy_epoch: Mutex::new(1),
        denied: Mutex::new(Vec::new()),
        oversized_manifest: AtomicBool::new(false),
        corrupt: AtomicBool::new(false),
        wrong_mime: AtomicBool::new(false),
        down: AtomicBool::new(false),
        slow: AtomicBool::new(false),
        counts: Counts::default(),
    });
    let app = Router::new()
        .route("/media/v1/keys", get(ownerkeys))
        .route("/media/v1/policy", get(owner_policy))
        .route("/media/v1/manifest/{copy}", get(owner_manifest))
        .route("/media/v1/o/{copy}/{sha}", get(owner_object))
        .with_state(owner.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Harness {
        owner,
        origin,
        dir: TempDir::new(),
        keys,
        clock,
    }
}

impl Harness {
    async fn pins(&self) -> anyhow::Result<Pins> {
        media_cache::load_or_pin(&Owner::new(self.origin.clone())?, &self.dir.0, None).await
    }
    async fn cache(&self, max_bytes: u64) -> Arc<MediaCache> {
        let pins = self.pins().await.unwrap();
        let store = Store::open(&self.dir.0, max_bytes).unwrap();
        let cache = MediaCache::new(
            Owner::new(self.origin.clone()).unwrap(),
            &pins,
            store,
            self.clock.clone(),
        )
        .unwrap();
        let _ = cache.renew_lease().await;
        Arc::new(cache)
    }
    fn page(&self, copy: &str, index: usize) -> Page {
        self.owner.copies.lock().unwrap()[copy][index].clone()
    }
    fn objects(&self) -> usize {
        self.owner.counts.object.load(Ordering::SeqCst)
    }
    fn manifests(&self) -> usize {
        self.owner.counts.manifest.load(Ordering::SeqCst)
    }
}

async fn get_object(cache: &Arc<MediaCache>, copy: &str, hash: &str) -> axum::http::Response<Body> {
    media_cache::router(cache.clone())
        .oneshot(
            Request::get(format!("/media/v1/o/{copy}/{hash}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn body(response: axum::http::Response<Body>) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

async fn ready(cache: &Arc<MediaCache>) -> StatusCode {
    media_cache::router(cache.clone())
        .oneshot(Request::get("/media/v1/ready").body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn media_cache_miss_then_warm_hits_ask_the_owner_nothing() {
    let h = harness().await;
    let cache = h.cache(1 << 20).await;
    assert_eq!(ready(&cache).await, StatusCode::NO_CONTENT);
    let health = media_cache::router(cache.clone())
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    let health_ready = media_cache::router(cache.clone())
        .oneshot(Request::get("/health/ready").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(health_ready.status(), StatusCode::NO_CONTENT);
    let (bytes, mime) = h.page(COPY, 0);
    let hash = sha(&bytes);

    let response = get_object(&cache, COPY, &hash).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-komika-cache"], "miss");
    assert_eq!(response.headers()[header::CONTENT_TYPE], mime.as_str());
    assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    let cache_control = response.headers()[header::CACHE_CONTROL]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        cache_control.contains("immutable") && cache_control.contains("s-maxage=6"),
        "{cache_control}"
    );
    assert_eq!(body(response).await, bytes);
    assert_eq!((h.manifests(), h.objects()), (1, 1));

    for _ in 0..3 {
        let response = get_object(&cache, COPY, &hash).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-komika-cache"], "hit");
        assert_eq!(body(response).await, bytes);
    }
    // The second page needs only its object: the manifest is held.
    let (second, _) = h.page(COPY, 1);
    assert_eq!(
        get_object(&cache, COPY, &sha(&second)).await.status(),
        StatusCode::OK
    );
    assert_eq!((h.manifests(), h.objects()), (1, 2));

    // A restarted cache serves from disk: the manifest and object files are checked again.
    let restarted = h.cache(1 << 20).await;
    let response = get_object(&restarted, COPY, &hash).await;
    assert_eq!(response.headers()["x-komika-cache"], "hit");
    assert_eq!(body(response).await, bytes);
    assert_eq!((h.manifests(), h.objects()), (1, 2));
}

#[tokio::test]
async fn media_cache_refuses_corrupt_or_mislabelled_owner_bytes() {
    let h = harness().await;
    let cache = h.cache(1 << 20).await;
    let (bytes, _) = h.page(COPY, 0);
    let hash = sha(&bytes);

    h.owner.corrupt.store(true, Ordering::SeqCst);
    assert_eq!(
        get_object(&cache, COPY, &hash).await.status(),
        StatusCode::BAD_GATEWAY
    );
    assert!(!cache.store().contains(&Key::Object(hash.clone())));
    h.owner.corrupt.store(false, Ordering::SeqCst);

    h.owner.wrong_mime.store(true, Ordering::SeqCst);
    assert_eq!(
        get_object(&cache, COPY, &hash).await.status(),
        StatusCode::BAD_GATEWAY
    );
    assert!(!cache.store().contains(&Key::Object(hash.clone())));
    h.owner.wrong_mime.store(false, Ordering::SeqCst);

    let response = get_object(&cache, COPY, &hash).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body(response).await, bytes);
}

#[tokio::test]
async fn media_cache_refuses_an_oversized_owner_manifest() {
    let h = harness().await;
    let cache = h.cache(1 << 20).await;
    let (bytes, _) = h.page(COPY, 0);
    let hash = sha(&bytes);
    h.owner.oversized_manifest.store(true, Ordering::SeqCst);
    assert_eq!(
        get_object(&cache, COPY, &hash).await.status(),
        StatusCode::BAD_GATEWAY
    );
    assert!(!cache.store().contains(&Key::Manifest(COPY.to_string())));
    assert_eq!(h.objects(), 0);
    h.owner.oversized_manifest.store(false, Ordering::SeqCst);
    assert_eq!(
        get_object(&cache, COPY, &hash).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn media_cache_refuses_a_manifest_for_another_copy_and_unknown_objects() {
    let h = harness().await;
    let cache = h.cache(1 << 20).await;
    *h.owner.swap_manifest.lock().unwrap() = Some((COPY.into(), OTHER_COPY.into()));
    let (other, _) = h.page(OTHER_COPY, 0);
    // The other copy's signed manifest, served under this copy's path, is refused.
    assert_eq!(
        get_object(&cache, COPY, &sha(&other)).await.status(),
        StatusCode::BAD_GATEWAY
    );
    *h.owner.swap_manifest.lock().unwrap() = None;

    // A hash that is not in the copy's manifest is never fetched, and the manifest is
    // asked for again at most once a minute.
    let (bytes, _) = h.page(COPY, 0);
    assert_eq!(
        get_object(&cache, COPY, &sha(&bytes)).await.status(),
        StatusCode::OK
    );
    let manifests = h.manifests();
    let objects = h.objects();
    for _ in 0..3 {
        assert_eq!(
            get_object(&cache, COPY, &sha(&other)).await.status(),
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!((h.manifests(), h.objects()), (manifests, objects));
    h.clock.advance(61);
    assert_eq!(
        get_object(&cache, COPY, &sha(&other)).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!((h.manifests(), h.objects()), (manifests + 1, objects));

    // An unknown copy is remembered as unknown for a minute too.
    let unknown = "mc_00000000000000000000000000000000";
    for _ in 0..3 {
        assert_eq!(
            get_object(&cache, unknown, &sha(&bytes)).await.status(),
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(h.manifests(), manifests + 2);
    // Bad paths never reach the owner.
    for path in [
        "/media/v1/o/mc_x/abc",
        "/media/v1/o/mc_0123456789abcdef0123456789abcdef/ABC",
    ] {
        let response = media_cache::router(cache.clone())
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    assert_eq!(h.manifests(), manifests + 2);
}

#[tokio::test]
async fn media_cache_serves_through_lease_grace_then_fails_closed() {
    let h = harness().await;
    let cache = h.cache(1 << 20).await;
    let (bytes, _) = h.page(COPY, 0);
    let hash = sha(&bytes);
    assert_eq!(
        get_object(&cache, COPY, &hash).await.status(),
        StatusCode::OK
    );

    h.owner.down.store(true, Ordering::SeqCst);
    h.clock.advance(601);
    // Expired, owner down: inside the grace, held objects serve with s-maxage 0.
    let response = get_object(&cache, COPY, &hash).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers()[header::CACHE_CONTROL]
        .to_str()
        .unwrap()
        .ends_with("s-maxage=0"));
    assert_eq!(ready(&cache).await, StatusCode::NO_CONTENT);
    // A miss in the grace cannot reach the owner.
    let (second, _) = h.page(COPY, 1);
    assert_eq!(
        get_object(&cache, COPY, &sha(&second)).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );

    h.clock.advance(LEASE_GRACE_SECS);
    assert_eq!(
        get_object(&cache, COPY, &hash).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(ready(&cache).await, StatusCode::SERVICE_UNAVAILABLE);

    // The owner returns: a request renews the lease and serving resumes.
    h.owner.down.store(false, Ordering::SeqCst);
    h.clock.advance(10);
    assert_eq!(
        get_object(&cache, COPY, &hash).await.status(),
        StatusCode::OK
    );
    assert_eq!(ready(&cache).await, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn media_cache_without_any_lease_serves_nothing() {
    let h = harness().await;
    let primed = h.cache(1 << 20).await;
    let (bytes, _) = h.page(COPY, 0);
    let hash = sha(&bytes);
    assert_eq!(primed.object(COPY, &hash).await.unwrap().bytes, bytes);
    assert!(primed.store().contains(&Key::Manifest(COPY.to_string())));
    assert!(primed.store().contains(&Key::Object(hash.clone())));
    drop(primed);
    let pins = h.pins().await.unwrap();
    let objects = h.objects();
    h.owner.down.store(true, Ordering::SeqCst);
    let cache = Arc::new(
        MediaCache::new(
            Owner::new(h.origin.clone()).unwrap(),
            &pins,
            Store::open(&h.dir.0, 1 << 20).unwrap(),
            h.clock.clone(),
        )
        .unwrap(),
    );
    assert!(cache.renew_lease().await.is_err());
    assert_eq!(ready(&cache).await, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        cache.object(COPY, &hash).await.err(),
        Some(Failure::Unavailable)
    );
    assert_eq!(h.objects(), objects);
}

#[tokio::test]
async fn media_cache_refuses_a_denied_copy_without_a_stored_manifest() {
    let h = harness().await;
    *h.owner.denied.lock().unwrap() = vec![OTHER_COPY.to_string()];
    let cache = h.cache(1 << 20).await;
    assert!(!cache
        .store()
        .contains(&Key::Manifest(OTHER_COPY.to_string())));
    let manifests = h.manifests();
    let policy = h.owner.counts.policy.load(Ordering::SeqCst);
    h.owner.down.store(true, Ordering::SeqCst);
    let (bytes, _) = h.page(OTHER_COPY, 0);
    let hash = sha(&bytes);
    assert_eq!(
        cache.object(OTHER_COPY, &hash).await.err(),
        Some(Failure::NotFound)
    );
    assert_eq!(
        get_object(&cache, OTHER_COPY, &hash).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!((h.manifests(), h.objects()), (manifests, 0));
    assert_eq!(h.owner.counts.policy.load(Ordering::SeqCst), policy);
}

#[tokio::test]
async fn media_cache_refuses_denied_copies_even_when_stored() {
    let h = harness().await;
    let cache = h.cache(1 << 20).await;
    let (bytes, _) = h.page(COPY, 0);
    let hash = sha(&bytes);
    assert_eq!(
        get_object(&cache, COPY, &hash).await.status(),
        StatusCode::OK
    );

    *h.owner.denied.lock().unwrap() = vec![COPY.to_string()];
    *h.owner.policy_epoch.lock().unwrap() = 2;
    cache.renew_lease().await.unwrap();
    let objects = h.objects();
    assert_eq!(
        get_object(&cache, COPY, &hash).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(h.objects(), objects);
    // Other copies still serve; the denial stays through the grace.
    let (other, _) = h.page(OTHER_COPY, 0);
    assert_eq!(
        get_object(&cache, OTHER_COPY, &sha(&other)).await.status(),
        StatusCode::OK
    );
    h.owner.down.store(true, Ordering::SeqCst);
    h.clock.advance(601);
    assert_eq!(
        get_object(&cache, COPY, &hash).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get_object(&cache, OTHER_COPY, &sha(&other)).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn media_cache_renews_a_lease_older_than_the_manifest() {
    let h = harness().await;
    let cache = h.cache(1 << 20).await;
    // The owner moves to epoch 3 after the cache took its lease (epoch 1).
    *h.owner.policy_epoch.lock().unwrap() = 3;
    let policy = h.owner.counts.policy.load(Ordering::SeqCst);
    let (bytes, _) = h.page(COPY, 0);
    assert_eq!(
        get_object(&cache, COPY, &sha(&bytes)).await.status(),
        StatusCode::OK
    );
    assert_eq!(h.owner.counts.policy.load(Ordering::SeqCst), policy + 1);
}

#[tokio::test]
async fn media_cache_keeps_its_pins_when_the_owner_rotates_keys() {
    let h = harness().await;
    let first = h.pins().await.unwrap();
    let cache = h.cache(1 << 20).await;
    let (bytes, _) = h.page(COPY, 0);
    assert_eq!(
        get_object(&cache, COPY, &sha(&bytes)).await.status(),
        StatusCode::OK
    );

    // The owner now signs with a new key. Its manifests and leases are refused.
    *h.owner.signer.lock().unwrap() = Arc::new(Signer::load(&h.keys.0.join("b.key")).unwrap());
    let (other, _) = h.page(OTHER_COPY, 0);
    assert_eq!(
        get_object(&cache, OTHER_COPY, &sha(&other)).await.status(),
        StatusCode::BAD_GATEWAY
    );
    assert!(cache.renew_lease().await.is_err());
    // What was verified under the pinned key still serves while the old lease lives.
    assert_eq!(
        get_object(&cache, COPY, &sha(&bytes)).await.status(),
        StatusCode::OK
    );

    // A restart keeps the pins: no silent re-pin.
    assert_eq!(h.pins().await.unwrap(), first);
    let restarted = h.cache(1 << 20).await;
    assert_eq!(ready(&restarted).await, StatusCode::SERVICE_UNAVAILABLE);

    // Only the operator command changes them.
    media_cache::run_cli(&[
        "repin".into(),
        "--owner".into(),
        h.origin.clone(),
        "--dir".into(),
        h.dir.0.display().to_string(),
    ])
    .await
    .unwrap();
    let second = h.pins().await.unwrap();
    assert_ne!(second, first);
    assert_eq!(second.kids(), vec![h.owner.signer().kid.as_str()]);
    let repinned = h.cache(1 << 20).await;
    assert_eq!(
        get_object(&repinned, OTHER_COPY, &sha(&other))
            .await
            .status(),
        StatusCode::OK
    );
    // The stored manifest signed by the old key no longer verifies: it is fetched again.
    let manifests = h.manifests();
    assert_eq!(
        get_object(&repinned, COPY, &sha(&bytes)).await.status(),
        StatusCode::OK
    );
    assert_eq!(h.manifests(), manifests + 1);
}

#[tokio::test]
async fn media_cache_first_pin_checks_owner_and_expected_key() {
    let h = harness().await;
    let owner = Owner::new(h.origin.clone()).unwrap();
    let error = media_cache::load_or_pin(&owner, &h.dir.0, Some("0000000000000000"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("expected key"), "{error}");
    assert!(!h.dir.0.join("pins.json").exists());
    let kid = h.owner.signer().kid.clone();
    // A key set that lists the expected key but is signed by another key is refused,
    // even though it is self-signed.
    let attacker = Signer::load(&h.keys.0.join("attacker.key")).unwrap();
    let forged = attacker
        .sign(&KeySetBody {
            v: 1,
            issued_at: 0,
            keys: vec![h.owner.signer().public_entry(), attacker.public_entry()],
        })
        .unwrap();
    let error = crate::media_cache::owner::first_pins(&h.origin, &forged, Some(&kid)).unwrap_err();
    assert!(
        error.to_string().contains("not signed by the expected key"),
        "{error}"
    );
    assert!(crate::media_cache::owner::first_pins(&h.origin, &forged, None).is_ok());
    media_cache::load_or_pin(&owner, &h.dir.0, Some(&kid))
        .await
        .unwrap();
    // The directory is bound to its owner.
    let elsewhere = Owner::new("http://127.0.0.1:1".into()).unwrap();
    let error = media_cache::load_or_pin(&elsewhere, &h.dir.0, None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("pinned to owner"), "{error}");
    // An owner without media routes cannot be pinned.
    h.owner.down.store(true, Ordering::SeqCst);
    let fresh = TempDir::new();
    assert!(media_cache::load_or_pin(&owner, &fresh.0, None)
        .await
        .is_err());
    assert!(!fresh.0.join("pins.json").exists());
}

#[tokio::test]
async fn media_cache_evicts_least_recently_used_within_the_budget() {
    let dir = TempDir::new();
    let store = Store::open(&dir.0, 10).unwrap();
    let key = |n: u8| Key::Object(sha(&[n]));
    store.put(&key(1), b"aaaa".to_vec()).await.unwrap();
    store.put(&key(2), b"bbbb".to_vec()).await.unwrap();
    assert!(store.get(&key(1)).await.unwrap().is_some());
    store.put(&key(3), b"cccc".to_vec()).await.unwrap();
    assert!(store.contains(&key(1)) && store.contains(&key(3)));
    assert!(!store.contains(&key(2)));
    assert_eq!(store.total_bytes(), 8);
    assert!(!dir
        .0
        .join("objects")
        .join(&sha(&[2])[..2])
        .join(sha(&[2]))
        .exists());
    // Larger than the whole budget: served, never stored.
    store.put(&key(4), vec![0; 11]).await.unwrap();
    assert!(!store.contains(&key(4)));
    // A smaller budget at the next start evicts down to it.
    drop(store);
    let store = Store::open(&dir.0, 5).unwrap();
    assert!(store.total_bytes() <= 5);
    assert_eq!(
        [key(1), key(3)]
            .iter()
            .filter(|k| store.contains(k))
            .count(),
        1
    );
}

#[tokio::test]
async fn media_cache_stale_removal_keeps_a_newer_file() {
    let dir = TempDir::new();
    let store = Store::open(&dir.0, 1 << 20).unwrap();
    let hash = sha(b"object");
    for (key, path) in [
        (
            Key::Object(hash.clone()),
            dir.0.join("objects").join(&hash[..2]).join(&hash),
        ),
        (
            Key::Manifest(COPY.to_string()),
            dir.0.join("manifests").join(COPY),
        ),
    ] {
        store.put(&key, b"old".to_vec()).await.unwrap();
        let (bytes, g1) = store.get(&key).await.unwrap().unwrap();
        assert_eq!(bytes, b"old");
        assert_eq!(store.get(&key).await.unwrap(), Some((bytes, g1)));
        store.put(&key, b"newer bytes".to_vec()).await.unwrap();
        store.remove(&key, g1);
        assert!(store.contains(&key));
        assert_eq!(std::fs::read(&path).unwrap(), b"newer bytes");
        let (bytes, g2) = store.get(&key).await.unwrap().unwrap();
        assert_eq!(bytes, b"newer bytes");
        assert_ne!(g1, g2);
        assert_eq!(store.total_bytes(), bytes.len() as u64);
        store.remove(&key, g2);
        assert!(!store.contains(&key));
        assert!(!path.exists());
        assert_eq!(store.total_bytes(), 0);
    }
}

#[tokio::test]
async fn media_cache_recovers_from_a_crash_mid_write() {
    let h = harness().await;
    let cache = h.cache(1 << 20).await;
    let (bytes, _) = h.page(COPY, 0);
    let hash = sha(&bytes);
    assert_eq!(
        get_object(&cache, COPY, &hash).await.status(),
        StatusCode::OK
    );
    drop(cache);

    // A crash left a temp file, a stray file, and (damaged disk) a short object file.
    let object = h.dir.0.join("objects").join(&hash[..2]).join(&hash);
    std::fs::write(h.dir.0.join("tmp").join("partial"), b"half").unwrap();
    std::fs::write(h.dir.0.join("objects").join("stray"), b"x").unwrap();
    std::fs::write(&object, &bytes[..4]).unwrap();
    let cache = h.cache(1 << 20).await;
    assert_eq!(std::fs::read_dir(h.dir.0.join("tmp")).unwrap().count(), 0);
    assert!(!h.dir.0.join("objects").join("stray").exists());

    let objects = h.objects();
    let response = get_object(&cache, COPY, &hash).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-komika-cache"], "miss");
    assert_eq!(body(response).await, bytes);
    assert_eq!(h.objects(), objects + 1);
    assert_eq!(std::fs::read(&object).unwrap(), bytes);

    // A damaged manifest file is dropped and fetched again.
    std::fs::write(h.dir.0.join("manifests").join(COPY), b"{").unwrap();
    let restarted = h.cache(1 << 20).await;
    let manifests = h.manifests();
    assert_eq!(
        get_object(&restarted, COPY, &hash).await.status(),
        StatusCode::OK
    );
    assert_eq!(h.manifests(), manifests + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn media_cache_concurrent_misses_fetch_once() {
    let h = harness().await;
    let cache = h.cache(1 << 20).await;
    let (bytes, _) = h.page(COPY, 0);
    let hash = sha(&bytes);
    // The owner answers slowly, so all eight misses overlap.
    h.owner.slow.store(true, Ordering::SeqCst);
    let requests: Vec<_> = (0..8)
        .map(|_| {
            let cache = cache.clone();
            let hash = hash.clone();
            tokio::spawn(async move { cache.object(COPY, &hash).await.map(|served| served.bytes) })
        })
        .collect();
    for request in requests {
        assert_eq!(request.await.unwrap().ok(), Some(bytes.clone()));
    }
    assert_eq!((h.manifests(), h.objects()), (1, 1));
    assert_eq!(cache.inflight_len(), 0);

    // Cancel holders and waiters on shared and different keys.
    let (second, _) = h.page(COPY, 1);
    let (other, _) = h.page(OTHER_COPY, 0);
    for _ in 0..16 {
        let requests: Vec<_> = (0..12)
            .map(|i| {
                let cache = cache.clone();
                let (copy, hash) = if i % 2 == 0 {
                    (COPY, sha(&second))
                } else {
                    (OTHER_COPY, sha(&other))
                };
                tokio::spawn(async move { cache.object(copy, &hash).await.map(|_| ()) })
            })
            .collect();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        for request in &requests {
            request.abort();
        }
        for request in requests {
            let _ = request.await;
        }
        assert_eq!(cache.inflight_len(), 0);
    }
    h.owner.slow.store(false, Ordering::SeqCst);
    assert_eq!(
        get_object(&cache, COPY, &sha(&second)).await.status(),
        StatusCode::OK
    );
    assert_eq!(cache.inflight_len(), 0);
}

#[tokio::test]
async fn media_cache_cli_arguments() {
    for (argv, message) in [
        (vec!["--dir", "/x", "--max-bytes", "1G"], "--owner"),
        (
            vec!["--owner", "https://o.example", "--max-bytes", "1G"],
            "--dir",
        ),
        (
            vec!["--owner", "https://o.example", "--dir", "/x"],
            "--max-bytes",
        ),
        (
            vec![
                "--owner",
                "https://o.example/path",
                "--dir",
                "/x",
                "--max-bytes",
                "1",
            ],
            "origin",
        ),
        (
            vec![
                "--owner",
                "https://o.example",
                "--dir",
                "/x",
                "--max-bytes",
                "0",
            ],
            "positive",
        ),
        (
            vec![
                "--owner",
                "https://o.example",
                "--dir",
                "/x",
                "--max-bytes",
                "1G",
                "--extra",
            ],
            "unknown",
        ),
        (
            vec![
                "repin",
                "--owner",
                "https://o.example",
                "--dir",
                "/x",
                "--max-bytes",
                "1G",
            ],
            "unknown",
        ),
    ] {
        let argv: Vec<String> = argv.into_iter().map(String::from).collect();
        let error = media_cache::run_cli(&argv).await.unwrap_err().to_string();
        assert!(error.contains(message), "{argv:?}: {error}");
    }
}
