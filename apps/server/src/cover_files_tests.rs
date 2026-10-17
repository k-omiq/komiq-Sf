use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::cover;
use crate::cover_copy::{self, Batch};
use crate::cover_store::test_support::{legacy_pool, store, store_with_legacy, TestDir};
use crate::cover_store::{self, CopyCursor, CopyPhase, CoverStore, Kind, Master, ObjectRow};
use crate::database::TestPoolOptions;
use crate::db::{Database, SqliteDatabase};
use crate::locks::pause_points::PausePoints;

async fn pools() -> (Database, CoverStore, TestDir) {
    let main = TestPoolOptions::new()
        .max_connections(4)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!("./migrations_pg")
        .run(main.write())
        .await
        .unwrap();
    let dir = TestDir::new();
    let covers = store(&dir);
    (main.foreground(), covers, dir)
}

async fn work(main: &Database, id: &str, pointer: Option<i64>) {
    sqlx::query(
        "INSERT INTO work(id,cover_cached_version,created_at,updated_at) VALUES($1,$2,now(),now())",
    )
    .bind(id)
    .bind(pointer)
    .execute(main)
    .await
    .unwrap();
}

async fn indexed(main: &Database, asset: &str, kind: Kind) -> ObjectRow {
    cover_store::row(main, asset, kind).await.unwrap().unwrap()
}

async fn master(main: &Database, covers: &CoverStore, asset: &str) -> Master {
    covers
        .read_master(main, asset, None)
        .await
        .unwrap()
        .unwrap()
        .0
}

async fn pointer(main: &Database, work_id: &str) -> Option<i64> {
    sqlx::query_scalar("SELECT cover_cached_version FROM work WHERE id=$1")
        .bind(work_id)
        .fetch_one(main)
        .await
        .unwrap()
}

async fn assert_publication(main: &Database, covers: &CoverStore, old: &Master) {
    let row = indexed(main, "w_cover", Kind::Master).await;
    assert_eq!(row.sha, old.sha);
    assert_eq!(row.version, old.version);
    assert_eq!(pointer(main, "w_cover").await, old.version);
    assert_eq!(
        std::fs::read(covers.object_path(&old.sha)).unwrap(),
        old.bytes
    );
    let served = master(main, covers, "w_cover").await;
    assert_eq!(
        (served.bytes, served.sha, served.version),
        (old.bytes.clone(), old.sha, old.version)
    );
    assert!(!served.legacy);
}

#[tokio::test]
async fn aborted_publication_after_staging_preserves_old_cover() {
    let (main, covers, _dir) = pools().await;
    work(&main, "w_cover", None).await;
    cover::put_work_cover(&main, &covers, "w_cover", b"v1")
        .await
        .unwrap();
    let old = master(&main, &covers, "w_cover").await;
    let points = PausePoints::default();
    let armed = points.arm("cover-locked");
    let writer = main.clone();
    let cache = covers.clone();
    let publishing = tokio::spawn(cover::COVER_PUBLICATION_PAUSE.scope(points, async move {
        cover::put_work_cover(&writer, &cache, "w_cover", b"v2").await
    }));
    tokio::time::timeout(Duration::from_secs(30), armed.reached())
        .await
        .unwrap();
    let staged: Vec<_> = std::fs::read_dir(covers.tmp_dir()).unwrap().collect();
    assert_eq!(
        staged.len(),
        1,
        "v2 must be durably staged before its publish locks"
    );
    assert_eq!(
        std::fs::read(staged[0].as_ref().unwrap().path()).unwrap(),
        b"v2"
    );
    assert_publication(&main, &covers, &old).await;
    publishing.abort();
    assert!(publishing.await.unwrap_err().is_cancelled());
    // Wait for the dropped transaction's rollback before inspecting the result.
    let mut tx = main.begin().await.unwrap();
    let _guards = cover::lock_assets_and_files(&mut tx, &["w_cover"], &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_publication(&main, &covers, &old).await;
    assert_eq!(std::fs::read_dir(covers.tmp_dir()).unwrap().count(), 0);
}

#[tokio::test]
async fn failed_commit_after_placing_cover_preserves_old_publication() {
    let (main, covers, _dir) = pools().await;
    work(&main, "w_cover", None).await;
    cover::put_work_cover(&main, &covers, "w_cover", b"v1")
        .await
        .unwrap();
    let old = master(&main, &covers, "w_cover").await;
    sqlx::raw_sql("CREATE FUNCTION fail_cover_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'cover commit rejected'; END $$; CREATE CONSTRAINT TRIGGER fail_cover_commit AFTER UPDATE ON work DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION fail_cover_commit()")
        .execute(&main).await.unwrap();
    let error = cover::put_work_cover(&main, &covers, "w_cover", b"v2")
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("cover commit rejected"),
        "{error:#}"
    );
    assert_eq!(
        std::fs::read(covers.object_path(&cover_store::sha256(b"v2"))).unwrap(),
        b"v2",
        "the failure must happen after placing the new file"
    );
    assert_publication(&main, &covers, &old).await;
}

