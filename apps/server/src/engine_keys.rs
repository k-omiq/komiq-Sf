//! Source-native keys captured beside the engine's numeric catalogue cache.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::watch;
use uuid::Uuid;

use crate::db::Database;
use crate::suwayomi::{
    ChapterManga, ChapterOrigin, SuwayomiChapter, SuwayomiClient, SuwayomiManga,
};

#[cfg(test)]
tokio::task_local! { static ENGINE_READ_PAUSE: crate::locks::pause_points::PausePoints; }

/// Key-only bootstrap for unconfigured clients (tests and offline rebuilds).
/// The running server adopts through the marker/fingerprint fence.
pub async fn adopt(pool: &Database) -> Result<Uuid> {
    if let Some(id) = active(pool).await? {
        return Ok(id);
    }
    let mut tx = pool.begin().await?;
    let id = adopt_on(&mut tx).await?;
    tx.commit().await?;
    Ok(id)
}

async fn adopt_on(conn: &mut sqlx::PgConnection) -> Result<Uuid> {
    crate::engine_generation::shared(conn, None).await?;
    if let Some(id) = active_on(conn).await? {
        return Ok(id);
    }
    sqlx::query("LOCK TABLE engine_generation IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *conn)
        .await?;
    let id: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM engine_generation WHERE state = 'active' ORDER BY first_seen, id LIMIT 1",
    )
    .fetch_optional(&mut *conn)
    .await?;
    let id = match id {
        Some(id) => id,
        None => {
            let id = Uuid::new_v4();
            sqlx::query("INSERT INTO engine_generation (id, first_seen, last_seen, state) VALUES ($1, now(), now(), 'active')")
                .bind(id).execute(&mut *conn).await?;
            id
        }
    };
    Ok(id)
}

pub(crate) fn token(value: Option<&str>) -> Result<Option<Uuid>> {
    Ok(value.map(Uuid::parse_str).transpose()?)
}

async fn active(pool: &Database) -> Result<Option<Uuid>> {
    active_on(&mut *pool.acquire().await?).await
}

async fn active_on(conn: &mut sqlx::PgConnection) -> Result<Option<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM engine_generation WHERE state = 'active' ORDER BY first_seen, id LIMIT 1",
    )
    .fetch_optional(conn)
    .await?)
}

/// Validate a complete scan before either half publishes, so a moved chapter
/// cannot leave freshly written metadata behind. Writers repeat this under locks.
pub(crate) async fn verify_snapshot(
    pool: &Database,
    manga: &SuwayomiManga,
    chapters: &[SuwayomiChapter],
) -> Result<()> {
    let result = async {
        let parent: Option<(String,String)> = sqlx::query_as("SELECT m.source_id,m.manga_key FROM engine_manga_key m JOIN engine_generation g ON g.id=m.generation WHERE g.state='active' AND m.manga_id=$1")
            .bind(manga.id).fetch_optional(pool).await?;
        if parent.as_ref().is_some_and(|(source,key)| manga.url.as_ref().is_some_and(|url| source != &manga.source_id || key != url)) {
            return Err(crate::engine_generation::FenceError::BindingMismatch { manga_id: manga.id, chapter_id: None }.into());
        }
        let ids: Vec<_> = chapters.iter().map(|c| c.id).collect();
        let bound: Vec<(i64,i64,String)> = sqlx::query_as("SELECT c.chapter_id,c.manga_id,c.chapter_key FROM engine_chapter_key c JOIN engine_generation g ON g.id=c.generation WHERE g.state='active' AND c.chapter_id=ANY($1)")
            .bind(&ids).fetch_all(pool).await?;
        if let Some(c) = chapters.iter().find(|c| c.manga_id != manga.id
            || c.manga.as_ref().is_some_and(|m| parent.as_ref().is_some_and(|(source,key)| source != &m.source_id || key != &m.url))
            || c.url.as_ref().is_some_and(|url| bound.iter().any(|old| old.0==c.id && (old.1!=c.manga_id || old.2!=*url)))) {
            return Err(crate::engine_generation::FenceError::BindingMismatch { manga_id: manga.id, chapter_id: Some(c.id) }.into());
        }
        Ok(())
    }.await;
    crate::engine_generation::observe_write(pool, result)
}

pub async fn capture_manga(pool: &Database, manga: &SuwayomiManga) -> Result<()> {
    crate::engine_generation::wait(pool).await?;
    let result = capture_manga_inner(pool, manga).await;
    crate::engine_generation::observe_write(pool, result)
}

async fn capture_manga_inner(pool: &Database, manga: &SuwayomiManga) -> Result<()> {
    if manga_unchanged_on(&mut *pool.acquire().await?, manga).await? {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    crate::engine_generation::shared(&mut tx, token(manga.engine_generation.as_deref())?).await?;
    crate::locks::advisory(
        &mut tx,
        &crate::series_cache::series_publication_keys(&manga.id.to_string()),
    )
    .await?;
    capture_manga_on(&mut tx, manga).await?;
    crate::engine_generation::commit_ready(pool)?;
    tx.commit().await?;
    Ok(())
}

async fn manga_unchanged_on(conn: &mut sqlx::PgConnection, manga: &SuwayomiManga) -> Result<bool> {
    let Some(key) = manga.url.as_deref() else {
        return Ok(true);
    };
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM engine_manga_key m \
         JOIN engine_generation g ON g.id = m.generation \
         WHERE g.state = 'active' AND m.manga_id = $1 AND m.source_id = $2 \
           AND m.manga_key = $3 AND m.web_url IS NOT DISTINCT FROM $4 \
           AND m.title IS NOT DISTINCT FROM $5)",
    )
    .bind(manga.id)
    .bind(&manga.source_id)
    .bind(key)
    .bind(&manga.real_url)
    .bind(&manga.title)
    .fetch_one(conn)
    .await?)
}

/// The caller holds the series publication keys until the cache and keys commit.
pub(crate) async fn capture_manga_on(
    conn: &mut sqlx::PgConnection,
    manga: &SuwayomiManga,
) -> Result<()> {
    crate::engine_generation::shared(conn, token(manga.engine_generation.as_deref())?).await?;
    // DB-derived and old cached shapes have no source key. Never erase a live key.
    let Some(key) = manga.url.as_deref() else {
        return Ok(());
    };
    let generation = adopt_on(conn).await?;
    store_manga_on(
        conn,
        generation,
        manga.id,
        &manga.source_id,
        key,
        manga.real_url.as_deref(),
        &manga.title,
    )
    .await
}

