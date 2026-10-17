//! M9 W3a: the work's own chapter list.
//!
//! A work has one stored list of entries (`work_chapter`). Every source copy (`chapter`
//! row) links to ONE entry (`work_chapter_link`), once, at match time:
//!
//! * A numbered copy links by its spine key (`round(number * 100)`): the entry whose
//!   `match_key` is that key, or a new one with `sort_key` = the number. A title word
//!   (extra, omake, special, side story, bonus) only marks a NEW entry special.
//! * An unnumbered copy gets its own special entry. Its `match_key` is `x:` plus the
//!   copy's stable source identity (the MangaDex chapter id, or the Suwayomi source id
//!   and chapter URL), so the same copy finds its entry again after it vanishes or after
//!   an engine reset. It is placed just after the entry released latest before it.
//!
//! A link is never recomputed: a refresh that renumbers or retitles a copy keeps its
//! entry. A copy that vanishes loses its link (cascade); its entry stays, and a returning
//! copy with the same `match_key` links back.
//!
//! The matcher runs inside the chapter write transactions, after the work and source
//! locks those writers already hold. It reads only unlinked copies, so a write that adds
//! nothing new writes nothing here.
//!
//! `work_chapter_state.ready` means "every copy of the work is linked". Only the backfill
//! drain ([`drain_once`]) sets it true, through [`mark_ready_on`]: that is the single
//! place a work becomes ready, and the place W3b converts legacy unticks.
use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use std::collections::HashMap;
use std::time::Duration;

use crate::db::Database;

/// One entry moved by a merge or split: `from` is the old entry id, `to` the entry the
/// copies (and, for W3b, progress) now belong to. `from == to` when the entry itself
/// moved to the other work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryMove {
    pub from: String,
    pub to: String,
}

/// Title words that mark a numbered entry special when the matcher creates it.
fn is_special_title(title: Option<&str>) -> bool {
    let Some(title) = title else { return false };
    let lower = title.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    words.iter().any(|w| {
        matches!(
            *w,
            "extra" | "extras" | "omake" | "omakes" | "special" | "specials" | "bonus"
        )
    }) || words
        .windows(2)
        .any(|p| p[0] == "side" && (p[1] == "story" || p[1] == "stories"))
}

/// One unlinked copy, with what the matcher needs.
#[derive(sqlx::FromRow, Debug)]
struct Copy {
    id: String,
    external_id: String,
    number: Option<String>,
    title: Option<String>,
    chapter_key: Option<String>,
    label: Option<String>,
    released: Option<DateTime<Utc>>,
    source_type: String,
    source_id: Option<String>,
}

const UNLINKED_COPY_SELECT: &str = "SELECT c.id, c.external_id, c.number, c.title, \
        c.chapter_key, c.label, COALESCE(c.readable_at, c.published_at) AS released, \
        ss.source_type, ss.source_id \
   FROM chapter c JOIN source_series ss ON ss.id = c.source_series_id \
  WHERE NOT EXISTS (SELECT 1 FROM work_chapter_link l WHERE l.chapter_id = c.id)";

/// Link the unlinked copies of one source mapping (all of them, or only the copy with
/// `external_id`). The caller holds the mapping's work and source locks (every chapter
/// writer does) and owns the transaction. Returns how many copies it linked.
pub(crate) async fn link_source_copies_on(
    conn: &mut PgConnection,
    source_series_id: &str,
    external_id: Option<&str>,
) -> Result<u64> {
    crate::catalog::guards::assert_in_transaction(conn, "work_chapter::link_source_copies_on");
    let Some(work_id): Option<String> =
        sqlx::query_scalar("SELECT work_id FROM source_series WHERE id = $1")
            .bind(source_series_id)
            .fetch_optional(&mut *conn)
            .await?
    else {
        return Ok(0);
    };
    let copies: Vec<Copy> = sqlx::query_as(&format!(
        "{UNLINKED_COPY_SELECT} AND c.source_series_id = $1 \
           AND ($2::text IS NULL OR c.external_id = $2)"
    ))
    .bind(source_series_id)
    .bind(external_id)
    .fetch_all(&mut *conn)
    .await?;
    link_copies_on(conn, &work_id, copies, false).await
}

