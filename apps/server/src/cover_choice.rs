//! Cover choice: which stored cover a work shows, and which covers are kept.
//!
//! Three rules, strongest first:
//!
//! 1. An admin cover wins. It is the work's `cover_upload` row: an uploaded file, or a
//!    cover the admin picked from the work's gallery ([`pick`], `choice` names it).
//! 2. Otherwise the cover ranking (`cover_source_rank`, the admin Covers page) decides:
//!    the cover comes from the highest-ranked allowed source that supplies one.
//!    Unranked sources keep the default order after the ranked ones (MangaDex first,
//!    then the oldest mapping), the order the cover crawl and repair always used
//!    (`cover::ranked_cover_sources`). Source bars still apply first.
//! 3. The crawl fills a missing cover from the first source that answers.
//!
//! [`rank_pass`] moves published covers to their ranked source after a ranking change
//! and once a day after that. [`prune_pass`] (setting `cover_prune`, off by default)
//! removes the stored covers of source mappings that no work shows. Both keep a cursor
//! in `instance_setting`, so a pass cut off by a restart resumes where it stopped; each
//! runs under its own job lease and writes on the Serial lane.

use anyhow::Result;
use chrono::Utc;

use crate::cover::{self, CoverOrigin, RepairCoverSource};
use crate::cover_store::{self, CoverStore};
use crate::db::Database;
use crate::job_lease::{JobFamily, JobLease};
use crate::mangadex::MangaDexClient;
use crate::source_budget::{SourceBudgets, SourceUse};

/// What a cover fetch needs.
pub(crate) struct Clients<'a> {
    pub main: &'a Database,
    pub covers: &'a CoverStore,
    pub mangadex: &'a MangaDexClient,
    pub suwayomi: &'a crate::suwayomi::SuwayomiClient,
    pub budgets: &'a SourceBudgets,
    pub shutdown: &'a tokio::sync::watch::Receiver<bool>,
    /// An admin waits on this call: a paced source's budget is taken at once or the
    /// source is skipped, never queued for.
    pub waiting: bool,
}

/// One cover the admin can pick for a work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Choice {
    /// `md:<file name>` (a MangaDex cover) or `src:<source_series id>` (an engine
    /// mapping's cover).
    pub key: String,
    /// `mangadex` or the Suwayomi source id.
    pub source_id: String,
    pub source: RepairCoverSource,
    /// Display URL: the MangaDex thumbnail, or the engine thumbnail path.
    pub thumbnail_url: String,
    pub volume: Option<String>,
    pub lang: Option<String>,
    /// The MangaDex cover the work's metadata names as its main one.
    pub primary: bool,
}