#[tokio::test]
async fn missing_source_file_is_a_miss_and_guarded_fill_replaces_stale_index() {
    let (main, covers, _dir) = pools().await;
    work(&main, "w_cover", None).await;
    sqlx::query("UPDATE work SET cover_file_name='cover.jpg' WHERE id='w_cover'")
        .execute(&main)
        .await
        .unwrap();
    sqlx::query("INSERT INTO source_series(id,work_id,source_type,source_key,created_at) VALUES('ss_cover','w_cover','mangadex','md-cover',now())")
        .execute(&main).await.unwrap();
    cover::put_work_cover(&main, &covers, "w_cover", b"source-v1")
        .await
        .unwrap();
    let old = indexed(&main, "w_cover", Kind::Master).await;
    std::fs::remove_file(covers.object_path(&old.sha)).unwrap();
    assert!(covers
        .read_master(&main, "w_cover", None)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        indexed(&main, "w_cover", Kind::Master).await,
        old,
        "the fill must handle a live row whose file disappeared"
    );
    assert!(cover::put_work_cover_for_missing_anchor(
        &main,
        &covers,
        "w_cover",
        b"source-v2",
        "md-cover",
        "cover.jpg",
    )
    .await
    .unwrap());
    let fresh = master(&main, &covers, "w_cover").await;
    assert_eq!(fresh.bytes, b"source-v2");
    assert_eq!(fresh.sha, cover_store::sha256(b"source-v2"));
    assert!(fresh.version > old.version);
    assert_eq!(pointer(&main, "w_cover").await, fresh.version);
}

#[tokio::test]
async fn missing_upload_file_is_restored_with_the_same_bytes_and_version() {
    let (main, covers, _dir) = pools().await;
    work(&main, "w_cover", None).await;
    assert!(
        cover::put_uploaded_work_cover(&main, &covers, "w_cover", b"uploaded")
            .await
            .unwrap()
    );
    let old = master(&main, &covers, "w_cover").await;
    let row = indexed(&main, "w_cover", Kind::Master).await;
    std::fs::remove_file(covers.object_path(&old.sha)).unwrap();
    assert!(!covers.object_path(&old.sha).exists());
    let restored = master(&main, &covers, "w_cover").await;
    assert_eq!(
        (restored.bytes, restored.version),
        (old.bytes.clone(), old.version)
    );
    assert_publication(&main, &covers, &old).await;
    assert_eq!(indexed(&main, "w_cover", Kind::Master).await, row);
}

