//! Two MangaDex entries are never one work.
//!
//! MangaDex lists a mature and an erotic version of one series, a colored edition and a
//! sequel as separate entries, often under one name and by one author. Before
//! `pair::assess` and `catalog::merge_works_on` refused it, the duplicate sweep could
//! fold two such entries into one work. That work showed one entry's metadata mixed
//! with the other's covers, credits and alt titles. Splitting the two apart again left
//! the split-off work with a title and nothing else: the merge had moved its metadata
//! onto the survivor, and the metadata backfill only recorded what the FIRST MangaDex
//! mapping of a work said.
//!
//! [`spawn`] repairs this once per database:
//!
//!  1. every work with more than one MangaDex mapping is split: the first mapping (rank
//!     order `created_at`, `id`, the one that gave the work its metadata) stays, and
//!     each other mapping gets its own work. Engine sources stay on the original;
//!  2. waiting reviews the rules now call two works are closed;
//!  3. every split-apart pair of MangaDex works fetches its metadata and covers from
//!     MangaDex again and loses what the merge folded in from its partner ([`refetch`]);
//!  4. each of those works chooses its cover again.
//!
//! A maintenance flag records completion. While MangaDex is off the job parks after step
//! 2 and goes on when the switch is turned on. An admin split of a MangaDex mapping runs
//! step 3 and 4 for its pair at once (`splitSourceSeries`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::watch;

use crate::catalog::{self, normalize::normalize_title, work_metadata::FieldGroup, WorkInput};
use crate::db::Database;
use crate::graphql::AppState;

const FLAG: &str = "mangadex_unmerge_v1";
/// Let boot finish (migrations, the first reconcile) before splitting works.
const START_DELAY: Duration = Duration::from_secs(120);
const RETRY_DELAY: Duration = Duration::from_secs(600);

/// Run the one-time repair in the background until it completes.
pub fn spawn(state: Arc<AppState>, mut shutdown: watch::Receiver<bool>) {
    tokio::spawn(async move {
        if !nap(START_DELAY, &mut shutdown).await {
            return;
        }
        loop {
            match run(&state, &mut shutdown).await {
                Ok(true) => return,
                Ok(false) => {}
                Err(error) => {
                    crate::db::observe_database_error(&error);
                    tracing::warn!(error = %format!("{error:#}"), "mangadex unmerge failed; will retry");
                }
            }
            if !nap(RETRY_DELAY, &mut shutdown).await {
                return;
            }
        }
    });
}

/// One attempt. `Ok(true)` once the repair is complete (now or before).
async fn run(st: &AppState, shutdown: &mut watch::Receiver<bool>) -> Result<bool> {
    let pool = &st.pool;
    if catalog::maintenance_flag_present(pool, FLAG).await? {
        return Ok(true);
    }
    let split = split_merged(pool).await?;
    let dismissed = crate::graphql::dismiss_distinct_reviews(pool).await?;
    tracing::info!(
        split,
        dismissed,
        "mangadex unmerge: merged entries split apart"
    );
    if st.mangadex.wait_enabled("mangadex-unmerge", shutdown).await {
        return Ok(false);
    }
    let pairs = split_mangadex_pairs(pool).await?;
    let refetched = refetch(st, &pairs, shutdown).await?;
    crate::db::retry_transaction(|| catalog::set_maintenance_flag(pool, FLAG)).await?;
    tracing::info!(pairs = pairs.len(), refetched, "mangadex unmerge: complete");
    Ok(true)
}