/// The matcher. Numbered copies first (their entries are what specials are placed
/// against), then unnumbered ones in release order, undated last.
async fn link_copies_on(
    conn: &mut PgConnection,
    work_id: &str,
    copies: Vec<Copy>,
    defer_unkeyed: bool,
) -> Result<u64> {
    if copies.is_empty() {
        return Ok(0);
    }
    let now = Utc::now();
    let mut numbered: Vec<(Copy, String, f64)> = Vec::new();
    let mut unnumbered: Vec<Copy> = Vec::new();
    for c in copies {
        let label =
            crate::chapter_label::chapter_display(None, c.number.as_deref(), c.title.as_deref());
        match label.number() {
            Some(n) => {
                let key = label.key(&c.external_id);
                numbered.push((c, key, n));
            }
            None => unnumbered.push(c),
        }
    }
    let mut link_chapter: Vec<String> = Vec::new();
    let mut link_entry: Vec<String> = Vec::new();
    let mut created = 0u64;

    if !numbered.is_empty() {
        // One new entry per distinct key; the first copy of a key names it.
        let mut new_ids = Vec::new();
        let mut keys = Vec::new();
        let mut numbers = Vec::new();
        let mut labels = Vec::new();
        let mut specials = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (c, key, n) in &numbered {
            if seen.insert(key.clone()) {
                new_ids.push(super::new_id("wc_"));
                keys.push(key.clone());
                numbers.push(crate::chapter_label::format_number(*n));
                labels.push(
                    c.label
                        .clone()
                        .unwrap_or_else(|| crate::chapter_label::format_number(*n)),
                );
                specials.push(is_special_title(c.title.as_deref()));
            }
        }
        // A new numbered entry takes the work's existing override for its key: this is
        // the one-time copy of `chapter_override` onto the entry.
        created += sqlx::query(
            "INSERT INTO work_chapter \
                 (id, work_id, match_key, sort_key, number, label, special, special_by, \
                  hidden, title_override, created_at) \
             SELECT n.id, $1, n.key, n.key::numeric * 0.01, n.number, n.label, n.special, \
                    CASE WHEN n.special THEN 'matcher' END, COALESCE(co.hidden, false), \
                    co.title_override, $7 \
               FROM UNNEST($2::text[], $3::text[], $4::text[], $5::text[], $6::bool[]) \
                    AS n(id, key, number, label, special) \
               LEFT JOIN chapter_override co ON co.work_id = $1 AND co.chapter_key = n.key \
             ON CONFLICT (work_id, match_key) WHERE match_key IS NOT NULL DO NOTHING",
        )
        .bind(work_id)
        .bind(&new_ids)
        .bind(&keys)
        .bind(&numbers)
        .bind(&labels)
        .bind(&specials)
        .bind(now)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        let entries: HashMap<String, String> = sqlx::query_as::<_, (String, String)>(
            "SELECT match_key, id FROM work_chapter WHERE work_id = $1 AND match_key = ANY($2)",
        )
        .bind(work_id)
        .bind(&keys)
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .collect();
        for (c, key, _) in &numbered {
            let entry = entries
                .get(key)
                .ok_or_else(|| anyhow::anyhow!("work_chapter: no entry for key {key}"))?;
            link_chapter.push(c.id.clone());
            link_entry.push(entry.clone());
        }
    }

    // Numbered links first: the specials below are placed against them.
    let mut linked = if link_chapter.is_empty() {
        0
    } else {
        sqlx::query(
            "INSERT INTO work_chapter_link (chapter_id, work_chapter_id, linked_by, linked_at) \
             SELECT u.c, u.e, 'matcher', $3 FROM UNNEST($1::text[], $2::text[]) AS u(c, e) \
             ON CONFLICT (chapter_id) DO NOTHING",
        )
        .bind(&link_chapter)
        .bind(&link_entry)
        .bind(now)
        .execute(&mut *conn)
        .await?
        .rows_affected()
    };
    if !unnumbered.is_empty() {
        // Suwayomi copies take their stable identity from the active engine generation's
        // keys, captured in this same transaction before the spine write.
        let suwayomi: Vec<String> = unnumbered
            .iter()
            .filter(|c| c.source_type == "suwayomi")
            .map(|c| c.id.clone())
            .collect();
        let native: HashMap<String, NativeKey> = if suwayomi.is_empty() {
            HashMap::new()
        } else {
            sqlx::query_as::<_, NativeKey>(
                "SELECT c.id, emk.source_id, eck.chapter_key, \
                        COALESCE(emk.backfilled_at IS NOT NULL, false) AS backfilled \
                   FROM chapter c \
                   JOIN source_series ss ON ss.id = c.source_series_id \
                   LEFT JOIN engine_generation g ON g.state = 'active' \
                   LEFT JOIN engine_manga_key emk \
                     ON emk.generation = g.id AND emk.manga_id::text = ss.source_key \
                   LEFT JOIN engine_chapter_key eck \
                     ON eck.generation = emk.generation AND eck.manga_id = emk.manga_id \
                    AND eck.chapter_id::text = c.external_id \
                  WHERE c.id = ANY($1)",
            )
            .bind(&suwayomi)
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .map(|key| (key.id.clone(), key))
            .collect()
        };
        unnumbered.sort_by(|a, b| match (a.released, b.released) {
            (Some(x), Some(y)) => x.cmp(&y).then_with(|| a.id.cmp(&b.id)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.id.cmp(&b.id),
        });
        for c in &unnumbered {
            if defer_unkeyed
                && c.source_type == "suwayomi"
                && native
                    .get(&c.id)
                    .is_none_or(|key| key.chapter_key.is_none() && !key.backfilled)
            {
                continue;
            }
            let key = special_match_key(c, &native);
            let existing: Option<String> = sqlx::query_scalar(
                "SELECT id FROM work_chapter WHERE work_id = $1 AND match_key = $2",
            )
            .bind(work_id)
            .bind(&key)
            .fetch_optional(&mut *conn)
            .await?;
            let entry = match existing {
                Some(id) => id,
                None => {
                    let id = super::new_id("wc_");
                    let label = crate::chapter_label::chapter_display(
                        None,
                        c.number.as_deref(),
                        c.title.as_deref(),
                    )
                    .text();
                    sqlx::query(&format!(
                        "INSERT INTO work_chapter \
                             (id, work_id, match_key, sort_key, number, label, special, \
                              special_by, hidden, title_override, created_at) \
                         SELECT $1, $2, $3, ({PLACE_SQL}), NULL, $5, true, 'matcher', \
                                COALESCE(co.hidden, false), co.title_override, $6 \
                           FROM chapter c \
                           LEFT JOIN chapter_override co \
                             ON co.work_id = $2 AND co.chapter_key = c.chapter_key \
                          WHERE c.id = $7"
                    ))
                    .bind(&id)
                    .bind(work_id)
                    .bind(&key)
                    .bind(c.released)
                    .bind(c.label.clone().unwrap_or(label))
                    .bind(now)
                    .bind(&c.id)
                    .execute(&mut *conn)
                    .await?;
                    created += 1;
                    id
                }
            };
            // Linked now, so the next special of this batch is placed after it.
            sqlx::query(
                "INSERT INTO work_chapter_link (chapter_id, work_chapter_id, linked_by, linked_at) \
                 VALUES ($1, $2, 'matcher', $3) ON CONFLICT (chapter_id) DO NOTHING",
            )
            .bind(&c.id)
            .bind(&entry)
            .bind(now)
            .execute(&mut *conn)
            .await?;
            linked += 1;
        }
    }

    if created > 0 {
        bump_list_rev_on(conn, work_id).await?;
    }
    Ok(linked)
}

/// Where a new special goes, as one SQL expression over `$2` (work) and `$4` (release):
/// just after the entry released latest at or before it (an undated special: after the
/// last entry), or before the first entry when nothing came earlier. Specials live
/// strictly between two hundredths, so a numbered chapter never lands on one; `* 0.5`
/// keeps the midpoint exact (numeric division would round).
const PLACE_SQL: &str = "WITH released AS ( \
        SELECT wc.sort_key, MIN(COALESCE(c.readable_at, c.published_at)) AS at \
          FROM work_chapter wc \
          JOIN work_chapter_link l ON l.work_chapter_id = wc.id \
          JOIN chapter c ON c.id = l.chapter_id \
         WHERE wc.work_id = $2 \
         GROUP BY wc.id, wc.sort_key \
     ), lo AS ( \
        SELECT CASE WHEN $4::timestamptz IS NOT NULL \
                    THEN (SELECT sort_key FROM released WHERE at <= $4::timestamptz \
                           ORDER BY at DESC, sort_key DESC LIMIT 1) \
                    ELSE (SELECT MAX(sort_key) FROM work_chapter WHERE work_id = $2) END AS v \
     ), first AS ( \
        SELECT COALESCE(MIN(sort_key), 0) AS v FROM work_chapter WHERE work_id = $2 \
     ) \
     SELECT CASE WHEN lo.v IS NOT NULL THEN \
              (lo.v + LEAST(COALESCE((SELECT MIN(sort_key) FROM work_chapter \
                                       WHERE work_id = $2 AND sort_key > lo.v), \
                                     floor(lo.v * 100) * 0.01 + 0.01), \
                            floor(lo.v * 100) * 0.01 + 0.01)) * 0.5 \
            ELSE ((ceil(first.v * 100) - 1) * 0.01 + first.v) * 0.5 END \
       FROM lo, first";

#[derive(sqlx::FromRow)]
struct NativeKey {
    id: String,
    source_id: Option<String>,
    chapter_key: Option<String>,
    backfilled: bool,
}

/// The `x:` key of an unnumbered copy: its stable identity at its source.
fn special_match_key(c: &Copy, native: &HashMap<String, NativeKey>) -> String {
    match c.source_type.as_str() {
        "mangadex" => format!("x:{}", c.external_id),
        "suwayomi" => match native
            .get(&c.id)
            .and_then(|key| key.source_id.as_ref().zip(key.chapter_key.as_ref()))
        {
            Some((source, url)) => format!("x:{source}:{url}"),
            // No engine key for this copy yet: its numeric id is the only identity there
            // is. It does not survive an engine reset; the URL form does.
            None => format!(
                "x:{}:#{}",
                c.source_id.as_deref().unwrap_or(""),
                c.external_id
            ),
        },
        other => format!("x:{other}:{}", c.external_id),
    }
}

async fn bump_list_rev_on(conn: &mut PgConnection, work_id: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO work_chapter_state (work_id, ready, list_rev, updated_at) \
         VALUES ($1, false, 1, now()) \
         ON CONFLICT (work_id) DO UPDATE SET list_rev = work_chapter_state.list_rev + 1, \
                                             updated_at = now()",
    )
    .bind(work_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// THE place a work becomes ready. Only the drain calls it, with the work locked and no
/// unlinked copy left. W3b converts the work's legacy unticks here, in the same
/// transaction.
pub(crate) async fn mark_ready_on(conn: &mut PgConnection, work_id: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO work_chapter_state (work_id, ready, list_rev, updated_at) \
         VALUES ($1, true, 0, now()) \
         ON CONFLICT (work_id) DO UPDATE SET ready = true, updated_at = now() \
         WHERE NOT work_chapter_state.ready",
    )
    .bind(work_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

// ── Backfill drain ────────────────────────────────────────────────────────────────

/// Copies linked per transaction. A work with more runs over several transactions and
/// becomes ready in the last one.
const COPY_BATCH: i64 = 2_000;
/// Works visited per pass.
const WORK_BATCH: i64 = 50;
/// Copies linked per pass before it yields to the gap; this is the WAL throttle.
const PASS_BUDGET: u64 = 5_000;
const BATCH_GAP: Duration = Duration::from_millis(500);
const BOOT_DELAY: Duration = Duration::from_secs(120);
const IDLE_RECHECK: Duration = Duration::from_secs(10 * 60);

/// What one drain pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DrainOutcome {
    pub works_visited: u64,
    pub works_ready: u64,
    pub copies_linked: u64,
}

/// One pass over up to [`WORK_BATCH`] works that are not ready, in id order after
/// `after`. Returns the outcome and the cursor for the next pass (`None`: the end of
/// the work list was reached). The work list is derived from the data (no state row, or
/// `ready = false`), so a killed drain resumes by asking again; the cursor only stops
/// one failing work from starving the rest within a process.
pub async fn drain_once(
    pool: &Database,
    after: Option<&str>,
) -> Result<(DrainOutcome, Option<String>)> {
    let works: Vec<String> = sqlx::query_scalar(
        "SELECT w.id FROM work w \
          WHERE NOT EXISTS (SELECT 1 FROM work_chapter_state s \
                             WHERE s.work_id = w.id AND s.ready) \
            AND ($1::text IS NULL OR w.id > $1) \
          ORDER BY w.id LIMIT $2",
    )
    .bind(after)
    .bind(WORK_BATCH)
    .fetch_all(pool)
    .await?;
    let mut out = DrainOutcome::default();
    // The cursor names the last work this pass is done with; a work cut short by the
    // pass budget is visited again by the next pass.
    let mut cursor = after.map(str::to_string);
    for work in &works {
        out.works_visited += 1;
        loop {
            match crate::db::retry_transaction(|| drain_work_attempt(pool, work)).await {
                Ok((linked, ready)) => {
                    out.copies_linked += linked;
                    if ready {
                        out.works_ready += 1;
                        break;
                    }
                    if linked == 0 {
                        // Engine-key backfill has not reached the remaining specials.
                        break;
                    }
                    if out.copies_linked >= PASS_BUDGET {
                        return Ok((out, cursor));
                    }
                }
                Err(e) => {
                    // One failing work must not stall the rest; it is retried on the
                    // next walk over the work list.
                    crate::db::observe_database_error(&e);
                    tracing::warn!(work_id = %work, error = %e, "work chapters: drain of one work failed");
                    break;
                }
            }
        }
        cursor = Some(work.clone());
        if out.copies_linked >= PASS_BUDGET {
            return Ok((out, cursor));
        }
    }
    if (works.len() as i64) < WORK_BATCH {
        cursor = None;
    }
    Ok((out, cursor))
}

/// Link one batch of a work's copies; mark it ready once none is left. `ready` is also
/// true for a work that no longer exists (merged away or deleted): nothing to do.
async fn drain_work_attempt(pool: &Database, work_id: &str) -> Result<(u64, bool)> {
    let mut tx = pool.begin().await?;
    // The work lock excludes every chapter writer of its sources (they lock the work
    // first), merges and splits.
    let present: Option<String> =
        sqlx::query_scalar("SELECT id FROM work WHERE id = $1 FOR NO KEY UPDATE")
            .bind(work_id)
            .fetch_optional(&mut *tx)
            .await?;
    if present.is_none() {
        tx.rollback().await?;
        return Ok((0, true));
    }
    #[cfg(test)]
    crate::catalog::guards::pause("work-chapter-drain-locked").await;
    // Numbered copies first, then specials in release order, so a special is placed
    // against the numbered entries released before it.
    let copies: Vec<Copy> = sqlx::query_as(&format!(
        "{UNLINKED_COPY_SELECT} AND ss.work_id = $1 \
          ORDER BY COALESCE(c.chapter_key LIKE 'x:%', false), \
                   COALESCE(c.readable_at, c.published_at) NULLS LAST, c.id \
          LIMIT $2"
    ))
    .bind(work_id)
    .bind(COPY_BATCH)
    .fetch_all(&mut *tx)
    .await?;
    let linked = link_copies_on(&mut tx, work_id, copies, true).await?;
    let ready: bool = sqlx::query_scalar(
        "SELECT NOT EXISTS (SELECT 1 FROM chapter c \
           JOIN source_series ss ON ss.id = c.source_series_id \
          WHERE ss.work_id = $1 AND NOT EXISTS \
                (SELECT 1 FROM work_chapter_link l WHERE l.chapter_id = c.id))",
    )
    .bind(work_id)
    .fetch_one(&mut *tx)
    .await?;
    if ready {
        mark_ready_on(&mut tx, work_id).await?;
    }
    tx.commit().await?;
    Ok((linked, ready))
}

/// Rows still to do, for the log line and the gate.
pub async fn remaining(pool: &Database) -> Result<(i64, i64)> {
    Ok(sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM work w WHERE NOT EXISTS \
                   (SELECT 1 FROM work_chapter_state s WHERE s.work_id = w.id AND s.ready)), \
                (SELECT COUNT(*) FROM chapter c WHERE NOT EXISTS \
                   (SELECT 1 FROM work_chapter_link l WHERE l.chapter_id = c.id))",
    )
    .fetch_one(pool)
    .await?)
}