async fn store_manga_on(
    conn: &mut sqlx::PgConnection,
    generation: Uuid,
    manga_id: i64,
    source_id: &str,
    key: &str,
    web_url: Option<&str>,
    title: &str,
) -> Result<()> {
    let existing: Option<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT source_id, manga_key, web_url, title FROM engine_manga_key WHERE generation = $1 AND manga_id = $2",
    ).bind(generation).bind(manga_id).fetch_optional(&mut *conn).await?;
    if existing
        .as_ref()
        .is_some_and(|(source, old_key, web, old_title)| {
            source == source_id
                && old_key == key
                && web.as_deref() == web_url
                && old_title.as_deref() == Some(title)
        })
    {
        return Ok(());
    }
    if existing
        .as_ref()
        .is_some_and(|(source, old_key, _, _)| source != source_id || old_key != key)
    {
        return Err(crate::engine_generation::FenceError::BindingMismatch {
            manga_id,
            chapter_id: None,
        }
        .into());
    }
    sqlx::query(
        "INSERT INTO engine_manga_key (generation, manga_id, source_id, manga_key, web_url, title, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, now()) \
         ON CONFLICT (generation, manga_id) DO UPDATE SET \
           source_id = excluded.source_id, manga_key = excluded.manga_key, \
           web_url = excluded.web_url, title = excluded.title, updated_at = excluded.updated_at \
         WHERE (engine_manga_key.source_id, engine_manga_key.manga_key, engine_manga_key.web_url, engine_manga_key.title) \
           IS DISTINCT FROM (excluded.source_id, excluded.manga_key, excluded.web_url, excluded.title)",
    ).bind(generation).bind(manga_id).bind(source_id).bind(key).bind(web_url).bind(title)
        .execute(conn).await?;
    Ok(())
}

#[derive(Debug, sqlx::FromRow, PartialEq, Eq)]
struct ChapterKey {
    chapter_id: i64,
    manga_id: i64,
    source_order: i32,
    chapter_key: String,
    web_url: Option<String>,
}

#[cfg(test)]
async fn capture_chapters(
    pool: &Database,
    manga_id: i64,
    chapters: &[SuwayomiChapter],
) -> Result<()> {
    if chapters_unchanged_on(&mut *pool.acquire().await?, manga_id, chapters).await? {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    crate::locks::advisory(
        &mut tx,
        &crate::series_cache::series_publication_keys(&manga_id.to_string()),
    )
    .await?;
    capture_chapters_on(&mut tx, manga_id, chapters).await?;
    tx.commit().await?;
    Ok(())
}

/// Remember which publications were committed before requesting engine data.
/// Visibility also catches writers already running when the request starts, whose
/// timestamps can precede the request even though they commit while it is in flight.
async fn publication_snapshot(pool: &Database) -> Result<String> {
    Ok(sqlx::query_scalar("SELECT pg_current_snapshot()::text")
        .fetch_one(pool)
        .await?)
}

async fn publication_changed_on(
    conn: &mut sqlx::PgConnection,
    manga_id: i64,
    snapshot: &str,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM suwayomi_series WHERE id = $1 \
           AND NOT pg_visible_in_snapshot(xmin::text::xid8, $2::pg_snapshot)) \
         OR EXISTS (SELECT 1 FROM engine_manga_key m JOIN engine_generation g ON g.id = m.generation \
           WHERE g.state = 'active' AND m.manga_id = $1 \
           AND NOT pg_visible_in_snapshot(m.xmin::text::xid8, $2::pg_snapshot))",
    ).bind(manga_id).bind(snapshot).fetch_one(conn).await?)
}

/// Publish the engine's parent/list pair only if no newer cache or key capture
/// committed while the request was in flight. Stale rebuildable keys may be dropped.
async fn capture_snapshot(
    pool: &Database,
    manga: &SuwayomiManga,
    chapters: &[SuwayomiChapter],
    snapshot: &str,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    crate::engine_generation::shared(&mut tx, token(manga.engine_generation.as_deref())?).await?;
    crate::locks::advisory(
        &mut tx,
        &crate::series_cache::series_publication_keys(&manga.id.to_string()),
    )
    .await?;
    if publication_changed_on(&mut tx, manga.id, snapshot).await? {
        tx.rollback().await?;
        return Ok(());
    }
    capture_manga_on(&mut tx, manga).await?;
    capture_chapters_on(&mut tx, manga.id, chapters).await?;
    sqlx::query(
        "UPDATE engine_manga_key m SET backfilled_at = s.updated_at \
         FROM suwayomi_series s, engine_generation g \
         WHERE m.manga_id = $1 AND s.id = m.manga_id AND g.id = m.generation \
           AND g.state = 'active' AND m.backfilled_at IS DISTINCT FROM s.updated_at",
    )
    .bind(manga.id)
    .execute(&mut *tx)
    .await?;
    crate::engine_generation::commit_ready(pool)?;
    tx.commit().await?;
    Ok(())
}

fn chapter_keys(manga_id: i64, chapters: &[SuwayomiChapter]) -> Vec<ChapterKey> {
    chapters
        .iter()
        .filter_map(|c| {
            Some(ChapterKey {
                chapter_id: c.id,
                manga_id,
                source_order: c.source_order?,
                chapter_key: c.url.clone()?,
                web_url: c.real_url.clone(),
            })
        })
        .collect()
}

async fn existing_chapters_on(
    conn: &mut sqlx::PgConnection,
    generation: Uuid,
    manga_id: i64,
) -> Result<BTreeMap<i64, ChapterKey>> {
    let existing: Vec<ChapterKey> = sqlx::query_as(
        "SELECT chapter_id, manga_id, source_order, chapter_key, web_url FROM engine_chapter_key \
         WHERE generation = $1 AND manga_id = $2",
    )
    .bind(generation)
    .bind(manga_id)
    .fetch_all(conn)
    .await?;
    Ok(existing.into_iter().map(|c| (c.chapter_id, c)).collect())
}

pub(crate) async fn chapters_unchanged_on(
    conn: &mut sqlx::PgConnection,
    manga_id: i64,
    chapters: &[SuwayomiChapter],
) -> Result<bool> {
    if let Some(manga) = chapters.iter().find_map(|c| c.manga.as_ref()) {
        if !chapter_manga_unchanged_on(conn, manga_id, manga).await? {
            return Ok(false);
        }
    }
    let incoming = chapter_keys(manga_id, chapters);
    let Some(generation) = active_on(conn).await? else {
        return Ok(incoming.is_empty());
    };
    let existing = existing_chapters_on(conn, generation, manga_id).await?;
    let retained: BTreeSet<_> = chapters.iter().map(|c| c.id).collect();
    Ok(existing.keys().all(|id| retained.contains(id))
        && incoming
            .iter()
            .all(|c| existing.get(&c.chapter_id) == Some(c)))
}

async fn chapter_manga_unchanged_on(
    conn: &mut sqlx::PgConnection,
    manga_id: i64,
    manga: &ChapterManga,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM engine_manga_key m JOIN engine_generation g ON g.id = m.generation \
         WHERE g.state = 'active' AND m.manga_id = $1 AND m.source_id = $2 AND m.manga_key = $3 \
           AND m.web_url IS NOT DISTINCT FROM $4 AND m.title IS NOT DISTINCT FROM $5)",
    ).bind(manga_id).bind(&manga.source_id).bind(&manga.url).bind(&manga.real_url)
        .bind(&manga.title).fetch_one(conn).await?)
}

