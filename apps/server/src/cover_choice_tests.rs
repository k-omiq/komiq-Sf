//! Cover choice on PostgreSQL: the ranking order, the ranking pass, admin picks and
//! the unpicked-cover prune.

use std::sync::atomic::Ordering;
use std::time::Duration;

use super::*;
use crate::cover_store::test_support::{store, TestDir};
use crate::cover_store::Kind;
use crate::database::TestPoolOptions;
use crate::suwayomi::{testsrv, SuwayomiClient};

async fn pools() -> (Database, CoverStore, TestDir) {
    let main = TestPoolOptions::new()
        .max_connections(4)
        .connect("sqlite::memory:")
        .await
        .unwrap()
        .foreground();
    sqlx::migrate!("./migrations_pg")
        .run(main.write())
        .await
        .unwrap();
    let dir = TestDir::new();
    let covers = store(&dir);
    (main, covers, dir)
}

async fn work(main: &Database, id: &str) {
    sqlx::query(
        "INSERT INTO work (id, primary_title, cover_file_name, created_at, updated_at) \
         VALUES ($1, 'Title', 'main.jpg', now(), now())",
    )
    .bind(id)
    .execute(main)
    .await
    .unwrap();
}

async fn mapping(main: &Database, work: &str, id: &str, source: &str, key: &str, created: &str) {
    let kind = if source == "mangadex" {
        "mangadex"
    } else {
        "suwayomi"
    };
    let source_id = (kind == "suwayomi").then_some(source);
    sqlx::query(
        "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6::text::timestamptz)",
    )
    .bind(id)
    .bind(work)
    .bind(kind)
    .bind(source_id)
    .bind(key)
    .bind(created)
    .execute(main)
    .await
    .unwrap();
}

/// A distinct processed cover per seed.
fn webp(seed: u8) -> Vec<u8> {
    let image = image::RgbImage::from_fn(24, 36, |x, y| {
        image::Rgb([seed, (x * 7) as u8 ^ seed, (y * 5) as u8])
    });
    let mut jpeg = Vec::new();
    image::DynamicImage::ImageRgb8(image)
        .write_to(
            &mut std::io::Cursor::new(&mut jpeg),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    cover::process_cover(&jpeg).unwrap()
}

async fn master_sha(main: &Database, asset: &str) -> Option<Vec<u8>> {
    sqlx::query_scalar("SELECT sha256 FROM cover_object WHERE asset_key = $1 AND kind = 'master'")
        .bind(cover_store::index_key(asset))
        .fetch_optional(main)
        .await
        .unwrap()
}

async fn provenance(main: &Database, work: &str) -> (Option<i64>, Option<String>) {
    sqlx::query_as("SELECT cover_cached_version, cover_source FROM work WHERE id = $1")
        .bind(work)
        .fetch_one(main)
        .await
        .unwrap()
}

async fn admin_row(main: &Database, work: &str) -> Option<Option<String>> {
    sqlx::query_scalar("SELECT choice FROM cover_upload WHERE work_id = $1")
        .bind(work)
        .fetch_optional(main)
        .await
        .unwrap()
}

struct Engine {
    origin: testsrv::TestOrigin,
    mangadex: MangaDexClient,
    suwayomi: SuwayomiClient,
    budgets: SourceBudgets,
    shutdown: tokio::sync::watch::Receiver<bool>,
}

async fn engine() -> Engine {
    let origin = testsrv::spawn(testsrv::jpeg(40, 60), Duration::ZERO).await;
    let mangadex = MangaDexClient::new("cover-choice", 100.0, 100.0)
        .with_bases(&origin.base_url, &origin.base_url);
    mangadex.set_enabled(true);
    let suwayomi = SuwayomiClient::new(origin.base_url.clone(), None, None);
    Engine {
        origin,
        mangadex,
        suwayomi,
        budgets: SourceBudgets::default(),
        shutdown: tokio::sync::watch::channel(false).1,
    }
}

fn clients<'a>(main: &'a Database, covers: &'a CoverStore, e: &'a Engine) -> Clients<'a> {
    Clients {
        main,
        covers,
        mangadex: &e.mangadex,
        suwayomi: &e.suwayomi,
        budgets: &e.budgets,
        shutdown: &e.shutdown,
        waiting: false,
    }
}

fn ids(sources: &[(i32, RepairCoverSource)]) -> Vec<String> {
    sources
        .iter()
        .map(|(_, source)| match source {
            RepairCoverSource::MangaDex { .. } => "mangadex".to_string(),
            RepairCoverSource::Suwayomi { source_id, .. } => source_id.clone(),
        })
        .collect()
}

