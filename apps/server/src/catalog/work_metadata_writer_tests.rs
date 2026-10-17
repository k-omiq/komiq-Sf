use super::*;
use work_metadata::{Attribution, FieldGroup, Observation};

async fn pool() -> Result<Database> {
    let pool = Database::connect("sqlite::memory:").await?;
    sqlx::migrate!("./migrations_pg").run(pool.write()).await?;
    Ok(pool)
}

fn mangadex_input() -> WorkInput {
    WorkInput {
        primary_title: Some("MangaDex original title".into()),
        primary_lang: Some("en".into()),
        description: Some("MangaDex description".into()),
        descriptions: vec![("en".into(), "MangaDex description".into())],
        author: Some("MangaDex Author".into()),
        artist: Some("MangaDex Artist".into()),
        credits: vec![
            ("author".into(), "MangaDex Author".into()),
            ("artist".into(), "MangaDex Artist".into()),
        ],
        status: Some("ongoing".into()),
        aliases: vec![Alias {
            raw: "MangaDex alias".into(),
            lang: Some("en".into()),
        }],
        tags: vec!["Action".into()],
        tag_details: vec![SourceTag {
            id: Some("action-id".into()),
            name: "Action".into(),
            group: "genre".into(),
        }],
        ..Default::default()
    }
}

fn manga(id: i64, source: &str, title: &str) -> crate::suwayomi::SuwayomiManga {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "sourceId": source,
        "title": title,
        "author": "Allowed Author",
        "artist": "Allowed Artist",
        "description": "Allowed description",
        "genre": ["Fantasy"],
        "status": "COMPLETED",
        "inLibrary": false
    }))
    .unwrap()
}

async fn record(
    pool: &Database,
    work: &str,
    mapping: &str,
    observation: &Observation,
) -> Result<()> {
    crate::db::retry_transaction(|| async {
        let mut tx = pool.begin().await?;
        crate::locks::lock_works(&mut tx, &[work]).await?;
        work_metadata::record_on(&mut tx, mapping, observation, Attribution::Observed).await?;
        tx.commit().await?;
        Ok(())
    })
    .await
}

async fn link_suwayomi(
    pool: &Database,
    work: &str,
    m: &crate::suwayomi::SuwayomiManga,
) -> Result<String> {
    crate::series_cache::put_series(pool, m).await?;
    let mapping = upsert_source_series(
        pool,
        work,
        "suwayomi",
        &m.source_id,
        &m.id.to_string(),
        None,
        false,
    )
    .await?;
    record(pool, work, &mapping, &Observation::from_suwayomi(m)).await?;
    Ok(mapping)
}

async fn mapping(pool: &Database, key: &str) -> Result<String> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM source_series WHERE source_type = 'mangadex' AND source_key = $1",
    )
    .bind(key)
    .fetch_one(pool)
    .await?)
}

async fn versions(pool: &Database, mapping: &str) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT field_group || ':' || slot || ':' || lang || ':' || value || '=' || xmin::text \
         FROM work_metadata_contribution WHERE source_series_id = $1 \
         ORDER BY field_group, slot, lang, value",
    )
    .bind(mapping)
    .fetch_all(pool)
    .await?)
}

#[tokio::test]
async fn mangadex_upsert_records_observed_values_on_its_mapping() -> Result<()> {
    let pool = pool().await?;
    let work = upsert_work_from_mangadex(&pool, "md-observed", &mangadex_input()).await?;
    let mapping = mapping(&pool, "md-observed").await?;
    let rows: Vec<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT field_group, slot, lang, value, attribution FROM work_metadata_contribution \
         WHERE source_series_id = $1 ORDER BY field_group, slot, lang, value",
    )
    .bind(&mapping)
    .fetch_all(&pool)
    .await?;
    for (group, slot, lang, value) in [
        ("title", "primary", "en", "MangaDex original title"),
        ("title", "alias", "en", "MangaDex alias"),
        ("description", "primary", "", "MangaDex description"),
        ("description", "localized", "en", "MangaDex description"),
        ("credits", "author", "", "MangaDex Author"),
        ("credits", "artist", "", "MangaDex Artist"),
        ("tags", "tag", "", "Action"),
        ("status", "status", "", "ongoing"),
    ] {
        assert!(rows.contains(&(
            group.into(),
            slot.into(),
            lang.into(),
            value.into(),
            "observed".into(),
        )));
    }
    assert!(rows.iter().all(|row| row.4 == "observed"));
    let owner: String = sqlx::query_scalar("SELECT work_id FROM source_series WHERE id = $1")
        .bind(&mapping)
        .fetch_one(&pool)
        .await?;
    assert_eq!(owner, work);
    let detail: serde_json::Value = sqlx::query_scalar(
        "SELECT detail FROM work_metadata_contribution WHERE source_series_id = $1 \
         AND field_group = 'tags' AND value = 'Action'",
    )
    .bind(&mapping)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        detail,
        serde_json::json!({"id": "action-id", "group": "genre"})
    );
    Ok(())
}