/// The covers `work_id` can show, in cover order: MangaDex covers (main one first,
/// then by volume) at MangaDex's place, each engine mapping at its own. Barred sources
/// offer none.
pub(crate) async fn choices(main: &Database, work_id: &str) -> Result<Vec<Choice>> {
    let barred = crate::catalog::work_metadata::barred_sql(
        &crate::catalog::work_metadata::mapping_source_sql("ss"),
        "'cover'",
    );
    let rank = crate::catalog::work_metadata::cover_rank_sql("ss");
    let mut out: Vec<(i32, Choice)> = Vec::new();
    let md =
        crate::db_types::query_tuple::<(String, i32, String, Option<String>, Option<String>, i64)>(
            &format!(
        "SELECT a.source_key, a.position, wc.cover_file_name, wc.lang, wc.volume, wc.is_primary \
         FROM (SELECT ss.source_key, {rank} AS position FROM source_series ss \
               WHERE ss.work_id = $1 AND ss.source_type = 'mangadex' AND NOT {barred} \
               ORDER BY ss.created_at, ss.id LIMIT 1) a \
         JOIN work_cover wc ON wc.work_id = $1"
    ),
        )
        .bind(work_id)
        .fetch_all(main)
        .await?;
    if let Some((manga_id, position)) = md.first().map(|r| (r.0.clone(), r.1)) {
        let sorted = crate::catalog::sort_work_covers(
            md.into_iter()
                .map(|(_, _, file, lang, volume, primary)| (file, lang, volume, primary))
                .collect(),
        );
        for c in sorted {
            out.push((
                position,
                Choice {
                    key: format!("md:{}", c.file_name),
                    source_id: "mangadex".into(),
                    thumbnail_url: crate::mangadex::cover_thumb_url(&manga_id, &c.file_name),
                    source: RepairCoverSource::MangaDex {
                        manga_id: manga_id.clone(),
                        file_name: c.file_name,
                    },
                    volume: c.volume,
                    lang: c.lang,
                    primary: c.is_primary,
                },
            ));
        }
    }
    let engine: Vec<(String, String, String, i32)> = sqlx::query_as(&format!(
        "SELECT ss.id, ss.source_key, COALESCE(ss.source_id, ss.source_type), {rank} \
         FROM source_series ss WHERE ss.work_id = $1 AND ss.source_type = 'suwayomi' \
           AND NOT {barred} ORDER BY {rank}, ss.created_at, ss.id"
    ))
    .bind(work_id)
    .fetch_all(main)
    .await?;
    for (id, key, source_id, position) in engine {
        let Some(manga_id) = key.parse::<i64>().ok().filter(|id| *id >= 0) else {
            continue;
        };
        let path = format!("/api/v1/manga/{manga_id}/thumbnail");
        out.push((
            position,
            Choice {
                key: format!("src:{id}"),
                source_id: source_id.clone(),
                thumbnail_url: path.clone(),
                source: RepairCoverSource::Suwayomi { path, source_id },
                volume: None,
                lang: None,
                primary: false,
            },
        ));
    }
    // Stable: equal positions keep MangaDex first, then the mapping order.
    out.sort_by_key(|(position, _)| *position);
    Ok(out.into_iter().map(|(_, choice)| choice).collect())
}

/// The work's admin cover state: `(choice, uploaded, cover_source)`. `choice` is the
/// picked gallery key; `uploaded` is an admin cover with no gallery key (a file).
pub(crate) async fn admin_cover(
    main: &Database,
    work_id: &str,
) -> Result<(Option<String>, bool, Option<String>)> {
    let row: Option<(Option<String>, bool, Option<String>)> = sqlx::query_as(
        "SELECT u.choice, u.work_id IS NOT NULL AND u.choice IS NULL, w.cover_source \
         FROM work w LEFT JOIN cover_upload u ON u.work_id = w.id WHERE w.id = $1",
    )
    .bind(work_id)
    .fetch_optional(main)
    .await?;
    Ok(row.unwrap_or_default())
}

/// The processed cover one source supplies: an engine mapping's stored source cover
/// when there is one (no fetch), otherwise a download paced by the source's budget
/// (taken at once, or not at all, when an admin waits).
async fn processed_cover(c: &Clients<'_>, source: &RepairCoverSource) -> Result<Option<Vec<u8>>> {
    if let RepairCoverSource::Suwayomi { path, source_id } = source {
        if let Some(id) = crate::engine_route::cover_manga_id(path) {
            if let Ok(Some((master, _))) =
                cover::read_suwayomi_cover(c.main, c.covers, id, None).await
            {
                return Ok(Some(master.bytes));
            }
        }
        let paid = if c.waiting {
            c.budgets.try_spend_burst(source_id, SourceUse::Cover)
        } else {
            c.budgets
                .spend(source_id, SourceUse::Cover, c.shutdown)
                .await
        };
        if !paid {
            return Ok(None);
        }
    }
    let work = cover::master_cover_work().await;
    // The spend above pays for a regional engine's lookup too.
    let fetched = crate::engine_route::prepaid(cover::fetch_cover_source(
        c.main, c.mangadex, c.suwayomi, source,
    ))
    .await;
    let Some(bytes) = fetched else {
        return Ok(None);
    };
    Ok(Some(
        work.process(move || cover::process_cover(&bytes)).await??,
    ))
}