/// Authoritative replacement: remove absent ids even for an empty list. Retained
/// display-only rows lack source keys, so they cannot erase a captured binding.
pub(crate) async fn capture_chapters_on(
    conn: &mut sqlx::PgConnection,
    manga_id: i64,
    chapters: &[SuwayomiChapter],
) -> Result<()> {
    crate::engine_generation::shared(
        conn,
        chapters
            .first()
            .map(|c| token(c.engine_generation.as_deref()))
            .transpose()?
            .flatten(),
    )
    .await?;
    if let Some(c) = chapters.iter().find(|c| c.manga_id != manga_id) {
        return Err(crate::engine_generation::FenceError::BindingMismatch {
            manga_id,
            chapter_id: Some(c.id),
        }
        .into());
    }
    if let Some(manga) = chapters.iter().find_map(|c| c.manga.as_ref()) {
        if chapters
            .iter()
            .filter_map(|c| c.manga.as_ref())
            .any(|m| m.source_id != manga.source_id || m.url != manga.url)
        {
            return Err(crate::engine_generation::FenceError::BindingMismatch {
                manga_id,
                chapter_id: chapters.first().map(|c| c.id),
            }
            .into());
        }
        let generation = adopt_on(conn).await?;
        store_manga_on(
            conn,
            generation,
            manga_id,
            &manga.source_id,
            &manga.url,
            manga.real_url.as_deref(),
            &manga.title,
        )
        .await?;
    }
    let incoming = chapter_keys(manga_id, chapters);
    let generation = if incoming.is_empty() {
        let Some(generation) = active_on(conn).await? else {
            return Ok(());
        };
        generation
    } else {
        adopt_on(conn).await?
    };
    let retained: BTreeSet<_> = chapters.iter().map(|c| c.id).collect();
    store_chapters_on(conn, generation, manga_id, incoming, Some(&retained)).await
}

async fn store_chapters_on(
    conn: &mut sqlx::PgConnection,
    generation: Uuid,
    manga_id: i64,
    incoming: Vec<ChapterKey>,
    retained: Option<&BTreeSet<i64>>,
) -> Result<()> {
    let ids: Vec<_> = incoming.iter().map(|c| c.chapter_id).collect();
    let bound: Vec<(i64, i64, String)> = sqlx::query_as("SELECT chapter_id,manga_id,chapter_key FROM engine_chapter_key WHERE generation=$1 AND chapter_id=ANY($2)")
        .bind(generation).bind(&ids).fetch_all(&mut *conn).await?;
    if let Some(c) = incoming.iter().find(|c| {
        bound
            .iter()
            .any(|old| old.0 == c.chapter_id && (old.1 != c.manga_id || old.2 != c.chapter_key))
    }) {
        return Err(crate::engine_generation::FenceError::BindingMismatch {
            manga_id,
            chapter_id: Some(c.chapter_id),
        }
        .into());
    }
    let existing = existing_chapters_on(conn, generation, manga_id).await?;
    let removed: Vec<_> = existing
        .values()
        .filter(|old| {
            retained.is_some_and(|ids| !ids.contains(&old.chapter_id))
                || incoming
                    .iter()
                    .any(|c| c.chapter_key == old.chapter_key && c.chapter_id != old.chapter_id)
        })
        .map(|c| c.chapter_id)
        .collect();
    let changed: Vec<_> = incoming
        .into_iter()
        .filter(|c| existing.get(&c.chapter_id) != Some(c))
        .collect();
    if changed.is_empty() && removed.is_empty() {
        return Ok(());
    }
    // Release removed/reassigned coordinates before inserting replacement ids,
    // including when two retained chapters exchange their source keys.
    let deleted: Vec<_> = removed
        .into_iter()
        .chain(changed.iter().map(|c| c.chapter_id))
        .collect();
    sqlx::query("DELETE FROM engine_chapter_key WHERE generation = $1 AND chapter_id = ANY($2)")
        .bind(generation)
        .bind(&deleted)
        .execute(&mut *conn)
        .await?;
    for chunk in changed.chunks(100) {
        let mut q = sqlx::QueryBuilder::<sqlx::Postgres>::new(
            "INSERT INTO engine_chapter_key (generation, chapter_id, manga_id, source_order, chapter_key, web_url, updated_at) ",
        );
        q.push_values(chunk, |mut b, c| {
            b.push_bind(generation)
                .push_bind(c.chapter_id)
                .push_bind(c.manga_id)
                .push_bind(c.source_order)
                .push_bind(&c.chapter_key)
                .push_bind(&c.web_url)
                .push("now()");
        });
        q.push(" ON CONFLICT (generation, chapter_id) DO UPDATE SET \
            manga_id = excluded.manga_id, source_order = excluded.source_order, \
            chapter_key = excluded.chapter_key, web_url = excluded.web_url, updated_at = excluded.updated_at \
            WHERE (engine_chapter_key.manga_id, engine_chapter_key.source_order, engine_chapter_key.chapter_key, engine_chapter_key.web_url) \
              IS DISTINCT FROM (excluded.manga_id, excluded.source_order, excluded.chapter_key, excluded.web_url)");
        q.build().execute(&mut *conn).await?;
    }
    // A parent row version represents every key publication, including deletions
    // and empty replacements, so in-flight engine reads cannot resurrect old keys.
    sqlx::query("UPDATE engine_manga_key SET updated_at = clock_timestamp(), backfilled_at = NULL WHERE generation = $1 AND manga_id = $2")
        .bind(generation).bind(manga_id).execute(&mut *conn).await?;
    Ok(())
}

pub async fn chapter_origin(
    pool: &Database,
    engine: &SuwayomiClient,
    chapter_id: i64,
) -> Result<Option<ChapterOrigin>> {
    let result = chapter_origin_inner(pool, engine, chapter_id).await;
    crate::engine_generation::observe_write(pool, result)
}