/// Step 1: give every MangaDex mapping but a work's first one its own work. Returns how
/// many mappings moved. Errors while a work still carries two, so the next attempt
/// tries again (a concurrent admin split makes one split here fail harmlessly).
async fn split_merged(pool: &Database) -> Result<u64> {
    const MERGED: &str = "SELECT work_id FROM source_series WHERE source_type = 'mangadex' \
                          GROUP BY work_id HAVING COUNT(*) > 1 ORDER BY work_id";
    let works: Vec<String> = sqlx::query_scalar(MERGED).fetch_all(pool).await?;
    let mut moved = 0u64;
    for work in &works {
        let mappings: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM source_series WHERE work_id = $1 AND source_type = 'mangadex' \
             ORDER BY created_at, id",
        )
        .bind(work)
        .fetch_all(pool)
        .await?;
        for mapping in mappings.iter().skip(1) {
            match catalog::split_source_series(pool, work, std::slice::from_ref(mapping), None)
                .await
            {
                Ok(out) => {
                    moved += out.moved_sources;
                    tracing::info!(
                        from = %work, to = %out.new_work_id, mapping = %mapping,
                        "mangadex unmerge: split a MangaDex entry off a merged work"
                    );
                }
                Err(error) => tracing::warn!(
                    work = %work, mapping = %mapping, error = %format!("{error:#}"),
                    "mangadex unmerge: split failed"
                ),
            }
        }
    }
    let left: Vec<String> = sqlx::query_scalar(MERGED).fetch_all(pool).await?;
    if !left.is_empty() {
        anyhow::bail!("{} works still carry two MangaDex entries", left.len());
    }
    Ok(moved)
}

/// Every pair of split-apart works that each carry one MangaDex mapping now.
async fn split_mangadex_pairs(pool: &Database) -> Result<Vec<(String, String)>> {
    Ok(sqlx::query_as(
        "SELECT ws.work_a, ws.work_b FROM work_split ws \
          WHERE (SELECT COUNT(*) FROM source_series s \
                  WHERE s.work_id = ws.work_a AND s.source_type = 'mangadex') = 1 \
            AND (SELECT COUNT(*) FROM source_series s \
                  WHERE s.work_id = ws.work_b AND s.source_type = 'mangadex') = 1 \
          ORDER BY ws.work_a, ws.work_b",
    )
    .fetch_all(pool)
    .await?)
}

/// Fetch every work of `pairs` that carries exactly one MangaDex mapping from MangaDex
/// again (metadata and the full cover set), drop what its partners' entries list and
/// its own does not (covers, credits, localized descriptions, alt titles, external
/// ids), then choose its cover again. Returns how many works were refetched.
///
/// A work MangaDex no longer returns is skipped. Needs the MangaDex switch on.
pub(crate) async fn refetch(
    st: &AppState,
    pairs: &[(String, String)],
    shutdown: &watch::Receiver<bool>,
) -> Result<usize> {
    let mut partners: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (a, b) in pairs {
        partners.entry(a.clone()).or_default().insert(b.clone());
        partners.entry(b.clone()).or_default().insert(a.clone());
    }
    let works: Vec<String> = partners.keys().cloned().collect();
    let anchors: Vec<(String, String)> = sqlx::query_as(
        "SELECT work_id, MIN(source_key) FROM source_series \
          WHERE work_id = ANY($1) AND source_type = 'mangadex' \
          GROUP BY work_id HAVING COUNT(*) = 1",
    )
    .bind(&works)
    .fetch_all(&st.pool)
    .await?;
    let anchor: BTreeMap<String, String> = anchors.into_iter().collect();
    let ids: Vec<String> = anchor.values().cloned().collect();
    let mut fetched: HashMap<String, (WorkInput, bool)> = HashMap::new();
    for chunk in ids.chunks(100) {
        for (id, input, covers) in crate::graphql::fetch_enrichment(st, chunk).await? {
            fetched.insert(id, (input, covers));
        }
    }

    let mut refetched = Vec::new();
    for (work, mangadex_id) in &anchor {
        let Some((input, covers)) = fetched.get(mangadex_id) else {
            tracing::warn!(work = %work, mangadex_id = %mangadex_id, "mangadex unmerge: MangaDex no longer lists this entry");
            continue;
        };
        let others: Vec<Partner<'_>> = partners[work]
            .iter()
            .filter_map(|p| {
                let (input, covers) = fetched.get(anchor.get(p)?)?;
                Some(Partner {
                    work: p,
                    input,
                    covers: *covers,
                })
            })
            .collect();
        crate::db::retry_transaction(|| {
            persist(&st.pool, work, mangadex_id, input, *covers, &others)
        })
        .await?;
        refetched.push(work);
    }

    let clients = crate::cover_choice::Clients {
        main: &st.pool,
        covers: &st.cover_pool,
        mangadex: &st.mangadex,
        suwayomi: &st.suwayomi,
        budgets: &st.source_budgets,
        shutdown,
        waiting: false,
    };
    for work in &refetched {
        if let Err(error) = crate::cover_choice::reselect_work(&clients, work, true).await {
            tracing::warn!(work = %work, error = %format!("{error:#}"), "mangadex unmerge: cover choice failed");
        }
    }
    Ok(refetched.len())
}