#[tokio::test]
async fn the_ranking_orders_cover_sources_and_no_ranking_keeps_the_default() {
    let (main, _covers, _dir) = pools().await;
    work(&main, "w_rank").await;
    mapping(&main, "w_rank", "ss_md", "mangadex", "md-1", "2026-01-03").await;
    mapping(&main, "w_rank", "ss_a", "src-a", "7", "2026-01-01").await;
    mapping(&main, "w_rank", "ss_b", "src-b", "8", "2026-01-02").await;
    let default = cover::ranked_cover_sources(&main, "w_rank").await.unwrap();
    assert_eq!(ids(&default), ["mangadex", "src-a", "src-b"]);
    assert!(default.iter().all(|(position, _)| *position == i32::MAX));

    save_rank(&main, &["src-b".into(), "mangadex".into()])
        .await
        .unwrap();
    assert_eq!(load_rank(&main).await.unwrap(), ["src-b", "mangadex"]);
    let ranked = cover::ranked_cover_sources(&main, "w_rank").await.unwrap();
    assert_eq!(ids(&ranked), ["src-b", "mangadex", "src-a"]);
    // The gallery follows the same order: MangaDex covers sit at MangaDex's place.
    sqlx::query(
        "INSERT INTO work_cover (work_id, cover_file_name, volume, is_primary) \
         VALUES ('w_rank', 'main.jpg', '1', true), ('w_rank', 'v2.jpg', '2', false)",
    )
    .execute(&main)
    .await
    .unwrap();
    let keys: Vec<String> = choices(&main, "w_rank")
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.key)
        .collect();
    assert_eq!(keys, ["src:ss_b", "md:main.jpg", "md:v2.jpg", "src:ss_a"]);

    assert!(save_rank(&main, &["src-a".into(), "src-a".into()])
        .await
        .is_err());
    assert!(save_rank(&main, &["not valid!".into()]).await.is_err());
    assert_eq!(load_rank(&main).await.unwrap(), ["src-b", "mangadex"]);

    save_rank(&main, &[]).await.unwrap();
    assert!(load_rank(&main).await.unwrap().is_empty());
    assert!(load_cursor(&main, RANK_PASS).await.unwrap().is_none());
    assert_eq!(
        ids(&cover::ranked_cover_sources(&main, "w_rank").await.unwrap()),
        ["mangadex", "src-a", "src-b"]
    );
}

#[tokio::test]
async fn the_ranking_pass_moves_covers_to_the_ranked_source_but_never_an_admin_cover() {
    let (main, covers, _dir) = pools().await;
    let e = engine().await;
    for (w, key) in [("w_auto", "7"), ("w_admin", "8")] {
        work(&main, w).await;
        mapping(&main, w, &format!("ss_md_{w}"), "mangadex", w, "2026-01-01").await;
        mapping(&main, w, &format!("ss_a_{w}"), "src-a", key, "2026-01-02").await;
        cover::put_source_work_cover(&main, &covers, w, &webp(1), "mangadex")
            .await
            .unwrap();
        cover::put_suwayomi_cover(&main, &covers, key.parse().unwrap(), &webp(2))
            .await
            .unwrap();
    }
    cover::put_uploaded_work_cover(&main, &covers, "w_admin", &webp(3))
        .await
        .unwrap();
    let before_admin = master_sha(&main, "w_admin").await;
    let (before, _) = provenance(&main, "w_auto").await;

    // No ranking: the default order already gave MangaDex, nothing moves.
    let c = clients(&main, &covers, &e);
    assert_eq!(rank_pass(&c, None).await.unwrap(), 0);

    save_rank(&main, &["src-a".into()]).await.unwrap();
    assert_eq!(rank_pass(&c, None).await.unwrap(), 1);
    let (after, source) = provenance(&main, "w_auto").await;
    assert_eq!(source.as_deref(), Some("src-a"));
    assert!(after > before);
    assert_eq!(
        master_sha(&main, "w_auto").await,
        master_sha(&main, "s:7").await
    );
    assert_eq!(master_sha(&main, "w_admin").await, before_admin);
    assert_eq!(admin_row(&main, "w_admin").await, Some(None));
    // The stored source cover was used: no download.
    assert_eq!(e.origin.hits.load(Ordering::SeqCst), 0);

    // Done for the day: a second pass does nothing, and a stale version is refused.
    assert_eq!(rank_pass(&c, None).await.unwrap(), 0);
    let origin = CoverOrigin::Ranked {
        source: "mangadex",
        version: before,
    };
    assert!(
        !cover::put_chosen_work_cover(&main, &covers, "w_auto", &webp(4), origin)
            .await
            .unwrap()
    );
    assert_eq!(
        provenance(&main, "w_auto").await.1.as_deref(),
        Some("src-a")
    );
}