async fn chapter_origin_inner(
    pool: &Database,
    engine: &SuwayomiClient,
    chapter_id: i64,
) -> Result<Option<ChapterOrigin>> {
    crate::engine_generation::wait(pool).await?;
    let read_generation = engine.read_generation();
    if let Some(stored) = stored_origin_on(&mut *pool.acquire().await?, chapter_id).await? {
        engine.validate_read_generation(read_generation).await?;
        return Ok(Some(stored));
    }
    let snapshot = publication_snapshot(pool).await?;
    let generation = engine.fence.as_ref().and_then(|f| f.generation());
    let origin = engine.chapter_origin(chapter_id).await?;
    #[cfg(test)]
    if let Ok(points) = ENGINE_READ_PAUSE.try_with(Clone::clone) {
        points.hit("engine-read").await;
    }
    if let Some(o) = &origin {
        let mut tx = pool.begin().await?;
        crate::engine_generation::shared(&mut tx, generation).await?;
        crate::locks::advisory(
            &mut tx,
            &crate::series_cache::series_publication_keys(&o.manga_id.to_string()),
        )
        .await?;
        let stored = stored_origin_on(&mut tx, chapter_id).await?;
        if stored.is_some() || publication_changed_on(&mut tx, o.manga_id, &snapshot).await? {
            tx.rollback().await?;
            return Ok(stored);
        }
        let generation = adopt_on(&mut tx).await?;
        store_manga_on(
            &mut tx,
            generation,
            o.manga_id,
            &o.source_id,
            &o.manga_url,
            o.manga_web_url.as_deref(),
            &o.manga_title,
        )
        .await?;
        // A single-chapter miss is a patch, never an authoritative full list.
        store_chapters_on(
            &mut tx,
            generation,
            o.manga_id,
            vec![ChapterKey {
                chapter_id,
                manga_id: o.manga_id,
                source_order: o.source_order,
                chapter_key: o.chapter_url.clone(),
                web_url: o.chapter_web_url.clone(),
            }],
            None,
        )
        .await?;
        crate::engine_generation::commit_ready(pool)?;
        tx.commit().await?;
    }
    Ok(origin)
}

async fn stored_origin_on(
    conn: &mut sqlx::PgConnection,
    chapter_id: i64,
) -> Result<Option<ChapterOrigin>> {
    let stored: Option<ChapterOrigin> = sqlx::query_as(
        "SELECT c.manga_id, m.source_id, m.manga_key AS manga_url, COALESCE(s.title, m.title) AS manga_title, \
                c.chapter_key AS chapter_url, c.web_url AS chapter_web_url, \
                m.web_url AS manga_web_url, c.source_order \
         FROM engine_generation g \
         JOIN engine_chapter_key c ON c.generation = g.id \
         JOIN engine_manga_key m ON m.generation = c.generation AND m.manga_id = c.manga_id \
         LEFT JOIN suwayomi_series s ON s.id = c.manga_id \
         WHERE g.state = 'active' AND c.chapter_id = $1 \
           AND (s.title IS NOT NULL OR m.title IS NOT NULL)",
    )
    .bind(chapter_id)
    .fetch_optional(conn)
    .await?;
    Ok(stored)
}

const BACKFILL_PER_TICK: i64 = 4;

pub fn spawn(pool: Database, engine: SuwayomiClient, shutdown: watch::Receiver<bool>) {
    tokio::spawn(crate::task::supervise(
        "engine-key-backfill",
        Duration::from_secs(30),
        shutdown.clone(),
        move || run_loop(pool.clone(), engine.clone(), shutdown.clone()),
    ));
}

async fn run_loop(pool: Database, engine: SuwayomiClient, mut shutdown: watch::Receiver<bool>) {
    let mut cursor = None;
    let pace = Duration::from_secs(crate::scanner::SOURCE_TICK_SECONDS);
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + pace / 2, pace);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if *shutdown.borrow() {
            return;
        }
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = ticker.tick() => {}
        }
        tokio::select! {
            _ = shutdown.changed() => return,
            result = backfill_tick(&pool, &engine, &mut cursor) => {
                if let Err(e) = result {
                    let e = crate::db::observed_database_error(e);
                    tracing::warn!(error = %e, "engine key backfill failed");
                }
            }
        }
    }
}