/// A split-apart partner of the work being refetched, with its own fresh record.
struct Partner<'a> {
    work: &'a str,
    input: &'a WorkInput,
    covers: bool,
}

/// One work's fresh record, in one transaction: the MangaDex identity keys, the work
/// and its partners locked in id order, external ids the merge moved onto a partner
/// moved back, the upsert, the partners' leftovers removed, the completion markers.
async fn persist(
    pool: &Database,
    work: &str,
    mangadex_id: &str,
    input: &WorkInput,
    covers: bool,
    partners: &[Partner<'_>],
) -> Result<()> {
    let mut tx = pool.begin().await?;
    crate::locks::advisory(&mut tx, &catalog::mangadex_upsert_keys(mangadex_id, input)).await?;
    if catalog::mangadex_owner_on(&mut tx, mangadex_id)
        .await?
        .as_deref()
        != Some(work)
    {
        // Moved since it was read: it is no longer this repair's to fix.
        return Ok(());
    }
    let mut locked: Vec<&str> = std::iter::once(work)
        .chain(partners.iter().map(|p| p.work))
        .collect();
    locked.sort_unstable();
    locked.dedup();
    crate::locks::lock_works(&mut tx, &locked).await?;
    if catalog::mangadex_owner_on(&mut tx, mangadex_id)
        .await?
        .as_deref()
        != Some(work)
    {
        return Err(crate::locks::RestartTransaction.into());
    }

    // `work_external_id` is unique per (provider, id), so the merge moved the partner's
    // AniList/MAL ids onto the survivor and the upsert cannot add them back. An id both
    // entries list stays where it is.
    for (provider, external) in &input.external_ids {
        for partner in partners {
            if partner
                .input
                .external_ids
                .iter()
                .any(|(p, e)| p == provider && e == external)
            {
                continue;
            }
            sqlx::query(
                "UPDATE work_external_id SET work_id = $1 \
                  WHERE provider = $2 AND external_id = $3 AND work_id = $4",
            )
            .bind(work)
            .bind(provider)
            .bind(external)
            .bind(partner.work)
            .execute(&mut *tx)
            .await?;
        }
    }

    catalog::upsert_work_from_mangadex_on(&mut tx, mangadex_id, input).await?;
    let barred = catalog::work_metadata::barred_groups_on(&mut tx, "mangadex").await?;
    if drop_partner_leftovers(&mut tx, work, input, covers, partners, &barred).await? {
        catalog::rebuild_work_alias_tokens(&mut tx, work).await?;
        catalog::upsert_browse_work_inner(&mut tx, work, true).await?;
    }
    let ids = [mangadex_id.to_string()];
    catalog::mark_metadata_synced_on(&mut tx, &ids).await?;
    if covers {
        catalog::mark_covers_synced_on(&mut tx, &ids).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Remove what a partner's MangaDex entry lists and this work's own entry does not.
/// Only rows that provably came from the partner go; anything else on the work stays.
/// A group MangaDex is barred from is left alone, since the upsert did not refresh it.
/// Returns whether alt titles were removed.
async fn drop_partner_leftovers(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    work: &str,
    input: &WorkInput,
    covers: bool,
    partners: &[Partner<'_>],
    barred: &HashSet<FieldGroup>,
) -> Result<bool> {
    if covers {
        let own: HashSet<&str> = input.covers.iter().map(|c| c.file_name.as_str()).collect();
        let theirs: Vec<&str> = partners
            .iter()
            .filter(|p| p.covers)
            .flat_map(|p| p.input.covers.iter().map(|c| c.file_name.as_str()))
            .filter(|name| !own.contains(name))
            .collect();
        sqlx::query("DELETE FROM work_cover WHERE work_id = $1 AND cover_file_name = ANY($2)")
            .bind(work)
            .bind(&theirs)
            .execute(&mut **tx)
            .await?;
    }

    if !barred.contains(&FieldGroup::Credits) {
        let own: HashSet<(&str, &str)> = input
            .credits
            .iter()
            .map(|(role, name)| (role.as_str(), name.trim()))
            .collect();
        let (roles, names): (Vec<&str>, Vec<&str>) = partners
            .iter()
            .flat_map(|p| p.input.credits.iter())
            .map(|(role, name)| (role.as_str(), name.trim()))
            .filter(|credit| !own.contains(credit))
            .unzip();
        sqlx::query(
            "DELETE FROM work_credit c USING UNNEST($2::text[], $3::text[]) AS u(role, name) \
              WHERE c.work_id = $1 AND c.role = u.role AND c.name = u.name",
        )
        .bind(work)
        .bind(&roles)
        .bind(&names)
        .execute(&mut **tx)
        .await?;
    }

    if !barred.contains(&FieldGroup::Description) {
        let own: HashSet<&str> = input.descriptions.iter().map(|(l, _)| l.as_str()).collect();
        let (langs, texts): (Vec<&str>, Vec<&str>) = partners
            .iter()
            .flat_map(|p| p.input.descriptions.iter())
            .map(|(lang, text)| (lang.as_str(), text.as_str()))
            .filter(|(lang, _)| !own.contains(lang))
            .unzip();
        sqlx::query(
            "DELETE FROM work_description d \
              USING UNNEST($2::text[], $3::text[]) AS u(lang, description) \
              WHERE d.work_id = $1 AND d.lang = u.lang AND d.description = u.description",
        )
        .bind(work)
        .bind(&langs)
        .bind(&texts)
        .execute(&mut **tx)
        .await?;
    }

    if barred.contains(&FieldGroup::Title) {
        return Ok(false);
    }
    let titles = |input: &WorkInput| -> Vec<String> {
        input
            .primary_title
            .iter()
            .chain(input.aliases.iter().map(|a| &a.raw))
            .map(|t| normalize_title(t))
            .filter(|t| !t.is_empty())
            .collect()
    };
    // Keep the titles this work's own entry, its engine sources and its admin title give.
    let mut keep: HashSet<String> = titles(input).into_iter().collect();
    let others: Vec<String> = sqlx::query_scalar(
        "SELECT c.value FROM work_metadata_contribution c \
           JOIN source_series ss ON ss.id = c.source_series_id \
          WHERE ss.work_id = $1 AND ss.source_type <> 'mangadex' \
            AND c.field_group = 'title' AND c.slot IN ('primary', 'alias') \
         UNION ALL \
         SELECT t FROM work w, LATERAL (VALUES (w.primary_title), (w.title_override)) v(t) \
          WHERE w.id = $1 AND t IS NOT NULL",
    )
    .bind(work)
    .fetch_all(&mut **tx)
    .await?;
    keep.extend(others.iter().map(|t| normalize_title(t)));
    let theirs: Vec<String> = partners
        .iter()
        .flat_map(|p| titles(p.input))
        .filter(|t| !keep.contains(t))
        .collect();
    let removed =
        sqlx::query("DELETE FROM work_alias WHERE work_id = $1 AND normalized_title = ANY($2)")
            .bind(work)
            .bind(&theirs)
            .execute(&mut **tx)
            .await?
            .rows_affected();
    Ok(removed > 0)
}

/// Sleep, waking early on shutdown. False when shutting down.
async fn nap(duration: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    if *shutdown.borrow() {
        return false;
    }
    tokio::select! {
        _ = tokio::time::sleep(duration) => !*shutdown.borrow(),
        changed = shutdown.changed() => changed.is_ok() && !*shutdown.borrow(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Alias, Cover};

    async fn pool() -> Database {
        let pool = Database::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations_pg")
            .run(pool.write())
            .await
            .unwrap();
        pool
    }

    const TITLE: &str = "Deriheru Yondara Moto Doukyuusei ga Kita";

    /// One MangaDex entry of the series: `tag` tells the two versions apart.
    fn version(tag: &str, anilist: &str) -> WorkInput {
        WorkInput {
            primary_title: Some(TITLE.into()),
            description: Some(format!("The {tag} version.")),
            descriptions: vec![
                ("en".into(), format!("The {tag} version.")),
                (
                    format!("{tag}-lang"),
                    format!("Only the {tag} version says this."),
                ),
            ],
            author: Some("Same Author".into()),
            credits: vec![
                ("author".into(), "Same Author".into()),
                ("artist".into(), format!("{tag} artist")),
            ],
            aliases: vec![
                Alias {
                    raw: TITLE.into(),
                    lang: Some("ja-ro".into()),
                },
                Alias {
                    raw: format!("{tag} alt title"),
                    lang: Some("en".into()),
                },
            ],
            external_ids: vec![("al".into(), anilist.into())],
            cover_file_name: Some(format!("{tag}.jpg")),
            covers: vec![Cover {
                file_name: format!("{tag}.jpg"),
                lang: Some("ja".into()),
                volume: Some("1".into()),
                is_primary: true,
            }],
            is_nsfw: true,
            ..Default::default()
        }
    }

    async fn rows(pool: &Database, sql: &str, work: &str) -> Vec<String> {
        let mut out: Vec<String> = sqlx::query_scalar(sql)
            .bind(work)
            .fetch_all(pool)
            .await
            .unwrap();
        out.sort();
        out
    }

    /// Fold `loser` into `survivor` the way the old duplicate sweep did, so the repair
    /// has a merged pair to work on (`merge_works` now refuses two MangaDex entries).
    async fn old_merge(pool: &Database, loser: &str, survivor: &str) {
        for sql in [
            "INSERT INTO work_alias (id, work_id, normalized_title, raw_title, lang) \
             SELECT 'x' || id, $2, normalized_title, raw_title, lang FROM work_alias \
              WHERE work_id = $1 ON CONFLICT DO NOTHING",
            "INSERT INTO work_cover (work_id, cover_file_name, lang, volume, is_primary) \
             SELECT $2, cover_file_name, lang, volume, FALSE FROM work_cover WHERE work_id = $1",
            "INSERT INTO work_credit (work_id, role, name) SELECT $2, role, name \
             FROM work_credit WHERE work_id = $1 ON CONFLICT DO NOTHING",
            "INSERT INTO work_description (work_id, lang, description) SELECT $2, lang, description \
             FROM work_description WHERE work_id = $1 ON CONFLICT DO NOTHING",
            "UPDATE work_external_id SET work_id = $2 WHERE work_id = $1",
            // The metadata backfill only recorded the first MangaDex mapping of a work.
            "DELETE FROM work_metadata_contribution WHERE source_series_id IN \
             (SELECT id FROM source_series WHERE work_id = $1)",
            "UPDATE source_series SET work_id = $2, created_at = now() WHERE work_id = $1",
            "DELETE FROM work WHERE id = $1 AND $2 <> ''",
        ] {
            sqlx::query(sql)
                .bind(loser)
                .bind(survivor)
                .execute(pool)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn two_mangadex_entries_are_refused_by_the_merge() {
        let pool = pool().await;
        let a = catalog::upsert_work_from_mangadex(&pool, "md-mature", &version("mature", "1"))
            .await
            .unwrap();
        let b = catalog::upsert_work_from_mangadex(&pool, "md-erotic", &version("erotic", "2"))
            .await
            .unwrap();
        let error = catalog::merge_works_ex(&pool, None, &b, &a)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains(catalog::MANGADEX_MERGE_REFUSED));
        let works: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(works, 2);
    }

    /// A merged mature + erotic pair comes apart, and each side ends up with its own
    /// entry's metadata, covers, credits, alt titles and AniList id, and nothing else.
    #[tokio::test]
    async fn a_merged_pair_is_split_and_each_side_gets_only_its_own_metadata() {
        let pool = pool().await;
        let mature = version("mature", "1");
        let erotic = version("erotic", "2");
        let a = catalog::upsert_work_from_mangadex(&pool, "md-mature", &mature)
            .await
            .unwrap();
        let b = catalog::upsert_work_from_mangadex(&pool, "md-erotic", &erotic)
            .await
            .unwrap();
        old_merge(&pool, &b, &a).await;
        // An engine copy of the series, merged in too.
        sqlx::query(
            "INSERT INTO source_series (id, work_id, source_type, source_key, created_at) \
             VALUES ('ss-site', $1, 'suwayomi', '77', now())",
        )
        .bind(&a)
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(split_merged(&pool).await.unwrap(), 1);
        let new: String = sqlx::query_scalar(
            "SELECT work_id FROM source_series WHERE source_type = 'mangadex' AND source_key = 'md-erotic'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_ne!(new, a);
        let pairs = split_mangadex_pairs(&pool).await.unwrap();
        assert_eq!(pairs.len(), 1);

        // What `refetch` does with the fresh records, without the network.
        let to_a = [Partner {
            work: &new,
            input: &erotic,
            covers: true,
        }];
        let to_new = [Partner {
            work: &a,
            input: &mature,
            covers: true,
        }];
        persist(&pool, &a, "md-mature", &mature, true, &to_a)
            .await
            .unwrap();
        persist(&pool, &new, "md-erotic", &erotic, true, &to_new)
            .await
            .unwrap();

        for (work, tag, other, anilist) in [
            (&a, "mature", "erotic", "1"),
            (&new, "erotic", "mature", "2"),
        ] {
            let description: Option<String> =
                sqlx::query_scalar("SELECT description FROM work WHERE id = $1")
                    .bind(work)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(
                description.as_deref(),
                Some(format!("The {tag} version.").as_str())
            );
            assert_eq!(
                rows(
                    &pool,
                    "SELECT cover_file_name FROM work_cover WHERE work_id = $1",
                    work
                )
                .await,
                [format!("{tag}.jpg")]
            );
            assert_eq!(
                rows(
                    &pool,
                    "SELECT name FROM work_credit WHERE work_id = $1",
                    work
                )
                .await,
                ["Same Author".to_string(), format!("{tag} artist")]
            );
            assert_eq!(
                rows(
                    &pool,
                    "SELECT lang FROM work_description WHERE work_id = $1",
                    work
                )
                .await,
                ["en".to_string(), format!("{tag}-lang")]
            );
            let aliases = rows(
                &pool,
                "SELECT raw_title FROM work_alias WHERE work_id = $1",
                work,
            )
            .await;
            assert!(aliases.contains(&format!("{tag} alt title")), "{aliases:?}");
            assert!(
                !aliases.contains(&format!("{other} alt title")),
                "{aliases:?}"
            );
            assert_eq!(
                rows(&pool, "SELECT external_id FROM work_external_id WHERE work_id = $1 AND provider = 'al'", work).await,
                [anilist.to_string()]
            );
        }
        // The engine source stays on the original.
        let site: String =
            sqlx::query_scalar("SELECT work_id FROM source_series WHERE id = 'ss-site'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(site, a);
    }

    #[tokio::test]
    async fn waiting_reviews_of_two_mangadex_entries_are_closed() {
        let pool = pool().await;
        let a = catalog::upsert_work_from_mangadex(&pool, "md-mature", &version("mature", "1"))
            .await
            .unwrap();
        let b = catalog::upsert_work_from_mangadex(&pool, "md-erotic", &version("erotic", "2"))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO merge_candidate (id, source_series_id, candidate_work_id, score, method, status, created_at) \
             SELECT 'mc1', id, $2, 0.9, 'title_exact', 'pending', now() FROM source_series WHERE work_id = $1",
        )
        .bind(&b)
        .bind(&a)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            crate::graphql::dismiss_distinct_reviews(&pool)
                .await
                .unwrap(),
            1
        );
        let status: String =
            sqlx::query_scalar("SELECT status FROM merge_candidate WHERE id = 'mc1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "dismissed");
    }
}