/// Why a pick did not change the cover.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PickError {
    /// The work does not offer this cover (any more).
    Unknown,
    /// The source did not supply a usable image.
    Unavailable,
    /// The work changed under the pick (a bar, a takedown, a merge): try again.
    Refused,
}

/// Admin: make the gallery cover `key` the work's admin cover. It is fetched now,
/// stored like an upload (it survives the loss of the cover files) and published with
/// the work's cards in one transaction.
pub(crate) async fn pick(
    c: &Clients<'_>,
    work_id: &str,
    key: &str,
) -> Result<std::result::Result<(), PickError>> {
    let Some(choice) = choices(c.main, work_id)
        .await?
        .into_iter()
        .find(|choice| choice.key == key)
    else {
        return Ok(Err(PickError::Unknown));
    };
    let webp = match processed_cover(c, &choice.source).await {
        Ok(Some(webp)) => webp,
        Ok(None) => return Ok(Err(PickError::Unavailable)),
        Err(error) => {
            tracing::warn!(work_id, key, error = %format!("{error:#}"), "cover pick: the image could not be used");
            return Ok(Err(PickError::Unavailable));
        }
    };
    let origin = CoverOrigin::Pick {
        source: &choice.source_id,
        choice: &choice.key,
    };
    if !cover::put_chosen_work_cover(c.main, c.covers, work_id, &webp, origin).await? {
        return Ok(Err(PickError::Refused));
    }
    cover::clear_cover_issue(c.main, work_id).await;
    Ok(Ok(()))
}

/// Admin: drop the work's admin cover (a pick or an upload) and let the ranking choose
/// again at once. The old art stays shown until a ranked source supplies a cover.
/// Returns whether a new cover was published.
pub(crate) async fn clear_pick(c: &Clients<'_>, work_id: &str) -> Result<bool> {
    crate::db::retry_transaction(|| async {
        let mut tx = c.main.begin().await?;
        let _asset = cover::lock_asset(&mut tx, work_id).await?;
        crate::locks::lock_works(&mut tx, &[work_id]).await?;
        sqlx::query("DELETE FROM cover_upload WHERE work_id = $1")
            .bind(work_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    })
    .await?;
    reselect_work(c, work_id, true).await
}

/// Move `work_id`'s cover to its ranked source. Skips a work with an admin cover or a
/// takedown. Without `force` it only tries sources ranked above the one the cover came
/// from; with it, every source in rank order. The first source that supplies a usable
/// image is published, guarded by the version read here.
pub(crate) async fn reselect_work(c: &Clients<'_>, work_id: &str, force: bool) -> Result<bool> {
    let state: Option<(Option<String>, Option<i64>, bool, i32)> = sqlx::query_as(
        "SELECT w.cover_source, w.cover_cached_version, \
                EXISTS (SELECT 1 FROM cover_upload u WHERE u.work_id = w.id) \
                OR EXISTS (SELECT 1 FROM media_takedown td \
                           WHERE td.subject_kind = 'work' AND td.subject_id = w.id), \
                COALESCE((SELECT r.position FROM cover_source_rank r WHERE r.source_id = w.cover_source), 2147483647) \
         FROM work w WHERE w.id = $1",
    )
    .bind(work_id)
    .fetch_optional(c.main)
    .await?;
    let Some((_, version, skip, current)) = state else {
        return Ok(false);
    };
    if skip {
        return Ok(false);
    }
    for (position, source) in cover::ranked_cover_sources(c.main, work_id).await? {
        if !force && position >= current {
            break;
        }
        let webp = match processed_cover(c, &source).await {
            Ok(Some(webp)) => webp,
            Ok(None) => continue,
            Err(error) => {
                tracing::debug!(work_id, source = source.source_id(), error = %format!("{error:#}"), "cover ranking: source image unusable");
                continue;
            }
        };
        let origin = CoverOrigin::Ranked {
            source: source.source_id(),
            version,
        };
        let stored = cover::put_chosen_work_cover(c.main, c.covers, work_id, &webp, origin).await?;
        if stored {
            cover::clear_cover_issue(c.main, work_id).await;
        }
        return Ok(stored);
    }
    Ok(false)
}

/// A pass's position in `instance_setting`: the last key done, and when the last full
/// pass ended.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
struct PassCursor {
    after: String,
    /// Unix seconds.
    finished_at: Option<i64>,
}

const RANK_PASS: &str = "cover_rank_pass";
const PRUNE_PASS: &str = "cover_prune_pass";
const PASS_BATCH: i64 = 50;
/// A finished pass runs again after this long: new works and new mappings follow.
const PASS_INTERVAL: chrono::Duration = chrono::Duration::hours(24);

async fn load_cursor(main: &Database, key: &str) -> Result<Option<(String, PassCursor)>> {
    let raw: Option<String> =
        sqlx::query_scalar("SELECT value FROM instance_setting WHERE key = $1")
            .bind(key)
            .fetch_optional(main)
            .await?;
    Ok(raw.map(|raw| {
        let cursor = serde_json::from_str(&raw).unwrap_or_default();
        (raw, cursor)
    }))
}

async fn store_cursor<'e, E>(conn: E, key: &str, cursor: &PassCursor) -> Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query(
        "INSERT INTO instance_setting (key, value, updated_at) VALUES ($1, $2, now()) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(key)
    .bind(serde_json::to_string(cursor)?)
    .execute(conn)
    .await?;
    Ok(())
}