async fn backfill_tick(
    pool: &Database,
    engine: &SuwayomiClient,
    cursor: &mut Option<i64>,
) -> Result<()> {
    crate::engine_generation::wait(pool).await?;
    let generation = adopt(pool).await?;
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT s.id FROM suwayomi_series s WHERE ($2::bigint IS NULL OR s.id > $2) AND ( \
           NOT EXISTS (SELECT 1 FROM engine_manga_key m WHERE m.generation = $1 AND m.manga_id = s.id) \
           OR (NOT EXISTS (SELECT 1 FROM engine_manga_key m WHERE m.generation = $1 \
                AND m.manga_id = s.id AND m.backfilled_at = s.updated_at) \
             AND EXISTS (SELECT 1 FROM suwayomi_chapter c WHERE c.manga_id = s.id AND NOT EXISTS ( \
               SELECT 1 FROM engine_chapter_key k WHERE k.generation = $1 AND k.chapter_id = c.id)))) \
         ORDER BY s.id LIMIT $3",
    ).bind(generation).bind(*cursor).bind(BACKFILL_PER_TICK).fetch_all(pool).await?;
    if ids.is_empty() {
        *cursor = None;
    }
    for id in ids {
        // Advance past missing/broken engine rows too, so one cannot starve the rest.
        *cursor = Some(id);
        let snapshot = publication_snapshot(pool).await?;
        match engine.cached_series_and_chapters(id).await {
            Ok((m, chapters)) => {
                #[cfg(test)]
                if let Ok(points) = ENGINE_READ_PAUSE.try_with(Clone::clone) {
                    points.hit("engine-read").await;
                }
                let result = capture_snapshot(pool, &m, &chapters, &snapshot).await;
                crate::engine_generation::observe_write(pool, result)?;
            }
            Err(e) => tracing::debug!(manga_id = id, error = %e, "engine key backfill deferred"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::TestPoolOptions;
    use crate::suwayomi::testsrv;
    use serde_json::json;
    use std::sync::atomic::Ordering;

    async fn pool() -> Database {
        TestPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap()
    }

    fn manga(id: i64) -> SuwayomiManga {
        serde_json::from_value(manga_json(id)).unwrap()
    }

    fn manga_json(id: i64) -> serde_json::Value {
        json!({"id": id, "title": "Series", "url": "/manga/7", "realUrl": "https://example.org/manga/7",
            "status": "ONGOING", "inLibrary": false, "sourceId": "source", "source": {"lang": "en"}})
    }

    fn chapter(id: i64, manga_id: i64) -> SuwayomiChapter {
        serde_json::from_value(chapter_json(id, manga_id)).unwrap()
    }

    fn chapter_json(id: i64, manga_id: i64) -> serde_json::Value {
        json!({"id": id, "mangaId": manga_id, "url": format!("/chapter/{id}"),
            "realUrl": format!("https://example.org/chapter/{id}"), "sourceOrder": 3,
            "name": "Chapter", "chapterNumber": 1.0, "isRead": false,
            "isBookmarked": false, "isDownloaded": false, "lastPageRead": 0, "pageCount": 10})
    }

    fn client(engine: &testsrv::TestOrigin) -> SuwayomiClient {
        SuwayomiClient::new(
            engine.base_url.clone(),
            Some(engine.base_url.clone()),
            Some("source".into()),
        )
    }

    async fn stamps(pool: &Database) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT xmin::text, updated_at::text FROM engine_manga_key \
             UNION ALL SELECT xmin::text, updated_at::text FROM engine_chapter_key ORDER BY 1, 2",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn migration_applies_and_adoption_is_idempotent() {
        let pool = pool().await;
        crate::db::MIGRATOR_PG.run(pool.write()).await.unwrap();
        let id = adopt(&pool).await.unwrap();
        let first: (Uuid, String, String) =
            sqlx::query_as("SELECT id, first_seen::text, last_seen::text FROM engine_generation")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(adopt(&pool).await.unwrap(), id);
        let second: (Uuid, String, String) =
            sqlx::query_as("SELECT id, first_seen::text, last_seen::text FROM engine_generation")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(first, second);
        assert!(
            sqlx::query("UPDATE engine_generation SET state = 'unknown'")
                .execute(&pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("INSERT INTO engine_manga_key (generation, manga_id, source_id, manga_key, updated_at) VALUES ($1, 1, 's', 'm', now())")
                .bind(Uuid::new_v4())
                .execute(&pool)
                .await
                .is_err()
        );
        sqlx::query("UPDATE engine_generation SET state = 'detached'")
            .execute(&pool)
            .await
            .unwrap();
        assert_ne!(adopt(&pool).await.unwrap(), id);
    }

    #[tokio::test]
    async fn cache_capture_skips_unchanged_keys_and_updates_only_changed_values() {
        let pool = pool().await;
        let mut m = manga(7);
        let mut chapters = vec![chapter(8, 7), chapter(9, 7)];
        crate::series_cache::put_series(&pool, &m).await.unwrap();
        crate::series_cache::put_chapters(&pool, 7, &chapters)
            .await
            .unwrap();
        let first = stamps(&pool).await;
        assert_eq!(first.len(), 3);
        crate::series_cache::put_series(&pool, &m).await.unwrap();
        crate::series_cache::put_chapters(&pool, 7, &chapters)
            .await
            .unwrap();
        assert_eq!(stamps(&pool).await, first);

        // Only the key changes; the chapter display digest still matches.
        chapters[0].source_order = Some(4);
        crate::series_cache::put_chapters(&pool, 7, &chapters)
            .await
            .unwrap();
        let changed = stamps(&pool).await;
        assert_eq!(changed.iter().filter(|s| !first.contains(s)).count(), 2);
        let order: i32 =
            sqlx::query_scalar("SELECT source_order FROM engine_chapter_key WHERE chapter_id = 8")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(order, 4);
        m.real_url = None;
        capture_manga(&pool, &m).await.unwrap();
        let web: Option<String> = sqlx::query_scalar("SELECT web_url FROM engine_manga_key")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(web, None);
        let changed = stamps(&pool).await;
        capture_manga(&pool, &m).await.unwrap();
        assert_eq!(stamps(&pool).await, changed);

        // A title-only change must update the parent without invalidating chapters.
        m.title = "Updated engine title".into();
        capture_manga(&pool, &m).await.unwrap();
        let title: Option<String> = sqlx::query_scalar("SELECT title FROM engine_manga_key")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(title.as_deref(), Some("Updated engine title"));
        capture_snapshot(
            &pool,
            &m,
            &chapters,
            &publication_snapshot(&pool).await.unwrap(),
        )
        .await
        .unwrap();
        let titled = stamps(&pool).await;
        assert_eq!(titled.len(), 3);
        assert_eq!(titled.iter().filter(|s| !changed.contains(s)).count(), 1);
        capture_manga(&pool, &m).await.unwrap();
        capture_snapshot(
            &pool,
            &m,
            &chapters,
            &publication_snapshot(&pool).await.unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(stamps(&pool).await, titled);

        // Reconstructed display rows cannot replace stored keys with missing fields.
        m.url = None;
        chapters[0].url = None;
        capture_manga(&pool, &m).await.unwrap();
        capture_chapters(&pool, 7, &chapters).await.unwrap();
        assert_eq!(stamps(&pool).await, titled);
        sqlx::query("DELETE FROM engine_chapter_key")
            .execute(&pool)
            .await
            .unwrap();
        chapters[0].url = Some("/chapter/8".into());
        crate::series_cache::put_chapters(&pool, 7, &chapters)
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM engine_chapter_key")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2, "unchanged display rows still fill missing keys");
    }

    #[tokio::test]
    async fn chapter_origin_uses_stored_keys_without_an_engine_call() {
        let pool = pool().await;
        crate::series_cache::put_series(&pool, &manga(7))
            .await
            .unwrap();
        sqlx::query("UPDATE suwayomi_series SET title = 'Cached title' WHERE id = 7")
            .execute(&pool)
            .await
            .unwrap();
        capture_chapters(&pool, 7, &[chapter(8, 7)]).await.unwrap();
        let engine = testsrv::spawn_error(503, "engine unavailable").await;
        let origin = chapter_origin(&pool, &client(&engine), 8)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(origin.manga_title, "Cached title");
        assert_eq!(origin.source_id, "source");
        assert_eq!(origin.manga_url, "/manga/7");
        assert_eq!(origin.chapter_url, "/chapter/8");
        assert_eq!(
            origin.manga_web_url.as_deref(),
            Some("https://example.org/manga/7")
        );
        assert_eq!(
            origin.chapter_web_url.as_deref(),
            Some("https://example.org/chapter/8")
        );
        assert_eq!(engine.hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn chapter_origin_miss_stores_both_key_rows_and_web_urls() {
        let pool = pool().await;
        let engine = testsrv::spawn_status(200, "application/json", json!({"data": {"chapters": {"nodes": [{
            "url": "/chapter/8", "realUrl": "https://example.org/chapter/8", "sourceOrder": 3, "mangaId": 7,
            "manga": {"url": "/manga/7", "realUrl": "https://example.org/manga/7", "title": "Series", "sourceId": "source"}
        }]}}}).to_string().into_bytes(), Duration::ZERO).await;
        let client = client(&engine);
        let mut m = manga(7);
        m.url = None;
        crate::series_cache::put_series(&pool, &m).await.unwrap();
        let fetched = chapter_origin(&pool, &client, 8).await.unwrap();
        assert_eq!(fetched, chapter_origin(&pool, &client, 8).await.unwrap());
        assert_eq!(engine.hits.load(Ordering::SeqCst), 1);
        sqlx::query("DELETE FROM engine_manga_key")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(fetched, chapter_origin(&pool, &client, 8).await.unwrap());
        assert_eq!(
            engine.hits.load(Ordering::SeqCst),
            2,
            "both key rows are required"
        );
        assert_eq!(stamps(&pool).await.len(), 2);
    }

    #[tokio::test]
    async fn chapter_only_refresh_refuses_moved_parent_for_live_and_fallback_lists() {
        for fallback in [false, true] {
            let pool = pool().await;
            crate::series_cache::put_series(&pool, &manga(7))
                .await
                .unwrap();
            crate::series_cache::put_chapters(&pool, 7, &[chapter(8, 7)])
                .await
                .unwrap();
            // Keep every chapter field identical: the parent alone must defeat
            // the unchanged pre-check and replace the old-source identity.
            let mut c = chapter_json(8, 7);
            c["manga"] = json!({"sourceId": "new-source", "url": "/new/manga/7",
                "realUrl": "https://new.example/manga/7", "title": "New title"});
            let app = axum::Router::new().route(
                "/api/graphql",
                axum::routing::post(move |axum::Json(request): axum::Json<serde_json::Value>| {
                    let c = c.clone();
                    async move {
                        let query = request["query"].as_str().unwrap();
                        assert!(query.contains("manga { sourceId url realUrl title }"));
                        axum::Json(if query.contains("mutation FC") {
                            if fallback {
                                json!({"errors": [{"message": "source unavailable"}]})
                            } else {
                                json!({"data": {"fetchMangaAndChapters": {"chapters": [c]}}})
                            }
                        } else {
                            json!({"data": {"chapters": {"nodes": [c]}}})
                        })
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let engine = SuwayomiClient::new(url.clone(), Some(url), Some("source".into()));
            let chapters = engine.chapters(7).await.unwrap();
            let before = stamps(&pool).await;
            assert!(crate::series_cache::put_chapters(&pool, 7, &chapters)
                .await
                .is_err());
            assert_eq!(
                stamps(&pool).await,
                before,
                "a moved parent must leave both key and cache rows alone"
            );
            let origin = chapter_origin(&pool, &engine, 8).await.unwrap().unwrap();
            assert_eq!(origin.source_id, "source");
            assert_eq!(origin.manga_url, "/manga/7");
            server.abort();
        }
    }

    #[tokio::test]
    async fn backfill_drops_engine_reads_older_than_cache_or_key_publications() {
        for replacement in [0, 1, 2] {
            let pool = pool().await;
            crate::series_cache::put_series(&pool, &manga(7))
                .await
                .unwrap();
            let mut cached = chapter(8, 7);
            cached.url = None;
            crate::series_cache::put_chapters(&pool, 7, &[cached])
                .await
                .unwrap();
            let engine = testsrv::spawn_status(
                200,
                "application/json",
                json!({"data": {
                    "manga": manga_json(7), "chapters": {"nodes": [chapter_json(8, 7)]}
                }})
                .to_string()
                .into_bytes(),
                Duration::ZERO,
            )
            .await;
            let points = crate::locks::pause_points::PausePoints::default();
            let held = points.arm("engine-read");
            let writer = pool.clone();
            let engine_client = client(&engine);
            let backfill = tokio::spawn(ENGINE_READ_PAUSE.scope(points, async move {
                backfill_tick(&writer, &engine_client, &mut None).await
            }));
            tokio::time::timeout(Duration::from_secs(30), held.reached())
                .await
                .unwrap();
            match replacement {
                0 => crate::series_cache::put_chapters(&pool, 7, &[chapter(9, 7)])
                    .await
                    .unwrap(),
                1 => crate::series_cache::put_chapters(&pool, 7, &[])
                    .await
                    .unwrap(),
                _ => capture_chapters(&pool, 7, &[chapter(9, 7)]).await.unwrap(),
            }
            let before = stamps(&pool).await;
            held.release();
            backfill.await.unwrap().unwrap();
            assert_eq!(
                stamps(&pool).await,
                before,
                "stale backfill must not publish or mark completion"
            );
            let ids: Vec<i64> =
                sqlx::query_scalar("SELECT chapter_id FROM engine_chapter_key ORDER BY chapter_id")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            assert_eq!(ids, if replacement == 1 { vec![] } else { vec![9] });
            let cached: Vec<i64> =
                sqlx::query_scalar("SELECT id FROM suwayomi_chapter ORDER BY id")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            assert_eq!(
                cached,
                match replacement {
                    0 => vec![9],
                    1 => vec![],
                    _ => vec![8],
                }
            );
        }
    }

    #[tokio::test]
    async fn capture_drops_reads_started_while_another_publication_was_uncommitted() {
        let pool = pool().await;
        let m = manga(7);
        crate::series_cache::put_series(&pool, &m).await.unwrap();
        capture_chapters(&pool, 7, &[chapter(8, 7)]).await.unwrap();
        let mut newer = pool.begin().await.unwrap();
        crate::locks::advisory(
            &mut newer,
            &crate::series_cache::series_publication_keys("7"),
        )
        .await
        .unwrap();
        capture_chapters_on(&mut newer, 7, &[chapter(9, 7)])
            .await
            .unwrap();
        // The writer's timestamp already exists, but its commit is newer than
        // this request's view. Comparing timestamps alone would accept the read.
        let snapshot = publication_snapshot(&pool).await.unwrap();
        newer.commit().await.unwrap();
        let before = stamps(&pool).await;
        capture_snapshot(&pool, &m, &[chapter(8, 7)], &snapshot)
            .await
            .unwrap();
        assert_eq!(stamps(&pool).await, before);
        let ids: Vec<i64> = sqlx::query_scalar("SELECT chapter_id FROM engine_chapter_key")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(ids, vec![9]);
    }

    #[tokio::test]
    async fn chapter_origin_rechecks_and_drops_stale_misses_after_locking() {
        for retained in [false, true] {
            let pool = pool().await;
            crate::series_cache::put_series(&pool, &manga(7))
                .await
                .unwrap();
            let engine = testsrv::spawn_status(
                200,
                "application/json",
                json!({"data": {"chapters": {"nodes": [{
                    "url": "/old/chapter/8", "sourceOrder": 3, "mangaId": 7,
                    "manga": {"url": "/manga/7", "title": "Series", "sourceId": "source"}
                }]}}})
                .to_string()
                .into_bytes(),
                Duration::ZERO,
            )
            .await;
            let points = crate::locks::pause_points::PausePoints::default();
            let held = points.arm("engine-read");
            let writer = pool.clone();
            let engine_client = client(&engine);
            let lookup = tokio::spawn(ENGINE_READ_PAUSE.scope(points, async move {
                chapter_origin(&writer, &engine_client, 8).await
            }));
            tokio::time::timeout(Duration::from_secs(30), held.reached())
                .await
                .unwrap();
            let chapters = if retained {
                vec![chapter(8, 7)]
            } else {
                vec![chapter(9, 7)]
            };
            crate::series_cache::put_chapters(&pool, 7, &chapters)
                .await
                .unwrap();
            let before = stamps(&pool).await;
            held.release();
            let result = lookup.await.unwrap().unwrap();
            assert_eq!(
                result.as_ref().map(|o| o.chapter_url.as_str()),
                retained.then_some("/chapter/8")
            );
            assert_eq!(stamps(&pool).await, before);
        }
    }

    #[tokio::test]
    async fn chapter_origin_reassigned_key_displaces_only_its_old_owner() {
        let pool = pool().await;
        crate::series_cache::put_series(&pool, &manga(7))
            .await
            .unwrap();
        crate::series_cache::put_chapters(&pool, 7, &[chapter(8, 7), chapter(10, 7)])
            .await
            .unwrap();
        let engine = testsrv::spawn_status(200, "application/json", json!({"data": {"chapters": {"nodes": [{
            "url": "/chapter/8", "sourceOrder": 4, "mangaId": 7,
            "manga": {"url": "/manga/7", "realUrl": "https://example.org/manga/7", "title": "Series", "sourceId": "source"}
        }]}}}).to_string().into_bytes(), Duration::ZERO).await;
        let origin = chapter_origin(&pool, &client(&engine), 9)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(origin.chapter_url, "/chapter/8");
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT chapter_id, chapter_key FROM engine_chapter_key ORDER BY chapter_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![(9, "/chapter/8".into()), (10, "/chapter/10".into())]
        );
        assert_eq!(
            chapter_origin(&pool, &client(&engine), 9).await.unwrap(),
            Some(origin)
        );
        assert_eq!(engine.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn backfill_covers_absent_cached_chapters_until_the_cache_revision_changes() {
        for empty in [false, true] {
            let pool = pool().await;
            crate::series_cache::put_series(&pool, &manga(7))
                .await
                .unwrap();
            let mut c = chapter(8, 7);
            c.url = None;
            crate::series_cache::put_chapters(&pool, 7, &[c.clone()])
                .await
                .unwrap();
            let engine = testsrv::spawn_status(200, "application/json", json!({"data": {
                "manga": manga_json(7), "chapters": {"nodes": if empty { vec![] } else { vec![chapter_json(9, 7)] }}
            }}).to_string().into_bytes(), Duration::ZERO).await;
            let mut cursor = None;
            for _ in 0..5 {
                backfill_tick(&pool, &client(&engine), &mut cursor)
                    .await
                    .unwrap();
            }
            assert_eq!(
                engine.hits.load(Ordering::SeqCst),
                1,
                "successful absence must leave the pending set"
            );
            let cached: Vec<i64> =
                sqlx::query_scalar("SELECT id FROM suwayomi_chapter ORDER BY id")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            assert_eq!(
                cached,
                vec![8],
                "key backfill does not replace display rows"
            );
            let completed: bool = sqlx::query_scalar("SELECT m.backfilled_at = s.updated_at FROM engine_manga_key m JOIN suwayomi_series s ON s.id = m.manga_id")
                .fetch_one(&pool).await.unwrap();
            assert!(completed);
            c.name = "Changed cached chapter".into();
            crate::series_cache::put_chapters(&pool, 7, &[c])
                .await
                .unwrap();
            for _ in 0..3 {
                backfill_tick(&pool, &client(&engine), &mut cursor)
                    .await
                    .unwrap();
            }
            assert_eq!(
                engine.hits.load(Ordering::SeqCst),
                2,
                "a new cache revision needs fresh coverage"
            );
        }
    }

    #[tokio::test]
    async fn manga_identity_changes_abort_without_invalidating_old_bindings() {
        for change_source in [true, false] {
            let pool = pool().await;
            let mut m = manga(7);
            crate::series_cache::put_series(&pool, &m).await.unwrap();
            crate::series_cache::put_chapters(&pool, 7, &[chapter(8, 7), chapter(9, 7)])
                .await
                .unwrap();
            let before = stamps(&pool).await;
            if change_source {
                m.source_id = "new-source".into();
            } else {
                m.url = Some("/manga/new".into());
            }
            assert!(crate::series_cache::put_series(&pool, &m).await.is_err());
            assert_eq!(stamps(&pool).await, before);
            let cached = crate::series_cache::get_series(&pool, 7)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(cached.source_id, "source");
        }
    }

    #[tokio::test]
    async fn full_lists_remove_bindings_reuse_keys_and_clear_empty_lists_but_misses_only_patch() {
        let pool = pool().await;
        crate::series_cache::put_series(&pool, &manga(7))
            .await
            .unwrap();
        crate::series_cache::put_chapters(&pool, 7, &[chapter(8, 7), chapter(9, 7)])
            .await
            .unwrap();
        crate::series_cache::put_chapters(&pool, 7, &[chapter(9, 7)])
            .await
            .unwrap();
        let missing = testsrv::spawn_status(
            200,
            "application/json",
            json!({"data": {"chapters": {"nodes": []}}})
                .to_string()
                .into_bytes(),
            Duration::ZERO,
        )
        .await;
        assert_eq!(
            chapter_origin(&pool, &client(&missing), 8).await.unwrap(),
            None
        );
        assert_eq!(
            missing.hits.load(Ordering::SeqCst),
            1,
            "removed keys cannot answer origins"
        );

        let mut reused = chapter(9, 7);
        reused.url = Some("/chapter/8".into());
        assert!(
            crate::series_cache::put_chapters(&pool, 7, &[reused.clone()])
                .await
                .is_err()
        );
        // Reuse that coordinate again in one replacement, with no intermediate list.
        let mut replacement = chapter(10, 7);
        replacement.url = reused.url;
        crate::series_cache::put_chapters(&pool, 7, &[replacement])
            .await
            .unwrap();
        let engine = testsrv::spawn_status(
            200,
            "application/json",
            json!({"data": {"chapters": {"nodes": [{
                "url": "/chapter/11", "sourceOrder": 4, "mangaId": 7,
                "manga": {"url": "/manga/7", "title": "Series", "sourceId": "source"}
            }]}}})
            .to_string()
            .into_bytes(),
            Duration::ZERO,
        )
        .await;
        chapter_origin(&pool, &client(&engine), 11)
            .await
            .unwrap()
            .unwrap();
        let ids: Vec<i64> =
            sqlx::query_scalar("SELECT chapter_id FROM engine_chapter_key ORDER BY chapter_id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(ids, vec![10, 11], "a miss must retain sibling bindings");

        crate::series_cache::put_chapters(&pool, 7, &[])
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM engine_chapter_key")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        let stamp: String =
            sqlx::query_scalar("SELECT xmin::text FROM suwayomi_series WHERE id = 7")
                .fetch_one(&pool)
                .await
                .unwrap();
        let keys = stamps(&pool).await;
        crate::series_cache::put_chapters(&pool, 7, &[])
            .await
            .unwrap();
        let after: String =
            sqlx::query_scalar("SELECT xmin::text FROM suwayomi_series WHERE id = 7")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(after, stamp, "an unchanged empty list must not write");
        assert_eq!(stamps(&pool).await, keys);
    }

    #[tokio::test]
    async fn key_write_failures_roll_back_metadata_chapters_and_identity_invalidation() {
        let pool = pool().await;
        let mut m = manga(7);
        crate::series_cache::put_series(&pool, &m).await.unwrap();
        crate::series_cache::put_chapters(&pool, 7, &[chapter(8, 7)])
            .await
            .unwrap();
        let before = stamps(&pool).await;
        let series_stamp: String =
            sqlx::query_scalar("SELECT xmin::text FROM suwayomi_series WHERE id = 7")
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::raw_sql(
            "CREATE FUNCTION reject_engine_key_fn() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN RAISE EXCEPTION 'forced engine key failure'; END $$; \
             CREATE TRIGGER reject_manga_key BEFORE INSERT OR UPDATE ON engine_manga_key \
             FOR EACH ROW EXECUTE FUNCTION reject_engine_key_fn(); \
             CREATE TRIGGER reject_chapter_key BEFORE INSERT OR UPDATE ON engine_chapter_key \
             FOR EACH ROW EXECUTE FUNCTION reject_engine_key_fn()",
        )
        .execute(&pool)
        .await
        .unwrap();
        m.title = "New title".into();
        m.source_id = "new-source".into();
        assert!(crate::series_cache::put_series(&pool, &m).await.is_err());
        let cached = crate::series_cache::get_series(&pool, 7)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cached.title, "Series");
        assert_eq!(cached.source_id, "source");
        assert_eq!(
            stamps(&pool).await,
            before,
            "invalidation must roll back with the parent"
        );

        assert!(
            crate::series_cache::put_chapters(&pool, 7, &[chapter(9, 7)])
                .await
                .is_err()
        );
        let ids: Vec<i64> =
            sqlx::query_scalar("SELECT id FROM suwayomi_chapter WHERE manga_id = 7 ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            ids,
            vec![8],
            "a key failure must roll back the display cache"
        );
        assert_eq!(stamps(&pool).await, before);
        let after: String =
            sqlx::query_scalar("SELECT xmin::text FROM suwayomi_series WHERE id = 7")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(after, series_stamp);

        // The display is identical here: key-only changes still belong to the guarded unit.
        let mut changed = chapter(8, 7);
        changed.source_order = Some(99);
        assert!(crate::series_cache::put_chapters(&pool, 7, &[changed])
            .await
            .is_err());
        assert_eq!(stamps(&pool).await, before);
    }

    #[tokio::test]
    async fn origin_without_metadata_stores_title_and_leaves_unchanged_keys_unwritten() {
        let pool = pool().await;
        let engine = testsrv::spawn_status(
            200,
            "application/json",
            json!({"data": {"chapters": {"nodes": [{
                "url": "/chapter/8", "sourceOrder": 3, "mangaId": 7,
                "manga": {"url": "/manga/7", "title": "Engine title", "sourceId": "source"}
            }]}}})
            .to_string()
            .into_bytes(),
            Duration::ZERO,
        )
        .await;
        let engine_client = client(&engine);
        let first = chapter_origin(&pool, &engine_client, 8)
            .await
            .unwrap()
            .unwrap();
        let before = stamps(&pool).await;
        let second = chapter_origin(&pool, &engine_client, 8)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.manga_title, "Engine title");
        assert_eq!(second, first);
        assert_eq!(
            engine.hits.load(Ordering::SeqCst),
            1,
            "a stored title must serve an origin without metadata"
        );
        assert_eq!(
            stamps(&pool).await,
            before,
            "stored origin hits must not rewrite keys"
        );

        // An older binding with neither metadata nor a stored title is incomplete.
        sqlx::query("UPDATE engine_manga_key SET title = NULL")
            .execute(&pool)
            .await
            .unwrap();
        let untitled = stamps(&pool).await;
        assert_eq!(
            chapter_origin(&pool, &engine_client, 8).await.unwrap(),
            Some(first.clone())
        );
        let filled = stamps(&pool).await;
        assert_eq!(filled.iter().filter(|s| !untitled.contains(s)).count(), 1);
        assert_eq!(
            chapter_origin(&pool, &engine_client, 8).await.unwrap(),
            Some(first)
        );
        assert_eq!(engine.hits.load(Ordering::SeqCst), 2);
        assert_eq!(stamps(&pool).await, filled);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM suwayomi_series")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn backfill_reads_one_engine_query_per_manga_and_drains() {
        let pool = pool().await;
        let mut m = manga(7);
        m.url = None;
        crate::series_cache::put_series(&pool, &m).await.unwrap();
        let mut c = chapter(8, 7);
        c.url = None;
        crate::series_cache::put_chapters(&pool, 7, &[c])
            .await
            .unwrap();
        let engine = testsrv::spawn_status(
            200,
            "application/json",
            json!({"data": {
                "manga": manga_json(7), "chapters": {"nodes": [chapter_json(8, 7)]}
            }})
            .to_string()
            .into_bytes(),
            Duration::ZERO,
        )
        .await;
        let mut cursor = None;
        backfill_tick(&pool, &client(&engine), &mut cursor)
            .await
            .unwrap();
        assert_eq!(stamps(&pool).await.len(), 2);
        backfill_tick(&pool, &client(&engine), &mut cursor)
            .await
            .unwrap();
        backfill_tick(&pool, &client(&engine), &mut cursor)
            .await
            .unwrap();
        assert_eq!(engine.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn backfill_is_bounded_and_advances_past_failures() {
        let pool = pool().await;
        for id in 1..=6 {
            let mut m = manga(id);
            m.url = None;
            crate::series_cache::put_series(&pool, &m).await.unwrap();
        }
        let engine = testsrv::spawn_error(503, "engine unavailable").await;
        let mut cursor = None;
        backfill_tick(&pool, &client(&engine), &mut cursor)
            .await
            .unwrap();
        assert_eq!(cursor, Some(4));
        assert_eq!(engine.hits.load(Ordering::SeqCst), 4);
        backfill_tick(&pool, &client(&engine), &mut cursor)
            .await
            .unwrap();
        assert_eq!(cursor, Some(6));
        assert_eq!(engine.hits.load(Ordering::SeqCst), 6);
        backfill_tick(&pool, &client(&engine), &mut cursor)
            .await
            .unwrap();
        assert_eq!(cursor, None);
        backfill_tick(&pool, &client(&engine), &mut cursor)
            .await
            .unwrap();
        assert_eq!(engine.hits.load(Ordering::SeqCst), 10);
    }

    #[tokio::test]
    async fn backfill_stops_on_shutdown() {
        let pool = pool().await;
        let engine = testsrv::spawn_error(503, "engine unavailable").await;
        let (stop, shutdown) = watch::channel(false);
        let job = tokio::spawn(run_loop(pool, client(&engine), shutdown));
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), job)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(engine.hits.load(Ordering::SeqCst), 0);
    }
}
