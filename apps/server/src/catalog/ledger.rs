//! Phase C1 — the release ledger: who announced each chapter first, and when.
//!
//! `release_event` (migration 0091) answers one question the rest of the system could not
//! ask: *has this chapter already been announced?* `feed_series_updates` is keyed by WORK,
//! so it has no place to record that, and the consequence is F7 — a second source mirroring
//! a chapter a first source already reported re-floats the card to the top of /updates.
//!
//! Everything here exists to fill that table without ever putting a wrong time in it.
//!
//! THE ONE RULE THAT MATTERS: `first_seen_at` COMES FROM THE CHAPTER, NEVER FROM `now()`.
//! Seeding 1.3 M back-catalogue events with the current time would put the entire history
//! of the catalogue on page 1 of /updates, in one deploy, irreversibly as far as any user
//! is concerned. Every write below takes its time from
//! `COALESCE(chapter.readable_at, chapter.published_at)` and refuses anything it cannot
//! date — and [`assert_no_future_events`] is the backstop that proves it.

use crate::db::Database;
use anyhow::{bail, Result};

/// Works seeded per batch. Each is one grouped INSERT over that work's chapters.
const SEED_BATCH: i64 = 100;

/// The release clock, as SQL. `readable_at` when we have it, `published_at` otherwise —
/// §6.4's rule, and the reason is measured: MangaDex stamps external chapters
/// `publishAt = 2037-12-31`, and sampled bilibili chapters are `readableAt` a full two
/// weeks BEFORE their `publishAt`. The two are independent timestamps, not a
/// scheduled/actual pair, so `readable_at` is the one to sort and bound on.
const RELEASED_AT_SQL: &str = "COALESCE(c.readable_at, c.published_at)";

/// The ANNOUNCE clock, as SQL — when this chapter first became something we could put in
/// front of a reader. `?` binds "now", in millis.
///
/// This is the fix for bug (3): a chapter the source held behind early access is released
/// (per the source) days before we can see it, and ordering /updates by
/// [`RELEASED_AT_SQL`] therefore drops it into the feed already buried. Measured on a
/// 2026-08-11 snapshot: chapter 112.7 of `w_7d39e733…` released 2026-07-28 and reached our
/// ingest 2026-08-09.
///
/// THE `ss.created_at` GATE IS THE WHOLE DESIGN, NOT A DETAIL. Without it, `MAX(release,
/// now)` would stamp EVERY chapter of a newly-added series with the moment we added it —
/// so adding one series would float its entire back catalogue to the top of /updates, which
/// is the F7 flood the ledger was built to make impossible. The gate asks "were we already
/// watching this source when the chapter released?":
///
///   * YES → we were watching and did not have it, so the lateness is real. The chapter
///     became AVAILABLE when we first recorded it, and that is what /updates should order
///     by. `MAX(release, now)` and not bare `now`, so a chapter that is merely mirrored
///     promptly keeps its own release time rather than being nudged forward.
///   * NO → we started tracking this source AFTER the chapter released. Our first sighting
///     is when WE arrived, not when the chapter became available, and claiming otherwise
///     is exactly the invented timestamp this file's header rule forbids. Fall back to the
///     release clock.
///
/// `MAX(a, b)` here is SQLite's TWO-ARGUMENT SCALAR max, not the aggregate. That matters:
/// the statements below rely on having exactly ONE min/max aggregate so that their bare
/// columns (`label`, `source_series_id`) come from the winning row, and a second aggregate
/// would silently make those bare columns arbitrary.
const ANNOUNCE_AT_SQL: &str = "CASE WHEN ss.created_at < COALESCE(c.readable_at, c.published_at) THEN GREATEST(date_trunc('second', COALESCE(c.readable_at, c.published_at) + INTERVAL '500 microseconds'), $1) ELSE date_trunc('second', COALESCE(c.readable_at, c.published_at) + INTERVAL '500 microseconds') END";

/// WHAT COUNTS AS A SEEDABLE CHAPTER — the one definition, shared by the work-list, the
/// insert and the progress count. `?` binds "now", in millis.
///
/// These three MUST agree, and the first version of this file is the reason it is a
/// constant. The work-list asked only "does this chapter lack a ledger row?" while the
/// insert additionally refused chapters dated in the future, so a work holding a 2037
/// sentinel chapter was offered by the work-list forever, produced no insert, and was
/// offered again — an infinite seed loop that never reaches "complete" and re-runs the
/// query every second for the life of the process. A test caught it; a divergence in the
/// other direction (work-list narrower than the insert) would instead have left chapters
/// silently unrecorded, which is quieter and worse.
fn seedable_where(now_index: usize) -> String {
    let now_arg = crate::sql_params::placeholders(now_index, 1);
    format!("c.chapter_key IS NOT NULL AND {RELEASED_AT_SQL} IS NOT NULL AND date_trunc('second', {RELEASED_AT_SQL} + INTERVAL '500 microseconds') <= {now_arg}")
}

/// Seed the ledger for one batch of works. Returns how many works were seeded; `0` means
/// the ledger has caught up with the spine.
///
/// WHY THIS IS NOT A MIGRATION. The obvious place for a one-time seed is a `.sql` file, and
/// it cannot go there: at migration time the Suwayomi half of the spine does not exist yet
/// (Phase B's drains run in the background, after boot), so a migration-time seed would
/// permanently bake in a ledger that knows only the MangaDex half — and `INSERT OR IGNORE`
/// would then refuse to correct it, because the rows would already be there with the wrong
/// winner. The seed has to run after the spine, which means it has to run here.
///
/// WHY THERE IS NO CURSOR AND NO COMPLETION MARKER. The work-list is "a spine chapter with
/// no matching ledger row", derived from the data, so this is idempotent, resumable across
/// restarts, and self-healing when a work gains chapters later. The cost is that the idle
/// check is a scan rather than a flag read; at a 30-minute idle interval that is a few
/// seconds an hour, which is a fair price for having no second piece of state to get wrong.
pub async fn seed_batch(pool: &Database) -> Result<u64> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let works: Vec<String> = crate::db_types::query_scalar(&format!(
        "SELECT DISTINCT ss.work_id \
           FROM source_series ss \
           JOIN chapter c ON c.source_series_id = ss.id \
          WHERE {} \
            AND NOT EXISTS (SELECT 1 FROM release_event re \
                             WHERE re.work_id = ss.work_id \
                               AND re.chapter_key = c.chapter_key) \
          LIMIT $2",
        seedable_where(1)
    ))
    .bind(crate::db_types::epoch_millis(now_ms)?)
    .bind(SEED_BATCH)
    .fetch_all(pool)
    .await?;
    if works.is_empty() {
        return Ok(0);
    }
    let mut seeded = 0u64;
    for work_id in &works {
        seed_work(pool, work_id, now_ms).await?;
        seeded += 1;
    }
    Ok(seeded)
}