#[tokio::test]
async fn identical_mangadex_upsert_preserves_contributions_and_only_updates_work_stamps(
) -> Result<()> {
    let pool = pool().await?;
    let input = mangadex_input();
    let work = upsert_work_from_mangadex(&pool, "md-identical", &input).await?;
    let mapping = mapping(&pool, "md-identical").await?;
    let before = versions(&pool, &mapping).await?;
    sqlx::query(
        "UPDATE work SET updated_at = '2000-01-01T00:00:00Z', \
         metadata_synced_at = '2000-01-01T00:00:00Z' WHERE id = $1",
    )
    .bind(&work)
    .execute(&pool)
    .await?;
    sqlx::query(
        "CREATE FUNCTION reject_full_metadata_update() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RAISE EXCEPTION 'identical upsert ran the full metadata update'; END; $$",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "CREATE TRIGGER reject_full_metadata_update BEFORE UPDATE OF primary_title, primary_lang, \
         description, year, original_language, status, demographic, content_rating, is_nsfw, \
         author, artist, cover_phash, cover_file_name ON work \
         FOR EACH ROW EXECUTE FUNCTION reject_full_metadata_update()",
    )
    .execute(&pool)
    .await?;
    assert_eq!(
        upsert_work_from_mangadex(&pool, "md-identical", &input).await?,
        work
    );
    assert_eq!(versions(&pool, &mapping).await?, before);
    let stamped: bool = sqlx::query_scalar(
        "SELECT updated_at > '2000-01-01T00:00:00Z' \
         AND metadata_synced_at > '2000-01-01T00:00:00Z' FROM work WHERE id = $1",
    )
    .bind(&work)
    .fetch_one(&pool)
    .await?;
    assert!(stamped);
    Ok(())
}

#[tokio::test]
async fn newer_mangadex_refresh_keeps_the_oldest_mapping_effective() -> Result<()> {
    for (observed, mapped) in [(true, true), (false, true), (true, false), (false, false)] {
        let pool = pool().await?;
        let input = mangadex_input();
        let work = upsert_work_from_mangadex(&pool, "md-ranked-old", &input).await?;
        let oldest = mapping(&pool, "md-ranked-old").await?;
        sqlx::query("UPDATE source_series SET created_at = '2020-01-01' WHERE id = $1")
            .bind(&oldest)
            .execute(&pool)
            .await?;
        if !observed {
            sqlx::query("DELETE FROM work_metadata_contribution WHERE source_series_id = $1")
                .bind(&oldest)
                .execute(&pool)
                .await?;
        }
        if mapped {
            let newer = upsert_source_series(
                &pool,
                &work,
                "mangadex",
                "mangadex",
                "md-ranked-new",
                None,
                false,
            )
            .await?;
            sqlx::query("UPDATE source_series SET created_at = '2021-01-01' WHERE id = $1")
                .bind(&newer)
                .execute(&pool)
                .await?;
        } else {
            sqlx::query(
                "INSERT INTO work_external_id (work_id, provider, external_id) \
                 VALUES ($1, 'mangadex', 'md-ranked-new')",
            )
            .bind(&work)
            .execute(&pool)
            .await?;
        }
        let work_sql = "SELECT to_jsonb(w) - 'updated_at' - 'metadata_synced_at' - 'mangadex_details_synced_at' \
                        FROM work w WHERE id = $1";
        let before: serde_json::Value = sqlx::query_scalar(work_sql)
            .bind(&work)
            .fetch_one(&pool)
            .await?;
        let children_sql = "SELECT jsonb_build_object( \
            'descriptions', (SELECT jsonb_agg(to_jsonb(d) || jsonb_build_object('xmin', xmin::text) ORDER BY lang) FROM work_description d WHERE work_id = $1), \
            'credits', (SELECT jsonb_agg(to_jsonb(c) || jsonb_build_object('xmin', xmin::text) ORDER BY role, name) FROM work_credit c WHERE work_id = $1), \
            'tags', (SELECT jsonb_agg(to_jsonb(t) || jsonb_build_object('xmin', xmin::text) ORDER BY tag) FROM work_tag t WHERE work_id = $1))";
        let children: serde_json::Value = sqlx::query_scalar(children_sql)
            .bind(&work)
            .fetch_one(&pool)
            .await?;
        sqlx::raw_sql(
            "CREATE FUNCTION reject_ranked_metadata_update() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN RAISE EXCEPTION 'newer mapping wrote effective metadata'; END; $$; \
             CREATE TRIGGER reject_ranked_metadata_update BEFORE UPDATE OF primary_title, primary_lang, \
             description, author, artist, status ON work \
             FOR EACH ROW EXECUTE FUNCTION reject_ranked_metadata_update()",
        ).execute(&pool).await?;
        let revised = WorkInput {
            primary_title: Some("Newer title".into()),
            primary_lang: Some("ja".into()),
            description: Some("Newer description".into()),
            descriptions: vec![("fr".into(), "Newer localized description".into())],
            author: Some("Newer author".into()),
            artist: Some("Newer artist".into()),
            credits: vec![("author".into(), "Newer author".into())],
            status: Some("completed".into()),
            aliases: vec![Alias {
                raw: "Newer alias".into(),
                lang: None,
            }],
            tags: vec!["Mystery".into()],
            tag_details: Vec::new(),
            ..input
        };
        for _ in 0..2 {
            assert_eq!(
                upsert_work_from_mangadex(&pool, "md-ranked-new", &revised).await?,
                work
            );
            let after: serde_json::Value = sqlx::query_scalar(work_sql)
                .bind(&work)
                .fetch_one(&pool)
                .await?;
            assert_eq!(after, before, "observed={observed}, mapped={mapped}");
            let after: serde_json::Value = sqlx::query_scalar(children_sql)
                .bind(&work)
                .fetch_one(&pool)
                .await?;
            assert_eq!(after, children);
        }
        let newer = mapping(&pool, "md-ranked-new").await?;
        let value: String = sqlx::query_scalar(
            "SELECT value FROM work_metadata_contribution WHERE source_series_id = $1 \
             AND field_group = 'title' AND slot = 'primary'",
        )
        .bind(&newer)
        .fetch_one(&pool)
        .await?;
        assert_eq!(value, "Newer title", "recording stays unmasked");
        sqlx::query("DROP TRIGGER reject_ranked_metadata_update ON work")
            .execute(&pool)
            .await?;
        upsert_work_from_mangadex(&pool, "md-ranked-old", &revised).await?;
        let effective: (String, String, String, String, String, String) = sqlx::query_as(
            "SELECT primary_title, primary_lang, description, author, artist, status FROM work WHERE id = $1",
        ).bind(&work).fetch_one(&pool).await?;
        assert_eq!(
            effective,
            (
                "Newer title".into(),
                "ja".into(),
                "Newer description".into(),
                "Newer author".into(),
                "Newer artist".into(),
                "completed".into(),
            )
        );
        let tags: Vec<String> = sqlx::query_scalar(
            "SELECT tag FROM work_tag WHERE work_id = $1 AND source = 'mangadex' ORDER BY ord",
        )
        .bind(&work)
        .fetch_all(&pool)
        .await?;
        assert_eq!(tags, vec!["Mystery"]);
    }
    Ok(())
}

