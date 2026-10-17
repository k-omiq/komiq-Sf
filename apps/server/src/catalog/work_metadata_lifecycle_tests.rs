use super::*;
use crate::catalog::phase4_tests::{pool, work};
use work_metadata::{Attribution, Observation};

async fn mapping(
    pool: &Database,
    work: &str,
    kind: &str,
    source: &str,
    key: &str,
    created: &str,
) -> Result<String> {
    let id = upsert_source_series(pool, work, kind, source, key, None, false).await?;
    sqlx::query("UPDATE source_series SET created_at = $2::text::timestamptz WHERE id = $1")
        .bind(&id)
        .bind(created)
        .execute(pool)
        .await?;
    Ok(id)
}

async fn observe(pool: &Database, mapping: &str, title: &str) -> Result<()> {
    let work: String = sqlx::query_scalar("SELECT work_id FROM source_series WHERE id = $1")
        .bind(mapping)
        .fetch_one(pool)
        .await?;
    let observation = Observation::from_mangadex(&WorkInput {
        primary_title: Some(title.into()),
        aliases: vec![Alias {
            raw: format!("{title} alias"),
            lang: None,
        }],
        description: Some(format!("{title} description")),
        author: Some(format!("{title} author")),
        artist: Some(format!("{title} artist")),
        status: Some("completed".into()),
        ..Default::default()
    });
    crate::db::retry_transaction(|| async {
        let mut tx = pool.begin().await?;
        crate::locks::lock_works(&mut tx, &[&work]).await?;
        work_metadata::record_on(&mut tx, mapping, &observation, Attribution::Observed).await?;
        tx.commit().await?;
        Ok(())
    })
    .await
}

async fn contributions(pool: &Database) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT to_jsonb(c)::text || ':' || xmin::text FROM work_metadata_contribution c \
         ORDER BY source_series_id, field_group, slot, lang, value",
    )
    .fetch_all(pool)
    .await?)
}

async fn assert_projected(pool: &Database, work: &str, title: &str) -> Result<()> {
    let effective: (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT primary_title, description, author, artist, status FROM work WHERE id = $1",
    )
    .bind(work)
    .fetch_one(pool)
    .await?;
    assert_eq!(
        effective,
        (
            title.into(),
            Some(format!("{title} description")),
            Some(format!("{title} author")),
            Some(format!("{title} artist")),
            Some("completed".into()),
        )
    );
    let card_title: String =
        sqlx::query_scalar("SELECT title FROM browse_catalogue WHERE work_id = $1")
            .bind(work)
            .fetch_one(pool)
            .await?;
    assert_eq!(card_title, title);
    Ok(())
}

#[tokio::test]
async fn merge_preserves_contributions_and_projects_rank_and_barred_aliases() -> Result<()> {
    for barred in [false, true] {
        let pool = pool().await;
        let loser = work(&pool, "Loser alias").await;
        let survivor = work(&pool, "Survivor alias").await;
        let old = mapping(&pool, &loser, "suwayomi", "older", "1", "2020-01-01").await?;
        let new = mapping(&pool, &survivor, "suwayomi", "newer", "2", "2021-01-01").await?;
        observe(&pool, &old, "Loser").await?;
        observe(&pool, &new, "Survivor").await?;
        add_work_alias(&pool, &survivor, "Manual alias").await?;
        if barred {
            sqlx::query("INSERT INTO source_metadata_bar (source_id, field_group, created_at) VALUES ('older', 'title', now())")
                .execute(&pool).await?;
        }
        let before = contributions(&pool).await?;
        merge_works(&pool, &loser, &survivor).await?;
        assert_eq!(
            contributions(&pool).await?,
            before,
            "mapping moves do not rewrite observations"
        );
        let owners: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT ss.work_id FROM work_metadata_contribution c \
             JOIN source_series ss ON ss.id = c.source_series_id",
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(owners, vec![survivor.clone()]);
        if barred {
            let title: String = sqlx::query_scalar("SELECT primary_title FROM work WHERE id = $1")
                .bind(&survivor)
                .fetch_one(&pool)
                .await?;
            assert_eq!(title, "Survivor");
        } else {
            assert_projected(&pool, &survivor, "Loser").await?;
        }
        let aliases: Vec<String> =
            sqlx::query_scalar("SELECT raw_title FROM work_alias WHERE work_id = $1")
                .bind(&survivor)
                .fetch_all(&pool)
                .await?;
        assert_eq!(aliases.iter().any(|alias| alias == "Loser alias"), !barred);
        assert!(aliases.iter().any(|alias| alias == "Survivor alias"));
        assert!(aliases.iter().any(|alias| alias == "Manual alias"));
        let loser_token: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM work_alias_token WHERE work_id = $1 AND token = 'loser')",
        )
        .bind(&survivor)
        .fetch_one(&pool)
        .await?;
        assert_eq!(loser_token, !barred);
    }
    Ok(())
}

