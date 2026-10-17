use super::*;

use crate::media_manifest::store;
use crate::media_manifest_tests::TempDir;
use crate::media_pack::{build_index, parse_header, verify_pack, PackError, PackIndex};
use crate::media_policy::Kind;
use crate::media_policy_tests::*;
use crate::media_verify::{verify_manifest, CopyIdentity, ManifestBody, ManifestPage};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::{sync::atomic::Ordering, time::Duration};
use tower::ServiceExt;

fn manifest(objects: &[&[u8]]) -> ManifestBody {
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
        issued_at: 1,
        kid: String::new(),
        pages: objects
            .iter()
            .map(|bytes| ManifestPage {
                sha256: hex::encode(Sha256::digest(bytes)),
                length: bytes.len() as u64,
                mime: "image/png".into(),
                width: None,
                height: None,
            })
            .collect(),
    }
}

fn encode_header(index: &PackIndex) -> Vec<u8> {
    let json = serde_json::to_vec(index).unwrap();
    let mut bytes = b"KPK1".to_vec();
    bytes.extend_from_slice(&(json.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&json);
    bytes
}

#[test]
fn format_offsets_deduplication_length_and_roundtrip() {
    let manifest = manifest(&[b"credits", b"first", b"credits", b"last"]);
    let pack = build_index(&manifest).unwrap();
    assert_eq!(pack.unique, [0, 1, 3]);
    assert_eq!(&pack.header[..4], b"KPK1");
    assert_eq!(
        u32::from_be_bytes(pack.header[4..8].try_into().unwrap()) as usize,
        pack.header.len() - 8
    );
    let index = parse_header(&pack.header).unwrap();
    assert_eq!(index.v, 1);
    assert_eq!(index.copy_id, manifest.copy_id);
    assert_eq!(index.revision, manifest.revision);
    assert_eq!(
        index.pages.iter().map(|p| p.offset).collect::<Vec<_>>(),
        [0, 7, 0, 12]
    );
    assert_eq!(encode_header(&index), pack.header);
    assert_eq!(pack.total_len, pack.header.len() as u64 + 16);
    let mut bytes = pack.header;
    bytes.extend_from_slice(b"creditsfirstlast");
    assert_eq!(bytes.len() as u64, pack.total_len);
    assert_eq!(parse_header(&bytes).unwrap(), index);
    assert_eq!(
        verify_pack(&bytes, &manifest).unwrap(),
        [
            b"credits".to_vec(),
            b"first".to_vec(),
            b"credits".to_vec(),
            b"last".to_vec()
        ]
    );
    let empty = manifest_with_no_pages();
    let pack = build_index(&empty).unwrap();
    assert_eq!(pack.total_len, pack.header.len() as u64);
    assert!(verify_pack(&pack.header, &empty).unwrap().is_empty());
}

fn manifest_with_no_pages() -> ManifestBody {
    manifest(&[])
}

#[test]
fn format_hostile_headers_and_unrepresentable_lengths_are_refused() {
    for bytes in [b"".as_slice(), b"KPK1", b"KPK1\0\0\0\x02{"] {
        assert_eq!(parse_header(bytes), Err(PackError::ShortHeader));
    }
    assert_eq!(parse_header(b"NOPE\0\0\0\0"), Err(PackError::Magic));
    assert_eq!(
        parse_header(b"KPK1\xff\xff\xff\xff"),
        Err(PackError::IndexTooLarge)
    );
    assert_eq!(parse_header(b"KPK1\0\0\0\x01\xff"), Err(PackError::Json));
    let manifest = manifest(&[b"a", b"b"]);
    let mut index = parse_header(&build_index(&manifest).unwrap().header).unwrap();
    index.pages[0].offset = u64::MAX;
    assert_eq!(
        parse_header(&encode_header(&index)),
        Err(PackError::Overflow)
    );
    index.pages[0].offset = 0;
    index.v = 2;
    assert_eq!(
        parse_header(&encode_header(&index)),
        Err(PackError::Version)
    );
    let mut huge = manifest.clone();
    huge.pages[0].length = u64::MAX;
    assert!(matches!(build_index(&huge), Err(PackError::Overflow)));
    huge.pages.pop(); // Data fits u64, but the header plus data does not.
    assert!(matches!(build_index(&huge), Err(PackError::Overflow)));
    huge.pages[0].length = 1;
    huge.pages[0].mime = "x".repeat(1024 * 1024);
    assert!(matches!(build_index(&huge), Err(PackError::IndexTooLarge)));
}

#[test]
fn format_verification_rejects_changed_identity_metadata_ranges_and_bytes() {
    let manifest = manifest(&[b"a", b"b"]);
    let pack = build_index(&manifest).unwrap();
    let original = parse_header(&pack.header).unwrap();
    for change in 0..8 {
        let mut index = original.clone();
        match change {
            0 => index.copy_id.push('x'),
            1 => index.revision += 1,
            2 => {
                index.pages.pop();
            }
            3 => index.pages[0].length += 1,
            4 => index.pages[0].mime = "image/jpeg".into(),
            5 => index.pages[0].sha256 = "0".repeat(64),
            6 => index.pages[0].offset = 3,
            _ => index.pages[0].offset = u64::MAX,
        }
        let mut bytes = encode_header(&index);
        bytes.extend_from_slice(b"ab");
        assert!(verify_pack(&bytes, &manifest).is_err(), "change {change}");
    }
    let mut bytes = pack.header;
    bytes.extend_from_slice(b"ac");
    assert_eq!(verify_pack(&bytes, &manifest), Err(PackError::Hash));
    bytes.pop();
    assert_eq!(verify_pack(&bytes, &manifest), Err(PackError::Range));
    bytes.extend_from_slice(b"bc");
    assert_eq!(verify_pack(&bytes, &manifest), Err(PackError::Range));
}

type Dirs = (TempDir, crate::cover_store::test_support::TestDir);

async fn fixture() -> (Dirs, Engine, Arc<AppState>, String, String) {
    let dir = TempDir::new();
    let cover_dir = crate::cover_store::test_support::TestDir::new();
    let engine = Engine::spawn().await;
    let app = app(
        pool().await,
        &engine,
        &dir.0.join("signing.key"),
        &cover_dir,
    )
    .await;
    let (a, b) = two_copies(&app.pool).await;
    ((dir, cover_dir), engine, app, a, b)
}

async fn signed_manifest(app: &Arc<AppState>, copy: &str, revision: i32) -> ManifestBody {
    let response = get_path(app, &format!("/media/v1/manifest/{copy}?rev={revision}")).await;
    assert_eq!(response.status(), StatusCode::OK);
    verify_manifest(&body(response).await, &pins(app)).unwrap()
}

#[tokio::test]
async fn route_pack_matches_signed_revision_and_exact_headers() {
    let (_dir, engine, app, a, b) = fixture().await;
    // The requested revision remains exact even after a newer manifest is filed.
    let generation = engine_keys::adopt(&app.pool).await.unwrap();
    let (_, revision) = store::publish(
        &app.pool,
        Some(generation),
        &identity(8),
        &[collected(&page_bytes(0, 1)), collected(&page_bytes(0, 0))],
    )
    .await
    .unwrap();
    assert_eq!(revision, Some(2));
    for (copy, order) in [(&a, 0), (&b, 1)] {
        let manifest = signed_manifest(&app, copy, 1).await;
        let response = get_path(&app, &format!("/media/v1/p/{copy}/1")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/vnd.komika.pack"
        );
        assert_eq!(
            response.headers()[header::CONTENT_SECURITY_POLICY],
            "sandbox; default-src 'none'"
        );
        let cache = cache_control(&response);
        assert!(cache.starts_with("public, max-age=31536000, immutable, s-maxage="));
        let lease: i64 = cache.rsplit('=').next().unwrap().parse().unwrap();
        assert!((1..=media_policy::LEASE_SECS).contains(&lease));
        let length: u64 = response.headers()[header::CONTENT_LENGTH]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let bytes = body(response).await;
        assert_eq!(length, bytes.len() as u64);
        assert_eq!(
            verify_pack(&bytes, &manifest).unwrap(),
            [page_bytes(order, 0), page_bytes(order, 1)]
        );
    }
    assert_eq!(
        *engine.requests.lock().unwrap(),
        [(0, 0), (0, 1), (1, 0), (1, 1)]
    );
}

#[tokio::test]
async fn route_identical_pages_are_stored_and_fetched_once() {
    let (_dir, engine, app, _, _) = fixture().await;
    let generation = engine_keys::adopt(&app.pool).await.unwrap();
    sqlx::query("INSERT INTO engine_chapter_key(generation,chapter_id,manga_id,source_order,chapter_key,updated_at) VALUES($1,10,7,2,'/chapter-10',now())")
        .bind(generation).execute(&app.pool).await.unwrap();
    let page = page_bytes(2, 0);
    let (copy, revision) = store::publish(
        &app.pool,
        Some(generation),
        &identity(10),
        &[collected(&page), collected(&page)],
    )
    .await
    .unwrap();
    assert_eq!(revision, Some(1));
    let manifest = signed_manifest(&app, &copy, 1).await;
    let response = get_path(&app, &format!("/media/v1/p/{copy}/1")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let length: usize = response.headers()[header::CONTENT_LENGTH]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let bytes = body(response).await;
    assert_eq!(length, bytes.len());
    let index = parse_header(&bytes).unwrap();
    assert_eq!(index.pages[0].offset, index.pages[1].offset);
    assert_eq!(
        bytes.len(),
        build_index(&manifest).unwrap().header.len() + page.len()
    );
    assert_eq!(
        verify_pack(&bytes, &manifest).unwrap(),
        [page.clone(), page]
    );
    assert_eq!(*engine.requests.lock().unwrap(), [(2, 0)]);
}

fn refused(response: axum::response::Response) {
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(cache_control(&response), "no-store, must-revalidate");
}

#[tokio::test]
async fn route_refuses_denied_stale_missing_invalid_and_disabled_packs() {
    let (_dir, engine, mut app, a, b) = fixture().await;
    for revision in ["2", "0", "-1", "2147483648", "bad"] {
        refused(get_path(&app, &format!("/media/v1/p/{a}/{revision}")).await);
    }
    for copy in [
        "bad".to_string(),
        format!("mc_{}", "f".repeat(32)),
        format!("mc_{}", "A".repeat(32)),
        "%FF".to_string(),
    ] {
        refused(get_path(&app, &format!("/media/v1/p/{copy}/1")).await);
    }
    store::mark_stale(&app.pool, &b, 1).await.unwrap();
    refused(get_path(&app, &format!("/media/v1/p/{b}/1")).await);
    take_down(&app, Kind::Copy, &a).await;
    refused(get_path(&app, &format!("/media/v1/p/{a}/1")).await);
    Arc::get_mut(&mut app).unwrap().media_manifests = None;
    refused(get_path(&app, &format!("/media/v1/p/{a}/1")).await);
    assert!(engine.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn route_changed_second_object_breaks_body_and_marks_revision_stale() {
    let (_dir, engine, app, copy, _) = fixture().await;
    engine.wrong_second.store(true, Ordering::SeqCst);
    let response = get_path(&app, &format!("/media/v1/p/{copy}/1")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut chunks = response.into_body().into_data_stream();
    assert!(chunks.next().await.unwrap().unwrap().starts_with(b"KPK1"));
    assert_eq!(
        chunks.next().await.unwrap().unwrap().as_ref(),
        page_bytes(0, 0)
    );
    assert!(tokio::time::timeout(Duration::from_secs(2), chunks.next())
        .await
        .unwrap()
        .unwrap()
        .is_err());
    assert!(chunks.next().await.is_none());
    assert!(
        store::revision(&app.pool, &copy, 1)
            .await
            .unwrap()
            .unwrap()
            .stale
    );
    assert_eq!(*engine.requests.lock().unwrap(), [(0, 0), (0, 1)]);
    refused(get_path(&app, &format!("/media/v1/p/{copy}/1")).await);
}

#[tokio::test]
async fn route_cancel_after_header_stops_inflight_fetch_and_later_pages() {
    let (_dir, engine, app, copy, _) = fixture().await;
    engine.gates.lock().unwrap()[0] = Some(Arc::new(tokio::sync::Semaphore::new(0)));
    engine.delays.lock().unwrap()[1] = Duration::from_secs(1);
    let response = get_path(&app, &format!("/media/v1/p/{copy}/1")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut chunks = response.into_body().into_data_stream();
    assert!(chunks.next().await.unwrap().unwrap().starts_with(b"KPK1"));
    // Ensure there really is an in-flight HTTP fetch when the body is dropped.
    tokio::time::timeout(Duration::from_secs(1), engine.requested.notified())
        .await
        .unwrap();
    assert_eq!(*engine.requests.lock().unwrap(), [(0, 0)]);
    drop(chunks);
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(*engine.requests.lock().unwrap(), [(0, 0)]);
    // The producer releases its application owner immediately, well before the
    // delayed engine response. A detached, still-fetching task would retain it.
    assert_eq!(Arc::strong_count(&app), 1);
}

#[tokio::test]
async fn route_policy_change_during_fetch_aborts_before_the_object_is_sent() {
    let (_dir, engine, app, copy, _) = fixture().await;
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    engine.gates.lock().unwrap()[0] = Some(gate.clone());
    let response = get_path(&app, &format!("/media/v1/p/{copy}/1")).await;
    let mut chunks = response.into_body().into_data_stream();
    assert!(chunks.next().await.unwrap().unwrap().starts_with(b"KPK1"));
    tokio::time::timeout(Duration::from_secs(1), engine.requested.notified())
        .await
        .unwrap();
    take_down(&app, Kind::Copy, &copy).await;
    gate.add_permits(1);
    assert!(tokio::time::timeout(Duration::from_secs(2), chunks.next())
        .await
        .unwrap()
        .unwrap()
        .is_err());
    assert_eq!(*engine.requests.lock().unwrap(), [(0, 0)]);
    assert!(
        !store::revision(&app.pool, &copy, 1)
            .await
            .unwrap()
            .unwrap()
            .stale
    );
}

async fn send(
    app: Router,
    request: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    tokio::time::timeout(Duration::from_secs(5), app.oneshot(request))
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn route_pack_is_not_compressed_and_keeps_its_exact_length() {
    let (_dir, _engine, app, copy, _) = fixture().await;
    let manifest = signed_manifest(&app, &copy, 1).await;
    let expected = build_index(&manifest).unwrap().total_len;
    // The owner's own compression layer, as `main` installs it.
    let compressed = router(app.clone()).layer(compression_layer());
    let gzip = |path: &str| {
        axum::http::Request::get(path)
            .header(header::ACCEPT_ENCODING, "gzip")
            .body(axum::body::Body::empty())
            .unwrap()
    };
    let response = send(compressed.clone(), gzip(&format!("/media/v1/p/{copy}/1"))).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
    assert_eq!(
        response.headers()[header::CONTENT_LENGTH],
        expected.to_string()
    );
    let bytes = body(response).await;
    assert_eq!(bytes.len() as u64, expected);
    assert!(verify_pack(&bytes, &manifest).is_ok());
    // The same layer still compresses ordinary JSON answers.
    let policy = send(compressed, gzip("/media/v1/policy")).await;
    assert_eq!(policy.headers()[header::CONTENT_ENCODING], "gzip");
}

#[tokio::test]
async fn route_pack_head_answers_headers_without_fetching() {
    let (_dir, engine, app, copy, _) = fixture().await;
    let manifest = signed_manifest(&app, &copy, 1).await;
    let expected = build_index(&manifest).unwrap().total_len;
    let response = send(
        router(app.clone()),
        axum::http::Request::head(format!("/media/v1/p/{copy}/1"))
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/vnd.komika.pack"
    );
    assert_eq!(
        response.headers()[header::CONTENT_LENGTH],
        expected.to_string()
    );
    assert!(cache_control(&response).starts_with("public, max-age=31536000, immutable, s-maxage="));
    assert!(body(response).await.is_empty());
    assert!(engine.requests.lock().unwrap().is_empty());
    // A refused pack is refused for HEAD as well.
    store::mark_stale(&app.pool, &copy, 1).await.unwrap();
    let response = send(
        router(app.clone()),
        axum::http::Request::head(format!("/media/v1/p/{copy}/1"))
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(engine.requests.lock().unwrap().is_empty());
}