#[tokio::test]
async fn barred_mangadex_title_keeps_allowed_title_and_does_not_insert_aliases() -> Result<()> {
    let pool = pool().await?;
    let input = mangadex_input();
    let work = upsert_work_from_mangadex(&pool, "md-title-bar", &input).await?;
    let allowed = manga(701, "allowed", "Allowed source title");
    link_suwayomi(&pool, &work, &allowed).await?;
    work_metadata::set_bar(&pool, "mangadex", FieldGroup::Title, true).await?;
    let input = WorkInput {
        primary_title: Some("Barred replacement title".into()),
        aliases: vec![Alias {
            raw: "Barred new alias".into(),
            lang: None,
        }],
        ..input
    };
    upsert_work_from_mangadex(&pool, "md-title-bar", &input).await?;
    let title: Option<String> = sqlx::query_scalar("SELECT primary_title FROM work WHERE id = $1")
        .bind(&work)
        .fetch_one(&pool)
        .await?;
    assert_eq!(title.as_deref(), Some("Allowed source title"));
    let aliases: Vec<String> =
        sqlx::query_scalar("SELECT raw_title FROM work_alias WHERE work_id = $1")
            .bind(&work)
            .fetch_all(&pool)
            .await?;
    assert!(aliases.iter().any(|title| title == "Allowed source title"));
    assert!(!aliases.iter().any(|title| title == "Barred new alias"));
    let observed: String = sqlx::query_scalar(
        "SELECT value FROM work_metadata_contribution WHERE source_series_id = $1 \
         AND field_group = 'title' AND slot = 'primary'",
    )
    .bind(mapping(&pool, "md-title-bar").await?)
    .fetch_one(&pool)
    .await?;
    assert_eq!(observed, "Barred replacement title");
    Ok(())
}

#[tokio::test]
async fn barred_mangadex_description_credits_and_status_keep_allowed_values() -> Result<()> {
    let pool = pool().await?;
    let input = mangadex_input();
    let work = upsert_work_from_mangadex(&pool, "md-scalars-bar", &input).await?;
    link_suwayomi(&pool, &work, &manga(702, "allowed", "Allowed title")).await?;
    for group in [
        FieldGroup::Description,
        FieldGroup::Credits,
        FieldGroup::Status,
    ] {
        work_metadata::set_bar(&pool, "mangadex", group, true).await?;
    }
    let input = WorkInput {
        description: Some("Barred replacement description".into()),
        descriptions: vec![("fr".into(), "Barred localized description".into())],
        author: Some("Barred Author".into()),
        artist: Some("Barred Artist".into()),
        credits: vec![("author".into(), "Barred Author".into())],
        status: Some("cancelled".into()),
        ..input
    };
    upsert_work_from_mangadex(&pool, "md-scalars-bar", &input).await?;
    let row: (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as("SELECT description, author, artist, status FROM work WHERE id = $1")
        .bind(&work)
        .fetch_one(&pool)
        .await?;
    assert_eq!(
        row,
        (
            Some("Allowed description".into()),
            Some("Allowed Author".into()),
            Some("Allowed Artist".into()),
            Some("COMPLETED".into()),
        )
    );
    let children: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM work_description WHERE work_id = $1 AND lang = 'fr') + \
                (SELECT count(*) FROM work_credit WHERE work_id = $1 AND name = 'Barred Author')",
    )
    .bind(&work)
    .fetch_one(&pool)
    .await?;
    assert_eq!(children, 0);
    let observed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM work_metadata_contribution WHERE source_series_id = $1 \
         AND attribution = 'observed' AND value IN \
         ('Barred replacement description', 'Barred localized description', 'Barred Author', 'Barred Artist', 'cancelled')",
    ).bind(mapping(&pool, "md-scalars-bar").await?).fetch_one(&pool).await?;
    assert_eq!(observed, 5);
    Ok(())
}