#[tokio::test]
async fn a_pick_is_an_admin_cover_until_automatic_hands_it_back_to_the_ranking() {
    let (main, covers, _dir) = pools().await;
    let e = engine().await;
    work(&main, "w_pick").await;
    mapping(&main, "w_pick", "ss_md", "mangadex", "md-1", "2026-01-01").await;
    mapping(&main, "w_pick", "ss_a", "src-a", "7", "2026-01-02").await;
    sqlx::query(
        "INSERT INTO work_cover (work_id, cover_file_name, volume, is_primary) \
         VALUES ('w_pick', 'main.jpg', '1', true), ('w_pick', 'v2.jpg', '2', false)",
    )
    .execute(&main)
    .await
    .unwrap();
    cover::put_suwayomi_cover(&main, &covers, 7, &webp(2))
        .await
        .unwrap();
    let c = clients(&main, &covers, &e);

    assert_eq!(
        pick(&c, "w_pick", "md:missing.jpg").await.unwrap(),
        Err(PickError::Unknown)
    );
    pick(&c, "w_pick", "md:v2.jpg").await.unwrap().unwrap();
    assert_eq!(e.origin.hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        admin_row(&main, "w_pick").await,
        Some(Some("md:v2.jpg".into()))
    );
    assert_eq!(
        admin_cover(&main, "w_pick").await.unwrap(),
        (Some("md:v2.jpg".into()), false, Some("mangadex".into()))
    );

    // The ranking never replaces a pick.
    save_rank(&main, &["src-a".into()]).await.unwrap();
    assert_eq!(rank_pass(&c, None).await.unwrap(), 0);
    assert_eq!(
        provenance(&main, "w_pick").await.1.as_deref(),
        Some("mangadex")
    );

    // A pick from an engine mapping uses its stored cover.
    pick(&c, "w_pick", "src:ss_a").await.unwrap().unwrap();
    assert_eq!(
        admin_row(&main, "w_pick").await,
        Some(Some("src:ss_a".into()))
    );
    assert_eq!(
        master_sha(&main, "w_pick").await,
        master_sha(&main, "s:7").await
    );
    assert_eq!(e.origin.hits.load(Ordering::SeqCst), 1);

    // Automatic: the admin row goes and the top-ranked source supplies the cover.
    save_rank(&main, &["mangadex".into()]).await.unwrap();
    assert!(clear_pick(&c, "w_pick").await.unwrap());
    assert_eq!(admin_row(&main, "w_pick").await, None);
    assert_eq!(
        provenance(&main, "w_pick").await.1.as_deref(),
        Some("mangadex")
    );
    assert_eq!(
        admin_cover(&main, "w_pick").await.unwrap(),
        (None, false, Some("mangadex".into()))
    );
}

#[tokio::test]
async fn a_cover_bar_removes_a_pick_from_that_source_with_its_admin_row() {
    let (main, covers, _dir) = pools().await;
    let e = engine().await;
    work(&main, "w_bar").await;
    mapping(&main, "w_bar", "ss_a", "src-a", "7", "2026-01-02").await;
    cover::put_suwayomi_cover(&main, &covers, 7, &webp(2))
        .await
        .unwrap();
    let c = clients(&main, &covers, &e);
    pick(&c, "w_bar", "src:ss_a").await.unwrap().unwrap();
    crate::catalog::work_metadata::set_bar(
        &main,
        "src-a",
        crate::catalog::work_metadata::FieldGroup::Cover,
        true,
    )
    .await
    .unwrap();
    assert_eq!(admin_row(&main, "w_bar").await, None);
    assert_eq!(provenance(&main, "w_bar").await, (None, None));
    assert!(master_sha(&main, "w_bar").await.is_none());
    // A barred source offers nothing to pick.
    assert!(choices(&main, "w_bar").await.unwrap().is_empty());
    assert_eq!(
        pick(&c, "w_bar", "src:ss_a").await.unwrap(),
        Err(PickError::Unknown)
    );
}