#[tokio::test]
async fn admin_split_preserves_mapping_rows_and_projects_both_works() -> Result<()> {
    let pool = pool().await;
    let original = work(&pool, "Stale merged title").await;
    let detached = mapping(&pool, &original, "suwayomi", "detached", "11", "2020-01-01").await?;
    let kept = mapping(&pool, &original, "suwayomi", "kept", "12", "2021-01-01").await?;
    sqlx::raw_sql(
        "INSERT INTO suwayomi_series (id, title, status, source_id, updated_at) \
         VALUES (11, 'Cached detached title', 'ONGOING', 'detached', now()), \
                (12, 'Cached kept title', 'ONGOING', 'kept', now())",
    )
    .execute(&pool)
    .await?;
    observe(&pool, &detached, "Detached").await?;
    observe(&pool, &kept, "Kept").await?;
    let before = contributions(&pool).await?;
    let split = split_source_series(&pool, &original, &[detached.clone()], None).await?;
    assert_eq!(split.title, "Detached");
    assert_eq!(contributions(&pool).await?, before);
    let owners: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT c.source_series_id, ss.work_id FROM work_metadata_contribution c \
         JOIN source_series ss ON ss.id = c.source_series_id ORDER BY c.source_series_id",
    )
    .fetch_all(&pool)
    .await?;
    assert!(owners.contains(&(detached, split.new_work_id.clone())));
    assert!(owners.contains(&(kept, original.clone())));
    assert_eq!(owners.len(), 2);
    assert_projected(&pool, &original, "Kept").await?;
    assert_projected(&pool, &split.new_work_id, "Detached").await?;
    Ok(())
}

// phase_f::tests::fixture is private to its module. Use its same UUID-proof setup
// with the shared PostgreSQL pool helper and only the mappings this regression needs.
async fn phase_f_fixture(pool: &Database, redundant: bool) -> Result<(String, String, String)> {
    use crate::phase_f::{ALL_MANGADEX_PKG, PRIMARY_SOURCE_ID};
    let original = work(pool, "Stale original").await;
    let target = work(pool, "Stale target").await;
    sqlx::query("INSERT INTO source_extension (source_id, pkg_name, repo_url, updated_at) VALUES ($1, $2, 'local', now())")
        .bind(PRIMARY_SOURCE_ID).bind(ALL_MANGADEX_PKG).execute(pool).await?;
    let original_md = mapping(
        pool,
        &original,
        "mangadex",
        "mangadex",
        "11111111-1111-1111-1111-111111111111",
        "2020-01-01",
    )
    .await?;
    let target_md = mapping(
        pool,
        &target,
        "mangadex",
        "mangadex",
        "22222222-2222-2222-2222-222222222222",
        "2020-01-01",
    )
    .await?;
    let moved = mapping(
        pool,
        &original,
        "suwayomi",
        PRIMARY_SOURCE_ID,
        "101",
        "2019-01-01",
    )
    .await?;
    observe(pool, &original_md, "Original").await?;
    observe(pool, &target_md, "Target").await?;
    observe(pool, &moved, "Mirror").await?;
    let uuid = if redundant {
        "11111111-1111-1111-1111-111111111111"
    } else {
        "22222222-2222-2222-2222-222222222222"
    };
    let urls = [("101".into(), format!("/manga/{uuid}"))]
        .into_iter()
        .collect();
    let proof = crate::phase_f::record_uuids(pool, &urls, crate::phase_f::Apply::Yes).await?;
    assert_eq!(proof.resolved, 1);
    Ok((original, target, moved))
}