#[tokio::test]
async fn new_mangadex_work_is_seeded_unmasked_even_when_source_is_barred() -> Result<()> {
    let pool = pool().await?;
    for group in FieldGroup::ALL {
        work_metadata::set_bar(&pool, "mangadex", group, true).await?;
    }
    let work = upsert_work_from_mangadex(&pool, "md-new-barred", &mangadex_input()).await?;
    let row: (Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>) =
        sqlx::query_as("SELECT primary_title, primary_lang, description, author, artist, status FROM work WHERE id = $1")
            .bind(&work).fetch_one(&pool).await?;
    assert_eq!(
        row,
        (
            Some("MangaDex original title".into()),
            Some("en".into()),
            Some("MangaDex description".into()),
            Some("MangaDex Author".into()),
            Some("MangaDex Artist".into()),
            Some("ongoing".into()),
        )
    );
    assert!(!versions(&pool, &mapping(&pool, "md-new-barred").await?)
        .await?
        .is_empty());
    Ok(())
}

#[tokio::test]
async fn details_refresh_records_only_tags_on_the_named_mapping_and_masks_bars() -> Result<()> {
    let pool = pool().await?;
    let input = mangadex_input();
    let work = upsert_work_from_mangadex(&pool, "md-oldest", &input).await?;
    let oldest = mapping(&pool, "md-oldest").await?;
    let second = upsert_source_series(
        &pool,
        &work,
        "mangadex",
        "mangadex",
        "md-second",
        None,
        false,
    )
    .await?;
    sqlx::query("UPDATE source_series SET created_at = '2020-01-01T00:00:00Z' WHERE id = $1")
        .bind(&oldest)
        .execute(&pool)
        .await?;
    let before = versions(&pool, &oldest).await?;
    let named = WorkInput {
        tags: vec!["Mystery".into()],
        external_ids: vec![("mangadex".into(), "md-oldest".into())],
        ..input.clone()
    };
    let md_tags_sql =
        "SELECT tag FROM work_tag WHERE work_id = $1 AND source = 'mangadex' ORDER BY ord, tag";
    let md_tags: Vec<String> = sqlx::query_scalar(md_tags_sql)
        .bind(&work)
        .fetch_all(&pool)
        .await?;
    assert!(!md_tags.is_empty());
    refresh_mangadex_details(&pool, &work, "md-second", &named).await?;
    assert_eq!(versions(&pool, &oldest).await?, before);
    // The outranked mapping is recorded but does not replace the work's tags.
    let after_second: Vec<String> = sqlx::query_scalar(md_tags_sql)
        .bind(&work)
        .fetch_all(&pool)
        .await?;
    assert_eq!(after_second, md_tags);
    let second_rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT field_group, value, attribution FROM work_metadata_contribution WHERE source_series_id = $1",
    ).bind(&second).fetch_all(&pool).await?;
    assert_eq!(
        second_rows,
        vec![("tags".into(), "Mystery".into(), "observed".into())]
    );

    let explicit_oldest = WorkInput {
        tags: vec!["Fantasy".into()],
        ..input.clone()
    };
    refresh_mangadex_details(&pool, &work, "md-oldest", &explicit_oldest).await?;
    let oldest_tag: String = sqlx::query_scalar(
        "SELECT value FROM work_metadata_contribution WHERE source_series_id = $1 AND field_group = 'tags'",
    ).bind(&oldest).fetch_one(&pool).await?;
    assert_eq!(oldest_tag, "Fantasy");
    let after = versions(&pool, &oldest).await?;
    assert_eq!(
        before
            .iter()
            .filter(|row| !row.starts_with("tags:"))
            .collect::<Vec<_>>(),
        after
            .iter()
            .filter(|row| !row.starts_with("tags:"))
            .collect::<Vec<_>>()
    );
    sqlx::query(
        "INSERT INTO work_tag (work_id, tag, ord, source) VALUES ($1, 'Curated', 0, 'admin')",
    )
    .bind(&work)
    .execute(&pool)
    .await?;
    work_metadata::set_bar(&pool, "mangadex", FieldGroup::Tags, true).await?;
    let barred = WorkInput {
        tags: vec!["Comedy".into()],
        ..input
    };
    refresh_mangadex_details(&pool, &work, "md-oldest", &barred).await?;
    let tags: Vec<String> =
        sqlx::query_scalar("SELECT tag FROM work_tag WHERE work_id = $1 ORDER BY tag")
            .bind(&work)
            .fetch_all(&pool)
            .await?;
    assert_eq!(tags, vec!["Curated"]);
    let observed: String = sqlx::query_scalar(
        "SELECT value FROM work_metadata_contribution WHERE source_series_id = $1 AND field_group = 'tags'",
    ).bind(&oldest).fetch_one(&pool).await?;
    assert_eq!(observed, "Comedy");
    Ok(())
}