/// Run the drain in the background: paced, resumable, then a cheap idle recheck (new
/// works start not ready and are picked up here).
pub fn spawn(pool: Database, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    tokio::spawn(async move {
        async fn nap(d: Duration, shutdown: &mut tokio::sync::watch::Receiver<bool>) -> bool {
            tokio::select! {
                _ = tokio::time::sleep(d) => false,
                r = shutdown.changed() => r.is_err() || *shutdown.borrow(),
            }
        }
        if nap(BOOT_DELAY, &mut shutdown).await {
            return;
        }
        let mut cursor: Option<String> = None;
        // One walk is the work list from start to end; a walk that changed nothing (all
        // ready, or only failing works left) sleeps the long recheck.
        let mut walk = DrainOutcome::default();
        loop {
            if *shutdown.borrow() {
                return;
            }
            let gap = match drain_once(&pool, cursor.as_deref()).await {
                Ok((pass, next)) => {
                    walk.works_visited += pass.works_visited;
                    walk.works_ready += pass.works_ready;
                    walk.copies_linked += pass.copies_linked;
                    cursor = next;
                    if cursor.is_some() {
                        BATCH_GAP
                    } else {
                        let idle = walk.works_ready == 0 && walk.copies_linked == 0;
                        if !idle {
                            tracing::info!(
                                works_ready = walk.works_ready,
                                copies_linked = walk.copies_linked,
                                "work chapters: drain walked the work list"
                            );
                        }
                        walk = DrainOutcome::default();
                        if idle {
                            IDLE_RECHECK
                        } else {
                            BATCH_GAP
                        }
                    }
                }
                Err(e) => {
                    crate::db::observe_database_error(&e);
                    tracing::warn!(error = %e, "work chapters: drain pass failed; retrying later");
                    IDLE_RECHECK
                }
            };
            if nap(gap, &mut shutdown).await {
                return;
            }
        }
    });
}