#[tokio::test]
async fn phase_f_split_preserves_observations_and_projects_both_works() -> Result<()> {
    let pool = pool().await;
    let (original, target, moved) = phase_f_fixture(&pool, false).await?;
    let before = contributions(&pool).await?;
    let report = crate::phase_f::split_mismatched(&pool, crate::phase_f::Apply::Yes).await?;
    assert_eq!(report.acted, 1);
    assert_eq!(contributions(&pool).await?, before);
    let owner: String = sqlx::query_scalar("SELECT work_id FROM source_series WHERE id = $1")
        .bind(&moved)
        .fetch_one(&pool)
        .await?;
    assert_eq!(owner, target);
    assert_projected(&pool, &original, "Original").await?;
    assert_projected(&pool, &target, "Target").await?;
    Ok(())
}

#[tokio::test]
async fn phase_f_delete_projects_the_surviving_work() -> Result<()> {
    let pool = pool().await;
    let (original, _, deleted) = phase_f_fixture(&pool, true).await?;
    let report = crate::phase_f::delete_redundant(
        &pool,
        crate::phase_f::Apply::Yes,
        Some(crate::phase_f::DELETE_CONFIRM_TOKEN),
        false,
    )
    .await?;
    assert_eq!(report.deleted, 1);
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_metadata_contribution WHERE source_series_id = $1",
    )
    .bind(&deleted)
    .fetch_one(&pool)
    .await?;
    assert_eq!(remaining, 0);
    assert_projected(&pool, &original, "Original").await?;
    Ok(())
}

#[tokio::test]
async fn details_refresh_for_another_works_uuid_records_nothing() -> Result<()> {
    let pool = pool().await;
    let first_uuid = "11111111-1111-1111-1111-111111111111";
    let second_uuid = "22222222-2222-2222-2222-222222222222";
    let input = WorkInput {
        primary_title: Some("Original".into()),
        tags: vec!["Action".into()],
        ..Default::default()
    };
    let first = upsert_work_from_mangadex(&pool, first_uuid, &input).await?;
    let second = upsert_work_from_mangadex(&pool, second_uuid, &input).await?;
    let before = contributions(&pool).await?;
    refresh_mangadex_details(
        &pool,
        &first,
        second_uuid,
        &WorkInput {
            tags: vec!["Mystery".into()],
            external_ids: vec![
                ("mangadex".into(), first_uuid.into()),
                ("al".into(), "123".into()),
            ],
            ..input
        },
    )
    .await?;
    assert_eq!(
        contributions(&pool).await?,
        before,
        "neither work's observations change"
    );
    for (work, tag) in [(&first, "Action"), (&second, "Action")] {
        let tags: Vec<String> = sqlx::query_scalar(
            "SELECT tag FROM work_tag WHERE work_id = $1 AND source = 'mangadex'",
        )
        .bind(work)
        .fetch_all(&pool)
        .await?;
        assert_eq!(tags, vec![tag]);
    }
    let tracker: String = sqlx::query_scalar(
        "SELECT external_id FROM work_tracker_link WHERE work_id = $1 AND provider = 'al'",
    )
    .bind(&first)
    .fetch_one(&pool)
    .await?;
    assert_eq!(tracker, "123");
    Ok(())
}

