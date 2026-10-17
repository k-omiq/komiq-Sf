//! Seed provenance from stored MangaDex scalars and the Suwayomi cache. Existing
//! mapping/group observations always win; barred groups stay unobserved. This pass
//! never projects effective values.

use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::watch;

use crate::db::Database;

const FLAG: &str = "work_metadata_backfill_v1";
const BATCH_SIZE: i64 = 50;
const BATCH_GAP: Duration = Duration::from_millis(250);
const BOOT_DELAY: Duration = Duration::from_secs(120);
const RETRY_DELAY: Duration = Duration::from_secs(30 * 60);
// The Unicode White_Space characters stripped by Rust's str::trim, also used by
// Observation's live writers. PostgreSQL's default btrim strips only ASCII spaces.
const TRIM_CHARS: &str = "\u{0009}\u{000a}\u{000b}\u{000c}\u{000d} \u{0085}\u{00a0}\u{1680}\u{2000}\u{2001}\u{2002}\u{2003}\u{2004}\u{2005}\u{2006}\u{2007}\u{2008}\u{2009}\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct InsertedRows {
    pub observed: u64,
    pub inferred: u64,
    pub unknown: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Report {
    /// Works with at least one MangaDex or Suwayomi mapping, including mappings
    /// whose cache is missing or whose groups were already observed.
    pub works_scanned: u64,
    pub rows_inserted: InsertedRows,
    pub batches: u64,
    pub elapsed_ms: u128,
}

/// One cursor pass. Each batch commits independently and retries as a whole. A
/// failed or cancelled pass can safely start again from the beginning.
pub async fn run_pass(pool: &Database, batch_gap: Duration) -> Result<Report> {
    let started = Instant::now();
    let mut report = Report::default();
    let mut cursor: Option<String> = None;
    loop {
        let after = if cursor.is_some() {
            "w.id > $1"
        } else {
            "$1::text IS NULL"
        };
        let sql = format!(
            "SELECT w.id FROM work w \
              WHERE {after} \
                AND EXISTS (SELECT 1 FROM source_series ss WHERE ss.work_id = w.id \
                             AND ss.source_type IN ('mangadex', 'suwayomi')) \
              ORDER BY w.id LIMIT $2"
        );
        let ids: Vec<String> = sqlx::query_scalar(&sql)
            .bind(&cursor)
            .bind(BATCH_SIZE)
            .fetch_all(pool)
            .await?;
        let Some(last) = ids.last() else { break };
        if report.batches > 0 && !batch_gap.is_zero() {
            tokio::time::sleep(batch_gap).await;
        }
        let counts = crate::db::retry_transaction(|| insert_batch(pool, &ids)).await?;
        cursor = Some(last.clone());
        report.works_scanned += ids.len() as u64;
        report.batches += 1;
        report.rows_inserted.observed += counts.observed;
        report.rows_inserted.inferred += counts.inferred;
        report.rows_inserted.unknown += counts.unknown;
        #[cfg(test)]
        super::guards::pause("work-metadata-batch-complete").await;
    }
    report.elapsed_ms = started.elapsed().as_millis();
    Ok(report)
}

async fn insert_batch(pool: &Database, ids: &[String]) -> Result<InsertedRows> {
    let mut tx = pool.begin().await?;
    crate::locks::lock_works(&mut tx, ids).await?;
    let counts = insert_batch_on(&mut tx, ids).await?;
    tx.commit().await?;
    Ok(counts)
}