#[tokio::test]
async fn missing_fill_does_not_replace_an_upload_whose_file_is_lost() {
    let (main, covers, _dir) = pools().await;
    work(&main, "w_cover", None).await;
    sqlx::query("UPDATE work SET cover_file_name='cover.jpg' WHERE id='w_cover'")
        .execute(&main)
        .await
        .unwrap();
    sqlx::query("INSERT INTO source_series(id,work_id,source_type,source_key,created_at) VALUES('ss_cover','w_cover','mangadex','md-cover',now())")
        .execute(&main).await.unwrap();
    assert!(
        cover::put_uploaded_work_cover(&main, &covers, "w_cover", b"uploaded")
            .await
            .unwrap()
    );
    let upload = indexed(&main, "w_cover", Kind::Master).await;
    std::fs::remove_file(covers.object_path(&upload.sha)).unwrap();
    assert!(!cover::put_work_cover_for_missing_anchor(
        &main,
        &covers,
        "w_cover",
        b"source",
        "md-cover",
        "cover.jpg",
    )
    .await
    .unwrap());
    assert_eq!(indexed(&main, "w_cover", Kind::Master).await, upload);
    let backups: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cover_upload WHERE work_id='w_cover'")
            .fetch_one(&main)
            .await
            .unwrap();
    assert_eq!(backups, 1, "the upload backup survives");
    assert_eq!(master(&main, &covers, "w_cover").await.bytes, b"uploaded");
}

#[tokio::test]
async fn recompression_updates_upload_backup_and_source_publish_removes_it() {
    let (main, covers, _dir) = pools().await;
    work(&main, "w_cover", None).await;
    assert!(
        cover::put_uploaded_work_cover(&main, &covers, "w_cover", b"uploaded")
            .await
            .unwrap()
    );
    assert!(cover::put_work_cover_if_current(
        &main,
        &covers,
        "w_cover",
        b"recompressed",
        Some(b"uploaded"),
    )
    .await
    .unwrap());
    let current = master(&main, &covers, "w_cover").await;
    let backup: (Vec<u8>, Vec<u8>) =
        sqlx::query_as("SELECT bytes,sha256 FROM cover_upload WHERE work_id='w_cover'")
            .fetch_one(&main)
            .await
            .unwrap();
    assert_eq!(backup, (b"recompressed".to_vec(), current.sha.to_vec()));
    std::fs::remove_file(covers.object_path(&current.sha)).unwrap();
    assert_eq!(
        master(&main, &covers, "w_cover").await.bytes,
        b"recompressed"
    );
    assert_publication(&main, &covers, &current).await;

    cover::put_work_cover(&main, &covers, "w_cover", b"source")
        .await
        .unwrap();
    let backups: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cover_upload WHERE work_id='w_cover'")
            .fetch_one(&main)
            .await
            .unwrap();
    assert_eq!(backups, 0);
    let source = master(&main, &covers, "w_cover").await;
    assert_eq!(source.bytes, b"source");
    std::fs::remove_file(covers.object_path(&source.sha)).unwrap();
    assert!(
        covers
            .read_master(&main, "w_cover", None)
            .await
            .unwrap()
            .is_none(),
        "an old upload must not restore over the source master"
    );
}

async fn legacy_work(legacy: &SqliteDatabase, id: &str, bytes: &[u8], version: i64) {
    sqlx::query("INSERT INTO work_cover_blob VALUES($1,$2,$3,'2026-01-01')")
        .bind(id)
        .bind(bytes)
        .bind(version)
        .execute(legacy)
        .await
        .unwrap();
}

async fn legacy_source(legacy: &SqliteDatabase, id: i64, bytes: &[u8]) {
    sqlx::query("INSERT INTO suwayomi_cover_blob VALUES($1,$2,'2026-01-01')")
        .bind(id)
        .bind(bytes)
        .execute(legacy)
        .await
        .unwrap();
}

async fn legacy_variants(legacy: &SqliteDatabase, asset: &str) {
    for width in cover::VARIANT_WIDTHS {
        sqlx::query("INSERT INTO cover_variant VALUES($1,$2,$3,'2026-01-01')")
            .bind(asset)
            .bind(width)
            .bind(format!("{asset}-{width}").into_bytes())
            .execute(legacy)
            .await
            .unwrap();
    }
}