#[tokio::test]
async fn legacy_details_refresh_respects_the_mangadex_tag_bar() -> Result<()> {
    for barred in [false, true] {
        let pool = pool().await;
        let work = work(&pool, "Legacy").await;
        if barred {
            work_metadata::set_bar(&pool, "mangadex", work_metadata::FieldGroup::Tags, true)
                .await?;
        }
        refresh_mangadex_details(
            &pool,
            &work,
            "unmapped",
            &WorkInput {
                tags: vec!["Action".into()],
                ..Default::default()
            },
        )
        .await?;
        let tags: Vec<String> = sqlx::query_scalar(
            "SELECT tag FROM work_tag WHERE work_id = $1 AND source = 'mangadex'",
        )
        .bind(&work)
        .fetch_all(&pool)
        .await?;
        assert_eq!(tags, if barred { vec![] } else { vec!["Action"] });
        assert!(contributions(&pool).await?.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn merge_projects_the_older_mangadex_tag_observation_including_empty() -> Result<()> {
    for tag in [Some("Action"), None] {
        let pool = pool().await;
        let loser = upsert_work_from_mangadex(
            &pool,
            "md-old",
            &WorkInput {
                primary_title: Some("Older work".into()),
                tags: tag.into_iter().map(str::to_owned).collect(),
                tag_details: tag
                    .into_iter()
                    .map(|name| SourceTag {
                        name: name.into(),
                        id: Some("action-id".into()),
                        group: "genre".into(),
                    })
                    .collect(),
                ..Default::default()
            },
        )
        .await?;
        let survivor = upsert_work_from_mangadex(
            &pool,
            "md-new",
            &WorkInput {
                primary_title: Some("Newer work".into()),
                tags: vec!["Romance".into()],
                ..Default::default()
            },
        )
        .await?;
        sqlx::query("UPDATE source_series SET created_at = CASE source_key WHEN 'md-old' THEN '2020-01-01'::timestamptz ELSE '2021-01-01'::timestamptz END")
            .execute(&pool).await?;
        let before = contributions(&pool).await?;
        merge_works(&pool, &loser, &survivor).await?;
        assert_eq!(contributions(&pool).await?, before);
        let tags: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT tag, tag_id, tag_group FROM work_tag WHERE work_id = $1 AND source = 'mangadex' ORDER BY ord",
        ).bind(&survivor).fetch_all(&pool).await?;
        let expected: Vec<_> = tag
            .into_iter()
            .map(|name| {
                (
                    name.to_string(),
                    Some("action-id".to_string()),
                    Some("genre".to_string()),
                )
            })
            .collect();
        assert_eq!(tags, expected);
    }
    Ok(())
}

#[tokio::test]
async fn cover_gallery_is_hidden_by_mangadex_bar_and_restored_after_unbar() -> Result<()> {
    let pool = pool().await;
    let work = create_work(
        &pool,
        &WorkInput {
            primary_title: Some("Gallery".into()),
            covers: ["v10.jpg", "v2.jpg", "v1.jpg"]
                .into_iter()
                .map(|file| Cover {
                    file_name: file.into(),
                    lang: Some("ja".into()),
                    volume: Some(file[1..file.len() - 4].into()),
                    is_primary: file == "v1.jpg",
                })
                .collect(),
            ..Default::default()
        },
    )
    .await?;
    let md = mapping(
        &pool,
        &work,
        "mangadex",
        "mangadex",
        "md-gallery",
        "2020-01-01",
    )
    .await?;
    sqlx::query("UPDATE source_series SET source_id = NULL WHERE id = $1")
        .bind(&md)
        .execute(&pool)
        .await?;
    let before: Vec<String> = sqlx::query_scalar(
        "SELECT to_jsonb(wc)::text || ':' || xmin::text FROM work_cover wc \
         WHERE wc.work_id = $1 ORDER BY wc.cover_file_name",
    )
    .bind(&work)
    .fetch_all(&pool)
    .await?;
    let gallery = crate::graphql::load_cover_gallery(&pool, &work).await?;
    assert_eq!(
        gallery
            .iter()
            .map(|c| c.file_name.as_str())
            .collect::<Vec<_>>(),
        vec!["v1.jpg", "v2.jpg", "v10.jpg"]
    );
    for barred in [true, false] {
        work_metadata::set_bar(&pool, "mangadex", work_metadata::FieldGroup::Cover, barred).await?;
        let actual = crate::graphql::load_cover_gallery(&pool, &work).await?;
        if barred {
            assert!(actual.is_empty());
        } else {
            assert_eq!(actual.len(), gallery.len());
            for (actual, expected) in actual.iter().zip(&gallery) {
                assert_eq!(actual.file_name, expected.file_name);
                assert_eq!(actual.url, expected.url);
                assert_eq!(actual.thumbnail_url, expected.thumbnail_url);
                assert_eq!(actual.lang, expected.lang);
                assert_eq!(actual.volume, expected.volume);
                assert_eq!(actual.is_primary, expected.is_primary);
            }
        }
    }
    let after: Vec<String> = sqlx::query_scalar(
        "SELECT to_jsonb(wc)::text || ':' || xmin::text FROM work_cover wc \
         WHERE wc.work_id = $1 ORDER BY wc.cover_file_name",
    )
    .bind(&work)
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        after, before,
        "bars preserve gallery observations without writes"
    );
    Ok(())
}

async fn thumbnail_work(pool: &Database) -> Result<String> {
    let work = work(pool, "Thumbnail work").await;
    sqlx::query(
        "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
         VALUES ('ss_thumb_a', $1, 'suwayomi', 'thumb-a', '41', '2022-01-01'), \
                ('ss_thumb_b', $1, 'suwayomi', 'thumb-b', '42', '2021-01-01'), \
                ('ss_thumb_c', $1, 'suwayomi', NULL, '43', '2020-01-01')",
    )
    .bind(&work)
    .execute(pool)
    .await?;
    // Cache source IDs deliberately differ: policy belongs to the mapping. Browse
    // picks mapping ID order; the feed picks newest release, then numeric ID.
    sqlx::raw_sql(
        "INSERT INTO suwayomi_series \
             (id, title, thumbnail_url, status, source_id, in_library, latest_chapter_at, updated_at) \
         VALUES (41, 'First', '/thumb/41', 'ONGOING', 'cache-a', TRUE, 3000, now()), \
                (42, 'Second', '/thumb/42', 'ONGOING', 'cache-b', TRUE, 1000, now()), \
                (43, 'Third', '/thumb/43', 'ONGOING', 'cache-c', TRUE, 2000, now())",
    )
    .execute(pool)
    .await?;
    Ok(work)
}

async fn project_thumbnail_work(pool: &Database, work: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    crate::locks::lock_works(&mut tx, &[work]).await?;
    upsert_browse_work(&mut tx, work).await?;
    tx.commit().await?;
    Ok(())
}

async fn card_thumbnail(pool: &Database, table: &str, work: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT suwayomi_thumbnail FROM {table} WHERE work_id = $1"
    ))
    .bind(work)
    .fetch_one(pool)
    .await?)
}