#[tokio::test]
async fn tag_projection_replaces_observed_tags_without_rewriting_them_twice() -> Result<()> {
    let pool = pool().await?;
    let work = upsert_work_from_mangadex(&pool, "md-tags", &mangadex_input()).await?;
    let mapping = mapping(&pool, "md-tags").await?;
    sqlx::query(
        "INSERT INTO feed_series_updates (work_id, reader_id, title, released_at) \
         VALUES ($1, $1, 'Tags', now())",
    )
    .bind(&work)
    .execute(&pool)
    .await?;
    let input = WorkInput {
        tags: vec!["Mystery".into(), "Manhwa".into()],
        tag_details: vec![SourceTag {
            name: "Mystery".into(),
            id: Some("mystery-id".into()),
            group: "genre".into(),
        }],
        ..Default::default()
    };
    let mut observation = Observation::from_mangadex(&input);
    observation
        .groups
        .retain(|group, _| *group == FieldGroup::Tags);
    record(&pool, &work, &mapping, &observation).await?;
    let mut tx = pool.begin().await?;
    crate::locks::lock_works(&mut tx, &[&work]).await?;
    assert!(
        work_metadata::project_on(&mut tx, &work, &[FieldGroup::Tags])
            .await?
            .tags
    );
    tx.commit().await?;
    let tags: Vec<(String, i64, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT tag, ord, tag_id, tag_group FROM work_tag WHERE work_id = $1 \
         AND source = 'mangadex' ORDER BY ord",
    )
    .bind(&work)
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        tags,
        vec![
            (
                "Mystery".into(),
                0,
                Some("mystery-id".into()),
                Some("genre".into())
            ),
            ("Manhwa".into(), 1, None, None),
        ]
    );
    let types: (String, String) = sqlx::query_as(
        "SELECT b.comic_type, f.comic_type FROM browse_catalogue b \
         JOIN feed_series_updates f USING (work_id) WHERE b.work_id = $1",
    )
    .bind(&work)
    .fetch_one(&pool)
    .await?;
    assert_eq!(types, ("MANHWA".into(), "MANHWA".into()));
    let snapshot = "SELECT jsonb_build_object( \
        'tags', (SELECT jsonb_agg(to_jsonb(t) || jsonb_build_object('xmin', xmin::text) ORDER BY tag) FROM work_tag t WHERE work_id = $1), \
        'work', (SELECT xmin::text FROM work WHERE id = $1), \
        'browse', (SELECT xmin::text FROM browse_catalogue WHERE work_id = $1), \
        'feed', (SELECT xmin::text FROM feed_series_updates WHERE work_id = $1))";
    let before: serde_json::Value = sqlx::query_scalar(snapshot)
        .bind(&work)
        .fetch_one(&pool)
        .await?;
    let before_contributions = versions(&pool, &mapping).await?;
    let mut tx = pool.begin().await?;
    crate::locks::lock_works(&mut tx, &[&work]).await?;
    assert!(
        !work_metadata::project_on(&mut tx, &work, &[FieldGroup::Tags])
            .await?
            .any()
    );
    tx.commit().await?;
    let after: serde_json::Value = sqlx::query_scalar(snapshot)
        .bind(&work)
        .fetch_one(&pool)
        .await?;
    assert_eq!(after, before);
    assert_eq!(versions(&pool, &mapping).await?, before_contributions);
    Ok(())
}

#[tokio::test]
async fn tag_projection_keeps_tags_when_the_chosen_mapping_is_unobserved() -> Result<()> {
    let pool = pool().await?;
    let work = upsert_work_from_mangadex(&pool, "md-unobserved-tags", &mangadex_input()).await?;
    let mapping = mapping(&pool, "md-unobserved-tags").await?;
    sqlx::query("DELETE FROM work_metadata_contribution WHERE source_series_id = $1 AND field_group = 'tags'")
        .bind(&mapping).execute(&pool).await?;
    let mut tx = pool.begin().await?;
    crate::locks::lock_works(&mut tx, &[&work]).await?;
    assert!(
        !work_metadata::project_on(&mut tx, &work, &[FieldGroup::Tags])
            .await?
            .tags
    );
    tx.commit().await?;
    let tags: Vec<String> =
        sqlx::query_scalar("SELECT tag FROM work_tag WHERE work_id = $1 AND source = 'mangadex'")
            .bind(&work)
            .fetch_all(&pool)
            .await?;
    assert_eq!(tags, vec!["Action"]);
    Ok(())
}