#[tokio::test]
async fn the_prune_removes_only_unpicked_engine_covers_and_resumes_from_its_cursor() {
    let (main, covers, _dir) = pools().await;
    work(&main, "w_main").await;
    mapping(&main, "w_main", "ss_a", "src-a", "7", "2026-01-01").await;
    mapping(&main, "w_main", "ss_b", "src-b", "8", "2026-01-02").await;
    mapping(&main, "w_main", "ss_m", "src-m", "9", "2026-01-03").await;
    mapping(&main, "w_main", "ss_c", "src-c", "12", "2026-01-04").await;
    sqlx::query(
        "INSERT INTO source_extension (source_id, pkg_name, repo_url, updated_at) \
         VALUES ('src-m', 'eu.kanade.tachiyomi.extension.all.mangadex', 'https://repo', now())",
    )
    .execute(&main)
    .await
    .unwrap();
    // A second work shows the bytes of its own mapping, which src-c on w_main shares.
    work(&main, "w_other").await;
    mapping(&main, "w_other", "ss_o", "src-b", "11", "2026-01-01").await;
    // A third work has no stored cover: its source cover is all it has.
    work(&main, "w_bare").await;
    mapping(&main, "w_bare", "ss_bare", "src-b", "13", "2026-01-01").await;
    // A fourth work shows its source's cover from a separate fetch: other bytes.
    work(&main, "w_refetch").await;
    mapping(&main, "w_refetch", "ss_r", "src-a", "15", "2026-01-01").await;
    for (id, seed) in [
        (7, 1),
        (8, 2),
        (9, 3),
        (10, 4),
        (11, 6),
        (12, 6),
        (13, 7),
        (15, 10),
    ] {
        cover::put_suwayomi_cover(&main, &covers, id, &webp(seed))
            .await
            .unwrap();
    }
    cover::put_source_work_cover(&main, &covers, "w_main", &webp(1), "src-a")
        .await
        .unwrap();
    cover::put_source_work_cover(&main, &covers, "w_other", &webp(6), "src-b")
        .await
        .unwrap();
    cover::put_source_work_cover(&main, &covers, "w_refetch", &webp(11), "src-a")
        .await
        .unwrap();
    // s:12 has a card derivative: it goes with its master.
    let derivative = covers.stage(&webp(9)).await.unwrap();
    {
        let mut staged = derivative;
        let mut tx = main.begin().await.unwrap();
        let _locks = cover::lock_assets_and_files(&mut tx, &["s:12"], &[staged.sha])
            .await
            .unwrap();
        covers.place(&mut staged).await.unwrap();
        let master: [u8; 32] = master_sha(&main, "s:12").await.unwrap().try_into().unwrap();
        assert!(
            cover_store::put_derivative(&mut tx, "s:12", 192, &staged, &master)
                .await
                .unwrap()
        );
        tx.commit().await.unwrap();
    }

    let preview = prune_preview(&main).await.unwrap();
    let mut sources: Vec<(String, i64)> = preview
        .iter()
        .map(|(source, covers, _)| (source.clone(), *covers))
        .collect();
    sources.sort();
    // src-b's s:8 on w_main; not s:7 (w_main shows it), s:9 (MangaDex extension),
    // s:10 (no mapping), s:11 (its work shows it), s:12 (w_other shows its bytes),
    // s:13 (its work has no stored cover), s:15 (its work's cover came from src-a).
    assert_eq!(sources, [("src-b".to_string(), 1)]);
    let bytes: i64 = sqlx::query_scalar(
        "SELECT SUM(byte_length)::bigint FROM cover_object WHERE asset_key = 's:8'",
    )
    .fetch_one(&main)
    .await
    .unwrap();
    assert_eq!(preview[0].2, bytes);

    let shutdown = tokio::sync::watch::channel(false).1;
    assert_eq!(
        prune_pass(&main, &shutdown, None).await.unwrap(),
        0,
        "off by default"
    );
    assert!(master_sha(&main, "s:8").await.is_some());

    set_prune(&main, true).await.unwrap();
    assert_eq!(prune_pass(&main, &shutdown, None).await.unwrap(), 1);
    assert!(master_sha(&main, "s:8").await.is_none());
    for kept in [
        "s:7", "s:9", "s:10", "s:11", "s:12", "s:13", "s:15", "w_main", "w_other",
    ] {
        assert!(master_sha(&main, kept).await.is_some(), "{kept}");
    }
    assert!(cover_store::row(&main, "s:12", Kind::Width(192))
        .await
        .unwrap()
        .is_some());
    assert!(prune_preview(&main).await.unwrap().is_empty());
    let cursor = load_cursor(&main, PRUNE_PASS).await.unwrap().unwrap().1;
    assert!(cursor.finished_at.is_some());

    // Picking the other source makes s:7 unpicked; a pass inside the day waits, a
    // restarted one removes it, and turning the setting off stops the next pass.
    cover::put_source_work_cover(&main, &covers, "w_main", &webp(6), "src-c")
        .await
        .unwrap();
    assert_eq!(prune_pass(&main, &shutdown, None).await.unwrap(), 0);
    set_prune(&main, false).await.unwrap();
    assert_eq!(prune_pass(&main, &shutdown, None).await.unwrap(), 0);
    set_prune(&main, true).await.unwrap();
    assert_eq!(prune_pass(&main, &shutdown, None).await.unwrap(), 1);
    assert!(master_sha(&main, "s:7").await.is_none());
    assert!(master_sha(&main, "s:12").await.is_some());
}

