//! Socket-free cache pack tests: the owner transport dispatches an Axum router.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{header, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use crate::media_cache::owner::{Owner, Pins};
use crate::media_cache::store::{Key, Store};
use crate::media_cache::{self, Clock, MediaCache};
use crate::media_manifest::signing::Signer;
use crate::media_manifest_tests::TempDir;
use crate::media_pack::{build_index, parse_header, verify_pack};
use crate::media_verify::{verify_manifest, CopyIdentity, LeaseBody, ManifestBody, ManifestPage};

const COPY: &str = "mc_0123456789abcdef0123456789abcdef";
const OTHER: &str = "mc_fedcba9876543210fedcba9876543210";

fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

struct Mock {
    signer: Arc<Signer>,
    manifests: Mutex<HashMap<i32, Vec<u8>>>,
    manifest_requests: Mutex<Vec<String>>,
    chunks: Mutex<Vec<Bytes>>,
    packs: AtomicUsize,
    pulled: AtomicUsize,
    dropped: AtomicUsize,
    block_after: AtomicUsize,
    error_after: AtomicUsize,
    denied: AtomicBool,
    down: AtomicBool,
    policy_down: AtomicBool,
    declared_length: Mutex<Option<u64>>,
    clock: Clock,
}

struct Tracked {
    mock: Arc<Mock>,
    chunks: VecDeque<Bytes>,
    position: usize,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.mock.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

async fn policy(State(mock): State<Arc<Mock>>) -> Response {
    if mock.policy_down.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let now = mock.clock.now();
    let body = LeaseBody {
        v: 1,
        epoch: 1,
        issued_at: now,
        expires_at: now + 600,
        denied_copies: if mock.denied.load(Ordering::SeqCst) {
            vec![COPY.into()]
        } else {
            vec![]
        },
        denied_works: vec![],
        kid: mock.signer.kid.clone(),
    };
    mock.signer.sign(&body).unwrap().into_response()
}

async fn manifest(
    State(mock): State<Arc<Mock>>,
    Path(copy): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    mock.manifest_requests
        .lock()
        .unwrap()
        .push(query.get("rev").cloned().unwrap_or_default());
    if mock.down.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let revision = query
        .get("rev")
        .and_then(|rev| rev.parse::<i32>().ok())
        .unwrap_or(2);
    if copy != COPY {
        return StatusCode::NOT_FOUND.into_response();
    }
    let response = match mock.manifests.lock().unwrap().get(&revision) {
        Some(bytes) => bytes.clone().into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    };
    response
}

async fn pack(State(mock): State<Arc<Mock>>) -> Response {
    mock.packs.fetch_add(1, Ordering::SeqCst);
    if mock.down.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let chunks = mock.chunks.lock().unwrap().clone().into();
    let declared = *mock.declared_length.lock().unwrap();
    let stream = futures::stream::unfold(
        Tracked {
            mock,
            chunks,
            position: 0,
        },
        |mut tracked| async move {
            if tracked.position == tracked.mock.block_after.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            tokio::task::yield_now().await;
            if tracked.position == tracked.mock.error_after.load(Ordering::SeqCst) {
                tracked.mock.error_after.store(usize::MAX, Ordering::SeqCst);
                return Some((Err(std::io::Error::other("owner aborted")), tracked));
            }
            let bytes = tracked.chunks.pop_front()?;
            tracked.position += 1;
            tracked.mock.pulled.fetch_add(bytes.len(), Ordering::SeqCst);
            Some((Ok(bytes), tracked))
        },
    );
    let mut response = Body::from_stream(stream).into_response();
    if let Some(length) = declared {
        response
            .headers_mut()
            .insert(header::CONTENT_LENGTH, length.into());
    }
    response
}

struct Fixture {
    dir: TempDir,
    _keys: TempDir,
    mock: Arc<Mock>,
    router: Router,
    pins: Pins,
    manifest: ManifestBody,
    bytes: Vec<u8>,
}

impl Fixture {
    fn new() -> Self {
        let keys = TempDir::new();
        let signer = Arc::new(Signer::load(&keys.0.join("key")).unwrap());
        let clock = Clock::default();
        let objects = [
            b"first page".as_slice(),
            b"second page",
            b"first page",
            b"last page",
        ];
        let body = ManifestBody {
            v: 1,
            copy_id: COPY.into(),
            copy: CopyIdentity {
                source_id: "1".into(),
                manga_key: "/m".into(),
                chapter_key: "/c".into(),
            },
            revision: 1,
            policy_epoch: 1,
            issued_at: clock.now(),
            kid: signer.kid.clone(),
            pages: objects
                .iter()
                .map(|bytes| ManifestPage {
                    sha256: hash(bytes),
                    length: bytes.len() as u64,
                    mime: "image/png".into(),
                    width: None,
                    height: None,
                })
                .collect(),
        };
        let index = build_index(&body).unwrap();
        let mut bytes = index.header.clone();
        let mut chunks = vec![Bytes::copy_from_slice(&index.header)];
        for position in index.unique {
            bytes.extend_from_slice(objects[position]);
            chunks.push(Bytes::copy_from_slice(objects[position]));
        }
        let mut latest = body.clone();
        latest.revision = 2;
        let mock = Arc::new(Mock {
            manifests: Mutex::new(HashMap::from([
                (1, signer.sign(&body).unwrap()),
                (2, signer.sign(&latest).unwrap()),
            ])),
            signer: signer.clone(),
            manifest_requests: Mutex::new(vec![]),
            chunks: Mutex::new(chunks),
            packs: AtomicUsize::new(0),
            pulled: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
            block_after: AtomicUsize::new(usize::MAX),
            error_after: AtomicUsize::new(usize::MAX),
            denied: AtomicBool::new(false),
            down: AtomicBool::new(false),
            policy_down: AtomicBool::new(false),
            declared_length: Mutex::new(None),
            clock,
        });
        let router = Router::new()
            .route("/media/v1/policy", get(policy))
            .route("/media/v1/manifest/{copy}", get(manifest))
            .route("/media/v1/p/{copy}/{revision}", get(pack))
            .with_state(mock.clone());
        Self {
            dir: TempDir::new(),
            _keys: keys,
            mock,
            router,
            pins: Pins {
                owner: "http://localhost".into(),
                keys: vec![signer.public_entry()],
            },
            manifest: body,
            bytes,
        }
    }