/// Seed (or top up) one work's ledger rows from its spine chapters.
///
/// `INSERT OR IGNORE` is what makes first-source-wins a storage property rather than a
/// comparison: a chapter key already in the ledger is left exactly as it is, whoever is
/// writing and whatever time they bring.
///
/// `first_source_series_id` and `label` are BARE COLUMNS alongside a single `MIN()`. That
/// is SQLite's documented "bare columns in an aggregate query" rule — with exactly one
/// min/max aggregate, every bare column is taken from the row that produced it — so they
/// come from the EARLIEST-released row, i.e. from the source that actually won the race.
/// The same rule is what `refresh_feed_series_updates` already relies on.
pub(crate) async fn seed_work(pool: &Database, work_id: &str, now_ms: i64) -> Result<u64> {
    crate::db::retry_transaction(|| async {
        let mut tx = pool.begin().await?;
        let Some(work_id) = super::guards::resolve_work_optional(&mut tx, work_id).await? else {
            return Ok(0);
        };
    let n = sqlx::query(&format!(
        "INSERT INTO release_event \
             (work_id, chapter_key, first_seen_at, first_source_series_id, label, announce_at) \
         SELECT DISTINCT ON (c.chapter_key) ss.work_id, c.chapter_key, \
                date_trunc('second', {RELEASED_AT_SQL} + INTERVAL '500 microseconds'), \
                c.source_series_id, \
                COALESCE(c.label, c.number, 'Oneshot'), \
                {ANNOUNCE_AT_SQL} \
           FROM source_series ss \
           JOIN chapter c ON c.source_series_id = ss.id \
          WHERE ss.work_id = $2 AND {} \
          ORDER BY c.chapter_key, date_trunc('second', {RELEASED_AT_SQL} + INTERVAL '500 microseconds') ASC, c.id ASC ON CONFLICT DO NOTHING",
        seedable_where(3)
    ))
    // THREE BINDS, AND THE ORDER IS THE SQL TEXT ORDER, NOT THE ARGUMENT ORDER. The
    // `ANNOUNCE_AT_SQL` placeholder sits in the SELECT list, so it is the FIRST `?` in the
    // statement; `seedable_where(3)`'s bound is the last. Both are `now_ms`.
    .bind(crate::db_types::epoch_millis(now_ms)?)
    .bind(&work_id)
    .bind(crate::db_types::epoch_millis(now_ms)?)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(n)
    }).await
}

/// Record release events for everything one source_series currently carries.
///
/// This is the incremental path — the "the moment a new chapter is found by the scanner it
/// goes to the updates page" requirement. It is deliberately the SAME statement shape as
/// the seed, narrowed to one series, so the two writers cannot disagree about what a
/// release event looks like.
pub async fn record_source_series(pool: &Database, source_series_id: &str) -> Result<u64> {
    crate::db::retry_transaction(|| async {
        let mut tx = pool.begin().await?;
        crate::locks::lock_source_series(&mut tx, &[source_series_id]).await?;
        super::guards::held().await;
        let n = record_source_series_on(&mut tx, source_series_id).await?;
        tx.commit().await?;
        Ok(n)
    })
    .await
}

/// [`record_source_series`] inside the caller's transaction (T194, T210). Neither
/// begins nor commits, and never retries: the caller owns BEGIN through COMMIT and wraps
/// that whole unit in `db::retry_transaction`.
///
/// Guards the CALLER must already hold: `locks::lock_source_series` for this mapping
/// (which locks its work parent first). The events are then recorded for the owner the
/// caller locked, in the same commit as the chapters they describe, so a merge or a
/// split of that mapping can neither run between the two nor see one without the other.
pub async fn record_source_series_on(
    conn: &mut sqlx::PgConnection,
    source_series_id: &str,
) -> Result<u64> {
    super::guards::assert_in_transaction(conn, "ledger::record_source_series_on");
    let now_ms = chrono::Utc::now().timestamp_millis();
    let n = sqlx::query(&format!(
        "INSERT INTO release_event \
             (work_id, chapter_key, first_seen_at, first_source_series_id, label, announce_at) \
         SELECT DISTINCT ON (c.chapter_key) ss.work_id, c.chapter_key, \
                date_trunc('second', {RELEASED_AT_SQL} + INTERVAL '500 microseconds'), \
                c.source_series_id, \
                COALESCE(c.label, c.number, 'Oneshot'), \
                {ANNOUNCE_AT_SQL} \
           FROM source_series ss \
           JOIN chapter c ON c.source_series_id = ss.id \
          WHERE ss.id = $2 AND {} \
          ORDER BY c.chapter_key, date_trunc('second', {RELEASED_AT_SQL} + INTERVAL '500 microseconds') ASC, c.id ASC ON CONFLICT DO NOTHING",
        seedable_where(3)
    ))
    // Bind order is SQL TEXT order — see `seed_work`.
    .bind(crate::db_types::epoch_millis(now_ms)?)
    .bind(source_series_id)
    .bind(crate::db_types::epoch_millis(now_ms)?)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(n)
}

/// Move a merged-away work's release events onto its survivor, KEEPING THE EARLIEST
/// first-seen per chapter key.
///
/// Must run inside the merge transaction, BEFORE `DELETE FROM work`, or `ON DELETE CASCADE`
/// takes the history with it.
///
/// Getting the direction wrong here is not a subtle bug. `release_event` is what stops a
/// chapter being announced twice; if a merge kept the LATER of the two first-seen times,
/// every dedup merge would re-float the merged-in work's whole back catalogue to the top of
/// /updates — the F7 symptom, reintroduced by the fix for it.
pub async fn merge_release_events(
    tx: &mut sqlx::PgConnection,
    source_work: &str,
    target_work: &str,
) -> Result<u64> {
    let n = sqlx::query(
        "INSERT INTO release_event \
             (work_id, chapter_key, first_seen_at, first_source_series_id, label, announce_at) \
         SELECT $1, chapter_key, first_seen_at, first_source_series_id, label, announce_at \
           FROM release_event WHERE work_id = $2 \
         ON CONFLICT(work_id, chapter_key) DO UPDATE SET \
             first_source_series_id = CASE \
                 WHEN excluded.first_seen_at < release_event.first_seen_at \
                 THEN excluded.first_source_series_id \
                 ELSE release_event.first_source_series_id END, \
             label = CASE \
                 WHEN excluded.first_seen_at < release_event.first_seen_at \
                 THEN excluded.label ELSE release_event.label END, \
             -- The announce clock takes the EARLIEST of the two, for the same reason
             -- `first_seen_at` does: a merge that kept the later one would re-announce the
             -- merged-in work's whole back catalogue. COALESCEd on both sides so a row that
             -- predates migration 0099 (NULL) contributes its release clock rather than
             -- losing to a NULL, which is the identity this column is deployed with.
             announce_at = LEAST(COALESCE(release_event.announce_at, release_event.first_seen_at), \
                               COALESCE(excluded.announce_at, excluded.first_seen_at)), \
             -- Safe to assign alongside the CASEs that read it: every right-hand side in a
             -- SET list is evaluated against the PRE-UPDATE row, so `release_event
             -- .first_seen_at` above is the stored value regardless of assignment order.
             first_seen_at = LEAST(release_event.first_seen_at, excluded.first_seen_at)",
    )
    .bind(target_work)
    .bind(source_work)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    Ok(n)
}