#[tokio::test]
async fn the_warmer_skips_covers_the_prune_removes() {
    let (main, covers, _dir) = pools().await;
    work(&main, "w_warm").await;
    mapping(&main, "w_warm", "ss_a", "src-a", "7", "2026-01-01").await;
    mapping(&main, "w_warm", "ss_b", "src-b", "8", "2026-01-02").await;
    for id in [7, 8] {
        sqlx::query(
            "INSERT INTO suwayomi_series (id, title, status, source_id, in_library, thumbnail_url, updated_at) \
             VALUES ($1, 'Warm', 'ONGOING', 'src', true, $2, now())",
        )
        .bind(id)
        .bind(format!("/api/v1/manga/{id}/thumbnail"))
        .execute(&main)
        .await
        .unwrap();
    }
    cover::put_source_work_cover(&main, &covers, "w_warm", &webp(1), "src-a")
        .await
        .unwrap();
    assert_eq!(
        cover::pending_suwayomi_cover_count(&main, &covers)
            .await
            .unwrap(),
        2
    );
    set_prune(&main, true).await.unwrap();
    assert_eq!(
        cover::pending_suwayomi_cover_count(&main, &covers)
            .await
            .unwrap(),
        1
    );
}

/// Plan nodes a subquery runs more than once: work repeated per outer row.
fn repeated_subplans(node: &serde_json::Value, out: &mut Vec<String>) {
    if node["Parent Relationship"] == "SubPlan" && node["Actual Loops"].as_f64() > Some(1.0) {
        out.push(format!(
            "{} loops={}",
            node["Subplan Name"], node["Actual Loops"]
        ));
    }
    for child in node["Plans"].as_array().into_iter().flatten() {
        repeated_subplans(child, out);
    }
}

#[tokio::test]
async fn the_prune_preview_counts_each_source_in_one_pass() {
    let (main, covers, _dir) = pools().await;
    // One unpicked cover from each of 12 sources: the work shows its first source.
    for i in 0..12u8 {
        let w = format!("w_p{i}");
        work(&main, &w).await;
        mapping(
            &main,
            &w,
            &format!("ss_p{i}a"),
            "src-keep",
            &format!("{}", 100 + i as i64),
            "2026-01-01",
        )
        .await;
        mapping(
            &main,
            &w,
            &format!("ss_p{i}b"),
            &format!("src-{i}"),
            &format!("{}", 200 + i as i64),
            "2026-01-02",
        )
        .await;
        cover::put_suwayomi_cover(&main, &covers, 200 + i as i64, &webp(100 + i))
            .await
            .unwrap();
        cover::put_source_work_cover(&main, &covers, &w, &webp(150 + i), "src-keep")
            .await
            .unwrap();
    }
    let preview = prune_preview(&main).await.unwrap();
    assert_eq!(preview.len(), 12);
    for (source, count, bytes) in &preview {
        assert_eq!(*count, 1, "{source}");
        assert!(*bytes > 0, "{source}");
    }

    let plan: serde_json::Value = sqlx::query_scalar(&format!(
        "EXPLAIN (ANALYZE, FORMAT JSON) {}",
        prune_preview_sql()
    ))
    .fetch_one(&main)
    .await
    .unwrap();
    let mut repeated = Vec::new();
    repeated_subplans(&plan[0]["Plan"], &mut repeated);
    assert!(
        repeated.is_empty(),
        "work repeated per source: {repeated:?}"
    );
}