    async fn cache(&self, budget: u64) -> Arc<MediaCache> {
        let cache = Arc::new(
            MediaCache::new(
                Owner::from_router(self.router.clone()),
                &self.pins,
                Store::open(&self.dir.0, budget).unwrap(),
                self.mock.clock.clone(),
            )
            .unwrap(),
        );
        cache.renew_lease().await.unwrap();
        cache
    }

    fn key(&self) -> Key {
        Key::Pack(COPY.into(), 1)
    }
    fn path(&self) -> std::path::PathBuf {
        self.dir.0.join("packs").join(format!("{COPY}-1"))
    }
    fn clean(&self, cache: &MediaCache) {
        assert!(!cache.store().contains(&self.key()));
        assert!(!self.path().exists());
        assert_eq!(
            std::fs::read_dir(self.dir.0.join("tmp")).unwrap().count(),
            0
        );
        assert_eq!(cache.inflight_len(), 0);
    }
}

async fn request(cache: &Arc<MediaCache>, path: &str) -> Response {
    media_cache::router(cache.clone())
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn get_pack(cache: &Arc<MediaCache>) -> Response {
    request(cache, &format!("/media/v1/p/{COPY}/1")).await
}

async fn collect(response: Response) -> Result<Bytes, axum::Error> {
    axum::body::to_bytes(response.into_body(), usize::MAX).await
}

#[tokio::test]
async fn cache_pack_good_miss_then_hit_and_restart() {
    let f = Fixture::new();
    // Header and objects may straddle transport chunks, including repeated objects.
    *f.mock.chunks.lock().unwrap() = f.bytes.chunks(7).map(Bytes::copy_from_slice).collect();
    let cache = f.cache(1 << 20).await;
    for expected in ["miss", "hit"] {
        let response = get_pack(&cache).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-komika-cache"], expected);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/vnd.komika.pack"
        );
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            f.bytes.len().to_string()
        );
        assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
        assert!(response.headers()[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("s-maxage="));
        let bytes = collect(response).await.unwrap();
        assert_eq!(bytes.as_ref(), f.bytes);
        assert_eq!(verify_pack(&bytes, &f.manifest).unwrap().len(), 4);
    }
    assert_eq!(f.mock.packs.load(Ordering::SeqCst), 1);
    assert_eq!(*f.mock.manifest_requests.lock().unwrap(), ["1"]);
    assert_eq!(std::fs::read(f.path()).unwrap(), f.bytes);
    drop(cache);
    f.mock.down.store(true, Ordering::SeqCst);
    let cache = f.cache(1 << 20).await;
    let response = get_pack(&cache).await;
    assert_eq!(response.headers()["x-komika-cache"], "hit");
    assert_eq!(collect(response).await.unwrap().as_ref(), f.bytes);
    assert_eq!(f.mock.packs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cache_pack_corrupt_object_is_never_forwarded_or_stored() {
    let f = Fixture::new();
    f.mock.chunks.lock().unwrap()[2] = Bytes::from_static(b"wrong bytes");
    let cache = f.cache(1 << 20).await;
    let mut body = get_pack(&cache).await.into_body().into_data_stream();
    assert_eq!(
        body.next().await.unwrap().unwrap().as_ref(),
        build_index(&f.manifest).unwrap().header
    );
    assert_eq!(body.next().await.unwrap().unwrap().as_ref(), b"first page");
    assert!(body.next().await.unwrap().is_err());
    // Even retaining an errored body must not retain the upstream or fetch turn.
    assert_eq!(f.mock.dropped.load(Ordering::SeqCst), 1);
    f.clean(&cache);
}

#[tokio::test]
async fn cache_pack_wrong_headers_are_refused_before_serving() {
    for change in 0..5 {
        let f = Fixture::new();
        let mut header = build_index(&f.manifest).unwrap().header;
        match change {
            0 => header[0] ^= 1,
            1 => header[4..8].copy_from_slice(&u32::MAX.to_be_bytes()),
            _ => {
                let mut index = parse_header(&header).unwrap();
                match change {
                    2 => index.revision = 2,
                    3 => index.pages[0].mime = "image/gif".into(),
                    _ => index.pages[0].offset = 1,
                }
                let json = serde_json::to_vec(&index).unwrap();
                header = b"KPK1".to_vec();
                header.extend_from_slice(&(json.len() as u32).to_be_bytes());
                header.extend_from_slice(&json);
            }
        }
        f.mock.chunks.lock().unwrap()[0] = header.into();
        let cache = f.cache(1 << 20).await;
        assert_eq!(
            get_pack(&cache).await.status(),
            StatusCode::BAD_GATEWAY,
            "change {change}"
        );
        f.clean(&cache);
    }
}

#[tokio::test]
async fn cache_pack_short_errored_and_long_owner_bodies_store_nothing() {
    for change in 0..5 {
        let f = Fixture::new();
        match change {
            0 => {
                f.mock.chunks.lock().unwrap().pop();
            }
            1 => {
                f.mock.chunks.lock().unwrap()[3] = Bytes::from_static(b"last");
            }
            2 => {
                f.mock.error_after.store(2, Ordering::SeqCst);
            }
            3 => {
                f.mock
                    .chunks
                    .lock()
                    .unwrap()
                    .push(Bytes::from_static(b"extra"));
            }
            _ => {
                *f.mock.chunks.lock().unwrap() =
                    vec![Bytes::from([f.bytes.as_slice(), b"extra"].concat())];
            }
        }
        let cache = f.cache(1 << 20).await;
        let response = get_pack(&cache).await;
        if response.status() == StatusCode::OK {
            assert!(collect(response).await.is_err());
        } else {
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        }
        f.clean(&cache);
    }
    let f = Fixture::new();
    *f.mock.declared_length.lock().unwrap() = Some(f.bytes.len() as u64 + 1);
    let cache = f.cache(1 << 20).await;
    assert_eq!(get_pack(&cache).await.status(), StatusCode::BAD_GATEWAY);
    f.clean(&cache);
}

#[tokio::test]
async fn cache_pack_denied_unknown_invalid_and_expired_are_refused() {
    let f = Fixture::new();
    f.mock.denied.store(true, Ordering::SeqCst);
    let cache = f.cache(1 << 20).await;
    assert_eq!(get_pack(&cache).await.status(), StatusCode::NOT_FOUND);
    assert!(f.mock.manifest_requests.lock().unwrap().is_empty());
    assert_eq!(f.mock.packs.load(Ordering::SeqCst), 0);
    f.mock.denied.store(false, Ordering::SeqCst);
    f.mock.clock.advance(1);
    cache.renew_lease().await.unwrap();
    for path in [
        format!("/media/v1/p/{OTHER}/1"),
        format!("/media/v1/p/{COPY}/3"),
        "/media/v1/p/invalid/1".into(),
        format!("/media/v1/p/{COPY}/0"),
        format!("/media/v1/p/{COPY}/-1"),
        format!("/media/v1/p/{COPY}/2147483648"),
        format!("/media/v1/p/{COPY}/x"),
    ] {
        assert_eq!(
            request(&cache, &path).await.status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    f.mock
        .clock
        .advance(600 + crate::media_verify::LEASE_GRACE_SECS);
    f.mock.down.store(true, Ordering::SeqCst);
    f.mock.policy_down.store(true, Ordering::SeqCst);
    assert_eq!(
        get_pack(&cache).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    let no_lease = Arc::new(
        MediaCache::new(
            Owner::from_router(Router::new()),
            &f.pins,
            Store::open(&f.dir.0, 1 << 20).unwrap(),
            f.mock.clock.clone(),
        )
        .unwrap(),
    );
    assert_eq!(
        get_pack(&no_lease).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn cache_pack_stored_tampering_is_detected_and_removed() {
    for location in [0, 1, 2, 3] {
        let f = Fixture::new();
        let cache = f.cache(1 << 20).await;
        collect(get_pack(&cache).await).await.unwrap();
        let mut bytes = f.bytes.clone();
        match location {
            0 => bytes[0] ^= 1,
            1 => bytes[build_index(&f.manifest).unwrap().header.len()] ^= 1,
            2 => {
                bytes.pop();
            }
            _ => bytes.push(1),
        }
        std::fs::write(f.path(), bytes).unwrap();
        let response = get_pack(&cache).await;
        if response.status() == StatusCode::OK {
            assert!(collect(response).await.is_err());
        } else {
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        }
        f.clean(&cache);
        assert_eq!(f.mock.packs.load(Ordering::SeqCst), 1);
        assert_eq!(
            collect(get_pack(&cache).await).await.unwrap().as_ref(),
            f.bytes
        );
    }
}

#[tokio::test]
async fn cache_pack_cancel_mid_miss_drops_owner_and_temp_immediately() {
    let f = Fixture::new();
    f.mock.block_after.store(2, Ordering::SeqCst);
    let cache = f.cache(1 << 20).await;
    let mut stream = get_pack(&cache).await.into_body().into_data_stream();
    stream.next().await.unwrap().unwrap();
    stream.next().await.unwrap().unwrap();
    assert_eq!(std::fs::read_dir(f.dir.0.join("tmp")).unwrap().count(), 1);
    // Cancel while the next object's read is pending, not merely between polls.
    assert!(futures::poll!(Box::pin(stream.next())).is_pending());
    let pulled = f.mock.pulled.load(Ordering::SeqCst);
    drop(stream);
    assert_eq!(f.mock.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(f.mock.pulled.load(Ordering::SeqCst), pulled);
    f.clean(&cache);
    f.mock.block_after.store(usize::MAX, Ordering::SeqCst);
    assert_eq!(
        collect(get_pack(&cache).await).await.unwrap().as_ref(),
        f.bytes
    );
}

#[tokio::test]
async fn cache_pack_concurrent_misses_share_one_owner_fetch() {
    let f = Fixture::new();
    let cache = f.cache(1 << 20).await;
    let requests = (0..8).map(|_| {
        let cache = cache.clone();
        async move { collect(get_pack(&cache).await).await.unwrap() }
    });
    for bytes in futures::future::join_all(requests).await {
        assert_eq!(bytes.as_ref(), f.bytes);
    }
    assert_eq!(f.mock.packs.load(Ordering::SeqCst), 1);
    assert_eq!(*f.mock.manifest_requests.lock().unwrap(), ["1"]);
    assert_eq!(cache.inflight_len(), 0);
}

#[tokio::test]
async fn cache_pack_old_revision_does_not_replace_latest_and_manifest_is_exact() {
    let f = Fixture::new();
    let cache = f.cache(1 << 20).await;
    let latest = f.mock.manifests.lock().unwrap()[&2].clone();
    cache
        .store()
        .put(&Key::Manifest(COPY.into()), latest.clone())
        .await
        .unwrap();
    collect(get_pack(&cache).await).await.unwrap();
    assert_eq!(
        cache
            .store()
            .get(&Key::Manifest(COPY.into()))
            .await
            .unwrap()
            .unwrap()
            .0,
        latest
    );
    assert_eq!(
        verify_manifest(&latest, &f.pins.pinned().unwrap())
            .unwrap()
            .revision,
        2
    );
    for change in 0..3 {
        let f = Fixture::new();
        let mut body = f.manifest.clone();
        match change {
            0 => body.copy_id = OTHER.into(),
            1 => body.revision = 2,
            _ => body.kid = "bad".into(),
        }
        f.mock
            .manifests
            .lock()
            .unwrap()
            .insert(1, f.mock.signer.sign(&body).unwrap());
        let cache = f.cache(1 << 20).await;
        assert_eq!(get_pack(&cache).await.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(f.mock.packs.load(Ordering::SeqCst), 0);
        assert!(!cache.store().contains(&Key::Revision(COPY.into(), 1)));
    }
}

#[tokio::test]
async fn cache_pack_policy_change_during_stream_aborts_without_storing() {
    let f = Fixture::new();
    let cache = f.cache(1 << 20).await;
    let mut body = get_pack(&cache).await.into_body().into_data_stream();
    body.next().await.unwrap().unwrap();
    body.next().await.unwrap().unwrap();
    f.mock.denied.store(true, Ordering::SeqCst);
    f.mock.clock.advance(1);
    cache.renew_lease().await.unwrap();
    assert!(body.next().await.unwrap().is_err());
    f.clean(&cache);
    assert_eq!(get_pack(&cache).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cache_pack_eviction_counts_bytes_and_startup_cleans_crash_temps() {
    let f = Fixture::new();
    let cache = f.cache(1 << 20).await;
    collect(get_pack(&cache).await).await.unwrap();
    let revision = cache
        .store()
        .get(&Key::Revision(COPY.into(), 1))
        .await
        .unwrap()
        .unwrap()
        .0;
    assert_eq!(
        cache.store().total_bytes(),
        revision.len() as u64 + f.bytes.len() as u64
    );
    drop(cache);
    std::fs::write(f.dir.0.join("tmp").join("pack-crash"), &f.bytes[..10]).unwrap();
    let budget = f.bytes.len() as u64 + 10;
    let store = Store::open(&f.dir.0, budget).unwrap();
    assert_eq!(std::fs::read_dir(f.dir.0.join("tmp")).unwrap().count(), 0);
    assert!(store.total_bytes() <= budget);
    // Pack insertion participates in the same LRU as objects, and vice versa.
    store.put(&f.key(), f.bytes.clone()).await.unwrap();
    let object = Key::Object(hash(b"object"));
    store.put(&object, vec![0; 20]).await.unwrap();
    assert!(!store.contains(&f.key()));
    assert!(store.contains(&object));
    assert_eq!(store.total_bytes(), 20);
    let mut pending = store.begin(f.key(), f.bytes.len() as u64).unwrap().unwrap();
    pending.write(&f.bytes).await.unwrap();
    pending.sync().await.unwrap();
    pending.finish().unwrap();
    assert!(store.contains(&f.key()));
    assert!(!store.contains(&object));
    assert_eq!(store.total_bytes(), f.bytes.len() as u64);
}

#[tokio::test]
async fn cache_pack_larger_than_budget_streams_without_temp_or_storage() {
    let f = Fixture::new();
    let cache = f.cache(1).await;
    assert_eq!(
        collect(get_pack(&cache).await).await.unwrap().as_ref(),
        f.bytes
    );
    assert_eq!(cache.store().total_bytes(), 0);
    f.clean(&cache);
}

#[tokio::test]
async fn cache_pack_stalled_fill_answers_later_requests_unavailable() {
    let f = Fixture::new();
    // The first miss holds the fill turn while its owner body never sends a byte.
    f.mock.block_after.store(0, Ordering::SeqCst);
    let cache = f.cache(1 << 20).await;
    let first = tokio::spawn({
        let cache = cache.clone();
        async move { get_pack(&cache).await }
    });
    while f.mock.packs.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    tokio::time::pause();
    let start = tokio::time::Instant::now();
    let response = get_pack(&cache).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    assert!(start.elapsed() >= std::time::Duration::from_secs(30));
    assert_eq!(f.mock.packs.load(Ordering::SeqCst), 1);
    assert!(!first.is_finished());
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    tokio::time::resume();
    f.clean(&cache);
    f.mock.block_after.store(usize::MAX, Ordering::SeqCst);
    assert_eq!(
        collect(get_pack(&cache).await).await.unwrap().as_ref(),
        f.bytes
    );
}

#[tokio::test]
async fn cache_pack_policy_or_lease_failure_mid_hit_keeps_the_stored_pack() {
    let f = Fixture::new();
    let cache = f.cache(1 << 20).await;
    collect(get_pack(&cache).await).await.unwrap();
    for failure in ["denied", "lease"] {
        let response = get_pack(&cache).await;
        assert_eq!(response.headers()["x-komika-cache"], "hit");
        let mut body = response.into_body().into_data_stream();
        body.next().await.unwrap().unwrap();
        f.mock.clock.advance(1);
        if failure == "denied" {
            f.mock.denied.store(true, Ordering::SeqCst);
            cache.renew_lease().await.unwrap();
        } else {
            // The lease lapses and cannot be renewed.
            f.mock.policy_down.store(true, Ordering::SeqCst);
            f.mock
                .clock
                .advance(600 + crate::media_verify::LEASE_GRACE_SECS);
        }
        assert!(body.next().await.unwrap().is_err(), "{failure}");
        drop(body);
        assert!(cache.store().contains(&f.key()), "{failure}");
        assert_eq!(std::fs::read(f.path()).unwrap(), f.bytes, "{failure}");
        assert_eq!(cache.inflight_len(), 0);
        f.mock.denied.store(false, Ordering::SeqCst);
        f.mock.policy_down.store(false, Ordering::SeqCst);
        f.mock.clock.advance(1);
        cache.renew_lease().await.unwrap();
    }
    // A tampered stored object is still removed when its check fails mid-hit.
    let mut bytes = f.bytes.clone();
    bytes[build_index(&f.manifest).unwrap().header.len()] ^= 1;
    std::fs::write(f.path(), bytes).unwrap();
    let response = get_pack(&cache).await;
    assert_eq!(response.headers()["x-komika-cache"], "hit");
    assert!(collect(response).await.is_err());
    f.clean(&cache);
    assert_eq!(f.mock.packs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cache_pack_head_answers_headers_without_owner_stream_or_temp() {
    let f = Fixture::new();
    let cache = f.cache(1 << 20).await;
    let head = || async {
        media_cache::router(cache.clone())
            .oneshot(
                Request::head(format!("/media/v1/p/{COPY}/1"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    };
    for expected in ["miss", "hit"] {
        let response = head().await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-komika-cache"], expected);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/vnd.komika.pack"
        );
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            f.bytes.len().to_string()
        );
        assert!(response.headers()[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("s-maxage="));
        assert!(collect(response).await.unwrap().is_empty());
        if expected == "miss" {
            assert_eq!(f.mock.packs.load(Ordering::SeqCst), 0);
            f.clean(&cache);
            collect(get_pack(&cache).await).await.unwrap();
        }
    }
    assert_eq!(f.mock.packs.load(Ordering::SeqCst), 1);
    assert_eq!(cache.inflight_len(), 0);
}