/// `komika-server work-chapters status|drain`: what is left, or run the drain to the
/// end in the foreground at the background job's own pace (for a deploy check, or to
/// finish a large backfill on purpose). Prints rows, time and, on PostgreSQL, WAL.
pub async fn run_cli(pool: &Database, args: &[String]) -> Result<()> {
    let (works, copies) = remaining(pool).await?;
    println!("work-chapters: {works} work(s) not ready, {copies} unlinked copy(ies)");
    if args.first().map(String::as_str) != Some("drain") {
        return Ok(());
    }
    let wal_start: String = sqlx::query_scalar("SELECT pg_current_wal_lsn()::text")
        .fetch_one(pool)
        .await?;
    let started = std::time::Instant::now();
    let mut total = DrainOutcome::default();
    let mut cursor: Option<String> = None;
    let mut passes = 0u64;
    // One walk is the work list from the start to its end. A walk that changes nothing
    // ends the drain, also when a failing work is left behind (logged by drain_once).
    let mut walk = DrainOutcome::default();
    loop {
        let (pass, next) = drain_once(pool, cursor.as_deref()).await?;
        passes += 1;
        for t in [&mut total, &mut walk] {
            t.works_visited += pass.works_visited;
            t.works_ready += pass.works_ready;
            t.copies_linked += pass.copies_linked;
        }
        if next.is_none() {
            if walk.works_ready == 0 && walk.copies_linked == 0 {
                break;
            }
            walk = DrainOutcome::default();
        }
        cursor = next;
        tokio::time::sleep(BATCH_GAP).await;
    }
    let wal: i64 =
        sqlx::query_scalar("SELECT pg_wal_lsn_diff(pg_current_wal_lsn(), $1::pg_lsn)::bigint")
            .bind(&wal_start)
            .fetch_one(pool)
            .await?;
    let (works, copies) = remaining(pool).await?;
    println!(
        "work-chapters: drained in {:.1}s, {passes} pass(es): {} work(s) visited, {} ready, \
         {} copy(ies) linked, {wal} WAL bytes; left: {works} work(s), {copies} copy(ies)",
        started.elapsed().as_secs_f64(),
        total.works_visited,
        total.works_ready,
        total.copies_linked,
    );
    Ok(())
}