/// The deployment guard from §9's risk table: no event may claim to have happened in the
/// future. A single future row means something dated an event from `publishAt`, or from
/// `now()`, and page 1 of /updates is about to be wrong.
pub async fn assert_no_future_events(pool: &Database) -> Result<()> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let (future, max): (i64, Option<chrono::DateTime<chrono::Utc>>) = crate::db_types::query_tuple(
        "SELECT COUNT(*), MAX(first_seen_at) FROM release_event WHERE first_seen_at > $1",
    )
    .bind(crate::db_types::epoch_millis(now_ms)?)
    .fetch_one(pool)
    .await?;
    if future > 0 {
        bail!(
            "release ledger holds {future} event(s) in the future (latest {:?}, now {now_ms}) — \
             something dated a release from publishAt or from now()",
            max
        );
    }
    Ok(())
}

/// Is the ledger caught up with the spine — i.e. is it safe to let the feed's release clock
/// be projected from it (Phase C2)?
///
/// THE GATE EXISTS BECAUSE A PARTIAL LEDGER IS WORSE THAN NO LEDGER. `project_feed_from_ledger`
/// replaces a work's `released_at` with `MAX(first_seen_at)` over its events. While the seed
/// is still running, a work may have three of its four hundred chapters recorded — and the
/// newest is almost certainly not among them, because the seed walks works, not recency. Its
/// card would take the release time of an old chapter and SINK, visibly, on the live feed,
/// for as long as the seed takes. Every such card would then silently recover, which is the
/// kind of regression nobody reports and nobody can reproduce.
///
/// So the projection is inert until this returns true, and the feed keeps behaving exactly as
/// it does today. It then switches on by itself, atomically, at the next rebuild.
pub async fn is_complete(pool: &Database) -> Result<bool> {
    // THE MEMO IS PRODUCTION-ONLY, and that is about test isolation, not about the memo
    // being untestable: it is process-global state, and the test binary runs every test in
    // one process, in parallel, each against its own in-memory database — so one test
    // driving a ledger to completion would make every other test believe its own empty
    // ledger was ready. Under `cfg(test)` each call therefore gets a FRESH memo, which
    // (last check at the epoch, not latched) always falls through to the honest check, i.e.
    // exactly the behaviour the ledger tests were written against.
    //
    // The memo itself is covered by `the_ready_memo_*` tests, which drive
    // [`is_complete_memoised`] directly with their own [`ReadyMemo`] and their own clock.
    // Before this split it was `#[cfg(not(test))]` code with NO test of any kind reaching it
    // (§8h caveat c) — an untested one-way latch on the gate that decides whether a
    // half-seeded ledger is allowed to move live cards.
    #[cfg(not(test))]
    let memo = &READY;
    #[cfg(test)]
    let memo = &ReadyMemo::new();
    is_complete_memoised(pool, memo, chrono::Utc::now().timestamp_millis()).await
}

/// [`is_complete`] against an explicit memo and an explicit clock.
///
/// MEMOISED, BECAUSE THE HOT PATH CANNOT AFFORD THE QUERY. The honest check below scans
/// 1.44 M `chapter` rows with a primary-key probe each — a few seconds. That is fine once a
/// day. It is NOT fine on the callers that matter: the per-work projection runs once per
/// touched series per firehose PAGE (`catalog::publish_mirror_feed_row`) and once per
/// scanner detection (`scanner::upsert_feed_series_update`), so an
/// un-memoised check would put a multi-second scan inside the 15-minute chapter cycle, which
/// is precisely the kind of cost Phase D exists to remove.
///
/// While still false, re-check at most once a `READY_RECHECK_MS`, so the seed's own progress
/// is noticed promptly without the check itself becoming the load.
async fn is_complete_memoised(pool: &Database, memo: &ReadyMemo, now_ms: i64) -> Result<bool> {
    if let Some(answer) = memo.poll(now_ms) {
        return Ok(answer);
    }
    if crate::catalog::spine::remaining(pool).await? != (0, 0) {
        return Ok(false);
    }
    let (events, pending) = remaining(pool).await?;
    let ready = events > 0 && pending == 0;
    if ready && memo.latch() {
        tracing::info!(
            events,
            "release ledger complete — the feed clock is now projected from it"
        );
    }
    Ok(ready)
}

/// The one-way "the ledger is caught up" latch behind [`is_complete`], plus the
/// while-still-false rate limiter.
///
/// ## Why ONCE TRUE, ALWAYS TRUE is sound
///
/// Completeness means "every seedable spine chapter has a ledger row", and after the seed the
/// only things that add spine chapters are the same writers that record the event in the same
/// breath. A work that somehow slips through is caught by the daily reconciler, which reports
/// it as drift rather than pretending it is clean.
///
/// ## Why an EARLY latch is not possible
///
/// Latching requires BOTH `spine::remaining() == (0, 0)` and `events > 0 && pending == 0`.
/// The `events > 0` conjunct is what rules out the degenerate case §8h flagged: a database
/// with no seedable chapters answers `pending == 0` vacuously, but it also has no events, so
/// it cannot latch. A mid-seed database has `pending > 0` by construction (`seed_batch` and
/// `pending` share `seedable_where`). What remains is a database with exactly one event and
/// zero seedable chapters — i.e. an essentially empty one, which is not a state production
/// reaches with 1.44 M mirrored chapters.
///
/// ## And it is RECOVERABLE if it ever does
///
/// [`ReadyMemo::forget`] drops the latch, so a process that has latched can be put back into
/// re-checking without a restart. That is the honest answer to "a restore cannot flip it
/// back": the latch is an optimisation over a query whose answer is entirely in the database,
/// so forgetting it is always safe — the worst case is one slow honest check.
pub(crate) struct ReadyMemo {
    ready: std::sync::atomic::AtomicBool,
    last_check_ms: std::sync::atomic::AtomicI64,
}

impl ReadyMemo {
    pub(crate) const fn new() -> Self {
        Self {
            ready: std::sync::atomic::AtomicBool::new(false),
            // The epoch, so the first call always runs the honest check.
            last_check_ms: std::sync::atomic::AtomicI64::new(i64::MIN),
        }
    }

    /// `Some(answer)` to short-circuit, `None` meaning "run the honest check now" — in which
    /// case the recheck window is armed, whatever the answer turns out to be.
    fn poll(&self, now_ms: i64) -> Option<bool> {
        use std::sync::atomic::Ordering;
        if self.ready.load(Ordering::Relaxed) {
            return Some(true);
        }
        // Saturating, because `last_check_ms` starts at `i64::MIN`: a plain subtraction
        // overflows there and would panic in debug, which is a poor way for a cache to fail.
        if now_ms.saturating_sub(self.last_check_ms.load(Ordering::Relaxed)) < READY_RECHECK_MS {
            return Some(false);
        }
        self.last_check_ms.store(now_ms, Ordering::Relaxed);
        None
    }

    /// Latch. Returns whether THIS call was the transition, so the caller logs once.
    fn latch(&self) -> bool {
        !self.ready.swap(true, std::sync::atomic::Ordering::Relaxed)
    }

    /// Drop the latch and the recheck window. See the struct doc: always safe, because the
    /// answer lives in the database and this only ever costs one honest check.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn forget(&self) {
        use std::sync::atomic::Ordering;
        self.ready.store(false, Ordering::Relaxed);
        self.last_check_ms.store(i64::MIN, Ordering::Relaxed);
    }
}