async fn insert_batch_on(conn: &mut sqlx::PgConnection, ids: &[String]) -> Result<InsertedRows> {
    let (observed, inferred, unknown): (i64, i64, i64) = sqlx::query_as(
        r#"
        WITH works AS (
            SELECT w.id,
                   w.primary_title AS title, COALESCE(w.primary_lang, '') AS lang,
                   w.description,
                   CASE WHEN pa.field IS NOT NULL THEN pa.source_value ELSE w.author END AS author,
                   CASE WHEN pr.field IS NOT NULL THEN pr.source_value ELSE w.artist END AS artist,
                   CASE WHEN ps.field IS NOT NULL THEN ps.source_value ELSE w.status END AS status,
                   w.cover_file_name AS cover,
                   EXISTS (SELECT 1 FROM work_tag t
                           WHERE t.work_id = w.id AND t.source IN ('admin', 'mangadex')) AS higher_tags
              FROM work w
              LEFT JOIN work_field_pin pa ON pa.work_id = w.id AND pa.field = 'author'
              LEFT JOIN work_field_pin pr ON pr.work_id = w.id AND pr.field = 'artist'
              LEFT JOIN work_field_pin ps ON ps.work_id = w.id AND ps.field = 'status'
             WHERE w.id = ANY($1::text[])
        ), oldest_md AS (
            SELECT DISTINCT ON (ss.work_id) ss.id, ss.work_id
              FROM source_series ss JOIN works w ON w.id = ss.work_id
             WHERE ss.source_type = 'mangadex'
             ORDER BY ss.work_id, ss.created_at, ss.id
        ), cached AS (
            SELECT ss.id, ss.work_id, COALESCE(ss.source_id, ss.source_type) AS source_id,
                   sy.title, sy.description, sy.author, sy.artist, sy.status,
                   sy.thumbnail_url AS cover, sy.genre,
                   ARRAY(
                       SELECT DISTINCT btrim(g.value #>> '{}', $2) AS tag
                         FROM jsonb_array_elements(
                             CASE WHEN jsonb_typeof(sy.genre) = 'array'
                                  THEN sy.genre ELSE '[]'::jsonb END
                         ) g(value)
                        WHERE btrim(g.value #>> '{}', $2) <> ''
                          AND NOT EXISTS (
                              SELECT 1 FROM jsonb_array_elements(
                                  CASE WHEN jsonb_typeof(sy.genre) = 'array'
                                       THEN sy.genre ELSE '[]'::jsonb END
                              ) invalid(value) WHERE jsonb_typeof(invalid.value) <> 'string'
                          )
                        ORDER BY tag
                   ) AS genres
              FROM source_series ss JOIN works w ON w.id = ss.work_id
              JOIN suwayomi_series sy ON sy.id = public.sqlite_integer_pin(ss.source_key)
             WHERE ss.source_type = 'suwayomi'
        ), inputs AS (
            SELECT md.id, w.id AS work_id, true AS mangadex,
                   w.title, w.lang, w.description, w.author, w.artist, w.status, w.cover,
                   NULL::jsonb AS genre,
                   true AS same_title, true AS same_description, true AS same_credits,
                   true AS same_status, true AS same_tags
              FROM oldest_md md JOIN works w ON w.id = md.work_id
            UNION ALL
            SELECT c.id, c.work_id, false,
                   c.title, '', c.description, c.author, c.artist, c.status, c.cover, c.genre,
                   NULLIF(btrim(c.title, $2), '') IS NOT DISTINCT FROM NULLIF(btrim(w.title, $2), ''),
                   NULLIF(btrim(c.description, $2), '') IS NOT DISTINCT FROM NULLIF(btrim(w.description, $2), ''),
                   NULLIF(btrim(c.author, $2), '') IS NOT DISTINCT FROM NULLIF(btrim(w.author, $2), '')
                       AND NULLIF(btrim(c.artist, $2), '') IS NOT DISTINCT FROM NULLIF(btrim(w.artist, $2), ''),
                   NULLIF(btrim(c.status, $2), '') IS NOT DISTINCT FROM NULLIF(btrim(w.status, $2), ''),
                   NOT w.higher_tags AND c.genres = ARRAY(
                       SELECT DISTINCT u.tag FROM cached allowed
                       CROSS JOIN LATERAL unnest(allowed.genres) AS u(tag)
                        WHERE allowed.work_id = c.work_id
                          AND NOT EXISTS (SELECT 1 FROM source_metadata_bar b
                                           WHERE b.source_id = allowed.source_id
                                             AND b.field_group = 'tags')
                        ORDER BY u.tag
                   )
              FROM cached c JOIN works w ON w.id = c.work_id
        ), groups AS (
            SELECT i.id, g.field_group,
                   CASE WHEN i.mangadex THEN 'observed'
                        WHEN CASE g.field_group
                            WHEN 'title' THEN i.same_title
                            WHEN 'description' THEN i.same_description
                            WHEN 'credits' THEN i.same_credits
                            WHEN 'status' THEN i.same_status
                            WHEN 'tags' THEN i.same_tags
                            ELSE false END THEN 'inferred'
                        ELSE 'unknown' END AS attribution
              FROM inputs i
              CROSS JOIN (VALUES ('title'), ('description'), ('credits'),
                                 ('tags'), ('status'), ('cover')) g(field_group)
             WHERE NOT EXISTS (
                 SELECT 1 FROM work_metadata_contribution c
                  WHERE c.source_series_id = i.id AND c.field_group = g.field_group
             )
               AND NOT EXISTS (
                   SELECT 1 FROM source_series ss JOIN source_metadata_bar b
                     ON b.source_id = COALESCE(ss.source_id, ss.source_type)
                    WHERE ss.id = i.id AND b.field_group = g.field_group
               )
        ), values_given AS (
            SELECT i.id, v.field_group, v.slot, v.lang, v.value, 0 AS ord, NULL::jsonb AS detail
              FROM inputs i CROSS JOIN LATERAL (
                  VALUES ('title', 'primary', i.lang, i.title),
                         ('title', 'alias', '', CASE WHEN NOT i.mangadex THEN i.title END),
                         ('description', 'primary', '', i.description),
                         ('credits', 'author', '', i.author),
                         ('credits', 'artist', '', i.artist),
                         ('status', 'status', '', i.status),
                         ('cover', 'cover', '', i.cover)
              ) v(field_group, slot, lang, value)
             WHERE v.value IS NOT NULL AND btrim(v.value, $2) <> ''
            UNION ALL
            SELECT i.id, 'tags', 'tag', '', t.tag, t.ord::integer,
                   jsonb_build_object('id', t.tag_id, 'group', t.tag_group)
              FROM inputs i JOIN work_tag t ON t.work_id = i.work_id
             WHERE i.mangadex AND t.source = 'mangadex' AND btrim(t.tag, $2) <> ''
            UNION ALL
            SELECT i.id, 'tags', 'tag', '', g.value #>> '{}',
                   (g.position - 1)::integer, NULL::jsonb
              FROM inputs i CROSS JOIN LATERAL jsonb_array_elements(
                  CASE WHEN jsonb_typeof(i.genre) = 'array' THEN i.genre ELSE '[]'::jsonb END
              ) WITH ORDINALITY g(value, position)
             WHERE NOT i.mangadex AND jsonb_typeof(g.value) = 'string'
               AND btrim(g.value #>> '{}', $2) <> ''
        ), pending AS (
            SELECT g.id, g.field_group, v.slot, v.lang, v.value, v.ord, v.detail, g.attribution
              FROM groups g JOIN values_given v ON v.id = g.id AND v.field_group = g.field_group
            UNION ALL
            SELECT g.id, g.field_group, '', '', '', 0, NULL::jsonb, g.attribution
              FROM groups g
             WHERE NOT EXISTS (SELECT 1 FROM values_given v
                               WHERE v.id = g.id AND v.field_group = g.field_group)
        ), inserted AS (
            INSERT INTO work_metadata_contribution
                (source_series_id, field_group, slot, lang, value, ord, detail, attribution, observed_at)
            SELECT DISTINCT ON (id, field_group, slot, lang, md5(value))
                   id, field_group, slot, lang, value, ord, detail, attribution, now()
              FROM pending
             ORDER BY id, field_group, slot, lang, md5(value), ord
            ON CONFLICT (source_series_id, field_group, slot, lang, value_key) DO NOTHING
            RETURNING attribution
        )
        SELECT COUNT(*) FILTER (WHERE attribution = 'observed'),
               COUNT(*) FILTER (WHERE attribution = 'inferred'),
               COUNT(*) FILTER (WHERE attribution = 'unknown')
          FROM inserted
        "#,
    )
    .bind(ids)
    .bind(TRIM_CHARS)
    .fetch_one(conn)
    .await?;
    Ok(InsertedRows {
        observed: observed as u64,
        inferred: inferred as u64,
        unknown: unknown as u64,
    })
}

/// A one-time local backfill, staggered behind boot. Only complete, clean passes earn
/// the maintenance flag; an error retries the whole cursor pass after half an hour.
pub fn spawn(pool: Database, shutdown: watch::Receiver<bool>) {
    let pool = pool.background();
    tokio::spawn(crate::task::supervise(
        "work-metadata-backfill",
        RETRY_DELAY,
        shutdown.clone(),
        move || background_loop(pool.clone(), shutdown.clone()),
    ));
}

async fn background_loop(pool: Database, mut shutdown: watch::Receiver<bool>) {
    if !wait(BOOT_DELAY, &mut shutdown).await {
        return;
    }
    loop {
        let result = tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            result = run_if_needed(&pool) => result,
        };
        match result {
            Ok(Some(report)) => {
                tracing::info!(?report, "work metadata backfill complete");
                return;
            }
            Ok(None) => return,
            Err(error) => {
                let error = crate::db::observed_database_error(error);
                tracing::warn!(error = %error, "work metadata backfill failed; will retry");
            }
        }
        if !wait(RETRY_DELAY, &mut shutdown).await {
            return;
        }
    }
}

async fn run_if_needed(pool: &Database) -> Result<Option<Report>> {
    if super::maintenance_flag_present(pool, FLAG).await? {
        return Ok(None);
    }
    let started = Instant::now();
    let mut report = Report::default();
    loop {
        let pass = run_pass(pool, BATCH_GAP).await?;
        report.works_scanned += pass.works_scanned;
        report.batches += pass.batches;
        report.rows_inserted.observed += pass.rows_inserted.observed;
        report.rows_inserted.inferred += pass.rows_inserted.inferred;
        report.rows_inserted.unknown += pass.rows_inserted.unknown;
        // A merge can move an unobserved mapping behind the cursor. Check exactly
        // the mappings this backfill can seed before declaring completion.
        let pending: bool = sqlx::query_scalar(
            "WITH oldest_md AS ( \
                 SELECT DISTINCT ON (work_id) id, COALESCE(source_id, source_type) AS source_id FROM source_series \
                  WHERE source_type = 'mangadex' ORDER BY work_id, created_at, id \
             ), eligible AS ( \
                 SELECT id, source_id FROM oldest_md \
                 UNION ALL \
                 SELECT ss.id, COALESCE(ss.source_id, ss.source_type) FROM source_series ss \
                   JOIN suwayomi_series sy ON sy.id = public.sqlite_integer_pin(ss.source_key) \
                  WHERE ss.source_type = 'suwayomi' \
             ) SELECT EXISTS ( \
                 SELECT 1 FROM eligible e \
                 CROSS JOIN (VALUES ('title'), ('description'), ('credits'), \
                                    ('tags'), ('status'), ('cover')) g(field_group) \
                  WHERE NOT EXISTS (SELECT 1 FROM source_metadata_bar b \
                                     WHERE b.source_id = e.source_id AND b.field_group = g.field_group) \
                    AND NOT EXISTS (SELECT 1 FROM work_metadata_contribution c \
                                     WHERE c.source_series_id = e.id \
                                       AND c.field_group = g.field_group))",
        )
        .fetch_one(pool)
        .await?;
        if !pending {
            crate::db::retry_transaction(|| super::set_maintenance_flag(pool, FLAG)).await?;
            report.elapsed_ms = started.elapsed().as_millis();
            return Ok(Some(report));
        }
    }
}

async fn wait(duration: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    if *shutdown.borrow() {
        return false;
    }
    tokio::select! {
        biased;
        _ = shutdown.changed() => false,
        _ = tokio::time::sleep(duration) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    async fn pool() -> Database {
        let pool = crate::database::TestPoolOptions::new()
            .max_connections(4)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations_pg")
            .run(pool.write())
            .await
            .unwrap();
        pool
    }

    async fn mangadex_fixture(pool: &Database) {
        sqlx::raw_sql(
            r#"
            INSERT INTO work (id, primary_title, primary_lang, description, author, artist,
                              status, cover_file_name, title_override, description_override,
                              created_at, updated_at)
            VALUES ('md', 'Stored title', 'en', 'Stored description', 'Pinned author',
                    'Pinned artist', 'ongoing', 'cover.jpg', 'Title override',
                    'Description override', '2020-01-01', '2020-01-01');
            INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at)
            VALUES ('md-z-new', 'md', 'mangadex', 'mangadex', 'new', '2022-01-01'),
                   ('md-b-old', 'md', 'mangadex', 'mangadex', 'old-b', '2020-01-01'),
                   ('md-a-old', 'md', 'mangadex', 'mangadex', 'old-a', '2020-01-01');
            INSERT INTO work_field_pin (work_id, field, value, source_value, pinned_at)
            VALUES ('md', 'author', 'Pinned author', 'Source author', '2020-01-01'),
                   ('md', 'artist', 'Pinned artist', NULL, '2020-01-01'),
                   ('md', 'status', 'ongoing', 'completed', '2020-01-01');
            INSERT INTO work_tag (work_id, tag, ord, source, tag_id, tag_group)
            VALUES ('md', 'Action', 3, 'mangadex', 'action-id', 'genre'),
                   ('md', 'Long Strip', 9, 'mangadex', 'long-strip-id', 'format'),
                   ('md', 'Curated', 10, 'admin', NULL, NULL);
            INSERT INTO work_alias (id, work_id, normalized_title, raw_title, lang)
            VALUES ('alias-md', 'md', 'stored alias', 'Stored alias', 'en');
            INSERT INTO work_alias_token (work_id, token) VALUES ('md', 'stored'), ('md', 'alias');
            INSERT INTO work_credit (work_id, role, name) VALUES ('md', 'author', 'Listed author');
            INSERT INTO work_description (work_id, lang, description)
            VALUES ('md', 'fr', 'Localized description');
            "#,
        )
        .execute(pool)
        .await
        .unwrap();
    }

    async fn suwayomi_fixture(pool: &Database) {
        sqlx::raw_sql(
            r#"
            INSERT INTO work (id, primary_title, description, author, artist, status, created_at, updated_at)
            VALUES ('sy', ' Same title ', 'Same description', 'Pinned author', 'Artist',
                    'Pinned status', '2020-01-01', '2020-01-01'),
                   ('sy-empty', NULL, NULL, NULL, NULL, NULL, '2020-01-01', '2020-01-01');
            INSERT INTO work_field_pin (work_id, field, value, source_value, pinned_at)
            VALUES ('sy', 'author', 'Pinned author', 'Cache author', '2020-01-01'),
                   ('sy', 'status', 'Pinned status', 'ONGOING', '2020-01-01');
            INSERT INTO suwayomi_series (id, title, description, author, artist, status,
                                         genre, thumbnail_url, source_id, updated_at)
            VALUES (101, ' Same title ', ' Same description ', ' Cache author ', 'Artist',
                    ' ONGOING ', '[" Action ", "", "Drama", " Action ", "  "]',
                    ' /thumb.png ', 'source-a', '2020-01-01'),
                   (102, ' Other title ', NULL, 'Cache author', 'Other artist', 'COMPLETED',
                    '["Mystery"]', '/other.png', 'source-b', '2020-01-01'),
                   (103, '  ', NULL, NULL, NULL, ' ', '[]', NULL, 'source-c', '2020-01-01');
            INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at)
            VALUES ('sy-match', 'sy', 'suwayomi', 'source-a', '101', '2020-01-01'),
                   ('sy-different', 'sy', 'suwayomi', 'source-b', '102', '2020-01-01'),
                   ('sy-empty-map', 'sy-empty', 'suwayomi', 'source-c', '103', '2020-01-01'),
                   ('sy-no-cache', 'sy', 'suwayomi', 'missing', '999', '2020-01-01');
            INSERT INTO work_tag (work_id, tag, source)
            VALUES ('sy', 'Curated', 'admin'), ('sy', 'Action', 'suwayomi'),
                   ('sy-empty', 'Action', 'mangadex');
            "#,
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("UPDATE suwayomi_series SET title = $1, status = $2 WHERE id = 101")
            .bind("\u{3000}\nSame title\t\u{00a0}")
            .bind("\nONGOING\t")
            .execute(pool)
            .await
            .unwrap();
    }

    type Contribution = (String, String, String, i32, Option<Value>, String);

    async fn group(pool: &Database, mapping: &str, field: &str) -> Vec<Contribution> {
        sqlx::query_as(
            "SELECT slot, lang, value, ord, detail, attribution FROM work_metadata_contribution \
             WHERE source_series_id = $1 AND field_group = $2 ORDER BY ord, slot, lang, value",
        )
        .bind(mapping)
        .bind(field)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    async fn snapshot(pool: &Database, tables: &[&str]) -> Value {
        let mut snapshot = serde_json::Map::new();
        for table in tables {
            let rows: Value = sqlx::query_scalar(&format!(
                "SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb) \
                 FROM {table} t"
            ))
            .fetch_one(pool)
            .await
            .unwrap();
            snapshot.insert((*table).into(), rows);
        }
        Value::Object(snapshot)
    }

    #[tokio::test]
    async fn mangadex_uses_oldest_mapping_and_pin_source_values_only() {
        let pool = pool().await;
        mangadex_fixture(&pool).await;
        let report = run_pass(&pool, Duration::ZERO).await.unwrap();
        assert_eq!(report.works_scanned, 1);
        assert_eq!(report.batches, 1);
        assert_eq!(
            report.rows_inserted,
            InsertedRows {
                observed: 7,
                inferred: 0,
                unknown: 0
            }
        );
        for (field, slot, lang, value) in [
            ("title", "primary", "en", "Stored title"),
            ("description", "primary", "", "Stored description"),
            ("credits", "author", "", "Source author"),
            ("status", "status", "", "completed"),
            ("cover", "cover", "", "cover.jpg"),
        ] {
            assert_eq!(
                group(&pool, "md-a-old", field).await,
                vec![(
                    slot.into(),
                    lang.into(),
                    value.into(),
                    0,
                    None,
                    "observed".into()
                )]
            );
        }
        assert_eq!(
            group(&pool, "md-a-old", "tags").await,
            vec![
                (
                    "tag".into(),
                    "".into(),
                    "Action".into(),
                    3,
                    Some(json!({"id": "action-id", "group": "genre"})),
                    "observed".into()
                ),
                (
                    "tag".into(),
                    "".into(),
                    "Long Strip".into(),
                    9,
                    Some(json!({"id": "long-strip-id", "group": "format"})),
                    "observed".into()
                ),
            ]
        );
        let other: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM work_metadata_contribution WHERE source_series_id <> 'md-a-old'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(other, 0);
    }

    #[tokio::test]
    async fn suwayomi_attributes_each_group_from_trimmed_pin_aware_values() {
        let pool = pool().await;
        suwayomi_fixture(&pool).await;
        let report = run_pass(&pool, Duration::ZERO).await.unwrap();
        assert_eq!(
            report.rows_inserted,
            InsertedRows {
                observed: 0,
                inferred: 10,
                unknown: 13,
            }
        );
        for (mapping, expected) in [
            (
                "sy-match",
                [
                    "inferred", "inferred", "inferred", "inferred", "unknown", "unknown",
                ],
            ),
            (
                "sy-different",
                [
                    "unknown", "unknown", "unknown", "unknown", "unknown", "unknown",
                ],
            ),
        ] {
            for (field, attribution) in
                ["title", "description", "credits", "status", "tags", "cover"]
                    .into_iter()
                    .zip(expected)
            {
                let rows = group(&pool, mapping, field).await;
                assert!(!rows.is_empty(), "{mapping}/{field}");
                assert!(
                    rows.iter().all(|r| r.5 == attribution),
                    "{mapping}/{field}: {rows:?}"
                );
            }
        }
        assert_eq!(
            group(&pool, "sy-match", "title").await,
            vec![
                (
                    "alias".into(),
                    "".into(),
                    "\u{3000}\nSame title\t\u{00a0}".into(),
                    0,
                    None,
                    "inferred".into()
                ),
                (
                    "primary".into(),
                    "".into(),
                    "\u{3000}\nSame title\t\u{00a0}".into(),
                    0,
                    None,
                    "inferred".into()
                ),
            ]
        );
        assert_eq!(
            group(&pool, "sy-match", "tags").await,
            vec![
                (
                    "tag".into(),
                    "".into(),
                    " Action ".into(),
                    0,
                    None,
                    "unknown".into()
                ),
                (
                    "tag".into(),
                    "".into(),
                    "Drama".into(),
                    2,
                    None,
                    "unknown".into()
                ),
            ]
        );
        assert_eq!(group(&pool, "sy-match", "cover").await[0].2, " /thumb.png ");
        let missing_cache: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM work_metadata_contribution WHERE source_series_id = 'sy-no-cache'",
        ).fetch_one(&pool).await.unwrap();
        assert_eq!(missing_cache, 0);
    }

    #[tokio::test]
    async fn empty_groups_have_markers_and_mangadex_tags_preclude_tag_inference() {
        let pool = pool().await;
        suwayomi_fixture(&pool).await;
        sqlx::query(
            "INSERT INTO work (id, created_at, updated_at) VALUES ('empty-md', now(), now())",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO source_series (id, work_id, source_type, source_key, created_at) \
             VALUES ('empty-md-map', 'empty-md', 'mangadex', 'empty', now())",
        )
        .execute(&pool)
        .await
        .unwrap();
        run_pass(&pool, Duration::ZERO).await.unwrap();
        for field in ["title", "description", "credits", "status", "tags", "cover"] {
            let attribution = if matches!(field, "tags" | "cover") {
                "unknown"
            } else {
                "inferred"
            };
            assert_eq!(
                group(&pool, "sy-empty-map", field).await,
                vec![("".into(), "".into(), "".into(), 0, None, attribution.into())]
            );
            assert_eq!(
                group(&pool, "empty-md-map", field).await,
                vec![("".into(), "".into(), "".into(), 0, None, "observed".into())]
            );
        }
    }

    #[tokio::test]
    async fn barred_groups_stay_unobserved_after_another_sources_projection() {
        use crate::catalog::work_metadata::{self, FieldGroup};

        let pool = pool().await;
        mangadex_fixture(&pool).await;
        sqlx::raw_sql(
            "INSERT INTO suwayomi_series (id, title, status, source_id, updated_at) \
             VALUES (201, 'Allowed title', 'COMPLETED', 'allowed', now()); \
             INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
             VALUES ('allowed-map', 'md', 'suwayomi', 'allowed', '201', now()); \
             INSERT INTO work_metadata_contribution \
                 (source_series_id, field_group, slot, value, attribution, observed_at) \
             VALUES ('allowed-map', 'title', 'primary', 'Allowed title', 'observed', now())",
        ).execute(&pool).await.unwrap();
        work_metadata::set_bar(&pool, "mangadex", FieldGroup::Title, true)
            .await
            .unwrap();
        let title: String = sqlx::query_scalar("SELECT primary_title FROM work WHERE id = 'md'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(title, "Allowed title");
        run_pass(&pool, Duration::ZERO).await.unwrap();
        assert!(group(&pool, "md-a-old", "title").await.is_empty());
        for field in ["description", "credits", "tags", "status", "cover"] {
            assert!(!group(&pool, "md-a-old", field).await.is_empty(), "{field}");
        }
        assert_eq!(
            group(&pool, "allowed-map", "title").await[0].2,
            "Allowed title"
        );
        tokio::time::timeout(Duration::from_secs(5), run_if_needed(&pool))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(super::super::maintenance_flag_present(&pool, FLAG)
            .await
            .unwrap());
        assert!(group(&pool, "md-a-old", "title").await.is_empty());
    }

    #[tokio::test]
    async fn cached_tags_are_unknown_when_admin_tags_take_precedence() {
        let pool = pool().await;
        sqlx::raw_sql(
            "INSERT INTO work (id, created_at, updated_at) VALUES ('curated', now(), now()); \
             INSERT INTO work_tag (work_id, tag, source) VALUES ('curated', 'Drama', 'admin'); \
             INSERT INTO suwayomi_series (id, title, genre, status, source_id, updated_at) \
             VALUES (201, 'Title', '[\"Action\"]', 'ONGOING', 'source', now()); \
             INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
             VALUES ('curated-map', 'curated', 'suwayomi', 'source', '201', now())",
        ).execute(&pool).await.unwrap();
        assert_eq!(
            super::super::work_effective_genres(&pool, "curated").await,
            vec!["Drama"]
        );
        run_pass(&pool, Duration::ZERO).await.unwrap();
        assert_eq!(
            group(&pool, "curated-map", "tags").await,
            vec![(
                "tag".into(),
                "".into(),
                "Action".into(),
                0,
                None,
                "unknown".into(),
            )]
        );
    }

    #[tokio::test]
    async fn tag_inference_requires_the_allowed_cached_genre_union() {
        let pool = pool().await;
        suwayomi_fixture(&pool).await;
        sqlx::query("DELETE FROM work_tag WHERE work_id = 'sy'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            super::super::work_effective_genres(&pool, "sy").await.len(),
            3
        );
        run_pass(&pool, Duration::ZERO).await.unwrap();
        for mapping in ["sy-match", "sy-different"] {
            assert!(group(&pool, mapping, "tags")
                .await
                .iter()
                .all(|row| row.5 == "unknown"));
        }
        sqlx::raw_sql(
            "DELETE FROM work_metadata_contribution WHERE field_group = 'tags'; \
             INSERT INTO source_metadata_bar (source_id, field_group, created_at) \
             VALUES ('source-b', 'tags', now())",
        )
        .execute(&pool)
        .await
        .unwrap();
        run_pass(&pool, Duration::ZERO).await.unwrap();
        assert!(group(&pool, "sy-match", "tags")
            .await
            .iter()
            .all(|row| row.5 == "inferred"));
        assert!(group(&pool, "sy-different", "tags").await.is_empty());
    }

    #[tokio::test]
    async fn second_pass_inserts_nothing_and_preserves_observation_timestamps() {
        let pool = pool().await;
        mangadex_fixture(&pool).await;
        suwayomi_fixture(&pool).await;
        run_pass(&pool, Duration::ZERO).await.unwrap();
        let before = snapshot(&pool, &["work_metadata_contribution"]).await;
        let report = run_pass(&pool, Duration::ZERO).await.unwrap();
        assert_eq!(report.rows_inserted, InsertedRows::default());
        assert_eq!(report.works_scanned, 3);
        assert_eq!(
            before,
            snapshot(&pool, &["work_metadata_contribution"]).await
        );
        assert!(!super::super::maintenance_flag_present(&pool, FLAG)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn any_existing_row_keeps_its_entire_mapping_group_untouched() {
        let pool = pool().await;
        suwayomi_fixture(&pool).await;
        sqlx::raw_sql(
            "INSERT INTO work_metadata_contribution \
             (source_series_id, field_group, slot, lang, value, ord, detail, attribution, observed_at) \
             VALUES ('sy-match', 'title', 'primary', 'ja', 'Live title', 8, '{\"live\":true}', \
                     'observed', '2020-01-01'), \
                    ('sy-match', 'credits', 'author', '', 'Older inferred author', 4, NULL, \
                     'inferred', '2020-01-01')",
        ).execute(&pool).await.unwrap();
        let title = group(&pool, "sy-match", "title").await;
        let credits = group(&pool, "sy-match", "credits").await;
        run_pass(&pool, Duration::ZERO).await.unwrap();
        assert_eq!(title, group(&pool, "sy-match", "title").await);
        assert_eq!(credits, group(&pool, "sy-match", "credits").await);
        let untouched: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM work_metadata_contribution WHERE source_series_id = 'sy-match' \
             AND field_group IN ('title', 'credits') AND observed_at = '2020-01-01'::timestamptz",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(untouched, 2);
        assert!(!group(&pool, "sy-match", "description").await.is_empty());
    }

    #[tokio::test]
    async fn backfill_does_not_change_works_children_sources_or_bars() {
        let pool = pool().await;
        mangadex_fixture(&pool).await;
        suwayomi_fixture(&pool).await;
        let tables = [
            "work",
            "work_alias",
            "work_alias_token",
            "work_tag",
            "work_credit",
            "work_description",
            "work_field_pin",
            "source_metadata_bar",
            "source_series",
            "suwayomi_series",
            "browse_catalogue",
            "feed_series_updates",
            "maintenance_flag",
        ];
        let before = snapshot(&pool, &tables).await;
        run_pass(&pool, Duration::ZERO).await.unwrap();
        assert_eq!(before, snapshot(&pool, &tables).await);
    }

    #[tokio::test]
    async fn works_without_supported_mappings_are_skipped() {
        let pool = pool().await;
        sqlx::raw_sql(
            "INSERT INTO work (id, created_at, updated_at) \
             VALUES ('no-mapping', now(), now()), ('other-type', now(), now()); \
             INSERT INTO source_series (id, work_id, source_type, source_key, created_at) \
             VALUES ('unsupported', 'other-type', 'other', 'key', now())",
        )
        .execute(&pool)
        .await
        .unwrap();
        let report = run_pass(&pool, Duration::ZERO).await.unwrap();
        assert_eq!(report.works_scanned, 0);
        assert_eq!(report.batches, 0);
        assert_eq!(report.rows_inserted, InsertedRows::default());
    }

    #[tokio::test]
    async fn cursor_visits_all_works_in_fifty_work_batches() {
        let pool = pool().await;
        sqlx::raw_sql(
            "INSERT INTO work (id, primary_title, created_at, updated_at) \
             SELECT lpad(n::text, 3, '0'), 'Title ' || n, now(), now() FROM generate_series(1, 103) n; \
             INSERT INTO source_series (id, work_id, source_type, source_key, created_at) \
             SELECT 'mapping-' || id, id, 'mangadex', id, now() FROM work",
        ).execute(&pool).await.unwrap();
        let report = run_pass(&pool, Duration::ZERO).await.unwrap();
        assert_eq!(report.works_scanned, 103);
        assert_eq!(report.batches, 3);
        assert_eq!(
            report.rows_inserted,
            InsertedRows {
                observed: 103 * 6,
                inferred: 0,
                unknown: 0
            }
        );
        let report = run_pass(&pool, Duration::ZERO).await.unwrap();
        assert_eq!(report.rows_inserted, InsertedRows::default());
    }

    #[tokio::test]
    async fn clean_background_pass_sets_flag_and_later_runs_skip() {
        let pool = pool().await;
        mangadex_fixture(&pool).await;
        suwayomi_fixture(&pool).await;
        let report = run_if_needed(&pool).await.unwrap().unwrap();
        assert_eq!(report.rows_inserted.observed, 7);
        assert!(super::super::maintenance_flag_present(&pool, FLAG)
            .await
            .unwrap());
        let before = snapshot(&pool, &["work_metadata_contribution", "maintenance_flag"]).await;
        assert!(run_if_needed(&pool).await.unwrap().is_none());
        assert_eq!(
            before,
            snapshot(&pool, &["work_metadata_contribution", "maintenance_flag"]).await
        );
    }

    #[tokio::test]
    async fn completion_repeats_pass_when_merge_moves_a_mapping_behind_cursor() {
        let pool = pool().await;
        sqlx::raw_sql(
            "INSERT INTO work (id, primary_title, created_at, updated_at) \
             SELECT lpad(n::text, 3, '0'), 'Title ' || n, now(), now() FROM generate_series(1, 51) n; \
             INSERT INTO suwayomi_series (id, title, status, source_id, updated_at) \
             SELECT n, 'Title ' || n, 'ONGOING', 'source', now() FROM generate_series(1, 51) n; \
             INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
             SELECT 'mapping-' || id, id, 'suwayomi', 'source', id, now() FROM work",
        )
        .execute(&pool)
        .await
        .unwrap();
        let points = crate::locks::pause_points::PausePoints::default();
        let first_batch = points.arm("work-metadata-batch-complete");
        let backfill_pool = pool.clone();
        let backfill = tokio::spawn(
            super::super::guards::TEST_PAUSE
                .scope(points, async move { run_if_needed(&backfill_pool).await }),
        );
        tokio::time::timeout(Duration::from_secs(5), first_batch.reached())
            .await
            .unwrap();
        assert!(group(&pool, "mapping-051", "title").await.is_empty());
        super::super::merge_works(&pool, "051", "001")
            .await
            .unwrap();
        assert!(!super::super::maintenance_flag_present(&pool, FLAG)
            .await
            .unwrap());
        first_batch.release();
        let report = tokio::time::timeout(Duration::from_secs(10), backfill)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            report.batches, 2,
            "the second pass revisits the first fifty works"
        );
        assert_eq!(report.works_scanned, 100);
        for field in ["title", "description", "credits", "tags", "status", "cover"] {
            assert!(
                !group(&pool, "mapping-051", field).await.is_empty(),
                "{field}"
            );
        }
        assert!(super::super::maintenance_flag_present(&pool, FLAG)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn long_cached_description_backfills_once() {
        let pool = pool().await;
        suwayomi_fixture(&pool).await;
        sqlx::query(
            "UPDATE suwayomi_series SET description = \
             (SELECT string_agg(md5(n::text), '' ORDER BY n) FROM generate_series(1, 625) n) WHERE id = 101",
        ).execute(&pool).await.unwrap();
        run_pass(&pool, Duration::ZERO).await.unwrap();
        let valid: bool = sqlx::query_scalar(
            "SELECT length(value) = 20000 AND value_key = md5(value) \
             FROM work_metadata_contribution WHERE source_series_id = 'sy-match' \
               AND field_group = 'description' AND slot = 'primary'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(valid);
        let before = snapshot(&pool, &["work_metadata_contribution"]).await;
        assert_eq!(
            run_pass(&pool, Duration::ZERO).await.unwrap().rows_inserted,
            InsertedRows::default()
        );
        assert_eq!(
            snapshot(&pool, &["work_metadata_contribution"]).await,
            before
        );
    }

    #[tokio::test]
    async fn failed_batch_rolls_back_and_does_not_earn_completion_flag() {
        let pool = pool().await;
        mangadex_fixture(&pool).await;
        sqlx::raw_sql(
            "CREATE FUNCTION reject_backfill_row() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN RAISE EXCEPTION 'injected backfill failure'; END; $$; \
             CREATE TRIGGER reject_backfill_row BEFORE INSERT ON work_metadata_contribution \
             FOR EACH ROW EXECUTE FUNCTION reject_backfill_row()",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(run_if_needed(&pool).await.is_err());
        assert!(!super::super::maintenance_flag_present(&pool, FLAG)
            .await
            .unwrap());
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work_metadata_contribution")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
        sqlx::query("DROP TRIGGER reject_backfill_row ON work_metadata_contribution")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            run_if_needed(&pool)
                .await
                .unwrap()
                .unwrap()
                .rows_inserted
                .observed,
            7
        );
        assert!(super::super::maintenance_flag_present(&pool, FLAG)
            .await
            .unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn boot_and_retry_waits_stop_on_shutdown() {
        for delay in [BOOT_DELAY, RETRY_DELAY] {
            let (tx, mut shutdown) = watch::channel(false);
            let waiting = tokio::spawn(async move { wait(delay, &mut shutdown).await });
            tokio::task::yield_now().await;
            tx.send(true).unwrap();
            assert!(!waiting.await.unwrap());
        }
        let (tx, mut shutdown) = watch::channel(true);
        assert!(!wait(BOOT_DELAY, &mut shutdown).await);
        drop(tx);
    }
}