async fn finish_copy(main: &Database, covers: &CoverStore) -> Batch {
    let mut totals = Batch::default();
    for _ in 0..32 {
        let batch = cover_copy::run_batch(main, covers, 1).await.unwrap();
        totals.copied += batch.copied;
        totals.skipped += batch.skipped;
        if batch.done {
            totals.done = true;
            return totals;
        }
    }
    panic!("copy failed to finish its small fixture");
}

async fn assert_object(
    main: &Database,
    covers: &CoverStore,
    asset: &str,
    kind: Kind,
    bytes: &[u8],
) {
    let row = indexed(main, asset, kind).await;
    let expected: [u8; 32] = Sha256::digest(bytes).into();
    assert_eq!(row.sha, expected, "{asset} {kind:?}");
    assert_eq!(row.byte_length, bytes.len() as i32);
    let file = std::fs::read(covers.object_path(&row.sha)).unwrap();
    let file_sha: [u8; 32] = Sha256::digest(&file).into();
    assert_eq!(file_sha, expected);
    assert_eq!(file, bytes);
}

async fn copy_fixture(resume: bool) {
    let (main, _covers, dir) = pools().await;
    let legacy = legacy_pool().await;
    let mut covers = store_with_legacy(&dir, legacy.clone(), CopyCursor::default());
    sqlx::query("INSERT INTO engine_generation(id,first_seen,last_seen,state) VALUES($1,now(),now(),'active')")
        .bind(uuid::Uuid::new_v4()).execute(&main).await.unwrap();
    for (id, version, published) in [("w_a", 7, None), ("w_b", 8, Some(99))] {
        work(&main, id, published).await;
        legacy_work(&legacy, id, id.as_bytes(), version).await;
        legacy_variants(&legacy, id).await;
    }
    for id in [7, 10] {
        legacy_source(&legacy, id, format!("source-{id}").as_bytes()).await;
        legacy_variants(&legacy, &cover::suwayomi_variant_key(id)).await;
    }
    for (pkg, png) in [
        ("pkg.art", b"icon".as_slice()),
        ("pkg.miss", b"".as_slice()),
    ] {
        sqlx::query("INSERT INTO extension_icon_blob VALUES($1,$2,'2026-01-02 03:04:05')")
            .bind(pkg)
            .bind(png)
            .execute(&legacy)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO cover_maintenance VALUES($1,'',1,'2026-01-01')")
        .bind(cover_copy::RECOMPRESS_SETTING)
        .execute(&legacy)
        .await
        .unwrap();
    let mut copied = 0;
    let first_row = if resume {
        let batch = cover_copy::run_batch(&main, &covers, 1).await.unwrap();
        assert_eq!(
            batch,
            Batch {
                copied: 1,
                skipped: 0,
                done: false
            }
        );
        copied += batch.copied;
        let cursor = cover_store::load_cursor(&main).await.unwrap().unwrap();
        assert_eq!(
            cursor,
            CopyCursor {
                phase: CopyPhase::Work,
                after: "w_a".into()
            }
        );
        let first = indexed(&main, "w_a", Kind::Master).await;
        drop(covers);
        covers = store_with_legacy(&dir, legacy.clone(), cursor);
        Some(first)
    } else {
        None
    };
    let remaining = finish_copy(&main, &covers).await;
    assert_eq!(
        copied + remaining.copied,
        6,
        "restart must not replay committed assets"
    );
    assert_eq!(remaining.skipped, 0);
    assert!(covers.copy_done());
    assert!(covers.legacy().is_none());
    assert_eq!(
        cover_store::load_cursor(&main).await.unwrap(),
        Some(CopyCursor::done())
    );
    if let Some(first) = first_row {
        assert_eq!(
            indexed(&main, "w_a", Kind::Master).await,
            first,
            "resuming must not rewrite a previously copied row"
        );
    }

    let works: Vec<(String, Vec<u8>, i64)> =
        sqlx::query_as("SELECT work_id,webp,version FROM work_cover_blob")
            .fetch_all(&legacy)
            .await
            .unwrap();
    for (id, bytes, version) in works {
        assert_object(&main, &covers, &id, Kind::Master, &bytes).await;
        assert_eq!(
            indexed(&main, &id, Kind::Master).await.version,
            Some(version)
        );
    }
    let sources: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT manga_id,webp FROM suwayomi_cover_blob")
            .fetch_all(&legacy)
            .await
            .unwrap();
    for (id, bytes) in sources {
        assert_object(
            &main,
            &covers,
            &cover::suwayomi_variant_key(id),
            Kind::Master,
            &bytes,
        )
        .await;
    }
    let variants: Vec<(String, i64, Vec<u8>)> =
        sqlx::query_as("SELECT asset_key,width,webp FROM cover_variant")
            .fetch_all(&legacy)
            .await
            .unwrap();
    for (asset, width, bytes) in variants {
        let kind = Kind::Width(width as u32);
        assert_object(&main, &covers, &asset, kind, &bytes).await;
        assert_eq!(
            indexed(&main, &asset, kind).await.master,
            Some(indexed(&main, &asset, Kind::Master).await.sha)
        );
    }
    let icons: Vec<(String, Vec<u8>)> =
        sqlx::query_as("SELECT pkg_name,png FROM extension_icon_blob")
            .fetch_all(&legacy)
            .await
            .unwrap();
    for (pkg, png) in icons {
        let asset = cover_store::icon_asset(&pkg);
        assert_object(&main, &covers, &asset, Kind::Master, &png).await;
        assert_eq!(
            indexed(&main, &asset, Kind::Master)
                .await
                .updated_at
                .to_rfc3339(),
            "2026-01-02T03:04:05+00:00",
            "a tombstone's expiry must keep its SQLite age"
        );
    }
    assert_eq!(pointer(&main, "w_a").await, Some(7));
    assert_eq!(pointer(&main, "w_b").await, Some(99));
    let recompress: String = sqlx::query_scalar("SELECT value FROM instance_setting WHERE key=$1")
        .bind(cover_copy::RECOMPRESS_SETTING)
        .fetch_one(&main)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&recompress).unwrap()["done"],
        true
    );
    let before: Vec<(String, String, Vec<u8>, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT asset_key,kind,sha256,updated_at FROM cover_object ORDER BY asset_key,kind",
    )
    .fetch_all(&main)
    .await
    .unwrap();
    assert_eq!(before.len(), 14);
    let settings: Vec<(String, String, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("SELECT key,value,updated_at FROM instance_setting ORDER BY key")
            .fetch_all(&main)
            .await
            .unwrap();
    assert_eq!(
        cover_copy::run_batch(&main, &covers, 1).await.unwrap(),
        Batch {
            copied: 0,
            skipped: 0,
            done: true
        }
    );
    let after: Vec<(String, String, Vec<u8>, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT asset_key,kind,sha256,updated_at FROM cover_object ORDER BY asset_key,kind",
    )
    .fetch_all(&main)
    .await
    .unwrap();
    assert_eq!(
        after, before,
        "a finished copy must leave every row untouched"
    );
    let settings_after: Vec<(String, String, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("SELECT key,value,updated_at FROM instance_setting ORDER BY key")
            .fetch_all(&main)
            .await
            .unwrap();
    assert_eq!(
        settings_after, settings,
        "a finished copy must not rewrite its cursor"
    );
}

#[tokio::test]
async fn copy_preserves_all_masters_variants_icons_and_newer_pointers() {
    copy_fixture(false).await;
}

#[tokio::test]
async fn copy_resumes_from_persisted_cursor_without_duplicate_work() {
    copy_fixture(true).await;
}

#[tokio::test]
async fn copy_keeps_newer_publication_and_skips_missing_and_taken_down_works() {
    let (main, _covers, dir) = pools().await;
    let legacy = legacy_pool().await;
    let covers = store_with_legacy(&dir, legacy.clone(), CopyCursor::default());
    work(&main, "w_fresh", None).await;
    work(&main, "w_taken", None).await;
    for id in ["w_fresh", "w_missing", "w_taken"] {
        legacy_work(&legacy, id, b"old-master", 7).await;
        legacy_variants(&legacy, id).await;
    }
    cover::put_work_cover(&main, &covers, "w_fresh", b"new-master")
        .await
        .unwrap();
    let fresh = indexed(&main, "w_fresh", Kind::Master).await;
    sqlx::query("INSERT INTO media_takedown(subject_kind,subject_id,epoch,reason,created_at) VALUES('work','w_taken',1,'test',now())")
        .execute(&main).await.unwrap();
    assert_eq!(
        finish_copy(&main, &covers).await,
        Batch {
            copied: 0,
            skipped: 3,
            done: true
        }
    );
    assert_eq!(indexed(&main, "w_fresh", Kind::Master).await, fresh);
    assert_eq!(master(&main, &covers, "w_fresh").await.bytes, b"new-master");
    assert_eq!(pointer(&main, "w_fresh").await, fresh.version);
    assert_eq!(pointer(&main, "w_taken").await, None);
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT asset_key,kind FROM cover_object")
        .fetch_all(&main)
        .await
        .unwrap();
    assert_eq!(
        rows,
        vec![("w:w_fresh".into(), "master".into())],
        "old derivatives and skipped works must not be indexed"
    );
}

#[tokio::test]
async fn copy_stops_without_advancing_when_its_numeric_generation_detaches() {
    let (main, _covers, dir) = pools().await;
    let legacy = legacy_pool().await;
    legacy_source(&legacy, 7, b"old-generation").await;
    legacy_variants(&legacy, "s:7").await;
    let cursor = CopyCursor {
        phase: CopyPhase::Source,
        after: String::new(),
    };
    cover_store::save_cursor(&main, &cursor).await.unwrap();
    let covers = store_with_legacy(&dir, legacy.clone(), cursor.clone());
    let old = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO engine_generation(id,first_seen,last_seen,state) VALUES($1,now(),now(),'active')")
        .bind(old).execute(&main).await.unwrap();
    let points = PausePoints::default();
    let armed = points.arm("transaction-open");
    let writer = main.clone();
    let cache = covers.clone();
    let copying = tokio::spawn(
        crate::database::TRANSACTION_OPEN_PAUSE.scope(points, async move {
            cover_copy::run_batch(&writer, &cache, 1).await
        }),
    );
    tokio::time::timeout(Duration::from_secs(30), armed.reached())
        .await
        .unwrap();
    assert_eq!(
        crate::engine_generation::current(&main).await.unwrap(),
        Some(old)
    );
    // The batch has read and staged its rows, but has not acquired the generation lock.
    let new = uuid::Uuid::new_v4();
    let mut tx = main.begin().await.unwrap();
    sqlx::query("UPDATE engine_generation SET state='detached' WHERE id=$1")
        .bind(old)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO engine_generation(id,first_seen,last_seen,state) VALUES($1,now(),now(),'active')")
        .bind(new).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    armed.release();
    assert_eq!(copying.await.unwrap().unwrap(), Batch::default());
    assert_eq!(covers.copy_cursor(), cursor);
    assert_eq!(cover_store::load_cursor(&main).await.unwrap(), Some(cursor));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM cover_object")
        .fetch_one(&main)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(
        crate::engine_generation::current(&main).await.unwrap(),
        Some(new)
    );
    let legacy_count: i64 = sqlx::query_scalar("SELECT count(*) FROM suwayomi_cover_blob")
        .fetch_one(&legacy)
        .await
        .unwrap();
    assert_eq!(
        legacy_count, 1,
        "a generation check, rather than an empty cache, stopped the copy"
    );
}

#[tokio::test]
async fn sqlite_fallback_is_limited_by_cursor_and_active_engine_gate() {
    let (main, _covers, dir) = pools().await;
    let legacy = legacy_pool().await;
    let covers = store_with_legacy(&dir, legacy.clone(), CopyCursor::default());
    work(&main, "w_cover", Some(7)).await;
    legacy_work(&legacy, "w_cover", b"legacy-work", 7).await;
    legacy_variants(&legacy, "w_cover").await;
    let (read, derivative) = covers
        .read_master(&main, "w_cover", Some(192))
        .await
        .unwrap()
        .unwrap();
    assert!(read.legacy);
    assert_eq!(
        (read.bytes, read.version),
        (b"legacy-work".to_vec(), Some(7))
    );
    assert_eq!(derivative, Some(b"w_cover-192".to_vec()));
    covers.set_copy_cursor(CopyCursor {
        phase: CopyPhase::Work,
        after: "w_cover".into(),
    });
    assert!(covers
        .read_master(&main, "w_cover", None)
        .await
        .unwrap()
        .is_none());

    let engine = crate::engine_generation::tests::MockEngine::spawn().await;
    let mut client = crate::suwayomi::SuwayomiClient::new(engine.url.clone(), None, None);
    let fence = crate::engine_generation::Fence::attach(&main, &covers, &mut client)
        .await
        .unwrap();
    assert!(crate::engine_generation::commit_ready(&main).is_ok());
    legacy_source(&legacy, 7, b"legacy-numeric").await;
    let numeric = master(&main, &covers, "s:7").await;
    assert!(numeric.legacy);
    assert_eq!(numeric.bytes, b"legacy-numeric");
    covers.set_copy_cursor(CopyCursor {
        phase: CopyPhase::Source,
        after: "7".into(),
    });
    assert!(covers
        .read_master(&main, "s:7", None)
        .await
        .unwrap()
        .is_none());

    covers.set_copy_cursor(CopyCursor::default());
    engine.data.lock().await.marker = Some(uuid::Uuid::new_v4().to_string());
    fence.check().await;
    assert!(crate::engine_generation::commit_ready(&main).is_err());
    // Refill after quarantine's purge so only the read guard can prevent fallback.
    sqlx::query("INSERT OR REPLACE INTO suwayomi_cover_blob VALUES(7,$1,'2026-01-01')")
        .bind(b"detached-numeric".as_slice())
        .execute(&legacy)
        .await
        .unwrap();
    assert!(covers
        .read_master(&main, "s:7", None)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        master(&main, &covers, "w_cover").await.bytes,
        b"legacy-work"
    );
}

#[tokio::test]
async fn takedown_removes_index_upload_and_legacy_rows_during_copy() {
    let (main, _covers, dir) = pools().await;
    let legacy = legacy_pool().await;
    let covers = store_with_legacy(&dir, legacy.clone(), CopyCursor::default());
    work(&main, "w_cover", None).await;
    work(&main, "w_kept", None).await;
    assert!(
        cover::put_uploaded_work_cover(&main, &covers, "w_cover", b"upload")
            .await
            .unwrap()
    );
    cover::put_work_cover(&main, &covers, "w_kept", b"kept")
        .await
        .unwrap();
    cover::put_suwayomi_cover(&main, &covers, 7, b"numeric")
        .await
        .unwrap();
    cover::put_suwayomi_cover(&main, &covers, 8, b"kept-numeric")
        .await
        .unwrap();
    for asset in ["w_cover", "w_kept", "s:7", "s:8"] {
        let read = master(&main, &covers, asset).await;
        for width in cover::VARIANT_WIDTHS {
            cover::put_current_variant(&main, &covers, asset, width, &read, b"small")
                .await
                .unwrap();
        }
        legacy_variants(&legacy, asset).await;
        // Old version-keyed derivatives must be deleted as well.
        legacy_variants(&legacy, &format!("{asset}@v1")).await;
    }
    legacy_work(&legacy, "w_cover", b"legacy-upload", 7).await;
    legacy_work(&legacy, "w_kept", b"legacy-kept", 7).await;
    legacy_source(&legacy, 7, b"legacy-numeric").await;
    legacy_source(&legacy, 8, b"legacy-kept-numeric").await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM cover_object")
        .fetch_one(&main)
        .await
        .unwrap();
    assert_eq!(
        count, 12,
        "precondition: masters and both derivative rungs exist"
    );
    cover::delete_for_takedown(&main, &covers, &["w_cover".into()], &[7])
        .await
        .unwrap();
    assert_eq!(pointer(&main, "w_cover").await, None);
    let rows: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT asset_key FROM cover_object ORDER BY asset_key")
            .fetch_all(&main)
            .await
            .unwrap();
    assert_eq!(rows, ["s:8", "w:w_kept"]);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM cover_object")
        .fetch_one(&main)
        .await
        .unwrap();
    assert_eq!(count, 6, "unrelated masters and derivatives survive");
    let uploads: i64 = sqlx::query_scalar("SELECT count(*) FROM cover_upload")
        .fetch_one(&main)
        .await
        .unwrap();
    assert_eq!(uploads, 0);
    let works: Vec<String> = sqlx::query_scalar("SELECT work_id FROM work_cover_blob")
        .fetch_all(&legacy)
        .await
        .unwrap();
    let sources: Vec<i64> = sqlx::query_scalar("SELECT manga_id FROM suwayomi_cover_blob")
        .fetch_all(&legacy)
        .await
        .unwrap();
    let variants: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT asset_key FROM cover_variant ORDER BY asset_key")
            .fetch_all(&legacy)
            .await
            .unwrap();
    assert_eq!(works, ["w_kept"]);
    assert_eq!(sources, [8]);
    assert_eq!(variants, ["s:8", "s:8@v1", "w_kept", "w_kept@v1"]);
    for asset in ["w_cover", "s:7"] {
        assert!(
            covers
                .read_master(&main, asset, Some(192))
                .await
                .unwrap()
                .is_none(),
            "deletion must reach SQLite so reads cannot resurrect {asset}"
        );
    }
    finish_copy(&main, &covers).await;
    assert!(cover_store::row(&main, "w_cover", Kind::Master)
        .await
        .unwrap()
        .is_none());
    assert!(cover_store::row(&main, "s:7", Kind::Master)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn reused_file_publication_waits_for_cover_file_advisory_lock() {
    let (main, covers, _dir) = pools().await;
    work(&main, "w_cover", None).await;
    cover::put_work_cover(&main, &covers, "w_cover", b"same-file")
        .await
        .unwrap();
    let old = master(&main, &covers, "w_cover").await;
    assert!(covers.object_path(&old.sha).exists());
    let mut held = main.begin().await.unwrap();
    crate::locks::advisory(&mut held, &[cover_store::file_key(&old.sha)])
        .await
        .unwrap();
    let writer = main.clone();
    let cache = covers.clone();
    let publishing = tokio::spawn(async move {
        cover::put_work_cover(&writer, &cache, "w_cover", b"same-file").await
    });
    cover::wait_for_writer_lock(&main).await;
    assert!(
        !publishing.is_finished(),
        "the file key must serialize even a reused object"
    );
    assert_eq!(pointer(&main, "w_cover").await, old.version);
    held.commit().await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), publishing)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let new = master(&main, &covers, "w_cover").await;
    assert_eq!(new.bytes, old.bytes);
    assert_eq!(new.sha, old.sha);
    assert!(new.version > old.version);
    assert_eq!(pointer(&main, "w_cover").await, new.version);
}