#[cfg(not(test))]
static READY: ReadyMemo = ReadyMemo::new();
/// How often to re-ask while the answer is still "no". Long enough that the scan is not the
/// load, short enough that the seed's completion is noticed within a chapter cycle.
const READY_RECHECK_MS: i64 = 60_000;

/// What one reconciler pass found. `drifted == 0` is the healthy state and the thing the
/// pass exists to assert.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DriftReport {
    pub rows_before: i64,
    pub rows_after: i64,
    pub drifted: i64,
    pub sample: Vec<String>,
    pub duration_ms: i64,
}

/// The columns drift is measured on: a work's release clock and its chapter label. These
/// are what the incremental writers maintain and what /updates sorts and prints, so a
/// disagreement here is a user-visible one. Title, cover and comic_type are deliberately
/// excluded — the rebuild legitimately refreshes those from `work`, and counting them would
/// make every run report drift that means nothing.
const DRIFT_COLUMNS: &str = "SELECT work_id, released_at, latest_chapter FROM feed_series_updates";

/// PHASE C3 — run the wholesale refresh chain and REPORT what it had to change.
///
/// Before C2 this chain was the mechanism that kept /updates correct, and it ran every
/// `CATALOGUE_SYNC_INTERVAL_SECS`. Now that both halves have incremental writers, it runs
/// daily and its output is a measurement: **how much did the incremental path miss?**
///
/// The measurement is the whole point. "We added incremental writers" is a claim; a run
/// that rebuilds from scratch and finds it changed nothing is evidence. And when it does
/// find drift, the sample says where to look instead of leaving a bare count.
///
/// Note the reconciler CORRECTS as it measures — it is a real rebuild, not a dry run. So a
/// non-zero `drifted` describes a window that has already closed, not a live fault.
pub async fn reconcile_feed(pool: &Database) -> Result<DriftReport> {
    use std::collections::HashMap;
    let started = std::time::Instant::now();

    let snapshot: HashMap<String, (i64, Option<String>)> =
        crate::db_types::query_tuple::<(String, i64, Option<String>)>(DRIFT_COLUMNS)
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|(w, r, l)| (w, (r, l)))
            .collect();

    // A chain error is returned only after the report is written: a late stage (Browse,
    // search) can fail after the feed was already corrected, and the follow-up run would
    // then find nothing left to report. An early failure leaves the feed as it was, so
    // its report shows no drift and the follow-up measures the real one.
    let rebuilt = crate::catalog::refresh_feed_updates(pool).await;
    let reported = write_drift_report(pool, &snapshot, started).await;
    match rebuilt {
        Ok(_) => reported,
        Err(error) => {
            // The chain's error is the one the reconciler must act on and log.
            if let Err(report_error) = reported {
                crate::db::observe_database_error(&report_error);
                tracing::warn!(error = %report_error, "feed reconciler: drift report failed");
            }
            Err(error)
        }
    }
}

/// Compare `feed_series_updates` with the rows captured before the rebuild and store the
/// result as the single `feed_reconcile_report` row.
async fn write_drift_report(
    pool: &Database,
    snapshot: &std::collections::HashMap<String, (i64, Option<String>)>,
    started: std::time::Instant,
) -> Result<DriftReport> {
    let after: Vec<(String, i64, Option<String>)> = crate::db_types::query_tuple(DRIFT_COLUMNS)
        .fetch_all(pool)
        .await?;

    let mut drifted = 0i64;
    let mut sample = Vec::new();
    for (work_id, released_at, label) in &after {
        match snapshot.get(work_id) {
            Some((was_at, was_label)) if *was_at == *released_at && was_label == label => {}
            Some((was_at, was_label)) => {
                drifted += 1;
                if sample.len() < 8 {
                    sample.push(format!(
                        "{work_id}: {was_at}/{was_label:?} -> {released_at}/{label:?}"
                    ));
                }
            }
            // A row the rebuild ADDED. Expected for a brand-new work: the incremental
            // projection updates existing rows and does not create them (see §7z), so this
            // is the residual Phase D's tighter cycle shrinks rather than a fault.
            None => {
                drifted += 1;
                if sample.len() < 8 {
                    sample.push(format!("{work_id}: new row"));
                }
            }
        }
    }

    let report = DriftReport {
        rows_before: snapshot.len() as i64,
        rows_after: after.len() as i64,
        drifted,
        sample,
        duration_ms: started.elapsed().as_millis() as i64,
    };
    sqlx::query(
        "INSERT INTO feed_reconcile_report \
             (id, ran_at, duration_ms, rows_before, rows_after, drifted, sample) \
         VALUES (1, $1, $2, $3, $4, $5, $6) \
         ON CONFLICT(id) DO UPDATE SET \
             ran_at = excluded.ran_at, duration_ms = excluded.duration_ms, \
             rows_before = excluded.rows_before, rows_after = excluded.rows_after, \
             drifted = excluded.drifted, sample = excluded.sample",
    )
    .bind(chrono::Utc::now())
    .bind(report.duration_ms)
    .bind(report.rows_before)
    .bind(report.rows_after)
    .bind(report.drifted)
    .bind(report.sample.join(" | "))
    .execute(pool)
    .await?;
    Ok(report)
}

