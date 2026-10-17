use super::*;
use crate::dedup::{Candidate, Decision};

async fn pool() -> Database {
    Database::connect("sqlite::memory:").await.unwrap()
}

async fn work(pool: &Database, id: &str, title: &str) {
    sqlx::query(
        "INSERT INTO work (id, primary_title, author, artist, created_at, updated_at) \
         VALUES ($1, $2, 'Masashi Kishimoto', 'Artist', now(), now())",
    )
    .bind(id)
    .bind(title)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO work_alias (id, work_id, normalized_title, raw_title) VALUES ($1, $2, $3, $4)",
    )
    .bind(format!("alias_{id}"))
    .bind(id)
    .bind(normalize_title(title))
    .bind(title)
    .execute(pool)
    .await
    .unwrap();
}

async fn mapping(pool: &Database, work: &str, id: &str, source: Option<&str>, key: &str) {
    sqlx::query(
        "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
         VALUES ($1, $2, 'suwayomi', $3, $4, now())",
    )
    .bind(id)
    .bind(work)
    .bind(source)
    .bind(key)
    .execute(pool)
    .await
    .unwrap();
}

async fn credits(pool: &Database, mapping: &str, attribution: &str) {
    for (role, value) in [("author", "Masashi Kishimoto"), ("artist", "Artist")] {
        sqlx::query(
            "INSERT INTO work_metadata_contribution \
             (source_series_id, field_group, slot, value, attribution, observed_at) \
             VALUES ($1, 'credits', $2, $3, $4, now())",
        )
        .bind(mapping)
        .bind(role)
        .bind(value)
        .bind(attribution)
        .execute(pool)
        .await
        .unwrap();
    }
}

async fn bar(pool: &Database, source: &str, group: &str) {
    sqlx::query(
        "INSERT INTO source_metadata_bar (source_id, field_group, created_at) VALUES ($1, $2, now())",
    )
    .bind(source)
    .bind(group)
    .execute(pool)
    .await
    .unwrap();
}

async fn cached_genres(pool: &Database, id: i64, source: &str, genres: &[&str]) {
    sqlx::query(
        "INSERT INTO suwayomi_series (id, title, genre, status, source_id, updated_at) \
         VALUES ($1, 'T', $2, 'ONGOING', $3, now())",
    )
    .bind(id)
    .bind(sqlx::types::Json(genres))
    .bind(source)
    .execute(pool)
    .await
    .unwrap();
}

async fn assert_match_credits(pool: &Database, work: &str, expected: (Option<&str>, Option<&str>)) {
    let single = load_match_data(pool, work).await.unwrap().unwrap();
    assert_eq!(
        (single.author.as_deref(), single.artist.as_deref()),
        expected
    );
    let batch = load_match_data_batch(pool, &[work.into()]).await.unwrap();
    let batch = &batch[work];
    assert_eq!((batch.author.as_deref(), batch.artist.as_deref()), expected);
}