#[tokio::test]
async fn unpin_status_refreshes_feed_before_the_callers_browse_upsert() -> Result<()> {
    let pool = pool().await?;
    let input = WorkInput {
        status: Some("ONGOING".into()),
        ..mangadex_input()
    };
    let work = upsert_work_from_mangadex(&pool, "md-status-unpin", &input).await?;
    link_suwayomi(&pool, &work, &manga(712, "allowed", "Allowed title")).await?;
    sqlx::query(
        "INSERT INTO feed_series_updates (work_id, reader_id, title, status, released_at) \
         VALUES ($1, $1, 'Status', 'ONGOING', now())",
    )
    .bind(&work)
    .execute(&pool)
    .await?;
    let mut conn = pool.write().acquire().await?;
    pin_work_field(&mut conn, &work, PinnedField::Status, Some("ONGOING")).await?;
    drop(conn);
    work_metadata::set_bar(&pool, "mangadex", FieldGroup::Status, true).await?;
    let pinned: (String, String, String) = sqlx::query_as(
        "SELECT w.status, p.source_value, f.status FROM work w \
         JOIN work_field_pin p ON p.work_id = w.id AND p.field = 'status' \
         JOIN feed_series_updates f ON f.work_id = w.id WHERE w.id = $1",
    )
    .bind(&work)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        pinned,
        ("ONGOING".into(), "COMPLETED".into(), "ONGOING".into())
    );
    let mut tx = pool.begin().await?;
    unpin_work_field(&mut tx, &work, PinnedField::Status).await?;
    upsert_browse_work(&mut tx, &work).await?;
    tx.commit().await?;
    let statuses: (String, String, String) = sqlx::query_as(
        "SELECT w.status, f.status, b.status FROM work w \
         JOIN feed_series_updates f ON f.work_id = w.id \
         JOIN browse_catalogue b ON b.work_id = w.id WHERE w.id = $1",
    )
    .bind(&work)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        statuses,
        ("COMPLETED".into(), "COMPLETED".into(), "COMPLETED".into())
    );
    assert!(pinned_work_fields(&pool, &work).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn unpin_projects_allowed_credits_instead_of_the_stale_saved_source_value() -> Result<()> {
    let pool = pool().await?;
    let work = upsert_work_from_mangadex(&pool, "md-unpin", &mangadex_input()).await?;
    link_suwayomi(&pool, &work, &manga(703, "allowed", "Allowed title")).await?;
    let mut conn = pool.write().acquire().await?;
    pin_work_field(&mut conn, &work, PinnedField::Author, Some("Pinned Author")).await?;
    drop(conn);
    work_metadata::set_bar(&pool, "mangadex", FieldGroup::Credits, true).await?;
    // Exercise unpin's own projection even if source_value predates the policy change.
    sqlx::query("UPDATE work_field_pin SET source_value = 'MangaDex Author' WHERE work_id = $1 AND field = 'author'")
        .bind(&work).execute(&pool).await?;
    let mut conn = pool.write().acquire().await?;
    unpin_work_field(&mut conn, &work, PinnedField::Author).await?;
    drop(conn);
    let author: Option<String> = sqlx::query_scalar("SELECT author FROM work WHERE id = $1")
        .bind(&work)
        .fetch_one(&pool)
        .await?;
    assert_eq!(author.as_deref(), Some("Allowed Author"));
    assert!(pinned_work_fields(&pool, &work).await?.is_empty());
    Ok(())
}

fn enrolment<'a>(
    m: &'a crate::suwayomi::SuwayomiManga,
    key: &'a str,
) -> crate::graphql::Enrolment<'a> {
    crate::graphql::Enrolment {
        generation: None,
        source_id: &m.source_id,
        source_key: key,
        mangadex_uuid: None,
        candidate: crate::dedup::Candidate {
            title: m.title.clone(),
            author: m.author.clone(),
            description: m.description.clone(),
            ..Default::default()
        },
        work: WorkInput {
            primary_title: Some(m.title.clone()),
            description: m.description.clone(),
            author: m.author.clone(),
            artist: m.artist.clone(),
            status: Some(m.status.clone()),
            aliases: vec![Alias {
                raw: m.title.clone(),
                lang: None,
            }],
            ..Default::default()
        },
        observation: Observation::from_suwayomi(m),
        title: &m.title,
        consolidate: false,
        source_nsfw: false,
        pinned: false,
    }
}

async fn enrol(
    pool: &Database,
    m: &crate::suwayomi::SuwayomiManga,
) -> Result<crate::graphql::MatchResult> {
    let key = m.id.to_string();
    let enrolment = enrolment(m, &key);
    crate::db::retry_transaction(|| crate::graphql::enrol_source_series_once(pool, &enrolment))
        .await
}