/// How many ledger rows exist, and how many spine chapters still have none.
pub async fn remaining(pool: &Database) -> Result<(i64, i64)> {
    let events: i64 = crate::db_types::query_scalar("SELECT COUNT(*) FROM release_event")
        .fetch_one(pool)
        .await?;
    let pending: i64 = crate::db_types::query_scalar(&format!(
        "SELECT COUNT(*) FROM source_series ss \
           JOIN chapter c ON c.source_series_id = ss.id \
          WHERE {} \
            AND NOT EXISTS (SELECT 1 FROM release_event re \
                             WHERE re.work_id = ss.work_id \
                               AND re.chapter_key = c.chapter_key)",
        seedable_where(1)
    ))
    .bind(chrono::Utc::now())
    .fetch_one(pool)
    .await?;
    Ok((events, pending))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{
        find_source_series_id, replace_source_chapters, spine, suwayomi_spine_input,
        upsert_source_series, upsert_work_from_mangadex, ChapterInput, WorkInput,
    };

    /// `foreign_keys = ON` deliberately, matching `db.rs`. The cascade from `work` is
    /// load-bearing here — it is exactly what `merge_release_events` has to get ahead of.
    async fn pool() -> Database {
        let pool = Database::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations_pg")
            .run(pool.write())
            .await
            .unwrap();
        pool
    }

    /// Millis for an ISO instant, so tests can state expected times readably.
    fn ms(iso: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(iso)
            .unwrap()
            .timestamp_millis()
    }

    async fn work_with_two_sources(pool: &Database) -> (String, String, String) {
        // A title, because `feed_updates.title` is NOT NULL and the C3 reconciler builds
        // the real feed chain rather than a stub of it.
        let work = upsert_work_from_mangadex(
            pool,
            "md-led",
            &WorkInput {
                primary_title: Some("Both Halves".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let md = find_source_series_id(pool, "mangadex", "mangadex", "md-led")
            .await
            .unwrap()
            .unwrap();
        upsert_source_series(pool, &work, "suwayomi", "asura", "1", None, false)
            .await
            .unwrap();
        let sy = find_source_series_id(pool, "suwayomi", "asura", "1")
            .await
            .unwrap()
            .unwrap();
        (work, md, sy)
    }

    /// THE OWNER'S REQUIREMENT, VERBATIM: "if x source updates chapter y first, it will be
    /// registered in the updates page and if more sources update the chapters later, they
    /// won't hit the updates page since another extension updated it earlier."
    ///
    /// The mechanism is `PRIMARY KEY (work_id, chapter_key)` + `INSERT OR IGNORE`, so the
    /// later source's write is a no-op at the storage layer — there is no comparison to get
    /// backwards. Today's `released_at = MAX(...)` does the exact opposite (F7).
    #[tokio::test]
    async fn a_later_source_cannot_re_announce_a_chapter_someone_else_had_first() {
        let pool = pool().await;
        let (work, md, sy) = work_with_two_sources(&pool).await;

        // Suwayomi publishes chapter 10 on the 1st.
        replace_source_chapters(
            &pool,
            &sy,
            &[suwayomi_spine_input(
                1,
                "Chapter 10",
                10.0,
                None,
                Some(&ms("2026-06-01T00:00:00Z").to_string()),
            )],
        )
        .await
        .unwrap();
        // MangaDex mirrors the SAME chapter 10 a week later.
        crate::catalog::upsert_chapter(
            &pool,
            &md,
            &ChapterInput {
                external_id: "u-10".into(),
                number: Some("10".into()),
                lang: Some("en".into()),
                readable_at: Some("2026-06-08T00:00:00Z".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        while seed_batch(&pool).await.unwrap() > 0 {}

        let rows: Vec<(String, i64, Option<String>)> = crate::db_types::query_tuple(
            "SELECT chapter_key, first_seen_at, first_source_series_id FROM release_event \
             WHERE work_id = $1",
        )
        .bind(&work)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "one chapter, one event — not one per source");
        assert_eq!(rows[0].0, "1000", "the shared round(number*100) key");
        assert_eq!(
            rows[0].1,
            ms("2026-06-01T00:00:00Z"),
            "the FIRST source's time, not the later mirror's"
        );
        assert_eq!(
            rows[0].2.as_deref(),
            Some(sy.as_str()),
            "credited to Suwayomi"
        );

        // Re-running every writer must not move the card. This is the regression that
        // `released_at = MAX(...)` could never be made safe against.
        record_source_series(&pool, &md).await.unwrap();
        while seed_batch(&pool).await.unwrap() > 0 {}
        let after: i64 = crate::db_types::query_scalar(
            "SELECT first_seen_at FROM release_event WHERE work_id = $1",
        )
        .bind(&work)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            after,
            ms("2026-06-01T00:00:00Z"),
            "the clock never moves forward"
        );
    }

    /// REQUIREMENT 3, AND THE ONE CLAUSE THE WHOLE FEATURE BALANCES ON.
    ///
    /// [`ANNOUNCE_AT_SQL`] is a single `CASE` on `ss.created_at < release_clock`, and it has
    /// to be right in BOTH directions or it converts one bug into a worse one:
    ///
    ///   * too narrow → a chapter the source held behind early access keeps the source's own
    ///     release date, enters /updates already buried, and never gets its spotlight. That is
    ///     the reported bug: production had a chapter released 2026-07-28 that first reached
    ///     our ingest on 2026-08-09.
    ///   * too wide → every chapter of a NEWLY ADDED series is stamped with the moment we
    ///     added it, and adding one series floods page 1 with its entire back catalogue. That
    ///     is F7 resurrected, and it is the failure the ledger itself exists to prevent.
    ///
    /// So both directions are asserted here, against the real incremental writer rather than a
    /// hand-built statement, because the gate reads `source_series.created_at` — a column only
    /// the real write path joins to.
    #[tokio::test]
    async fn the_announce_clock_marks_late_discovery_but_never_back_catalogue() {
        let pool = pool().await;
        let now_ms = chrono::Utc::now().timestamp_millis();

        // Two works, each with one Suwayomi source. We started watching BOTH on 2026-06-01.
        let mut cases = Vec::new();
        for (ext, key) in [("md-late", "10"), ("md-back", "20")] {
            let w = upsert_work_from_mangadex(
                &pool,
                ext,
                &WorkInput {
                    primary_title: Some(ext.into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            upsert_source_series(&pool, &w, "suwayomi", "asura", key, None, false)
                .await
                .unwrap();
            let ss = find_source_series_id(&pool, "suwayomi", "asura", key)
                .await
                .unwrap()
                .unwrap();
            sqlx::query("UPDATE source_series SET created_at = $1 WHERE id = $2")
                .bind(crate::db_types::date("2026-06-01T00:00:00+00:00").unwrap())
                .bind(&ss)
                .execute(&pool)
                .await
                .unwrap();
            cases.push((w, ss));
        }

        // (1) LATE DISCOVERY. Released 2026-06-10 — after we were already watching this
        //     source — so the nine weeks between then and now are OUR lateness, not the
        //     chapter's age at the moment it became available to us.
        replace_source_chapters(
            &pool,
            &cases[0].1,
            &[suwayomi_spine_input(
                1,
                "Chapter 10",
                10.0,
                None,
                Some(&ms("2026-06-10T00:00:00Z").to_string()),
            )],
        )
        .await
        .unwrap();
        record_source_series(&pool, &cases[0].1).await.unwrap();

        // (2) BACK CATALOGUE. Released 2026-05-01, BEFORE we started watching. Our first
        //     sighting is when WE arrived, and claiming it as an announcement would put a
        //     three-month-old chapter on page 1.
        replace_source_chapters(
            &pool,
            &cases[1].1,
            &[suwayomi_spine_input(
                2,
                "Chapter 20",
                20.0,
                None,
                Some(&ms("2026-05-01T00:00:00Z").to_string()),
            )],
        )
        .await
        .unwrap();
        record_source_series(&pool, &cases[1].1).await.unwrap();

        let row = |work: String| {
            let pool = pool.clone();
            async move {
                crate::db_types::query_tuple::<(i64, Option<i64>)>(
                    "SELECT first_seen_at, announce_at FROM release_event WHERE work_id = $1",
                )
                .bind(&work)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };

        let (late_seen, late_announce) = row(cases[0].0.clone()).await;
        assert_eq!(
            late_seen,
            ms("2026-06-10T00:00:00Z"),
            "the RELEASE clock is untouched — it is still what the card prints"
        );
        let late_announce = late_announce.expect("a late discovery must record an announce clock");
        assert!(
            late_announce > late_seen,
            "the announce clock must be LATER than the release it reports: \
             announce={late_announce} release={late_seen}"
        );
        assert!(
            (late_announce - now_ms).abs() < 60_000,
            "and it must be ~now, i.e. when we could first show it: \
             announce={late_announce} now={now_ms}"
        );

        let (back_seen, back_announce) = row(cases[1].0.clone()).await;
        assert_eq!(back_seen, ms("2026-05-01T00:00:00Z"));
        assert_eq!(
            back_announce,
            Some(back_seen),
            "A CHAPTER THAT PREDATES OUR TRACKING MUST NOT BE ANNOUNCED. Its announce clock \
             is its release clock, so it sorts where its age says it should and adding a \
             series cannot flood /updates with that series' history."
        );
        assert!(
            back_announce.unwrap() < now_ms,
            "and specifically it must NOT be ~now"
        );
    }

    /// THE WORST DEPLOYMENT HAZARD IN THE PLAN, pinned as a test: seeding the ledger with
    /// `now()` would stamp the entire back catalogue as released this instant and dump
    /// ~1.3 M events onto page 1 of /updates.
    #[tokio::test]
    async fn seeding_takes_its_time_from_the_chapter_and_never_from_now() {
        let pool = pool().await;
        let (_work, md, _sy) = work_with_two_sources(&pool).await;
        for (ext, n, when) in [
            ("a", "1", "2019-01-01T00:00:00Z"),
            ("b", "2", "2020-06-15T12:00:00Z"),
            ("c", "3", "2021-11-30T23:59:00Z"),
        ] {
            crate::catalog::upsert_chapter(
                &pool,
                &md,
                &ChapterInput {
                    external_id: ext.into(),
                    number: Some(n.into()),
                    lang: Some("en".into()),
                    readable_at: Some(when.into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        while seed_batch(&pool).await.unwrap() > 0 {}

        let times: Vec<i64> = crate::db_types::query_scalar(
            "SELECT first_seen_at FROM release_event ORDER BY first_seen_at",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            times,
            vec![
                ms("2019-01-01T00:00:00Z"),
                ms("2020-06-15T12:00:00Z"),
                ms("2021-11-30T23:59:00Z")
            ],
            "each event keeps its own historical release time"
        );
        assert_no_future_events(&pool).await.unwrap();
    }

    /// MangaDex stamps external chapters `publishAt = 2037-12-31` as a sentinel. Admitting
    /// one would put an event ~11 years in the ledger's future, which would sit permanently
    /// at the top of /updates. It is excluded until it has a real readable time.
    #[tokio::test]
    async fn a_2037_sentinel_chapter_never_enters_the_ledger() {
        let pool = pool().await;
        let (_work, md, _sy) = work_with_two_sources(&pool).await;
        crate::catalog::upsert_chapter(
            &pool,
            &md,
            &ChapterInput {
                external_id: "ext-1".into(),
                number: Some("500".into()),
                lang: Some("en".into()),
                // Exactly production's shape: the sentinel publishAt, and no readableAt
                // until the A1b backfill supplies one.
                published_at: Some("2037-12-31T15:00:00+00:00".into()),
                external_url: Some("https://mangaplus.shueisha.co.jp/x".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        while seed_batch(&pool).await.unwrap() > 0 {}
        assert_eq!(
            crate::db_types::query_scalar::<i64>("SELECT COUNT(*) FROM release_event")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0,
            "a chapter that is not readable yet has not been released"
        );
        assert_no_future_events(&pool).await.unwrap();

        // Once it IS readable, it enters — with the readable time, not the sentinel.
        sqlx::query("UPDATE chapter SET readable_at = '2026-05-05T00:00:00Z'")
            .execute(&pool)
            .await
            .unwrap();
        while seed_batch(&pool).await.unwrap() > 0 {}
        let t: i64 = crate::db_types::query_scalar("SELECT first_seen_at FROM release_event")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(t, ms("2026-05-05T00:00:00Z"));
    }

    /// A merge must keep the EARLIEST first-seen per chapter. Keeping the later one would
    /// re-float the merged-in work's whole back catalogue — the F7 symptom, reintroduced by
    /// the fix for it.
    #[tokio::test]
    async fn merging_keeps_the_earliest_first_seen_and_survives_the_cascade() {
        let pool = pool().await;
        let a = upsert_work_from_mangadex(&pool, "md-a", &WorkInput::default())
            .await
            .unwrap();
        let b = upsert_work_from_mangadex(&pool, "md-b", &WorkInput::default())
            .await
            .unwrap();
        let ss_a = find_source_series_id(&pool, "mangadex", "mangadex", "md-a")
            .await
            .unwrap()
            .unwrap();
        let ss_b = find_source_series_id(&pool, "mangadex", "mangadex", "md-b")
            .await
            .unwrap()
            .unwrap();
        // Both works carry chapter 1; B saw it FIRST. B also has a chapter A lacks.
        for (ss, ext, n, when) in [
            (&ss_a, "a1", "1", "2026-03-01T00:00:00Z"),
            (&ss_b, "b1", "1", "2026-01-01T00:00:00Z"),
            (&ss_b, "b2", "2", "2026-02-01T00:00:00Z"),
        ] {
            crate::catalog::upsert_chapter(
                &pool,
                ss,
                &ChapterInput {
                    external_id: ext.into(),
                    number: Some(n.into()),
                    lang: Some("en".into()),
                    readable_at: Some(when.into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        while seed_batch(&pool).await.unwrap() > 0 {}

        let mut tx = pool.begin().await.unwrap();
        merge_release_events(&mut tx, &b, &a).await.unwrap();
        tx.commit().await.unwrap();

        let rows: Vec<(String, i64)> = crate::db_types::query_tuple(
            "SELECT chapter_key, first_seen_at FROM release_event WHERE work_id = $1 \
             ORDER BY chapter_key",
        )
        .bind(&a)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![
                ("100".to_string(), ms("2026-01-01T00:00:00Z")),
                ("200".to_string(), ms("2026-02-01T00:00:00Z")),
            ],
            "chapter 1 keeps B's EARLIER time; B's exclusive chapter 2 comes across"
        );

        // And the cascade the merge relies on still fires for the losing work.
        sqlx::query("DELETE FROM work WHERE id = $1")
            .bind(&b)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            crate::db_types::query_scalar::<i64>(
                "SELECT COUNT(*) FROM release_event WHERE work_id = $1"
            )
            .bind(&b)
            .fetch_one(&pool)
            .await
            .unwrap(),
            0
        );
    }

    /// PHASE C3. The reconciler must report ZERO drift when the incremental writers have
    /// already done their job — that is the whole signal. A reconciler that always reports
    /// drift is noise, and one that reports none because it is not looking is worse.
    #[tokio::test]
    async fn the_reconciler_reports_zero_drift_when_the_incremental_path_kept_up() {
        let pool = pool().await;
        let (work, md, _sy) = work_with_two_sources(&pool).await;
        crate::catalog::upsert_chapter(
            &pool,
            &md,
            &ChapterInput {
                external_id: "r1".into(),
                number: Some("7".into()),
                lang: Some("en".into()),
                readable_at: Some("2026-05-01T00:00:00Z".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        while seed_batch(&pool).await.unwrap() > 0 {}

        // First run builds the feed from nothing, so everything is "new" — expected, and
        // the reason the assertion below is on the SECOND run.
        let first = reconcile_feed(&pool).await.unwrap();
        assert_eq!(first.rows_before, 0);

        // Nothing changed in between, so a full rebuild must find nothing to change.
        let second = reconcile_feed(&pool).await.unwrap();
        assert_eq!(
            second.drifted, 0,
            "a rebuild over an already-correct feed must report no drift; sample: {:?}",
            second.sample
        );
        assert_eq!(second.rows_before, second.rows_after);

        // And the report is persisted for the console to read days later.
        let (drifted, rows): (i64, i64) = crate::db_types::query_tuple(
            "SELECT drifted, rows_after FROM feed_reconcile_report WHERE id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(drifted, 0);
        assert_eq!(rows, second.rows_after);

        // Now corrupt the feed behind the incremental writers' backs and prove the
        // reconciler NOTICES. Without this the zero above could just mean it is blind.
        sqlx::query("UPDATE feed_series_updates SET released_at = TIMESTAMPTZ '1970-01-01 00:00:00.001+00', latest_chapter = 'x'")
            .execute(&pool)
            .await
            .unwrap();
        let third = reconcile_feed(&pool).await.unwrap();
        assert!(
            third.drifted > 0 && !third.sample.is_empty(),
            "the reconciler must detect and sample a divergence, got {third:?}"
        );
        let _ = work;
    }

    /// PHASE C2's EXIT CRITERION, and the owner's requirement made observable: a second
    /// source mirroring a chapter that is already in the ledger must not move the card.
    ///
    /// The old rule — `released_at = MAX(feed_series_updates.released_at, excluded.released_at)`
    /// — did the opposite: the later source's clock won, and the card jumped back to the top
    /// of /updates announcing a chapter that had already been announced.
    #[tokio::test]
    async fn a_second_source_mirroring_a_chapter_does_not_refloat_the_card() {
        let pool = pool().await;
        let (work, md, sy) = work_with_two_sources(&pool).await;

        // Suwayomi published chapter 10 on 1 June. That is the release.
        replace_source_chapters(
            &pool,
            &sy,
            &[suwayomi_spine_input(
                1,
                "Chapter 10",
                10.0,
                None,
                Some(&ms("2026-06-01T00:00:00Z").to_string()),
            )],
        )
        .await
        .unwrap();
        while seed_batch(&pool).await.unwrap() > 0 {}

        sqlx::query(
            "INSERT INTO feed_series_updates (work_id, reader_id, title, released_at) \
             VALUES ($1, $2, 'Both Halves', TIMESTAMPTZ '1970-01-01 00:00:00+00')",
        )
        .bind(&work)
        .bind(&work)
        .execute(&pool)
        .await
        .unwrap();

        let mut conn = pool.write().acquire().await.unwrap();
        crate::catalog::project_feed_from_ledger(&mut conn, None)
            .await
            .unwrap();
        drop(conn);
        let (before, label): (i64, Option<String>) = crate::db_types::query_tuple(
            "SELECT released_at, latest_chapter FROM feed_series_updates WHERE work_id = $1",
        )
        .bind(&work)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(before, ms("2026-06-01T00:00:00Z"));
        assert_eq!(label.as_deref(), Some("10"), "the NUMBER, never a count");

        // A repeat projection and count fill change nothing, so they write no row version
        // (WAL item 6), and they still report every feed row they cover.
        let version = || async {
            crate::db_types::query_scalar::<String>(
                "SELECT xmin::text FROM feed_series_updates WHERE work_id = $1",
            )
            .bind(&work)
            .fetch_one(&pool)
            .await
            .unwrap()
        };
        let mut conn = pool.write().acquire().await.unwrap();
        crate::catalog::fill_en_chapter_count(&mut conn, Some(&work))
            .await
            .unwrap();
        let settled = version().await;
        assert_eq!(
            crate::catalog::project_feed_from_ledger(&mut conn, Some(&work))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            crate::catalog::fill_en_chapter_count(&mut conn, Some(&work))
                .await
                .unwrap(),
            1
        );
        drop(conn);
        assert_eq!(version().await, settled);

        // A WEEK LATER, MangaDex mirrors the very same chapter 10.
        crate::catalog::upsert_chapter(
            &pool,
            &md,
            &ChapterInput {
                external_id: "u-10".into(),
                number: Some("10".into()),
                lang: Some("en".into()),
                readable_at: Some("2026-06-08T00:00:00Z".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        while seed_batch(&pool).await.unwrap() > 0 {}
        record_source_series(&pool, &md).await.unwrap();
        let mut conn = pool.write().acquire().await.unwrap();
        crate::catalog::project_feed_from_ledger(&mut conn, None)
            .await
            .unwrap();
        drop(conn);

        let after: i64 = crate::db_types::query_scalar(
            "SELECT released_at FROM feed_series_updates WHERE work_id = $1",
        )
        .bind(&work)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            after,
            ms("2026-06-01T00:00:00Z"),
            "the card must NOT re-float — the chapter was already announced"
        );

        // …but a genuinely NEW chapter still moves it, or the feed would be frozen.
        crate::catalog::upsert_chapter(
            &pool,
            &md,
            &ChapterInput {
                external_id: "u-11".into(),
                number: Some("11".into()),
                lang: Some("en".into()),
                readable_at: Some("2026-06-09T00:00:00Z".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        while seed_batch(&pool).await.unwrap() > 0 {}
        let mut conn = pool.write().acquire().await.unwrap();
        crate::catalog::project_feed_from_ledger(&mut conn, None)
            .await
            .unwrap();
        drop(conn);
        let (moved, label2): (i64, Option<String>) = crate::db_types::query_tuple(
            "SELECT released_at, latest_chapter FROM feed_series_updates WHERE work_id = $1",
        )
        .bind(&work)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            moved,
            ms("2026-06-09T00:00:00Z"),
            "a NEW chapter does float it"
        );
        assert_eq!(
            label2.as_deref(),
            Some("11"),
            "and relabels to the new chapter"
        );
    }

    /// THE MEMOISED PATH, which until now NO test reached at all (§8h caveat c): it was
    /// `#[cfg(not(test))]`, so every ledger test above ran the honest check and the latch
    /// that production actually depends on was exercised for the first time by production.
    ///
    /// What this pins, in order:
    ///   1. an INCOMPLETE ledger never latches, however many times it is asked;
    ///   2. while incomplete, a second ask inside the recheck window is served from the memo
    ///      — that is the whole point of it, and it is why completion is noticed within a
    ///      minute rather than instantly;
    ///   3. completion latches, and the latch then answers WITHOUT touching the database.
    ///      Asserted by emptying `release_event` underneath it: an honest check would say
    ///      false. That is the one-way latch stated as an executable fact rather than a
    ///      comment, so anyone who makes it two-way sees this test change;
    ///   4. `forget()` recovers — the answer to "a restore cannot flip it back".
    #[tokio::test]
    async fn the_ready_memo_latches_only_on_a_real_completion_and_can_be_forgotten() {
        let pool = pool().await;
        let (_work, md, _sy) = work_with_two_sources(&pool).await;
        crate::catalog::upsert_chapter(
            &pool,
            &md,
            &ChapterInput {
                external_id: "c1".into(),
                number: Some("1".into()),
                lang: Some("en".into()),
                readable_at: Some("2020-01-01T00:00:00Z".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let memo = ReadyMemo::new();
        let t0 = 1_700_000_000_000_i64;

        // (1) Un-seeded: the chapter is in the spine with no event, so `pending > 0`.
        assert!(!is_complete_memoised(&pool, &memo, t0).await.unwrap());
        assert!(
            !is_complete_memoised(&pool, &memo, t0 + 10 * READY_RECHECK_MS)
                .await
                .unwrap(),
            "an incomplete ledger must never latch, however long it is asked for"
        );

        // (2) Seed it — and note the answer stays FALSE inside the recheck window, because
        //     the memo is deliberately not asking again yet.
        while seed_batch(&pool).await.unwrap() > 0 {}
        let t1 = t0 + 10 * READY_RECHECK_MS;
        assert!(
            !is_complete_memoised(&pool, &memo, t1 + READY_RECHECK_MS - 1)
                .await
                .unwrap(),
            "inside the recheck window the memo answers, not the database"
        );

        // (3) Past the window it asks, finds the ledger complete, and latches.
        let t2 = t1 + READY_RECHECK_MS;
        assert!(is_complete_memoised(&pool, &memo, t2).await.unwrap());
        sqlx::query("DELETE FROM release_event")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            !is_complete(&pool).await.unwrap(),
            "the honest check must now say false, or (3) proves nothing"
        );
        assert!(
            is_complete_memoised(&pool, &memo, t2 + 10 * READY_RECHECK_MS)
                .await
                .unwrap(),
            "once true, always true — the latch must not re-query"
        );

        // (4) …and it is recoverable, which is what makes the one-way latch acceptable.
        memo.forget();
        assert!(
            !is_complete_memoised(&pool, &memo, t2 + 20 * READY_RECHECK_MS)
                .await
                .unwrap(),
            "forget() must put the memo back into re-checking"
        );
    }

    /// AN EARLY LATCH IS IMPOSSIBLE, stated as a test rather than as an argument.
    ///
    /// §8h caveat (b) is real: `pending == 0` is VACUOUSLY true on a database with no
    /// seedable chapters, and if that alone latched, a process that started before the spine
    /// drained would project a live feed from an empty ledger forever. The `events > 0`
    /// conjunct is what rules it out, and this is the only thing standing between the gate
    /// and that failure — so it gets its own test.
    #[tokio::test]
    async fn an_empty_database_can_never_latch_the_ready_memo() {
        let pool = pool().await;
        let memo = ReadyMemo::new();
        // Nothing at all: no works, no chapters, no events. `spine::remaining` is (0, 0) and
        // `pending` is 0 — every condition except `events > 0` is satisfied.
        assert_eq!(
            crate::catalog::spine::remaining(&pool).await.unwrap(),
            (0, 0)
        );
        assert_eq!(remaining(&pool).await.unwrap(), (0, 0));
        assert!(
            !is_complete_memoised(&pool, &memo, 1_700_000_000_000)
                .await
                .unwrap(),
            "an empty ledger is not a complete one"
        );
        // And a work whose only chapter is UNDATED — the other shape of "no seedable
        // chapters" — must not latch either.
        let (_work, md, _sy) = work_with_two_sources(&pool).await;
        crate::catalog::upsert_chapter(
            &pool,
            &md,
            &ChapterInput {
                external_id: "undated".into(),
                number: Some("1".into()),
                lang: Some("en".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        while seed_batch(&pool).await.unwrap() > 0 {}
        assert_eq!(
            remaining(&pool).await.unwrap(),
            (0, 0),
            "an undated chapter is deliberately not seedable"
        );
        assert!(
            !is_complete_memoised(&pool, &ReadyMemo::new(), 1_700_000_000_000)
                .await
                .unwrap(),
            "a database with nothing datable in it is not a complete ledger"
        );
    }

    /// The projection must be inert until the ledger has caught up with the spine. A work
    /// with three of its four hundred chapters recorded would otherwise take an OLD
    /// chapter's release time and sink on the live feed for the duration of the seed.
    #[tokio::test]
    async fn the_projection_stays_inert_until_the_ledger_is_complete() {
        let pool = pool().await;
        let (work, md, _sy) = work_with_two_sources(&pool).await;
        crate::catalog::upsert_chapter(
            &pool,
            &md,
            &ChapterInput {
                external_id: "c1".into(),
                number: Some("1".into()),
                lang: Some("en".into()),
                readable_at: Some("2020-01-01T00:00:00Z".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO feed_series_updates (work_id, reader_id, title, released_at) \
             VALUES ($1, $2, 'T', TIMESTAMPTZ '1970-01-01 00:01:39.999+00')",
        )
        .bind(&work)
        .bind(&work)
        .execute(&pool)
        .await
        .unwrap();

        // Un-seeded ledger ⇒ not complete ⇒ the incremental path must change nothing.
        assert!(!is_complete(&pool).await.unwrap());
        assert_eq!(
            crate::catalog::project_feed_from_ledger_for_work(&pool, &work)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            crate::db_types::query_scalar::<i64>(
                "SELECT released_at FROM feed_series_updates WHERE work_id = $1"
            )
            .bind(&work)
            .fetch_one(&pool)
            .await
            .unwrap(),
            99999,
            "the live feed keeps behaving exactly as it does today"
        );

        // Once seeded it switches itself on.
        while seed_batch(&pool).await.unwrap() > 0 {}
        assert!(is_complete(&pool).await.unwrap());
        assert_eq!(
            crate::catalog::project_feed_from_ledger_for_work(&pool, &work)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            crate::db_types::query_scalar::<i64>(
                "SELECT released_at FROM feed_series_updates WHERE work_id = $1"
            )
            .bind(&work)
            .fetch_one(&pool)
            .await
            .unwrap(),
            ms("2020-01-01T00:00:00Z")
        );
    }

    /// The ledger seed must not start until the spine is complete: `INSERT OR IGNORE` never
    /// corrects a row, so a work seeded while half its sources are missing would credit the
    /// wrong source permanently.
    #[tokio::test]
    async fn the_ledger_seed_waits_for_the_spine() {
        let pool = pool().await;
        let (_work, md, _sy) = work_with_two_sources(&pool).await;
        crate::catalog::upsert_chapter(
            &pool,
            &md,
            &ChapterInput {
                external_id: "x".into(),
                number: Some("1".into()),
                lang: Some("en".into()),
                readable_at: Some("2026-01-01T00:00:00Z".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // A Suwayomi series with cached chapters that the spine has NOT materialised yet.
        sqlx::query(
            "INSERT INTO suwayomi_chapter (id, manga_id, name, chapter_number, upload_date, \
                                           page_count, updated_at) \
             VALUES (7, 1, 'Chapter 1', 1.0, '1735689600000', 0, '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        assert!(
            spine::seed_ledger_if_spine_complete(&pool)
                .await
                .unwrap()
                .is_none(),
            "the seed must hold off while the spine is still filling"
        );
        assert_eq!(
            crate::db_types::query_scalar::<i64>("SELECT COUNT(*) FROM release_event")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );

        spine::drain_suwayomi_series(&pool).await.unwrap();
        assert!(
            spine::seed_ledger_if_spine_complete(&pool)
                .await
                .unwrap()
                .is_some(),
            "and to run once it is complete"
        );
    }
}