/// Compare-and-set: a cursor a newer change replaced is left alone.
async fn advance_cursor(main: &Database, key: &str, seen: &str, next: &PassCursor) -> Result<()> {
    sqlx::query(
        "UPDATE instance_setting SET value = $3, updated_at = now() WHERE key = $1 AND value = $2",
    )
    .bind(key)
    .bind(seen)
    .bind(serde_json::to_string(next)?)
    .execute(main)
    .await?;
    Ok(())
}

/// The next batch to work on, or `None` when the pass is done for now. A pass
/// finished more than [`PASS_INTERVAL`] ago starts again from the beginning.
async fn next_cursor(main: &Database, key: &str) -> Result<Option<(String, PassCursor)>> {
    let Some((raw, cursor)) = load_cursor(main, key).await? else {
        return Ok(None);
    };
    match cursor.finished_at {
        None => Ok(Some((raw, cursor))),
        Some(done) if Utc::now().timestamp() - done < PASS_INTERVAL.num_seconds() => Ok(None),
        Some(_) => {
            let fresh = PassCursor::default();
            advance_cursor(main, key, &raw, &fresh).await?;
            Ok(load_cursor(main, key).await?)
        }
    }
}

fn after_batch(len: usize, last: Option<&String>) -> PassCursor {
    match last {
        Some(last) if len as i64 == PASS_BATCH => PassCursor {
            after: last.clone(),
            finished_at: None,
        },
        _ => PassCursor {
            after: String::new(),
            finished_at: Some(Utc::now().timestamp()),
        },
    }
}

/// The ranked sources, best first.
pub(crate) async fn load_rank(main: &Database) -> Result<Vec<String>> {
    Ok(
        sqlx::query_scalar("SELECT source_id FROM cover_source_rank ORDER BY position")
            .fetch_all(main)
            .await?,
    )
}

/// Replace the cover ranking with `sources` (best first; empty: the default order)
/// and restart the ranking pass. Ids must be valid and distinct.
pub(crate) async fn save_rank(main: &Database, sources: &[String]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for source in sources {
        anyhow::ensure!(
            crate::catalog::work_metadata::valid_source_id(source),
            "invalid source id"
        );
        anyhow::ensure!(seen.insert(source.as_str()), "a source is ranked twice");
    }
    crate::db::retry_transaction(|| async {
        let mut tx = main.begin().await?;
        sqlx::query("DELETE FROM cover_source_rank")
            .execute(&mut *tx)
            .await?;
        let positions: Vec<i32> = (0..sources.len() as i32).collect();
        sqlx::query(
            "INSERT INTO cover_source_rank (source_id, position) \
             SELECT * FROM UNNEST($1::text[], $2::int[])",
        )
        .bind(sources)
        .bind(&positions)
        .execute(&mut *tx)
        .await?;
        if sources.is_empty() {
            sqlx::query("DELETE FROM instance_setting WHERE key = $1")
                .bind(RANK_PASS)
                .execute(&mut *tx)
                .await?;
        } else {
            store_cursor(&mut *tx, RANK_PASS, &PassCursor::default()).await?;
        }
        tx.commit().await?;
        Ok(())
    })
    .await
}