#[tokio::test]
async fn enrolment_projects_the_cache_updated_after_capture() -> Result<()> {
    let pool = pool().await?;
    let stale = manga(711, "source", "Old");
    crate::series_cache::put_series(&pool, &stale).await?;
    let key = stale.id.to_string();
    let captured = enrolment(&stale, &key);
    let mut current = stale.clone();
    current.title = "New".into();
    current.description = Some("New description".into());
    current.author = Some("New author".into());
    current.artist = None;
    current.genre = vec!["Mystery".into(), "Action".into()];
    current.status = "ON_HIATUS".into();
    current.thumbnail_url = Some("new-cover.jpg".into());
    crate::series_cache::put_series(&pool, &current).await?;
    let result =
        crate::db::retry_transaction(|| crate::graphql::enrol_source_series_once(&pool, &captured))
            .await?;
    let mut conn = pool.acquire().await?;
    assert!(
        !work_metadata::differs_on(
            &mut conn,
            &result.source_series_id,
            &Observation::from_suwayomi(&current),
            Attribution::Observed,
        )
        .await?
    );
    let effective: (String, String, String, Option<String>, String) = sqlx::query_as(
        "SELECT primary_title, description, author, artist, status FROM work WHERE id = $1",
    )
    .bind(&result.work_id)
    .fetch_one(&mut *conn)
    .await?;
    assert_eq!(
        effective,
        (
            "New".into(),
            "New description".into(),
            "New author".into(),
            None,
            "ON_HIATUS".into(),
        )
    );
    Ok(())
}

#[tokio::test]
async fn enrolment_records_observed_rows_and_barred_author_cannot_auto_merge() -> Result<()> {
    let pool = pool().await?;
    let input = mangadex_input();
    let existing = upsert_work_from_mangadex(&pool, "md-enrol", &input).await?;
    let mut m = manga(704, "barred", "MangaDex alias");
    m.author = input.author.clone();
    m.description = input.description.clone();
    let trusted = enrol(&pool, &m).await?;
    assert_eq!(trusted.decision, "auto_merge");
    assert_eq!(trusted.work_id, existing);
    work_metadata::set_bar(&pool, "barred", FieldGroup::Credits, true).await?;
    m.id = 705;
    let result = enrol(&pool, &m).await?;
    assert_eq!(result.decision, "review");
    assert_ne!(result.work_id, existing);
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT value, attribution FROM work_metadata_contribution WHERE source_series_id = $1 \
         AND field_group = 'credits' AND slot = 'author'",
    )
    .bind(&result.source_series_id)
    .fetch_all(&pool)
    .await?;
    assert_eq!(rows, vec![("MangaDex Author".into(), "observed".into())]);
    let groups: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT field_group) FROM work_metadata_contribution WHERE source_series_id = $1",
    ).bind(&result.source_series_id).fetch_one(&pool).await?;
    assert_eq!(groups, 6);
    let before = versions(&pool, &result.source_series_id).await?;
    m.title = "A subsequent enrolment leaves refresh to put_series".into();
    assert_eq!(enrol(&pool, &m).await?.decision, "existing");
    assert_eq!(versions(&pool, &result.source_series_id).await?, before);
    Ok(())
}

#[tokio::test]
async fn put_series_projects_the_oldest_suwayomi_title_and_preserves_the_override() -> Result<()> {
    let pool = pool().await?;
    let mut oldest = manga(706, "first", "Original source title");
    let enrolled = enrol(&pool, &oldest).await?;
    let work = enrolled.work_id;
    let mut later = manga(707, "second", "Later source title");
    let later_mapping = link_suwayomi(&pool, &work, &later).await?;
    sqlx::query("UPDATE source_series SET created_at = '2020-01-01T00:00:00Z' WHERE id = $1")
        .bind(&enrolled.source_series_id)
        .execute(&pool)
        .await?;
    sqlx::query("UPDATE source_series SET created_at = '2021-01-01T00:00:00Z' WHERE id = $1")
        .bind(&later_mapping)
        .execute(&pool)
        .await?;
    sqlx::query("UPDATE work SET title_override = 'Curated title' WHERE id = $1")
        .bind(&work)
        .execute(&pool)
        .await?;
    oldest.title = "Oldest source revised title".into();
    assert!(crate::series_cache::put_series(&pool, &oldest).await?);
    let title: Option<String> = sqlx::query_scalar("SELECT primary_title FROM work WHERE id = $1")
        .bind(&work)
        .fetch_one(&pool)
        .await?;
    assert_eq!(title.as_deref(), Some("Oldest source revised title"));
    later.title = "Later source revised title".into();
    assert!(crate::series_cache::put_series(&pool, &later).await?);
    let row: (Option<String>, Option<String>) =
        sqlx::query_as("SELECT primary_title, title_override FROM work WHERE id = $1")
            .bind(&work)
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        row,
        (
            Some("Oldest source revised title".into()),
            Some("Curated title".into())
        )
    );
    let card: String = sqlx::query_scalar("SELECT title FROM browse_catalogue WHERE work_id = $1")
        .bind(&work)
        .fetch_one(&pool)
        .await?;
    assert_eq!(card, "Curated title");
    let before = versions(&pool, &enrolled.source_series_id).await?;
    work_metadata::observe_suwayomi(&pool, &oldest).await?;
    assert_eq!(versions(&pool, &enrolled.source_series_id).await?, before);
    Ok(())
}