// ── Merge and split ───────────────────────────────────────────────────────────────

/// Fold `source_work`'s entries into `target_work`, inside the merge's transaction and
/// before the source work is deleted. An entry whose `match_key` the target already has
/// maps onto that entry (its links move; the target's overrides win, a loser override
/// fills a target entry that has none); every other entry moves as it is. Returns the
/// entry map. The target stays ready only if both works were.
pub(crate) async fn merge_entries_on(
    conn: &mut PgConnection,
    source_work: &str,
    target_work: &str,
) -> Result<Vec<EntryMove>> {
    crate::catalog::guards::assert_in_transaction(conn, "work_chapter::merge_entries_on");
    let pairs: Vec<(String, String)> = sqlx::query_as(
        "SELECT l.id, s.id FROM work_chapter l \
           JOIN work_chapter s ON s.work_id = $2 AND s.match_key = l.match_key \
          WHERE l.work_id = $1 ORDER BY l.id",
    )
    .bind(source_work)
    .bind(target_work)
    .fetch_all(&mut *conn)
    .await?;
    if !pairs.is_empty() {
        let (from, to): (Vec<String>, Vec<String>) = pairs.iter().cloned().unzip();
        sqlx::query(
            "UPDATE work_chapter s SET hidden = l.hidden, title_override = l.title_override \
               FROM UNNEST($1::text[], $2::text[]) AS m(f, t) \
               JOIN work_chapter l ON l.id = m.f \
              WHERE s.id = m.t AND NOT s.hidden AND s.title_override IS NULL \
                AND (l.hidden OR l.title_override IS NOT NULL)",
        )
        .bind(&from)
        .bind(&to)
        .execute(&mut *conn)
        .await?;
        sqlx::query(
            "UPDATE work_chapter_link k SET work_chapter_id = m.t \
               FROM UNNEST($1::text[], $2::text[]) AS m(f, t) WHERE k.work_chapter_id = m.f",
        )
        .bind(&from)
        .bind(&to)
        .execute(&mut *conn)
        .await?;
        sqlx::query("DELETE FROM work_chapter WHERE id = ANY($1)")
            .bind(&from)
            .execute(&mut *conn)
            .await?;
    }
    let moved: Vec<String> =
        sqlx::query_scalar("UPDATE work_chapter SET work_id = $2 WHERE work_id = $1 RETURNING id")
            .bind(source_work)
            .bind(target_work)
            .fetch_all(&mut *conn)
            .await?;
    // The merge already folded legacy overrides with survivor precedence.
    sync_numbered_entries_from_overrides_on(conn, target_work).await?;
    sqlx::query(
        "INSERT INTO work_chapter_state (work_id, ready, list_rev, updated_at) \
         VALUES ($2, false, 1, now()) \
         ON CONFLICT (work_id) DO UPDATE SET \
           ready = work_chapter_state.ready \
                   AND COALESCE((SELECT ready FROM work_chapter_state WHERE work_id = $1), false), \
           list_rev = work_chapter_state.list_rev + 1, updated_at = now()",
    )
    .bind(source_work)
    .bind(target_work)
    .execute(&mut *conn)
    .await?;
    let mut map: Vec<EntryMove> = pairs
        .into_iter()
        .map(|(from, to)| EntryMove { from, to })
        .collect();
    map.extend(moved.into_iter().map(|id| EntryMove {
        from: id.clone(),
        to: id,
    }));
    map.sort_by(|a, b| a.from.cmp(&b.from));
    Ok(map)
}