/// Works whose published cover came from a source ranked below one that could supply
/// theirs, after `$1`, by id, at most `$2`. A work with an admin cover or a takedown
/// is never listed.
fn rank_candidates_sql() -> String {
    let barred = crate::catalog::work_metadata::barred_sql(
        &crate::catalog::work_metadata::mapping_source_sql("ss"),
        "'cover'",
    );
    let rank = crate::catalog::work_metadata::cover_rank_sql("ss");
    format!(
        "SELECT w.id FROM work w \
         WHERE w.id > $1 AND w.cover_cached_version IS NOT NULL \
           AND NOT EXISTS (SELECT 1 FROM cover_upload u WHERE u.work_id = w.id) \
           AND NOT EXISTS (SELECT 1 FROM media_takedown td \
                           WHERE td.subject_kind = 'work' AND td.subject_id = w.id) \
           AND EXISTS (SELECT 1 FROM source_series ss WHERE ss.work_id = w.id \
                 AND ss.source_type IN ('mangadex', 'suwayomi') AND NOT {barred} \
                 AND (ss.source_type <> 'mangadex' OR w.cover_file_name IS NOT NULL) \
                 AND {rank} < COALESCE((SELECT r.position FROM cover_source_rank r \
                                        WHERE r.source_id = w.cover_source), 2147483647)) \
         ORDER BY w.id LIMIT $2"
    )
}

/// Run the ranking pass from its cursor: works whose cover a better-ranked source can
/// replace get it ([`reselect_work`]). Stops at shutdown, when another server holds the
/// pass, after `max_batches` batches of 50, or when the pass is done for now. Returns
/// how many covers changed.
pub(crate) async fn rank_pass(c: &Clients<'_>, max_batches: Option<usize>) -> Result<u64> {
    let Some(lease) = JobLease::try_acquire(c.main, JobFamily::CoverRank).await? else {
        return Ok(0);
    };
    let result: Result<u64> = async {
        let sql = rank_candidates_sql();
        let mut changed = 0;
        let mut batches = 0;
        while !*c.shutdown.borrow() && max_batches.is_none_or(|max| batches < max) {
            let Some((raw, cursor)) = next_cursor(c.main, RANK_PASS).await? else {
                break;
            };
            let works: Vec<String> = sqlx::query_scalar(&sql)
                .bind(&cursor.after)
                .bind(PASS_BATCH)
                .fetch_all(c.main)
                .await?;
            for work in &works {
                if *c.shutdown.borrow() {
                    return Ok(changed);
                }
                match reselect_work(c, work, false).await {
                    Ok(true) => changed += 1,
                    Ok(false) => {}
                    Err(error) => {
                        let error = crate::db::observed_database_error(error);
                        tracing::warn!(work_id = %work, error = %format!("{error:#}"), "cover ranking: one work failed");
                    }
                }
            }
            advance_cursor(c.main, RANK_PASS, &raw, &after_batch(works.len(), works.last()))
                .await?;
            batches += 1;
        }
        Ok(changed)
    }
    .await;
    lease.release().await;
    result
}

/// Whether the prune setting is on.
pub(crate) async fn prune_enabled(main: &Database) -> bool {
    crate::instance::resolve(main, &crate::instance::COVER_PRUNE)
        .await
        .as_bool()
}