#[tokio::test]
async fn browse_thumbnail_skips_barred_sources_without_changing_card_identity() -> Result<()> {
    let pool = pool().await;
    let work = thumbnail_work(&pool).await?;
    project_thumbnail_work(&pool, &work).await?;
    assert_eq!(
        card_thumbnail(&pool, "browse_catalogue", &work)
            .await?
            .as_deref(),
        Some("/thumb/41")
    );
    for (source, expected) in [
        ("thumb-a", Some("/thumb/42")),
        ("thumb-b", Some("/thumb/43")),
        ("suwayomi", None),
    ] {
        work_metadata::set_bar(&pool, source, work_metadata::FieldGroup::Cover, true).await?;
        project_thumbnail_work(&pool, &work).await?;
        assert_eq!(
            card_thumbnail(&pool, "browse_catalogue", &work)
                .await?
                .as_deref(),
            expected
        );
        let reader: String =
            sqlx::query_scalar("SELECT reader_id FROM browse_catalogue WHERE work_id = $1")
                .bind(&work)
                .fetch_one(&pool)
                .await?;
        assert_eq!(reader, "41");
        let fallback = library_browse_fallback(&pool, std::slice::from_ref(&work)).await?;
        assert_eq!(fallback[0].suwayomi_thumbnail.as_deref(), expected);
    }
    work_metadata::set_bar(&pool, "thumb-a", work_metadata::FieldGroup::Cover, false).await?;
    project_thumbnail_work(&pool, &work).await?;
    assert_eq!(
        card_thumbnail(&pool, "browse_catalogue", &work)
            .await?
            .as_deref(),
        Some("/thumb/41")
    );
    Ok(())
}