/// Entries follow the copies of `moving` source mappings from `from_work` to `to_work`,
/// inside the split's transaction (call it before or after the mappings move; it reads
/// copies by mapping id). Per entry with a moving copy:
///   * the destination already has its `match_key`: the moving links join that entry;
///   * every copy moves: the entry moves;
///   * copies on both sides: the entry stays, and the destination gets a new entry with
///     the same key, order and flags for the moving copies.
/// Entries with no copies stay. `to_is_new`: the destination was created by this split,
/// so only the drain may mark it ready.
pub(crate) async fn split_entries_on(
    conn: &mut PgConnection,
    from_work: &str,
    to_work: &str,
    moving: &[String],
    to_is_new: bool,
) -> Result<Vec<EntryMove>> {
    crate::catalog::guards::assert_in_transaction(conn, "work_chapter::split_entries_on");
    let affected: Vec<(String, Option<String>, bool, Option<String>)> = sqlx::query_as(
        "SELECT wc.id, wc.match_key, bool_and(c.source_series_id = ANY($2)), \
                (SELECT d.id FROM work_chapter d WHERE d.work_id = $3 AND d.match_key = wc.match_key) \
           FROM work_chapter wc \
           JOIN work_chapter_link l ON l.work_chapter_id = wc.id \
           JOIN chapter c ON c.id = l.chapter_id \
          WHERE wc.work_id = $1 \
          GROUP BY wc.id, wc.match_key \
         HAVING bool_or(c.source_series_id = ANY($2)) \
          ORDER BY wc.id",
    )
    .bind(from_work)
    .bind(moving)
    .bind(to_work)
    .fetch_all(&mut *conn)
    .await?;
    let mut map = Vec::with_capacity(affected.len());
    for (entry, _key, all_moving, dest) in affected {
        let to = match dest {
            Some(dest) => dest,
            None if all_moving => {
                sqlx::query("UPDATE work_chapter SET work_id = $2 WHERE id = $1")
                    .bind(&entry)
                    .bind(to_work)
                    .execute(&mut *conn)
                    .await?;
                map.push(EntryMove {
                    from: entry.clone(),
                    to: entry,
                });
                continue;
            }
            None => {
                let id = super::new_id("wc_");
                sqlx::query(
                    "INSERT INTO work_chapter \
                         (id, work_id, match_key, sort_key, number, label, special, special_by, \
                          hidden, title_override, created_at) \
                     SELECT $1, $2, match_key, sort_key, number, label, special, special_by, \
                            hidden, title_override, now() \
                       FROM work_chapter WHERE id = $3",
                )
                .bind(&id)
                .bind(to_work)
                .bind(&entry)
                .execute(&mut *conn)
                .await?;
                id
            }
        };
        sqlx::query(
            "UPDATE work_chapter_link l SET work_chapter_id = $2 \
               FROM chapter c \
              WHERE l.work_chapter_id = $1 AND c.id = l.chapter_id \
                AND c.source_series_id = ANY($3)",
        )
        .bind(&entry)
        .bind(&to)
        .bind(moving)
        .execute(&mut *conn)
        .await?;
        map.push(EntryMove { from: entry, to });
    }
    // Numbered overrides are dual-written to `chapter_override`, the fallback for copies
    // not linked yet. The destination's own rows win.
    sqlx::query(
        "INSERT INTO chapter_override (work_id, chapter_key, hidden, title_override, updated_at) \
         SELECT $1, match_key, hidden, title_override, now() FROM work_chapter \
          WHERE work_id = $1 AND match_key IS NOT NULL AND match_key NOT LIKE 'x:%' \
            AND (hidden OR title_override IS NOT NULL) \
         ON CONFLICT DO NOTHING",
    )
    .bind(to_work)
    .execute(&mut *conn)
    .await?;
    // Unlinked copies still rely on legacy overrides until the drain reaches them.
    sqlx::query(
        "INSERT INTO chapter_override (work_id, chapter_key, hidden, title_override, updated_at) \
         SELECT $2, co.chapter_key, co.hidden, co.title_override, now() \
           FROM chapter_override co WHERE co.work_id = $1 AND co.chapter_key NOT LIKE 'x:%' \
            AND EXISTS (SELECT 1 FROM chapter c \
                         WHERE c.source_series_id = ANY($3) AND c.chapter_key = co.chapter_key \
                           AND NOT EXISTS (SELECT 1 FROM work_chapter_link l WHERE l.chapter_id = c.id)) \
         ON CONFLICT DO NOTHING",
    )
    .bind(from_work)
    .bind(to_work)
    .bind(moving)
    .execute(&mut *conn)
    .await?;
    sync_numbered_entries_from_overrides_on(conn, to_work).await?;
    let from_ready: bool = sqlx::query_scalar(
        "SELECT COALESCE((SELECT ready FROM work_chapter_state WHERE work_id = $1), false)",
    )
    .bind(from_work)
    .fetch_one(&mut *conn)
    .await?;
    // Only the drain makes a new destination ready. An existing one stays ready
    // only if the source was too.
    sqlx::query(
        "INSERT INTO work_chapter_state (work_id, ready, list_rev, updated_at) \
         VALUES ($1, false, 1, now()) \
         ON CONFLICT (work_id) DO UPDATE SET \
           ready = NOT $2 AND work_chapter_state.ready AND $3, \
           list_rev = work_chapter_state.list_rev + 1, updated_at = now()",
    )
    .bind(to_work)
    .bind(to_is_new)
    .bind(from_ready)
    .execute(&mut *conn)
    .await?;
    bump_list_rev_on(conn, from_work).await?;
    Ok(map)
}

// ── Overrides ─────────────────────────────────────────────────────────────────────