/// Turn the prune on or off. On starts a fresh pass; off stops a running one at its
/// next cover.
pub(crate) async fn set_prune(main: &Database, enabled: bool) -> Result<()> {
    let mut tx = main.begin().await?;
    crate::instance::store(
        &mut *tx,
        &crate::instance::COVER_PRUNE,
        if enabled { "on" } else { "off" },
    )
    .await?;
    if enabled {
        store_cursor(&mut *tx, PRUNE_PASS, &PassCursor::default()).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// SQL rows `(asset_key, source_id, work_id)`: the stored master of an engine mapping
/// whose work shows another cover. Never one whose bytes any work's master uses, the
/// source the work's cover came from (its master may be a separate fetch with other
/// bytes; the warmer applies the same rule in `unpicked_warm_sql`), the mapping an admin
/// picked, a MangaDex extension's cover, a work with no stored cover
/// of its own, or a cover not mapped to a work (the orphan sweep in `gc.rs` owns those).
/// `{scope}` narrows the rows.
fn unpicked_sql(scope: &str) -> String {
    format!(
        "SELECT c.asset_key, COALESCE(ss.source_id, ss.source_type) AS source_id, w.id AS work_id \
         FROM cover_object c \
         JOIN source_series ss ON ss.source_type = 'suwayomi' AND 's:' || ss.source_key = c.asset_key \
         JOIN work w ON w.id = ss.work_id \
         WHERE c.kind = 'master' AND c.asset_key LIKE 's:%' {scope} \
           AND EXISTS (SELECT 1 FROM cover_object wm WHERE wm.work_id = w.id AND wm.kind = 'master') \
           AND w.cover_source IS DISTINCT FROM COALESCE(ss.source_id, ss.source_type) \
           AND NOT EXISTS (SELECT 1 FROM cover_object u \
                           WHERE u.work_id IS NOT NULL AND u.kind = 'master' AND u.sha256 = c.sha256) \
           AND NOT EXISTS (SELECT 1 FROM cover_upload p \
                           WHERE p.work_id = w.id AND p.choice = 'src:' || ss.id) \
           AND NOT EXISTS (SELECT 1 FROM source_extension e \
                           WHERE e.source_id = ss.source_id AND e.pkg_name LIKE '%.mangadex')"
    )
}

/// The prune preview: rows `(source_id, covers, bytes)`. Bytes count only files no
/// kept cover shares, so the total is the space it frees. Every step runs once: a
/// per-source subquery here ran the whole byte count again for each source and
/// passed the request deadline on a catalogue with hundreds of sources.
pub(crate) fn prune_preview_sql() -> String {
    format!(
        "WITH cand AS MATERIALIZED ({}), \
         kept AS (SELECT DISTINCT o.sha256 FROM cover_object o \
                  WHERE NOT EXISTS (SELECT 1 FROM cand k WHERE k.asset_key = o.asset_key)), \
         freed AS (SELECT DISTINCT ON (o.sha256) o.sha256, o.byte_length, cand.source_id \
                   FROM cover_object o JOIN cand ON cand.asset_key = o.asset_key \
                   WHERE NOT EXISTS (SELECT 1 FROM kept WHERE kept.sha256 = o.sha256) \
                   ORDER BY o.sha256, cand.source_id), \
         bytes AS (SELECT source_id, SUM(byte_length)::bigint AS bytes FROM freed GROUP BY source_id) \
         SELECT cand.source_id, COUNT(*)::bigint, COALESCE(MAX(b.bytes), 0)::bigint \
         FROM cand LEFT JOIN bytes b ON b.source_id = cand.source_id \
         GROUP BY cand.source_id ORDER BY 3 DESC, 1",
        unpicked_sql("")
    )
}

/// What the prune would remove, per source: `(source_id, covers, bytes)`.
pub(crate) async fn prune_preview(main: &Database) -> Result<Vec<(String, i64, i64)>> {
    Ok(sqlx::query_as(&prune_preview_sql()).fetch_all(main).await?)
}

/// Run the prune from its cursor while the setting is on: each unpicked cover's index
/// rows go in their own transaction, under the engine generation and the cover keys of
/// the source asset and its work (the publication locks), after the rule is checked
/// again. The files go with GC once nothing names them. Stops at shutdown, when the
/// setting goes off, when another server holds the pass, after `max_batches` batches,
/// or when the pass is done for now. Returns how many covers it removed.
pub(crate) async fn prune_pass(
    main: &Database,
    shutdown: &tokio::sync::watch::Receiver<bool>,
    max_batches: Option<usize>,
) -> Result<u64> {
    if !prune_enabled(main).await {
        return Ok(0);
    }
    let Some(lease) = JobLease::try_acquire(main, JobFamily::CoverPrune).await? else {
        return Ok(0);
    };
    let result: Result<u64> = async {
        let batch_sql = format!(
            "{} ORDER BY c.asset_key LIMIT $2",
            unpicked_sql("AND c.asset_key > $1")
        );
        let check_sql = format!(
            "SELECT EXISTS ({})",
            // The work as read: a merge that moved the mapping since fails the check, and
            // the next pass sees the cover under its new work's lock.
            unpicked_sql("AND c.asset_key = $1 AND ss.work_id = $2")
        );
        let mut deleted = 0;
        let mut batches = 0;
        while !*shutdown.borrow() && max_batches.is_none_or(|max| batches < max) {
            if !prune_enabled(main).await {
                break;
            }
            let Some((raw, cursor)) = next_cursor(main, PRUNE_PASS).await? else {
                break;
            };
            let rows: Vec<(String, String, String)> = sqlx::query_as(&batch_sql)
                .bind(&cursor.after)
                .bind(PASS_BATCH)
                .fetch_all(main)
                .await?;
            for (asset, _, work) in &rows {
                if *shutdown.borrow() || !prune_enabled(main).await {
                    return Ok(deleted);
                }
                deleted += crate::db::retry_transaction(|| async {
                    let mut tx = main.begin().await?;
                    crate::engine_generation::shared(&mut tx, None).await?;
                    let _assets = cover::lock_assets_and_files(
                        &mut tx,
                        &[asset.as_str(), work.as_str()],
                        &[],
                    )
                    .await?;
                    let unpicked: bool = sqlx::query_scalar(&check_sql)
                        .bind(asset)
                        .bind(work)
                        .fetch_one(&mut *tx)
                        .await?;
                    let removed = if unpicked {
                        cover_store::delete_asset(&mut tx, asset).await? > 0
                    } else {
                        false
                    };
                    tx.commit().await?;
                    Ok(removed as u64)
                })
                .await?;
            }
            let last = rows.last().map(|(asset, _, _)| asset);
            advance_cursor(main, PRUNE_PASS, &raw, &after_batch(rows.len(), last)).await?;
            batches += 1;
        }
        Ok(deleted)
    }
    .await;
    lease.release().await;
    result
}

/// SQL condition over a `suwayomi_series` row aliased `s`: with the prune on, its cover
/// is one the prune removes, so the warmer leaves it alone. Bound to `on`.
pub(crate) fn unpicked_warm_sql(on: &str) -> String {
    format!(
        "({on} AND EXISTS (SELECT 1 FROM source_series ss JOIN work w ON w.id = ss.work_id \
           WHERE ss.source_type = 'suwayomi' AND ss.source_key = s.id::text \
             AND EXISTS (SELECT 1 FROM cover_object wm WHERE wm.work_id = w.id AND wm.kind = 'master') \
             AND w.cover_source IS DISTINCT FROM COALESCE(ss.source_id, ss.source_type) \
             AND NOT EXISTS (SELECT 1 FROM cover_upload p \
                             WHERE p.work_id = w.id AND p.choice = 'src:' || ss.id) \
             AND NOT EXISTS (SELECT 1 FROM source_extension e \
                             WHERE e.source_id = ss.source_id AND e.pkg_name LIKE '%.mangadex')))"
    )
}

#[cfg(test)]
#[path = "cover_choice_tests.rs"]
mod tests;