#[tokio::test]
async fn feed_thumbnail_skips_barred_sources_and_browse_reselects_a_stale_fallback() -> Result<()> {
    let pool = pool().await;
    let work = thumbnail_work(&pool).await?;
    refresh_feed_series_updates(&pool).await?;
    assert_eq!(
        card_thumbnail(&pool, "feed_series_updates", &work)
            .await?
            .as_deref(),
        Some("/thumb/41")
    );
    for (source, expected) in [
        ("thumb-a", Some("/thumb/43")),
        ("suwayomi", Some("/thumb/42")),
        ("thumb-b", None),
    ] {
        work_metadata::set_bar(&pool, source, work_metadata::FieldGroup::Cover, true).await?;
        // Browse must not copy the feed's previously materialized barred thumbnail.
        project_thumbnail_work(&pool, &work).await?;
        assert_eq!(
            card_thumbnail(&pool, "browse_catalogue", &work)
                .await?
                .as_deref(),
            expected
        );
        refresh_feed_series_updates(&pool).await?;
        assert_eq!(
            card_thumbnail(&pool, "feed_series_updates", &work)
                .await?
                .as_deref(),
            expected
        );
        let reader: String =
            sqlx::query_scalar("SELECT reader_id FROM feed_series_updates WHERE work_id = $1")
                .bind(&work)
                .fetch_one(&pool)
                .await?;
        assert_eq!(reader, "41");
    }
    Ok(())
}

#[tokio::test]
async fn no_cover_bars_preserve_card_fallbacks_versions_and_projection_row_versions() -> Result<()>
{
    for feed in [false, true] {
        for cached in [None, Some(17_i64)] {
            let pool = pool().await;
            let work = thumbnail_work(&pool).await?;
            sqlx::query("UPDATE work SET cover_cached_version = $2 WHERE id = $1")
                .bind(&work)
                .bind(cached)
                .execute(&pool)
                .await?;
            if feed {
                sqlx::query(
                    "INSERT INTO feed_series_updates \
                         (work_id, reader_id, title, cover_url, suwayomi_thumbnail, released_at) \
                     VALUES ($1, '43', 'Thumbnail work', '', '/existing-feed-thumb', now())",
                )
                .bind(&work)
                .execute(&pool)
                .await?;
            }
            project_thumbnail_work(&pool, &work).await?;
            let row: (String, Option<String>, Option<String>) = sqlx::query_as(
                "SELECT reader_id, cover_url, suwayomi_thumbnail FROM browse_catalogue WHERE work_id = $1",
            ).bind(&work).fetch_one(&pool).await?;
            assert_eq!(row.0, if feed { "43" } else { "41" });
            let expected_cover = if feed {
                Some(String::new())
            } else {
                cached.map(|v| format!("/covers/{work}.webp?v={v}"))
            };
            assert_eq!(row.1, expected_cover);
            assert_eq!(
                row.2.as_deref(),
                Some(if feed {
                    "/existing-feed-thumb"
                } else {
                    "/thumb/41"
                })
            );
            let snapshot_sql = "SELECT to_jsonb(b)::text || ':' || xmin::text FROM browse_catalogue b WHERE b.work_id = $1";
            let before: String = sqlx::query_scalar(snapshot_sql)
                .bind(&work)
                .fetch_one(&pool)
                .await?;
            project_thumbnail_work(&pool, &work).await?;
            let after: String = sqlx::query_scalar(snapshot_sql)
                .bind(&work)
                .fetch_one(&pool)
                .await?;
            assert_eq!(
                after, before,
                "unchanged projection must not rewrite the row"
            );
            let version: Option<i64> =
                sqlx::query_scalar("SELECT cover_cached_version FROM work WHERE id = $1")
                    .bind(&work)
                    .fetch_one(&pool)
                    .await?;
            assert_eq!(version, cached);
        }
    }
    Ok(())
}