/// Transferred legacy overrides also reach numbered entries already at the destination.
async fn sync_numbered_entries_from_overrides_on(
    conn: &mut PgConnection,
    work_id: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE work_chapter wc SET hidden = COALESCE(co.hidden, false), \
                                   title_override = co.title_override \
           FROM chapter_override co \
          WHERE wc.work_id = $1 AND co.work_id = $1 AND co.chapter_key = wc.match_key \
            AND wc.match_key NOT LIKE 'x:%' \
            AND (wc.hidden IS DISTINCT FROM COALESCE(co.hidden, false) \
                 OR wc.title_override IS DISTINCT FROM co.title_override)",
    )
    .bind(work_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// An admin edit of one entry. `None` leaves a field as it is.
#[derive(Debug, Default, Clone)]
pub struct EntryEdit {
    pub hidden: Option<bool>,
    /// `Some(None)` clears the title override.
    pub title_override: Option<Option<String>>,
    pub special: Option<bool>,
    /// A decimal, e.g. "10.005". Changing it reorders the list (documented, not
    /// compensated: progress pointers keep their entry).
    pub sort_key: Option<String>,
}

/// Apply `edit` to entry `entry_id` of `work_id` (locked by the caller). Numbered
/// entries also write `chapter_override` for their key, so the fallback for copies not
/// linked yet and an older server agree. Returns false when no such entry.
pub(crate) async fn edit_entry_on(
    conn: &mut PgConnection,
    work_id: &str,
    entry_id: &str,
    edit: &EntryEdit,
) -> Result<bool> {
    crate::catalog::guards::assert_in_transaction(conn, "work_chapter::edit_entry_on");
    let row: Option<(Option<String>, bool, Option<String>)> = sqlx::query_as(
        "UPDATE work_chapter SET \
           hidden = COALESCE($3, hidden), \
           title_override = CASE WHEN $4 THEN NULLIF(TRIM($5), '') ELSE title_override END, \
           special = COALESCE($6, special), \
           special_by = CASE WHEN $6 IS NULL THEN special_by ELSE 'admin' END, \
           sort_key = COALESCE($7::numeric, sort_key) \
         WHERE id = $1 AND work_id = $2 \
         RETURNING match_key, hidden, title_override",
    )
    .bind(entry_id)
    .bind(work_id)
    .bind(edit.hidden)
    .bind(edit.title_override.is_some())
    .bind(edit.title_override.clone().flatten())
    .bind(edit.special)
    .bind(edit.sort_key.as_deref())
    .fetch_optional(&mut *conn)
    .await?;
    let Some((match_key, hidden, title)) = row else {
        return Ok(false);
    };
    if let Some(key) = match_key.filter(|k| !k.starts_with("x:")) {
        write_override_row_on(conn, work_id, &key, hidden, title.as_deref()).await?;
    }
    bump_list_rev_on(conn, work_id).await?;
    Ok(true)
}

/// The legacy key form (`setChapterOverride(workId, chapterKey)`): after the caller
/// wrote `chapter_override`, matching entries (including links from legacy copy keys)
/// take the same state.
/// Gap 3 is gone: the key `"0"` reaches only chapter 0's entry, never an unnumbered one.
pub(crate) async fn sync_entry_from_override_on(
    conn: &mut PgConnection,
    work_id: &str,
    chapter_key: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE work_chapter wc SET \
           hidden = COALESCE(co.hidden, false), title_override = co.title_override \
           FROM (SELECT $1::text AS work_id, $2::text AS chapter_key) k \
           LEFT JOIN chapter_override co \
             ON co.work_id = k.work_id AND co.chapter_key = k.chapter_key \
          WHERE wc.work_id = $1 AND (wc.match_key = $2 OR EXISTS ( \
                SELECT 1 FROM work_chapter_link l \
                JOIN chapter c ON c.id = l.chapter_id \
                JOIN source_series ss ON ss.id = c.source_series_id \
                 WHERE l.work_chapter_id = wc.id AND ss.work_id = $1 AND c.chapter_key = $2 \
                   AND (wc.match_key LIKE 'x:%') = ($2 LIKE 'x:%')))",
    )
    .bind(work_id)
    .bind(chapter_key)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn write_override_row_on(
    conn: &mut PgConnection,
    work_id: &str,
    key: &str,
    hidden: bool,
    title: Option<&str>,
) -> Result<()> {
    if !hidden && title.is_none() {
        sqlx::query("DELETE FROM chapter_override WHERE work_id = $1 AND chapter_key = $2")
            .bind(work_id)
            .bind(key)
            .execute(&mut *conn)
            .await?;
    } else {
        sqlx::query(
            "INSERT INTO chapter_override (work_id, chapter_key, hidden, title_override, updated_at) \
             VALUES ($1, $2, $3, $4, now()) \
             ON CONFLICT (work_id, chapter_key) DO UPDATE SET hidden = excluded.hidden, \
               title_override = excluded.title_override, updated_at = excluded.updated_at",
        )
        .bind(work_id)
        .bind(key)
        .bind(hidden)
        .bind(title)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

// ── Reads ─────────────────────────────────────────────────────────────────────────

/// What a list shows for one copy: its entry, or (for a copy not linked yet) the
/// legacy override of its spine key.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CopyOverlay {
    pub entry_id: Option<String>,
    /// 1-based position over every entry, including hidden and copy-less entries.
    pub entry_order: Option<i64>,
    pub special: bool,
    pub hidden: bool,
    pub title_override: Option<String>,
}

/// One work's entry links and overrides, read in ONE statement and shared by every
/// chapter list of a request.
#[derive(Debug, Clone, Default)]
pub struct WorkEntries {
    pub ready: bool,
    by_copy: HashMap<(String, String), CopyOverlay>,
    by_key: HashMap<String, (bool, Option<String>)>,
}

impl WorkEntries {
    /// The overlay for a copy, by its source type, external id and spine key.
    pub fn overlay(&self, source_type: &str, external_id: &str, key: &str) -> CopyOverlay {
        if let Some(o) = self
            .by_copy
            .get(&(source_type.to_string(), external_id.to_string()))
        {
            return o.clone();
        }
        match self.by_key.get(key) {
            Some((hidden, title)) => CopyOverlay {
                hidden: *hidden,
                title_override: title.clone(),
                ..Default::default()
            },
            None => CopyOverlay::default(),
        }
    }
}

#[derive(sqlx::FromRow)]
struct EntryRow {
    kind: String,
    source_type: Option<String>,
    external_id: Option<String>,
    entry_id: Option<String>,
    entry_order: Option<i64>,
    special: Option<bool>,
    hidden: Option<bool>,
    title_override: Option<String>,
}

/// Load [`WorkEntries`] for `work_id`: copy links (`c` rows), legacy overrides (`o`) and
/// the ready flag (`s`), as one statement.
pub async fn work_entries(pool: &Database, work_id: &str) -> Result<WorkEntries> {
    let rows: Vec<EntryRow> = sqlx::query_as(
        "WITH ranked AS ( \
             SELECT id, row_number() OVER (ORDER BY sort_key, id) AS rank \
               FROM work_chapter WHERE work_id = $1 \
         ) \
         SELECT 'c' AS kind, ss.source_type, c.external_id, wc.id AS entry_id, \
                r.rank AS entry_order, wc.special, wc.hidden, wc.title_override \
           FROM source_series ss \
           JOIN chapter c ON c.source_series_id = ss.id \
           JOIN work_chapter_link l ON l.chapter_id = c.id \
           JOIN work_chapter wc ON wc.id = l.work_chapter_id \
           JOIN ranked r ON r.id = wc.id \
          WHERE ss.work_id = $1 \
         UNION ALL \
         SELECT 'o', NULL, co.chapter_key, NULL, NULL, NULL, co.hidden, co.title_override \
           FROM chapter_override co WHERE co.work_id = $1 \
         UNION ALL \
         SELECT 's', NULL, NULL, NULL, NULL, s.ready, NULL, NULL \
           FROM work_chapter_state s WHERE s.work_id = $1",
    )
    .bind(work_id)
    .fetch_all(pool)
    .await?;
    let mut out = WorkEntries::default();
    for r in rows {
        match r.kind.as_str() {
            "c" => {
                out.by_copy.insert(
                    (
                        r.source_type.unwrap_or_default(),
                        r.external_id.unwrap_or_default(),
                    ),
                    CopyOverlay {
                        entry_id: r.entry_id,
                        entry_order: r.entry_order,
                        special: r.special.unwrap_or(false),
                        hidden: r.hidden.unwrap_or(false),
                        title_override: r.title_override,
                    },
                );
            }
            "o" => {
                out.by_key.insert(
                    r.external_id.unwrap_or_default(),
                    (r.hidden.unwrap_or(false), r.title_override),
                );
            }
            _ => out.ready = r.special.unwrap_or(false),
        }
    }
    Ok(out)
}

/// One entry of the admin list, with its copies. Hidden entries and entries with no
/// copies are included.
#[derive(Debug, Clone)]
pub struct AdminEntry {
    pub id: String,
    pub match_key: Option<String>,
    pub sort_key: f64,
    pub sort_key_text: String,
    pub number: Option<String>,
    pub label: Option<String>,
    pub special: bool,
    pub special_by: Option<String>,
    pub hidden: bool,
    pub title_override: Option<String>,
    pub copies: Vec<AdminEntryCopy>,
}

#[derive(Debug, Clone)]
pub struct AdminEntryCopy {
    /// The copy's wire id: the MangaDex chapter id or the Suwayomi numeric id.
    pub chapter_id: String,
    pub source_series_id: String,
    pub source_type: String,
    pub source_id: Option<String>,
    pub number: Option<String>,
    pub title: Option<String>,
    pub lang: Option<String>,
    pub scanlator: Option<String>,
    pub released_at: Option<DateTime<Utc>>,
    pub linked_by: String,
}

/// The work's whole list, in order, for the admin editor. `ready` says whether reader
/// lists are built from it yet.
pub async fn admin_entry_list(pool: &Database, work_id: &str) -> Result<(bool, Vec<AdminEntry>)> {
    type Row = (
        String,
        Option<String>,
        f64,
        String,
        Option<String>,
        Option<String>,
        bool,
        Option<String>,
        bool,
        Option<String>,
    );
    let entries: Vec<Row> = sqlx::query_as(
        "SELECT id, match_key, sort_key::float8, sort_key::text, number, label, special, \
                special_by, hidden, title_override \
           FROM work_chapter wc WHERE wc.work_id = $1 ORDER BY wc.sort_key, wc.id",
    )
    .bind(work_id)
    .fetch_all(pool)
    .await?;
    type CopyRow = (
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<DateTime<Utc>>,
        String,
    );
    let copies: Vec<CopyRow> = sqlx::query_as(
        "SELECT l.work_chapter_id, c.external_id, ss.id, ss.source_type, ss.source_id, \
                c.number, c.title, c.lang, c.scanlator, \
                COALESCE(c.readable_at, c.published_at), l.linked_by \
           FROM work_chapter wc \
           JOIN work_chapter_link l ON l.work_chapter_id = wc.id \
           JOIN chapter c ON c.id = l.chapter_id \
           JOIN source_series ss ON ss.id = c.source_series_id \
          WHERE wc.work_id = $1 \
          ORDER BY ss.source_type DESC, ss.source_id NULLS FIRST, 10 NULLS FIRST, c.id",
    )
    .bind(work_id)
    .fetch_all(pool)
    .await?;
    let ready: bool = sqlx::query_scalar(
        "SELECT COALESCE((SELECT ready FROM work_chapter_state WHERE work_id = $1), false)",
    )
    .bind(work_id)
    .fetch_one(pool)
    .await?;
    let mut by_entry: HashMap<String, Vec<AdminEntryCopy>> = HashMap::new();
    for (
        entry,
        chapter_id,
        ssid,
        source_type,
        source_id,
        number,
        title,
        lang,
        scanlator,
        released,
        by,
    ) in copies
    {
        by_entry.entry(entry).or_default().push(AdminEntryCopy {
            chapter_id,
            source_series_id: ssid,
            source_type,
            source_id,
            number,
            title,
            lang,
            scanlator,
            released_at: released,
            linked_by: by,
        });
    }
    Ok((
        ready,
        entries
            .into_iter()
            .map(
                |(
                    id,
                    match_key,
                    sort_key,
                    sort_key_text,
                    number,
                    label,
                    special,
                    special_by,
                    hidden,
                    title_override,
                )| {
                    let copies = by_entry.remove(&id).unwrap_or_default();
                    AdminEntry {
                        id,
                        match_key,
                        sort_key,
                        sort_key_text,
                        number,
                        label,
                        special,
                        special_by,
                        hidden,
                        title_override,
                        copies,
                    }
                },
            )
            .collect(),
    ))
}

/// The work an entry belongs to, for the admin mutation's lock.
pub async fn entry_work(pool: &Database, entry_id: &str) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT work_id FROM work_chapter WHERE id = $1")
            .bind(entry_id)
            .fetch_optional(pool)
            .await?,
    )
}

#[cfg(test)]
mod unit_tests {
    use super::is_special_title;

    #[test]
    fn special_words_are_whole_words() {
        assert!(is_special_title(Some("Chapter 10.5 - Extra")));
        assert!(is_special_title(Some("Omake: beach day")));
        assert!(is_special_title(Some("Side Story 3")));
        assert!(is_special_title(Some("BONUS")));
        assert!(!is_special_title(Some("Extraordinary Day")));
        assert!(!is_special_title(Some("Chapter 3")));
        assert!(!is_special_title(None));
    }
}

#[cfg(test)]
#[path = "work_chapter_tests.rs"]
mod tests;