#[tokio::test]
async fn match_loaders_filter_only_credits_supported_exclusively_by_barred_sources() {
    let pool = pool().await;
    work(&pool, "w", "Ao").await;
    assert_match_credits(&pool, "w", (Some("Masashi Kishimoto"), Some("Artist"))).await;
    mapping(&pool, "w", "ss_barred", Some("barred"), "1").await;
    credits(&pool, "ss_barred", "observed").await;
    assert_match_credits(&pool, "w", (Some("Masashi Kishimoto"), Some("Artist"))).await;

    // A bar for a source with no mapping on this work does not affect it.
    bar(&pool, "unrelated", "credits").await;
    assert_match_credits(&pool, "w", (Some("Masashi Kishimoto"), Some("Artist"))).await;
    bar(&pool, "barred", "tags").await;
    assert_match_credits(&pool, "w", (Some("Masashi Kishimoto"), Some("Artist"))).await;
    bar(&pool, "barred", "credits").await;
    assert_match_credits(&pool, "w", (None, None)).await;

    mapping(&pool, "w", "ss_allowed", Some("allowed"), "2").await;
    credits(&pool, "ss_allowed", "inferred").await;
    assert_match_credits(&pool, "w", (Some("Masashi Kishimoto"), Some("Artist"))).await;
    sqlx::query(
        "UPDATE work_metadata_contribution SET attribution = 'unknown' WHERE source_series_id = 'ss_allowed'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_match_credits(&pool, "w", (Some("Masashi Kishimoto"), Some("Artist"))).await;

    sqlx::query(
        "UPDATE work_metadata_contribution SET value = 'Someone Else' WHERE source_series_id = 'ss_allowed'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_match_credits(&pool, "w", (None, None)).await;
    let stored: (Option<String>, Option<String>) =
        sqlx::query_as("SELECT author, artist FROM work WHERE id = 'w'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        stored,
        (Some("Masashi Kishimoto".into()), Some("Artist".into()))
    );
}

#[tokio::test]
async fn all_barred_matcher_hides_retained_credits_after_the_source_changes() {
    let pool = pool().await;
    work(&pool, "w", "Ao").await;
    mapping(&pool, "w", "ss", Some("barred"), "1").await;
    credits(&pool, "ss", "observed").await;
    work_metadata::set_bar(&pool, "barred", work_metadata::FieldGroup::Credits, true)
        .await
        .unwrap();
    let mut observation = work_metadata::Observation::from_mangadex(&WorkInput {
        author: Some("New Author".into()),
        artist: Some("New Artist".into()),
        ..Default::default()
    });
    observation
        .groups
        .retain(|group, _| *group == work_metadata::FieldGroup::Credits);
    let mut tx = pool.begin().await.unwrap();
    crate::locks::lock_works(&mut tx, &["w"]).await.unwrap();
    work_metadata::record_on(
        &mut tx,
        "ss",
        &observation,
        work_metadata::Attribution::Observed,
    )
    .await
    .unwrap();
    work_metadata::project_on(&mut tx, "w", &[work_metadata::FieldGroup::Credits])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let retained: (String, String) =
        sqlx::query_as("SELECT author, artist FROM work WHERE id = 'w'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(retained, ("Masashi Kishimoto".into(), "Artist".into()));
    let old_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM work_metadata_contribution WHERE value IN ('Masashi Kishimoto', 'Artist')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(old_rows, 0);
    assert_match_credits(&pool, "w", (None, None)).await;
    sqlx::query(
        "INSERT INTO work_field_pin (work_id, field, value, source_value, pinned_at) \
         VALUES ('w', 'author', 'Masashi Kishimoto', 'New Author', now()), \
                ('w', 'artist', 'Artist', 'New Artist', now())",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_match_credits(&pool, "w", (Some("Masashi Kishimoto"), Some("Artist"))).await;
}

#[tokio::test]
async fn unobserved_allowed_mapping_does_not_validate_retained_barred_credits() {
    use work_metadata::{Attribution, FieldGroup, Observation};

    let pool = pool().await;
    work(&pool, "w", "Ao").await;
    mapping(&pool, "w", "ss_x", Some("barred"), "1").await;
    credits(&pool, "ss_x", "observed").await;
    mapping(&pool, "w", "ss_y", Some("allowed"), "2").await;
    work_metadata::set_bar(&pool, "barred", FieldGroup::Credits, true)
        .await
        .unwrap();
    let mut observation = Observation::from_mangadex(&WorkInput {
        author: Some("New Author".into()),
        artist: Some("New Artist".into()),
        ..Default::default()
    });
    observation
        .groups
        .retain(|group, _| *group == FieldGroup::Credits);
    let mut tx = pool.begin().await.unwrap();
    crate::locks::lock_works(&mut tx, &["w"]).await.unwrap();
    work_metadata::record_on(&mut tx, "ss_x", &observation, Attribution::Observed)
        .await
        .unwrap();
    work_metadata::project_on(&mut tx, "w", &[FieldGroup::Credits])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let stored: (String, String) = sqlx::query_as("SELECT author, artist FROM work WHERE id = 'w'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, ("Masashi Kishimoto".into(), "Artist".into()));
    assert_match_credits(&pool, "w", (None, None)).await;
    let candidate = Candidate {
        title: "AO!".into(),
        author: Some("Kishimot, Masashi".into()),
        ..Default::default()
    };
    let mut conn = pool.acquire().await.unwrap();
    assert!(matches!(
        crate::dedup::resolve_on(&mut conn, &candidate, None).await.unwrap(),
        Decision::Review { work_id, .. } if work_id == "w"
    ));
}

#[tokio::test]
async fn pinned_author_counts_even_when_its_only_contributor_is_barred() {
    let pool = pool().await;
    work(&pool, "w", "Ao").await;
    mapping(&pool, "w", "ss", Some("barred"), "1").await;
    credits(&pool, "ss", "observed").await;
    bar(&pool, "barred", "credits").await;
    sqlx::query(
        "INSERT INTO work_field_pin (work_id, field, value, source_value, pinned_at) \
         VALUES ('w', 'author', 'Masashi Kishimoto', 'Masashi Kishimoto', now())",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_match_credits(&pool, "w", (Some("Masashi Kishimoto"), None)).await;
}

#[tokio::test]
async fn dedup_does_not_auto_merge_exact_titles_on_a_barred_author() {
    let pool = pool().await;
    work(&pool, "w", "Ao").await;
    mapping(&pool, "w", "ss", Some("barred"), "1").await;
    credits(&pool, "ss", "observed").await;
    let candidate = Candidate {
        title: "AO!".into(),
        author: Some("Kishimot, Masashi".into()),
        ..Default::default()
    };
    let mut conn = pool.acquire().await.unwrap();
    assert!(matches!(
        crate::dedup::resolve_on(&mut conn, &candidate, None).await.unwrap(),
        Decision::AutoMerge { work_id, method, .. } if work_id == "w" && method == "title_exact"
    ));
    drop(conn);
    bar(&pool, "barred", "credits").await;
    let mut conn = pool.acquire().await.unwrap();
    assert!(matches!(
        crate::dedup::resolve_on(&mut conn, &candidate, None).await.unwrap(),
        Decision::Review { work_id, .. } if work_id == "w"
    ));
}

#[tokio::test]
async fn pair_assessment_sees_missing_barred_credits() {
    let pool = pool().await;
    for id in ["left", "right"] {
        work(&pool, id, "Ao").await;
    }
    mapping(&pool, "left", "ss", Some("barred"), "1").await;
    credits(&pool, "ss", "observed").await;
    let right = load_match_data(&pool, "right").await.unwrap().unwrap();
    let left = load_match_data(&pool, "left").await.unwrap().unwrap();
    assert_eq!(pair::assess(&left, &right).verdict, pair::Verdict::Merge);
    bar(&pool, "barred", "credits").await;
    let left = load_match_data(&pool, "left").await.unwrap().unwrap();
    assert_eq!(
        (left.author.as_deref(), left.artist.as_deref()),
        (None, None)
    );
    let assessment = pair::assess(&left, &right);
    assert_eq!(assessment.verdict, pair::Verdict::Review);
    assert_eq!(assessment.reason, "short_title");
}

#[tokio::test]
async fn merge_recheck_accepts_the_filtered_matcher_snapshot() {
    let pool = pool().await;
    for id in ["left", "right"] {
        work(&pool, id, "Trust Crossing").await;
    }
    sqlx::query("UPDATE work SET year = 2020 WHERE id IN ('left', 'right')")
        .execute(&pool)
        .await
        .unwrap();
    mapping(&pool, "left", "ss", Some("barred"), "1").await;
    credits(&pool, "ss", "observed").await;
    bar(&pool, "barred", "credits").await;
    let left = load_match_data(&pool, "left").await.unwrap().unwrap();
    let right = load_match_data(&pool, "right").await.unwrap().unwrap();
    assert_eq!(pair::assess(&left, &right).verdict, pair::Verdict::Merge);
    let expected = (left.identity(None), right.identity(None));
    let mut tx = pool.begin().await.unwrap();
    merge_works_guarded_on(
        &mut tx,
        "left",
        "right",
        Some((&expected.0, &expected.1)),
        None,
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn genre_readers_skip_barred_mappings_and_legacy_source_ids() {
    let pool = pool().await;
    work(&pool, "w", "Trust Crossing").await;
    mapping(&pool, "w", "ss_1", Some("barred"), "1").await;
    mapping(&pool, "w", "ss_2", Some("allowed"), "2").await;
    mapping(&pool, "w", "ss_3", None, "3").await;
    cached_genres(&pool, 1, "barred", &["Manhwa"]).await;
    cached_genres(&pool, 2, "allowed", &["Drama"]).await;
    cached_genres(&pool, 3, "legacy", &["Horror"]).await;
    assert_eq!(work_effective_genres(&pool, "w").await.len(), 3);
    let batch = work_effective_genres_batch_checked(&pool, &["w".into()])
        .await
        .unwrap();
    assert_eq!(batch["w"].len(), 3);
    bar(&pool, "barred", "tags").await;
    bar(&pool, "suwayomi", "tags").await;
    assert_eq!(work_effective_genres(&pool, "w").await, vec!["Drama"]);
    let batch = work_effective_genres_batch_checked(&pool, &["w".into()])
        .await
        .unwrap();
    assert_eq!(batch["w"], vec!["Drama"]);
    bar(&pool, "allowed", "tags").await;
    assert!(work_effective_genres(&pool, "w").await.is_empty());
    assert!(work_effective_genres_batch_checked(&pool, &["w".into()])
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn tag_planner_removes_barred_cached_genres_and_keeps_allowed_ones() {
    let pool = pool().await;
    work(&pool, "w", "Trust Crossing").await;
    mapping(&pool, "w", "ss_1", Some("barred"), "1").await;
    mapping(&pool, "w", "ss_2", Some("allowed"), "2").await;
    cached_genres(&pool, 1, "barred", &["Action"]).await;
    cached_genres(&pool, 2, "allowed", &["Drama"]).await;
    assert_eq!(
        crate::tags::sync_suwayomi_tags(&pool)
            .await
            .unwrap()
            .inserted,
        2
    );
    bar(&pool, "barred", "tags").await;
    let sync = crate::tags::sync_suwayomi_tags(&pool).await.unwrap();
    assert_eq!((sync.inserted, sync.deleted), (0, 1));
    let tags: Vec<String> = sqlx::query_scalar("SELECT tag FROM work_tag WHERE work_id = 'w'")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(tags, vec!["Drama"]);
    bar(&pool, "allowed", "tags").await;
    assert_eq!(
        crate::tags::sync_suwayomi_tags(&pool)
            .await
            .unwrap()
            .deleted,
        1
    );
}

#[tokio::test]
async fn fresh_genres_from_a_barred_source_are_dropped_under_the_work_lock() {
    let pool = pool().await;
    work(&pool, "w", "Trust Crossing").await;
    mapping(&pool, "w", "ss", Some("barred"), "1").await;
    bar(&pool, "barred", "tags").await;
    let fresh = ["Gore".into(), "Seinen".into()];
    let sync = crate::tags::sync_work_suwayomi_tags(&pool, "w", Some("barred"), &fresh)
        .await
        .unwrap();
    assert_eq!(sync, crate::tags::SuwayomiTagSync::default());
    let demographic: Option<String> =
        sqlx::query_scalar("SELECT demographic FROM work WHERE id = 'w'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(demographic, None);
    assert!(work_effective_genres(&pool, "w").await.is_empty());
    mapping(&pool, "w", "ss_allowed", Some("allowed"), "2").await;
    let sync = crate::tags::sync_work_suwayomi_tags(&pool, "w", Some("allowed"), &fresh)
        .await
        .unwrap();
    assert_eq!((sync.inserted, sync.demographics), (1, 1));

    work(&pool, "legacy", "Legacy Work").await;
    mapping(&pool, "legacy", "ss_legacy", None, "3").await;
    bar(&pool, "suwayomi", "tags").await;
    let sync = crate::tags::sync_work_suwayomi_tags(&pool, "legacy", None, &fresh)
        .await
        .unwrap();
    assert_eq!(sync, crate::tags::SuwayomiTagSync::default());
}

#[tokio::test]
async fn comic_type_fallback_ignores_barred_source_genres() {
    let pool = pool().await;
    work(&pool, "w", "Trust Crossing").await;
    mapping(&pool, "w", "ss_1", Some("barred"), "1").await;
    cached_genres(&pool, 1, "barred", &["Manhwa"]).await;
    sqlx::query(
        "INSERT INTO browse_catalogue (work_id, reader_id, title, created_at) \
         VALUES ('w', '1', 'Trust Crossing', now())",
    )
    .execute(&pool)
    .await
    .unwrap();
    let mut tx = pool.begin().await.unwrap();
    crate::locks::lock_works(&mut tx, &["w"]).await.unwrap();
    fill_comic_types(&mut tx, "browse_catalogue", Some("w"))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let comic_type: String =
        sqlx::query_scalar("SELECT comic_type FROM browse_catalogue WHERE work_id = 'w'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(comic_type, "MANHWA");

    bar(&pool, "barred", "tags").await;
    for scope in [Some("w"), None] {
        let mut tx = pool.begin().await.unwrap();
        crate::locks::lock_works(&mut tx, &["w"]).await.unwrap();
        fill_comic_types(&mut tx, "browse_catalogue", scope)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let comic_type: String =
            sqlx::query_scalar("SELECT comic_type FROM browse_catalogue WHERE work_id = 'w'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(comic_type, "MANGA");
    }
}