#[tokio::test]
async fn stale_suwayomi_observation_reads_the_current_cache() -> Result<()> {
    let pool = pool().await?;
    let stale = manga(710, "source", "Older fetched title");
    let enrolled = enrol(&pool, &stale).await?;
    crate::series_cache::put_series(&pool, &stale).await?;
    let mut newer = stale.clone();
    newer.title = "Newer cached title".into();
    newer.author = Some("Newer author".into());
    newer.artist = Some("Newer artist".into());
    newer.description = Some("Newer description".into());
    newer.genre = vec!["Mystery".into(), "Action".into()];
    newer.status = "ON_HIATUS".into();
    newer.thumbnail_url = Some("new-cover.jpg".into());
    crate::series_cache::put_series(&pool, &newer).await?;
    let before = versions(&pool, &enrolled.source_series_id).await?;
    work_metadata::observe_suwayomi(&pool, &stale).await?;
    assert_eq!(versions(&pool, &enrolled.source_series_id).await?, before);
    let mut conn = pool.acquire().await?;
    assert!(
        !work_metadata::differs_on(
            &mut conn,
            &enrolled.source_series_id,
            &Observation::from_suwayomi(&newer),
            Attribution::Observed,
        )
        .await?
    );
    drop(conn);
    let effective: (String, String, String, String, String) = sqlx::query_as(
        "SELECT primary_title, description, author, artist, status FROM work WHERE id = $1",
    )
    .bind(&enrolled.work_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        effective,
        (
            "Newer cached title".into(),
            "Newer description".into(),
            "Newer author".into(),
            "Newer artist".into(),
            "ON_HIATUS".into(),
        )
    );
    sqlx::query("DELETE FROM suwayomi_series WHERE id = $1")
        .bind(stale.id)
        .execute(&pool)
        .await?;
    work_metadata::observe_suwayomi(&pool, &stale).await?;
    assert_eq!(versions(&pool, &enrolled.source_series_id).await?, before);
    Ok(())
}

#[tokio::test]
async fn suwayomi_observation_waits_for_cache_publication_before_locking_work() -> Result<()> {
    let pool = pool().await?.foreground();
    let initial = manga(711, "source", "Original title");
    let enrolled = enrol(&pool, &initial).await?;
    crate::series_cache::put_series(&pool, &initial).await?;
    let mut stale = initial.clone();
    stale.title = "Stale fetch".into();
    let mut publisher = pool.begin().await?;
    crate::locks::advisory(
        &mut publisher,
        &crate::series_cache::series_publication_keys("711"),
    )
    .await?;
    sqlx::query("UPDATE suwayomi_series SET title = 'Current publication' WHERE id = 711")
        .execute(&mut *publisher)
        .await?;
    let writer_pool = pool.clone();
    let observer =
        tokio::spawn(async move { work_metadata::observe_suwayomi(&writer_pool, &stale).await });
    let waited = super::phase4_tests::wait_for_lock(&pool).await;
    let mut work_guard = pool.begin().await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        crate::locks::lock_works(&mut work_guard, &[&enrolled.work_id]),
    )
    .await
    .expect("observer took a work lock before the publication keys")?;
    work_guard.rollback().await?;
    publisher.commit().await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), observer).await???;
    assert!(waited, "observation did not wait for the cache publication");
    let title: String = sqlx::query_scalar("SELECT primary_title FROM work WHERE id = $1")
        .bind(&enrolled.work_id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(title, "Current publication");
    Ok(())
}

#[tokio::test]
async fn put_series_keeps_the_cache_write_when_metadata_observation_fails() -> Result<()> {
    let pool = pool().await?;
    let mut m = manga(708, "source", "Original title");
    let enrolled = enrol(&pool, &m).await?;
    assert!(crate::series_cache::put_series(&pool, &m).await?);
    let before = versions(&pool, &enrolled.source_series_id).await?;
    sqlx::query(
        "CREATE FUNCTION fail_metadata_observation() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RAISE EXCEPTION 'forced metadata observation failure'; END; $$",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "CREATE TRIGGER fail_metadata_observation BEFORE INSERT ON work_metadata_contribution \
         FOR EACH ROW EXECUTE FUNCTION fail_metadata_observation()",
    )
    .execute(&pool)
    .await?;
    m.title = "New cached title".into();
    assert!(crate::series_cache::put_series(&pool, &m).await?);
    let cached: String = sqlx::query_scalar("SELECT title FROM suwayomi_series WHERE id = $1")
        .bind(m.id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(cached, "New cached title");
    assert_eq!(versions(&pool, &enrolled.source_series_id).await?, before);
    let title: Option<String> = sqlx::query_scalar("SELECT primary_title FROM work WHERE id = $1")
        .bind(&enrolled.work_id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(title.as_deref(), Some("Original title"));
    Ok(())
}

#[tokio::test]
async fn refused_put_series_does_not_record_or_project_metadata() -> Result<()> {
    let pool = pool().await?;
    let mut m = manga(709, "source", "Original title");
    let enrolled = enrol(&pool, &m).await?;
    let before = versions(&pool, &enrolled.source_series_id).await?;
    m.title = "Refused title".into();
    m.source = Some(crate::suwayomi::SuwayomiSourceLang {
        lang: Some("ja".into()),
    });
    assert!(!crate::series_cache::put_series(&pool, &m).await?);
    assert_eq!(versions(&pool, &enrolled.source_series_id).await?, before);
    let title: Option<String> = sqlx::query_scalar("SELECT primary_title FROM work WHERE id = $1")
        .bind(&enrolled.work_id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(title.as_deref(), Some("Original title"));
    Ok(())
}