#[tokio::test]
async fn cover_bar_refreshes_stored_thumbnail_fallbacks_and_unbar_restores_them() -> Result<()> {
    // Feed order is newest release (41, 43, 42); Browse without a feed row is mapping
    // order (41, 42, 43). `false` in the last column: that step must not rewrite a row.
    let feed_steps = [
        ("thumb-b", true, Some("/thumb/41"), false),
        ("thumb-a", true, Some("/thumb/43"), true),
        ("suwayomi", true, None, true),
        ("thumb-b", false, Some("/thumb/42"), true),
        ("suwayomi", false, Some("/thumb/43"), true),
        ("thumb-a", false, Some("/thumb/41"), true),
    ];
    let browse_steps = [
        ("thumb-b", true, Some("/thumb/41"), false),
        ("thumb-a", true, Some("/thumb/43"), true),
        ("suwayomi", true, None, true),
        ("thumb-b", false, Some("/thumb/42"), true),
        ("suwayomi", false, Some("/thumb/42"), false),
        ("thumb-a", false, Some("/thumb/41"), true),
    ];
    for feed in [true, false] {
        let pool = pool().await;
        let work = thumbnail_work(&pool).await?;
        if feed {
            refresh_feed_series_updates(&pool).await?;
        }
        project_thumbnail_work(&pool, &work).await?;
        let tables: &[&str] = if feed {
            &["feed_series_updates", "browse_catalogue"]
        } else {
            &["browse_catalogue"]
        };
        for table in tables {
            assert_eq!(
                card_thumbnail(&pool, table, &work).await?.as_deref(),
                Some("/thumb/41")
            );
        }
        let versions = |pool: Database, work: String| async move {
            let rows: Vec<String> = sqlx::query_scalar(
                "SELECT xmin::text FROM feed_series_updates WHERE work_id = $1 \
                 UNION ALL SELECT xmin::text FROM browse_catalogue WHERE work_id = $1",
            )
            .bind(&work)
            .fetch_all(&pool)
            .await?;
            Ok::<_, anyhow::Error>(rows)
        };
        let steps = if feed { feed_steps } else { browse_steps };
        for (source, barred, expected, writes) in steps {
            let before = versions(pool.clone(), work.clone()).await?;
            // No explicit re-projection: the bar change itself refreshes the cards.
            work_metadata::set_bar(&pool, source, work_metadata::FieldGroup::Cover, barred)
                .await?;
            for table in tables {
                assert_eq!(
                    card_thumbnail(&pool, table, &work).await?.as_deref(),
                    expected,
                    "{table} after {source} barred={barred}"
                );
                let reader: String = sqlx::query_scalar(&format!(
                    "SELECT reader_id FROM {table} WHERE work_id = $1"
                ))
                .bind(&work)
                .fetch_one(&pool)
                .await?;
                assert_eq!(reader, "41");
            }
            let after = versions(pool.clone(), work.clone()).await?;
            if writes {
                assert_ne!(after, before, "{source} barred={barred}");
            } else {
                assert_eq!(after, before, "{source} barred={barred} must not write");
            }
        }
        if feed {
            // The refreshed rows are what the periodic rebuild stores.
            work_metadata::set_bar(&pool, "thumb-a", work_metadata::FieldGroup::Cover, true)
                .await?;
            refresh_feed_series_updates(&pool).await?;
            assert_eq!(
                card_thumbnail(&pool, "feed_series_updates", &work)
                    .await?
                    .as_deref(),
                Some("/thumb/43")
            );
        }
    }
    Ok(())
}
