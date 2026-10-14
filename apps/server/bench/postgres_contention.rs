//! Existing scratch PostgreSQL overlap gate. No migration or disposable harness.
//! Run from any cwd; all paths are rooted at CARGO_MANIFEST_DIR.
//!
//! Two parts per profile. First a deterministic proof for each background kind:
//! its real transaction is held open at a pause while one real HTTP mutation is
//! sent, answered and verified in the database; only then is the background
//! transaction released (`proven_overlaps`). Then the free-running contested
//! workload, which is the measurement.
//!
//! The pass rule is one pure function, `evaluate_gate`. It refuses a run in
//! which user writes and background writes never met (`contention_exercised`),
//! and it reads every failure class the run can produce: failed reads and
//! writes, every application counter, server statistics, WARN/ERROR log events
//! and the server's own SQLSTATE lines. The server log file is required: a run
//! without it, or with a log whose line prefix does not show the SQLSTATE,
//! fails. Server statistics are database-wide, so the scratch database must
//! have no other client session.
//!
//! The measurement window has no fixed length. It lasts at least the configured
//! duration and until the rebuild worker has completed the wanted number of
//! rebuild cycles inside it, each with a Browse publication that changed a row; a hard cap ends it
//! and fails the gate.

use crate::*;
use serde_json::{json, Value};
use sqlx::Row;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::atomic::{AtomicBool, Ordering},
    sync::Mutex,
    time::{Duration, Instant},
};

type Timestamp = chrono::DateTime<chrono::Utc>;

fn knob(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

/// The same summary `loadtest.mjs` writes, with its percentile rule
/// (`a[min(n - 1, floor(p * n))]`) and its rate over the real wall time.
fn latency_result(
    mut samples: Vec<(f64, bool)>,
    rate: u64,
    wall_seconds: f64,
    users: usize,
) -> Value {
    samples.sort_by(|a, b| a.0.total_cmp(&b.0));
    let requests = samples.len();
    let ok = samples.iter().filter(|(_, ok)| *ok).count();
    let percentile = |p: f64| {
        samples
            .get(((requests as f64 * p).floor() as usize).min(requests.saturating_sub(1)))
            .map(|s| s.0)
            .unwrap_or(0.0)
    };
    let wall = wall_seconds.max(f64::EPSILON);
    json!({"rate":rate,"requests":requests,"ok":ok,"httpErrors":0,
        "gqlErrors":requests-ok,"netErrors":0,"rps":requests as f64 / wall,
        "okRps":ok as f64 / wall,"p50":percentile(0.5),"p90":percentile(0.9),
        "p95":percentile(0.95),"p99":percentile(0.99),"max":samples.last().map(|s| s.0).unwrap_or(0.0),
        "mean":samples.iter().map(|s|s.0).sum::<f64>() / requests.max(1) as f64,"users":users})
}

fn spread(mut values: Vec<f64>) -> Value {
    values.sort_by(f64::total_cmp);
    let at = |p: f64| {
        values
            .get(((values.len() as f64 * p).floor() as usize).min(values.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0.0)
    };
    json!({"p50":at(0.5),"p95":at(0.95),"max":values.last().copied().unwrap_or(0.0),
        "mean":values.iter().sum::<f64>() / values.len().max(1) as f64})
}

// Offline replay of the actual catalogue-sync persistence function. Read all
// submitted metadata from stored rows rather than fabricating an upstream reply.
async fn stored_catalogue_input(
    pool: &db::Database,
    work: &str,
) -> anyhow::Result<(String, catalog::WorkInput)> {
    let row = sqlx::query("SELECT w.*,e.external_id FROM work w JOIN work_external_id e ON e.work_id=w.id AND e.provider='mangadex' WHERE w.id=$1")
        .bind(work).fetch_one(pool).await?;
    let mut input = catalog::WorkInput {
        primary_title: row.try_get("primary_title")?,
        primary_lang: row.try_get("primary_lang")?,
        description: row.try_get("description")?,
        year: row.try_get("year")?,
        original_language: row.try_get("original_language")?,
        status: row.try_get("status")?,
        demographic: row.try_get("demographic")?,
        content_rating: row.try_get("content_rating")?,
        is_nsfw: row.try_get("is_nsfw")?,
        author: row.try_get("author")?,
        artist: row.try_get("artist")?,
        cover_phash: row.try_get("cover_phash")?,
        cover_file_name: row.try_get("cover_file_name")?,
        ..Default::default()
    };
    let aliases: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT raw_title,lang FROM work_alias WHERE work_id=$1 ORDER BY id")
            .bind(work)
            .fetch_all(pool)
            .await?;
    input.aliases = aliases
        .into_iter()
        .map(|(raw, lang)| catalog::Alias { raw, lang })
        .collect();
    input.descriptions = sqlx::query_as(
        "SELECT lang,description FROM work_description WHERE work_id=$1 ORDER BY lang",
    )
    .bind(work)
    .fetch_all(pool)
    .await?;
    input.credits =
        sqlx::query_as("SELECT role,name FROM work_credit WHERE work_id=$1 ORDER BY role,name")
            .bind(work)
            .fetch_all(pool)
            .await?;
    input.external_ids = sqlx::query_as("SELECT provider,external_id FROM work_external_id WHERE work_id=$1 ORDER BY provider,external_id")
        .bind(work).fetch_all(pool).await?;
    input.official_links = sqlx::query_as(
        "SELECT provider,external_id FROM work_tracker_link WHERE work_id=$1 ORDER BY provider",
    )
    .bind(work)
    .fetch_all(pool)
    .await?;
    let tags: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as("SELECT tag,tag_id,tag_group FROM work_tag WHERE work_id=$1 AND source='mangadex' ORDER BY ord,tag")
        .bind(work).fetch_all(pool).await?;
    for (name, id, group) in tags {
        input.tags.push(name.clone());
        input.tag_details.push(catalog::SourceTag {
            id,
            name,
            group: group.unwrap_or_default(),
        });
    }
    Ok((row.try_get("external_id")?, input))
}

/// The shared state the real router serves: background pool by default, MangaDex
/// off and the engine at an unavailable loopback port, so nothing reaches a network.
/// Also used by the foundation handler tests in `database.rs`.
pub(crate) fn app_state(pool: &db::Database, covers: &db::SqliteDatabase) -> Arc<AppState> {
    let mangadex = Arc::new(mangadex::MangaDexClient::new(
        "postgres-overlap-benchmark",
        5.0,
        40.0,
    ));
    mangadex.set_enabled(false);
    Arc::new(AppState {
        pool: pool.background(),
        cover_pool: covers.clone(),
        suwayomi: SuwayomiClient::new("http://127.0.0.1:1".into(), None, None),
        mangadex,
        admin_users: vec![],
        scan_health: std::sync::Mutex::new(ScanHealth::default()),
        auth_limiter: RateLimiter::new(100, 60),
        comment_limiter: graphql::comment_limiter(),
        rating_limiter: graphql::rating_limiter(),
        password_work: Arc::new(tokio::sync::Semaphore::new(4)),
        federated_limiter: RateLimiter::new(100, 60),
        session_ttl_secs: 3600,
        series_inflight: KeyedLocks::default(),
        chapters_inflight: KeyedLocks::default(),
        cover_crawl_running: Arc::new(AtomicBool::new(false)),
        catalogue_cover_phash: false,
        ext_icons_dir: std::path::PathBuf::new(),
        setup_code: instance::SetupCode::default(),
    })
}

async fn make_router(pool: &db::Database) -> (Router, db::SqliteDatabase, ReadyDb) {
    let covers = db::init_covers("sqlite::memory:").await.unwrap();
    let ready = ReadyDb::connect(pool).await.unwrap();
    let state = app_state(pool, &covers);
    let mut cfg = Config::from_env();
    cfg.bind_addr = "127.0.0.1".into();
    cfg.web_dir = None;
    cfg.img_proxy_enabled = false;
    cfg.cors_origins.clear();
    let trusted = Arc::new(TrustedProxies::new(vec![], false));
    let manager = Arc::new(cloudflare::Manager::new(
        pool.background(),
        cloudflare::Options::from_env(&cfg),
        trusted.clone(),
    ));
    let schema = graphql::schema_builder(state.clone(), true)
        .data(manager.clone())
        .finish();
    let router = public_router(
        &cfg,
        RouterState {
            schema,
            pool: pool.foreground(),
            ready_pool: ready.clone(),
            request_budgets: Arc::new(request_budget::RequestBudgets::default()),
            cover_pool: covers.clone(),
            trusted_proxies: trusted,
            upload_limiter: Arc::new(RateLimiter::new(100, 60)),
            app_state: state,
        },
        manager,
    );
    (router, covers, ready)
}

/// Every session this benchmark opens has an application name with this prefix.
const BENCH_APPLICATION: &str = "komika-bench";
/// The measured handle: its sixteen readers, its own leased writer, readiness.
const MAIN: &str = "komika-bench-main";
/// The measured handle's foreground writers, labelled at the start of a profile.
const FOREGROUND: &str = "komika-bench-foreground";
const CATALOGUE: &str = "komika-bench-catalogue";
const REBUILD: &str = "komika-bench-rebuild";
/// The private readers and catalogue snapshot sessions of a background handle.
const SNAPSHOT: &str = "komika-bench-snapshot";
/// Instruments: the lock-wait observer, per-request samples, effect checks,
/// statistics. Never one of the measured sixteen readers.
const OBSERVER: &str = "komika-bench-observer";
/// The benchmark's own perturbation of `work` rows. It is scaffolding, not a
/// background job: a wait on it never counts as contention.
const PERTURB: &str = "komika-bench-perturb";
/// The database-scoped background lease (`database.rs`), as pg_locks and the
/// server log show a bigint advisory key: high half, low half, objsubid 1.
const LEASE_KEY: i64 = 4930189972093101;
const STATISTICS: [&str; 7] = [
    "deadlocks",
    "xact_rollback",
    "sessions_killed",
    "sessions_fatal",
    "sessions_abandoned",
    "conflicts",
    "temp_files",
];

/// The database and its statistics epoch. A restart or a statistics reset between
/// two samples makes their difference meaningless, so both are recorded and compared.
async fn server_identity(executor: impl sqlx::PgExecutor<'_>) -> Value {
    let row: (String, Timestamp, Option<Timestamp>) = sqlx::query_as(
        "SELECT current_database()::text, pg_postmaster_start_time(), \
         (SELECT stats_reset FROM pg_stat_database WHERE datname = current_database())",
    )
    .fetch_one(executor)
    .await
    .unwrap();
    json!({"database":row.0,"postmaster_started_at":row.1.to_rfc3339(),
        "statistics_reset_at":row.2.map(|at| at.to_rfc3339())})
}

/// Client sessions in this database other than the asking one. With `own` false
/// the benchmark's own sessions (by application name) are left out as well.
/// A role without pg_read_all_stats sees another role's session with a NULL
/// backend type: that is counted as a client session, not skipped. Sessions
/// without a user (autovacuum workers) are not client sessions.
async fn other_sessions(executor: impl sqlx::PgExecutor<'_>, own: bool) -> Vec<Value> {
    let rows: Vec<(i32, String, String)> = sqlx::query_as(
        "SELECT pid, COALESCE(application_name, ''), COALESCE(state, '') FROM pg_stat_activity \
         WHERE datname = current_database() AND usesysid IS NOT NULL \
           AND (backend_type IS NULL OR backend_type = 'client backend') \
           AND pid <> pg_backend_pid() \
           AND ($1 OR COALESCE(application_name, '') NOT LIKE $2) ORDER BY pid",
    )
    .bind(own)
    .bind(format!("{BENCH_APPLICATION}%"))
    .fetch_all(executor)
    .await
    .unwrap();
    rows.into_iter()
        .map(|(pid, name, state)| json!({"pid":pid,"application_name":name,"state":state}))
        .collect()
}

/// Sessions for the instruments only. Plain connections with the server's own
/// defaults: they are not a lane of the server and are not measured.
async fn instrument_pool(pool: &db::Database, connections: u32) -> sqlx::PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(connections.max(2))
        .acquire_timeout(Duration::from_secs(30))
        .connect_with(
            pool.connect_options()
                .as_ref()
                .clone()
                .application_name(OBSERVER),
        )
        .await
        .unwrap()
}

/// pg_stat_database is fed by each backend when it flushes its pending counters:
/// at a transaction end once a second has passed since its last flush, otherwise
/// after up to ten idle seconds, and at exit. One read after a fixed sleep is
/// therefore not a synchronised value. This flushes the sampling session itself
/// with pg_stat_force_next_flush() and reads (each read a fresh autocommit
/// statement) until the row has not moved for `stable`, giving up after `limit`.
async fn settled_server_stats(
    instruments: &sqlx::PgPool,
    stable: Duration,
    limit: Duration,
) -> Value {
    let started = Instant::now();
    let mut connection = instruments.acquire().await.unwrap();
    let mut last: Option<Vec<i64>> = None;
    let mut unchanged_since = Instant::now();
    let mut samples = 0u64;
    let settled = loop {
        sqlx::query("SELECT pg_stat_force_next_flush()")
            .execute(&mut *connection)
            .await
            .unwrap();
        let row = sqlx::query(&format!(
            "SELECT {} FROM pg_stat_database WHERE datname = current_database()",
            STATISTICS.join(", ")
        ))
        .fetch_one(&mut *connection)
        .await
        .unwrap();
        let row: Vec<i64> = (0..STATISTICS.len())
            .map(|index| row.try_get(index).unwrap())
            .collect();
        samples += 1;
        if last.as_ref() != Some(&row) {
            last = Some(row);
            unchanged_since = Instant::now();
        } else if unchanged_since.elapsed() >= stable {
            break true;
        }
        if started.elapsed() >= limit {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    let row = last.expect("at least one statistics sample");
    let mut stats = json!({"settled":settled,"samples":samples,
        "settle_wait_ms":started.elapsed().as_millis() as u64});
    for (key, value) in STATISTICS.into_iter().zip(row) {
        stats[key] = json!(value);
    }
    stats
}

async fn benchmark_background_pool(pool: &db::Database, label: &str) -> db::Database {
    // Only this handle's leased writer is used for writes. Its few private readers
    // serve the catalogue functions' own lookups; the shared sixteen stay with HTTP.
    let limits = config::DatabaseLimits {
        read_connections: 4,
        foreground_connections: 1,
        ..Default::default()
    };
    let handle = db::Database::connect_for_benchmark_with_limits(
        pool.connect_options()
            .as_ref()
            .clone()
            .application_name(SNAPSHOT),
        limits,
    )
    .await
    .unwrap();
    // The label survives pool release. Only the leased writer is tagged; private
    // read snapshots are deliberately excluded from write-overlap observations.
    let mut connection = handle.write().acquire().await.unwrap();
    sqlx::query("SELECT set_config('application_name',$1,false)")
        .bind(label)
        .execute(&mut *connection)
        .await
        .unwrap();
    drop(connection);
    handle
}

/// Give every foreground writer of the measured handle its own application
/// name, so the lock-wait observer can tell a request's writer from a reader or
/// from the handle's leased writer. All connections are held at once, so each
/// one is reached; the label survives pool release. A connection the pool
/// replaces later is unlabelled, and its waits are then reported as `other`,
/// never as contention. Returns how many sessions carry the label.
async fn label_foreground(pool: &db::Database, connections: u32) -> i64 {
    let foreground = pool.foreground();
    let mut held = Vec::new();
    for _ in 0..connections.max(1) {
        held.push(foreground.write().acquire().await.unwrap());
    }
    for connection in &mut held {
        sqlx::query("SELECT set_config('application_name',$1,false)")
            .bind(FOREGROUND)
            .execute(&mut **connection)
            .await
            .unwrap();
    }
    drop(held);
    labelled_sessions(pool.read(), FOREGROUND).await
}

async fn labelled_sessions(executor: impl sqlx::PgExecutor<'_>, label: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_stat_activity \
         WHERE datname = current_database() AND application_name = $1",
    )
    .bind(label)
    .fetch_one(executor)
    .await
    .unwrap()
}

/// A statement of a labelled background writer inside an open transaction.
struct BackgroundActivity {
    application: String,
    xact_start: Timestamp,
    query_start: Timestamp,
    query: String,
}

async fn background_activity(
    executor: impl sqlx::PgExecutor<'_>,
) -> Result<Vec<BackgroundActivity>, sqlx::Error> {
    let rows: Vec<(String, Timestamp, Timestamp, String)> = sqlx::query_as(
        "SELECT application_name, xact_start, query_start, left(query, 200) FROM pg_stat_activity \
         WHERE datname = current_database() \
           AND application_name IN ('komika-bench-catalogue','komika-bench-rebuild') \
           AND xact_start IS NOT NULL AND query_start IS NOT NULL \
           AND query NOT LIKE 'SELECT pg_advisory_%' \
           AND query NOT LIKE 'SELECT pg_try_advisory_%' AND query <> 'ROLLBACK'",
    )
    .fetch_all(executor)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(application, xact_start, query_start, query)| BackgroundActivity {
                application,
                xact_start,
                query_start,
                query,
            },
        )
        .collect())
}

/// A background statement that writes or takes a lock, judged from the first
/// 200 characters pg_stat_activity was asked for. What the two background
/// writers run on their leased session in the merged tree: the publication's
/// `INSERT INTO public.<table> ... ON CONFLICT` and `DELETE FROM
/// public.<table>`, the search reconcile's `INSERT INTO work_search_alias` and
/// `DELETE FROM work_search_alias`, the catalogue replay's upserts, and the row
/// locks all of them take first (`SELECT id FROM work WHERE id = $1 FOR NO KEY
/// UPDATE`, or the batched `SELECT id FROM public.work WHERE id = ANY($1)
/// ORDER BY id FOR NO KEY UPDATE SKIP LOCKED`). The search reconcile no longer calls a function that locks the
/// `work` table, so nothing is matched by a function name.
fn is_write_statement(query: &str) -> bool {
    let upper = query.trim_start().to_ascii_uppercase();
    let writes = ["INSERT ", "UPDATE ", "DELETE ", "MERGE "];
    let row_locks = [
        " FOR UPDATE",
        " FOR NO KEY UPDATE",
        " FOR SHARE",
        " FOR KEY SHARE",
    ];
    [
        "INSERT", "UPDATE", "DELETE", "CREATE", "TRUNCATE", "LOCK", "MERGE",
    ]
    .iter()
    .any(|prefix| upper.starts_with(prefix))
        // A CTE counts only when the visible text has its data-changing part.
        || (upper.starts_with("WITH") && writes.iter().any(|word| upper.contains(word)))
        // A SELECT counts only when the visible text has its row-lock clause.
        || (upper.starts_with("SELECT") && row_locks.iter().any(|clause| upper.contains(clause)))
}

/// A background write statement that belongs to the measurement: the statement
/// AND its transaction started at or after the window start. A transaction that
/// was opened before the window (the one a deterministic sample held at its
/// pause) can run its statements inside the window; they are not counted.
fn starts_inside_window(activity: &BackgroundActivity, window_start: Timestamp) -> bool {
    activity.xact_start >= window_start
        && activity.query_start >= window_start
        && is_write_statement(&activity.query)
}

/// One perturbation of the rebuild worker: when it was applied, the rows its
/// UPDATE touched and the rows whose `year` really changed. The two differ for
/// a work with a pinned year: the `work_pin_year` trigger restores the pin.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Perturbation {
    at: Timestamp,
    touched: u64,
    changed: u64,
}

/// The four rebuild calls of one cycle, in the order the rebuild worker makes
/// them. In the merged tree the first chains into the second, and the second
/// into the third and the fourth, so one cycle rebuilds Browse three times.
const REBUILD_STEPS: [&str; 4] = [
    "refresh_feed_updates",
    "refresh_feed_series_updates",
    "refresh_browse_catalogue",
    "refresh_work_fts",
];

/// One completed cycle of the rebuild worker: the four calls and their intervals.
#[derive(Clone, Debug, PartialEq)]
struct RebuildCycle {
    began: Timestamp,
    ended: Timestamp,
    steps: [(Timestamp, Timestamp); 4],
}

/// What one background worker did: when it ran, each unit's interval and its error.
struct BackgroundRun {
    began: Timestamp,
    ended: Timestamp,
    units: Vec<(Timestamp, Timestamp)>,
    /// Every perturbation the rebuild worker applied.
    perturbations: Vec<Perturbation>,
    /// (start, name) of every rebuild call the rebuild worker began.
    step_marks: Vec<(Timestamp, &'static str)>,
    /// The rebuild worker's completed cycles; `units` are their intervals.
    cycles: Vec<RebuildCycle>,
    error: Option<String>,
}

impl BackgroundRun {
    /// Units that began and ended inside the interval. A unit that merely
    /// overlaps one of its edges is not counted.
    fn units_inside(&self, from: Timestamp, to: Timestamp) -> usize {
        self.units
            .iter()
            .filter(|(began, ended)| *began >= from && *ended <= to)
            .count()
    }
}

/// One work the catalogue worker replays: its MangaDex id and stored metadata.
struct Replay {
    work: String,
    external_id: String,
    input: catalog::WorkInput,
}

/// Replay the stored catalogue metadata of the replayed works, in turn, through
/// the real sync persistence path until told to stop. The first real BEGIN stops
/// at the armed pause, if any.
async fn catalogue_worker(
    catalogue: db::Database,
    points: locks::pause_points::PausePoints,
    stop: Arc<AtomicBool>,
    replays: Vec<Replay>,
) -> BackgroundRun {
    let began = chrono::Utc::now();
    let mut units = Vec::new();
    let result: anyhow::Result<()> = database::TRANSACTION_OPEN_PAUSE
        .scope(points, async {
            anyhow::ensure!(!replays.is_empty(), "no work to replay");
            for replay in replays.iter().cycle() {
                let unit = chrono::Utc::now();
                let adopted = catalog::upsert_work_from_mangadex(
                    &catalogue,
                    &replay.external_id,
                    &replay.input,
                )
                .await?;
                anyhow::ensure!(
                    adopted == replay.work,
                    "stored catalogue replay changed owner"
                );
                units.push((unit, chrono::Utc::now()));
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Ok(())
        })
        .await;
    if let Err(error) = &result {
        db::observe_database_error(error);
    }
    let ended = chrono::Utc::now();
    catalogue.close().await;
    BackgroundRun {
        began,
        ended,
        units,
        perturbations: Vec::new(),
        step_marks: Vec::new(),
        cycles: Vec::new(),
        error: result.err().map(|error| format!("{error:#}")),
    }
}

/// The benchmark's stand-in for "a catalogue sync changed some works since the
/// last rebuild". On a quiet copy a rebuild finds every projection row equal to
/// its source, publishes nothing and, in the merged tree, opens no writer
/// transaction at all. This flips `work.year` by one on a bounded slice of
/// works (a column a real sync changes and that `browse_catalogue` copies), on
/// its OWN session and without touching any projection table: the next Browse
/// rebuild must republish those rows. Nothing is restored; the database is a
/// scratch copy.
///
/// The slice must not contain a work the catalogue worker replays: the replay
/// writes the stored `year` back within milliseconds and projects the Browse
/// row itself, so the rebuild would find nothing to publish for it.
struct Perturber {
    connection: sqlx::PgConnection,
    works: Vec<String>,
}

impl Perturber {
    async fn connect(pool: &db::Database, works: Vec<String>) -> Self {
        use sqlx::Connection;
        let mut connection = sqlx::PgConnection::connect_with(
            &pool
                .connect_options()
                .as_ref()
                .clone()
                .application_name(PERTURB),
        )
        .await
        .unwrap();
        db::apply_session_limits(&mut connection, db::Lane::Background)
            .await
            .unwrap();
        Self { connection, works }
    }

    /// (rows the UPDATE touched, rows whose year really changed). Both CTEs see
    /// the statement's snapshot, so `before` holds the old year and `flipped`
    /// the stored one, after the `work_pin_year` trigger has had its say.
    async fn apply(&mut self) -> Result<(u64, u64), sqlx::Error> {
        let (touched, changed): (i64, i64) = sqlx::query_as(
            "WITH before AS (SELECT id, year FROM work WHERE id = ANY($1)), \
                  flipped AS (UPDATE work SET year = CASE WHEN COALESCE(year, 2000) % 2 = 0 \
                      THEN COALESCE(year, 2000) + 1 ELSE COALESCE(year, 2000) - 1 END \
                    WHERE id = ANY($1) RETURNING id, year) \
             SELECT COUNT(*), COUNT(*) FILTER (WHERE flipped.year IS DISTINCT FROM before.year) \
               FROM flipped JOIN before USING (id)",
        )
        .bind(&self.works)
        .fetch_one(&mut self.connection)
        .await?;
        Ok((touched as u64, changed as u64))
    }
}

/// The four real rebuilds in a loop: one pass is one rebuild cycle. The
/// perturbation is applied before each call that rebuilds Browse (the first
/// two do through the chain, the third directly), so every Browse rebuild has
/// rows to publish; it is not applied before the search reconcile, because two
/// flips with no Browse rebuild between them would cancel. The lease is taken
/// by the rebuild's own BEGIN, including publication; the first one stops at
/// the armed pause, if any. A completed cycle is pushed to `progress` at once,
/// so the window controller can count it while the worker runs.
async fn rebuild_worker(
    rebuild: db::Database,
    points: locks::pause_points::PausePoints,
    stop: Arc<AtomicBool>,
    mut perturber: Perturber,
    progress: Arc<Mutex<Vec<RebuildCycle>>>,
    max_cycles: Option<usize>,
) -> BackgroundRun {
    let began = chrono::Utc::now();
    let mut perturbations = Vec::new();
    let mut step_marks = Vec::new();
    let result: anyhow::Result<()> = database::TRANSACTION_OPEN_PAUSE
        .scope(points, async {
            loop {
                let cycle_began = chrono::Utc::now();
                let mut steps = [(cycle_began, cycle_began); 4];
                for (step, name) in REBUILD_STEPS.into_iter().enumerate() {
                    if stop.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    if step < 3 {
                        let (touched, changed) = perturber.apply().await?;
                        perturbations.push(Perturbation {
                            at: chrono::Utc::now(),
                            touched,
                            changed,
                        });
                    }
                    let step_began = chrono::Utc::now();
                    step_marks.push((step_began, name));
                    match step {
                        0 => catalog::refresh_feed_updates(&rebuild).await?,
                        1 => catalog::refresh_feed_series_updates(&rebuild).await?,
                        2 => catalog::refresh_browse_catalogue(&rebuild).await?,
                        _ => catalog::refresh_work_fts(&rebuild).await?,
                    };
                    steps[step] = (step_began, chrono::Utc::now());
                }
                progress.lock().unwrap().push(RebuildCycle {
                    began: cycle_began,
                    ended: chrono::Utc::now(),
                    steps,
                });
                if stop.load(Ordering::Relaxed)
                    || max_cycles.is_some_and(|limit| progress.lock().unwrap().len() >= limit)
                {
                    return Ok(());
                }
            }
        })
        .await;
    if let Err(error) = &result {
        db::observe_database_error(error);
    }
    let ended = chrono::Utc::now();
    {
        use sqlx::Connection;
        let _ = perturber.connection.close().await;
    }
    rebuild.close().await;
    let cycles = progress.lock().unwrap().clone();
    BackgroundRun {
        began,
        ended,
        units: cycles
            .iter()
            .map(|cycle| (cycle.began, cycle.ended))
            .collect(),
        perturbations,
        step_marks,
        cycles,
        error: result.err().map(|error| format!("{error:#}")),
    }
}

/// One "catalogue snapshot published" or "rebuilt browse catalogue" INFO event
/// of the rebuild, stamped when the log capture received it (client clock).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RebuildEvent {
    at: Timestamp,
    mark: RebuildMark,
}

#[derive(Clone, Debug, PartialEq)]
enum RebuildMark {
    /// `publish_snapshot` finished one table and says how many rows it changed.
    Published { table: String, changed: u64 },
    /// `refresh_browse_catalogue` finished: what follows in a chain is the
    /// search reconcile.
    BrowseRebuilt,
}

/// (Browse publications, rows they changed) that the rebuild reported between
/// two instants. The event marks the END of a publication.
fn browse_publications(events: &[RebuildEvent], from: Timestamp, to: Timestamp) -> (u64, u64) {
    events
        .iter()
        .filter(|event| event.at >= from && event.at <= to)
        .filter_map(|event| match &event.mark {
            RebuildMark::Published { table, changed } if table == "browse_catalogue" => {
                Some(*changed)
            }
            _ => None,
        })
        .fold((0, 0), |(count, rows), changed| (count + 1, rows + changed))
}

/// The rebuild cycles that count for a window: complete inside it (began at or
/// after its start, ended at or before its end) and with at least one Browse
/// publication that changed a row reported inside the cycle.
fn counted_cycles<'a>(
    cycles: &'a [RebuildCycle],
    events: &[RebuildEvent],
    from: Timestamp,
    to: Timestamp,
) -> Vec<&'a RebuildCycle> {
    cycles
        .iter()
        .filter(|cycle| cycle.began >= from && cycle.ended <= to)
        .filter(|cycle| browse_publications(events, cycle.began, cycle.ended).1 > 0)
        .collect()
}

/// What the background side of a lock wait was doing. The catalogue writer is
/// always the replay. The rebuild writer is placed by time, from the start of
/// each rebuild call and from the rebuild's own INFO events, because the
/// statement a blocker happens to run (a row lock, an authority read) does not
/// say which rebuild it belongs to.
const BACKGROUND_KINDS: [&str; 5] = [
    "catalogue_replay",
    "feed_rebuild",
    "browse_publication",
    "search_reconcile",
    "other",
];

/// From each instant on, the kind of work the rebuild writer does: the two
/// feed rebuilds until `feed_series_updates` is published, then the Browse
/// rebuild (tag sync, tag keys, publication of `browse_catalogue`, `tag` and
/// `feed_genre_facet`) until it reports itself done, then the search reconcile.
fn rebuild_timeline(
    step_marks: &[(Timestamp, &'static str)],
    events: &[RebuildEvent],
) -> Vec<(Timestamp, &'static str)> {
    let mut timeline: Vec<(Timestamp, &'static str)> = step_marks
        .iter()
        .map(|(at, step)| {
            let kind = match *step {
                "refresh_browse_catalogue" => "browse_publication",
                "refresh_work_fts" => "search_reconcile",
                _ => "feed_rebuild",
            };
            (*at, kind)
        })
        .chain(events.iter().filter_map(|event| match &event.mark {
            RebuildMark::Published { table, .. } if table == "feed_series_updates" => {
                Some((event.at, "browse_publication"))
            }
            RebuildMark::BrowseRebuilt => Some((event.at, "search_reconcile")),
            RebuildMark::Published { .. } => None,
        }))
        .collect();
    timeline.sort_by_key(|(at, _)| *at);
    timeline
}

fn background_kind(
    application: &str,
    at: Timestamp,
    timeline: &[(Timestamp, &'static str)],
) -> &'static str {
    match application {
        CATALOGUE => "catalogue_replay",
        REBUILD => timeline
            .iter()
            .rev()
            .find(|(from, _)| *from <= at)
            .map_or("other", |(_, kind)| kind),
        _ => "other",
    }
}

/// When the measurement window may end.
#[derive(Clone, Copy, Debug)]
struct WindowRule {
    /// KOMIKA_PG_BENCH_DURATION_SECS: the window is never shorter.
    duration: Duration,
    /// KOMIKA_PG_BENCH_MAX_DURATION_SECS: the window is never longer.
    cap: Duration,
    /// KOMIKA_PG_BENCH_MIN_REBUILD_CYCLES: counted rebuild cycles wanted.
    min_cycles: u64,
}

// Both profiles share scratch bearer sessions. The cap covers only each
// measurement window; proofs and setup/drain/settling run outside it. Keep a
// finite expiry with a generous day of margin, without live renewal or any
// change to production authentication.
const SESSION_FIXTURE_MARGIN_SECS: u64 = 24 * 60 * 60;

fn session_fixture_expiry(
    started: Timestamp,
    rule: &WindowRule,
    proof_wait: Duration,
) -> Option<(u64, Timestamp)> {
    let seconds = rule
        .cap
        .max(rule.duration)
        .as_secs()
        .checked_mul(2)?
        .checked_add(proof_wait.as_secs().checked_mul(4)?)?
        .checked_add(SESSION_FIXTURE_MARGIN_SECS)?;
    let delta = chrono::TimeDelta::try_seconds(i64::try_from(seconds).ok()?)?;
    Some((seconds, started.checked_add_signed(delta)?))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowEnd {
    Continue,
    /// The duration has passed and the cycles are there.
    Complete,
    /// The cap was reached first: the gate fails.
    Capped,
}

fn window_decision(elapsed: Duration, rule: &WindowRule, cycles: u64) -> WindowEnd {
    if elapsed >= rule.duration && cycles >= rule.min_cycles {
        WindowEnd::Complete
    } else if elapsed >= rule.cap.max(rule.duration) {
        WindowEnd::Capped
    } else {
        WindowEnd::Continue
    }
}

/// What ends a window: the shared cycle list, the log capture and the signals
/// the drivers watch.
struct WindowControl {
    rule: WindowRule,
    window_start: Timestamp,
    cycles: Arc<Mutex<Vec<RebuildCycle>>>,
    /// Without the capture no Browse publication can be seen. The window then
    /// ends at its duration and the gate fails on the missing capture.
    capture: Option<LogCapture>,
    /// Set at the window end: the write driver sends nothing after it.
    stop_sending: Arc<AtomicBool>,
    window_end: Arc<std::sync::OnceLock<Timestamp>>,
    /// Created at the window end: `loadtest.mjs --stop-file` then stops sending.
    stop_file: std::path::PathBuf,
    workers_finished: [tokio::sync::watch::Receiver<bool>; 2],
}

/// Wait for the end of the window and announce it. Returns (end, capped).
async fn run_window(
    control: WindowControl,
    started: tokio::time::Instant,
) -> (Timestamp, bool, bool) {
    loop {
        let now = chrono::Utc::now();
        let counted = match &control.capture {
            Some(capture) => counted_cycles(
                &control.cycles.lock().unwrap(),
                &capture.rebuild_events(),
                control.window_start,
                now,
            )
            .len() as u64,
            None => control.rule.min_cycles,
        };
        let decision = window_decision(started.elapsed(), &control.rule, counted);
        let worker_ended = control.workers_finished.iter().any(worker_finished);
        if worker_ended || decision != WindowEnd::Continue {
            let _ = control.window_end.set(now);
            control.stop_sending.store(true, Ordering::Relaxed);
            // A failure to create the file only lets the read driver run to its
            // own duration; the reads it sends are still all judged.
            let _ = std::fs::write(&control.stop_file, b"window ended\n");
            return (now, decision == WindowEnd::Capped, worker_ended);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn worker_finished(status: &tokio::sync::watch::Receiver<bool>) -> bool {
    *status.borrow() || status.has_changed().is_err()
}

fn spawn_background(
    job: impl std::future::Future<Output = BackgroundRun> + Send + 'static,
) -> (
    tokio::task::JoinHandle<BackgroundRun>,
    tokio::sync::watch::Receiver<bool>,
) {
    let (finished, status) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(async move {
        let run = job.await;
        let _ = finished.send(true);
        run
    });
    (handle, status)
}

async fn await_background(job: tokio::task::JoinHandle<BackgroundRun>) -> BackgroundRun {
    job.await.unwrap_or_else(|error| {
        let now = chrono::Utc::now();
        BackgroundRun {
            began: now,
            ended: now,
            units: Vec::new(),
            perturbations: Vec::new(),
            step_marks: Vec::new(),
            cycles: Vec::new(),
            error: Some(format!("background task did not return: {error}")),
        }
    })
}

/// The open transaction of one labelled background writer, as the server sees it:
/// (pid, transaction start, state, whether it owns the background lease, server clock).
async fn background_transaction(
    pool: &db::Database,
    label: &str,
) -> Vec<(i32, Timestamp, String, bool, Timestamp)> {
    sqlx::query_as(
        "SELECT a.pid, a.xact_start, COALESCE(a.state, ''), \
           EXISTS (SELECT 1 FROM pg_locks l WHERE l.pid = a.pid AND l.granted \
             AND l.locktype = 'advisory' AND l.objsubid = 1 \
             AND ((l.classid::bigint << 32) | l.objid::bigint) = $2), \
           clock_timestamp() \
         FROM pg_stat_activity a WHERE a.datname = current_database() \
           AND a.application_name = $1 AND a.xact_start IS NOT NULL",
    )
    .bind(label)
    .bind(LEASE_KEY)
    .fetch_all(pool.read())
    .await
    .unwrap()
}

/// The user write of one deterministic overlap sample: a dedicated account
/// favourites a work that the paused background transaction has not locked.
struct ProofWrite<'a> {
    pool: &'a db::Database,
    client: &'a reqwest::Client,
    endpoint: &'a str,
    token: &'a str,
    user: uuid::Uuid,
    work: &'a str,
    /// Tests only: stop after the mutation has been answered and before its
    /// effect is verified (point `proof-before-verify`).
    before_verify: Option<&'a locks::pause_points::PausePoints>,
}

/// One proven overlap. The background writer `label` is stopped right after its
/// real BEGIN (`opened`). While it stays there this sends a real HTTP mutation,
/// waits for its successful answer, verifies the stored effect, checks that the
/// SAME background transaction is still open, and only then releases it. The
/// pause is released on every path, so a failed proof never hangs the worker.
async fn prove_overlap(
    write: &ProofWrite<'_>,
    label: &str,
    opened: &locks::pause_points::ArmedPoint,
    wait: Duration,
    worker: Option<&mut tokio::sync::watch::Receiver<bool>>,
) -> Value {
    let mut proof = json!({"kind":label,"proven":false,"user_write":{"mutation":"setFavorite","work":write.work}});
    let failed = |mut proof: Value, reason: String| {
        opened.release();
        proof["reason"] = json!(reason);
        proof["released_at"] = json!(chrono::Utc::now().to_rfc3339());
        proof
    };
    let reached = tokio::time::timeout(wait, async {
        match worker {
            Some(status) => {
                if worker_finished(status) {
                    return false;
                }
                tokio::select! {
                    () = opened.reached() => true,
                    _ = status.changed() => false,
                }
            }
            None => {
                opened.reached().await;
                true
            }
        }
    })
    .await;
    if matches!(reached, Ok(false)) {
        return failed(
            proof,
            "background worker ended before its transaction opened".into(),
        );
    }
    if reached.is_err() {
        return failed(
            proof,
            format!(
                "no background transaction opened within {} s",
                wait.as_secs_f64()
            ),
        );
    }
    let before = background_transaction(write.pool, label).await;
    let [(pid, xact_start, state, lease, checked)] = before.as_slice() else {
        return failed(
            proof,
            format!(
                "expected one open {label} transaction, saw {}",
                before.len()
            ),
        );
    };
    proof["background"] = json!({"pid":pid,"xact_started_at":xact_start.to_rfc3339(),"state_while_held":state,
        "held_background_lease":lease,"open_checked_before_write_at":checked.to_rfc3339()});
    // (answer, server clock): no row at all, or the row with the favourite flag.
    async fn stored(
        write: &ProofWrite<'_>,
        favorite: Option<bool>,
    ) -> Result<(bool, Timestamp), sqlx::Error> {
        match favorite {
            None => sqlx::query_as("SELECT NOT EXISTS (SELECT 1 FROM user_library WHERE user_id=$1 AND series_id=$2), clock_timestamp()")
                .bind(write.user).bind(write.work).fetch_one(write.pool.read()).await,
            Some(favorite) => sqlx::query_as("SELECT EXISTS (SELECT 1 FROM user_library WHERE user_id=$1 AND series_id=$2 AND is_favorite=$3), clock_timestamp()")
                .bind(write.user).bind(write.work).bind(favorite).fetch_one(write.pool.read()).await,
        }
    }
    let absent = match stored(write, None).await {
        Ok(row) => row,
        Err(error) => {
            db::record_read_database_error(&error);
            return failed(proof, "could not read the user row before the write".into());
        }
    };
    proof["user_write"]["effect_absent_before"] = json!(absent.0);
    proof["user_write"]["sent_at"] = json!(chrono::Utc::now().to_rfc3339());
    let response = write
        .client
        .post(write.endpoint)
        .bearer_auth(write.token)
        .json(&json!({"query":format!(
            "mutation {{ setFavorite(seriesId:{},favorite:true) {{ id }} }}",
            json!(write.work)
        )}))
        .send()
        .await;
    let answer = match response {
        Ok(response) if response.status().is_success() => match response.json::<Value>().await {
            Ok(body)
                if body
                    .get("errors")
                    .and_then(Value::as_array)
                    .is_none_or(|errors| errors.is_empty()) =>
            {
                Ok(())
            }
            Ok(_) => Err("the mutation returned GraphQL errors".to_owned()),
            Err(_) => Err("the mutation answer was not JSON".to_owned()),
        },
        Ok(response) => Err(format!("the mutation returned HTTP {}", response.status())),
        Err(_) => Err("the mutation did not complete (network error or timeout)".to_owned()),
    };
    proof["user_write"]["returned_at"] = json!(chrono::Utc::now().to_rfc3339());
    if let Err(reason) = answer {
        return failed(proof, reason);
    }
    if let Some(points) = write.before_verify {
        points.hit("proof-before-verify").await;
    }
    let verified = match stored(write, Some(true)).await {
        Ok(row) => row,
        Err(error) => {
            db::record_read_database_error(&error);
            return failed(proof, "could not read the user row after the write".into());
        }
    };
    proof["user_write"]["effect_verified"] = json!(verified.0);
    proof["user_write"]["effect_verified_at"] = json!(verified.1.to_rfc3339());
    // Read again AFTER the verified commit: still the same transaction, still open.
    let after = background_transaction(write.pool, label).await;
    let still_open = after
        .iter()
        .find(|row| row.0 == *pid && row.1 == *xact_start);
    proof["background"]["open_checked_after_write_at"] =
        json!(still_open.map(|row| row.4.to_rfc3339()));
    proof["background"]["same_transaction_still_open"] = json!(still_open.is_some());
    opened.release();
    proof["released_at"] = json!(chrono::Utc::now().to_rfc3339());
    let reason = if !absent.0 {
        Some("the user row existed before the write, so the effect proves nothing")
    } else if !verified.0 {
        Some("the mutation succeeded but its effect is not stored")
    } else if still_open.is_none() {
        Some("the background transaction ended before the user write was verified")
    } else if !*lease {
        Some("the paused background writer did not own the background lease")
    } else {
        None
    };
    proof["proven"] = json!(reason.is_none());
    if let Some(reason) = reason {
        proof["reason"] = json!(reason);
    }
    proof
}

/// Dedicated accounts and an independent work for the two deterministic samples.
struct ProofAccounts<'a> {
    work: &'a str,
    catalogue: (&'a str, uuid::Uuid),
    rebuild: (&'a str, uuid::Uuid),
    wait: Duration,
}

/// The two real background workers, running, and their deterministic samples.
struct Background {
    catalogue: tokio::task::JoinHandle<BackgroundRun>,
    rebuild: tokio::task::JoinHandle<BackgroundRun>,
    catalogue_proof: Value,
    rebuild_proof: Value,
    /// The rebuild worker's completed cycles, as it completes them.
    rebuild_cycles: Arc<Mutex<Vec<RebuildCycle>>>,
    finished: [tokio::sync::watch::Receiver<bool>; 2],
}

fn roles_are_disjoint(replayed: &[String], perturbed: &[String]) -> bool {
    perturbed.iter().all(|work| !replayed.contains(work))
}

/// Part 1 of a profile. Start both real background workers and take one
/// deterministic overlap sample from each; the workers are returned running free
/// until `stop` is set. Separate handles contend for the same database-scoped
/// background lease, exactly as two server processes or the CLI would.
/// `replayed` are the works the catalogue worker replays; `perturbed` the
/// works whose `year` the rebuild worker flips before each Browse rebuild. The
/// two must be disjoint: in the merged tree a rebuild opens a writer
/// transaction only for a row that differs, and the replay undoes the
/// perturbation of a work it replays, so the rebuild sample would never see a
/// transaction open.
async fn start_background_with_proofs(
    pool: &db::Database,
    client: &reqwest::Client,
    endpoint: &str,
    replayed: &[String],
    perturbed: &[String],
    proof: &ProofAccounts<'_>,
    stop: &Arc<AtomicBool>,
) -> Background {
    assert!(
        roles_are_disjoint(replayed, perturbed),
        "a replayed work must not be perturbed"
    );
    let mut replays = Vec::new();
    for work in replayed {
        let (external_id, input) = stored_catalogue_input(pool, work).await.unwrap();
        replays.push(Replay {
            work: work.clone(),
            external_id,
            input,
        });
    }
    // Catalogue: its first real transaction is held open until a user write has
    // returned and is verified in the database.
    let catalogue = benchmark_background_pool(pool, CATALOGUE).await;
    let points = locks::pause_points::PausePoints::default();
    let catalogue_open = points.arm("transaction-open");
    let (catalogue_job, mut catalogue_finished) =
        spawn_background(catalogue_worker(catalogue, points, stop.clone(), replays));
    let catalogue_proof = prove_overlap(
        &ProofWrite {
            pool,
            client,
            endpoint,
            token: proof.catalogue.0,
            user: proof.catalogue.1,
            work: proof.work,
            before_verify: None,
        },
        CATALOGUE,
        &catalogue_open,
        proof.wait,
        Some(&mut catalogue_finished),
    )
    .await;
    // Rebuild: started after the catalogue sample, so it queues for the lease
    // behind free-running catalogue work, then is held at its own first BEGIN.
    // That BEGIN is the first publication turn with a changed row: at the
    // latest the Browse publication of the perturbed works, after the two feed
    // rebuilds have staged and found nothing to publish.
    let rebuild = benchmark_background_pool(pool, REBUILD).await;
    let perturber = Perturber::connect(pool, perturbed.to_vec()).await;
    let points = locks::pause_points::PausePoints::default();
    let rebuild_open = points.arm("transaction-open");
    let rebuild_cycles = Arc::new(Mutex::new(Vec::new()));
    let (rebuild_job, mut rebuild_finished) = spawn_background(rebuild_worker(
        rebuild,
        points,
        stop.clone(),
        perturber,
        rebuild_cycles.clone(),
        None,
    ));
    let rebuild_proof = prove_overlap(
        &ProofWrite {
            pool,
            client,
            endpoint,
            token: proof.rebuild.0,
            user: proof.rebuild.1,
            work: proof.work,
            before_verify: None,
        },
        REBUILD,
        &rebuild_open,
        proof.wait,
        Some(&mut rebuild_finished),
    )
    .await;
    Background {
        catalogue: catalogue_job,
        rebuild: rebuild_job,
        catalogue_proof,
        rebuild_proof,
        rebuild_cycles,
        finished: [catalogue_finished, rebuild_finished],
    }
}

/// The four mutations of the write mix. The first three are user-state writes
/// (the historical kinds); in the merged tree they take the work guard (`FOR
/// NO KEY UPDATE` on the `work` row). The fourth is an admin metadata edit: it
/// updates the `work` row itself and that work's Browse projection row. The
/// catalogue replay and the rebuild's publication turns lock the same `work`
/// rows, and the publication writes the same Browse rows.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum Kind {
    Favorite,
    Status,
    Progress,
    WorkEdit,
}

const KINDS: [Kind; 4] = [Kind::Favorite, Kind::Status, Kind::Progress, Kind::WorkEdit];

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Favorite => "setFavorite",
            Kind::Status => "setLibraryStatus",
            Kind::Progress => "setProgress",
            Kind::WorkEdit => "updateSeriesMetadata",
        }
    }
}

/// A work every side of the benchmark touches, and one of its English chapters.
#[derive(Clone, Debug)]
struct Contested {
    work: String,
    chapter: String,
}

/// What a write leaves in the database, for the effect check.
#[derive(Clone, Debug, Eq, PartialEq)]
enum StoredValue {
    Favorite(bool),
    Status(&'static str),
    Page(i64),
    Description(String),
}

/// One scheduled write. `account` indexes the profile's accounts (users first,
/// then admins); `target` indexes the contested works; `index` counts the
/// earlier writes of the same (account, kind) pair.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PlannedWrite {
    sequence: u64,
    account: usize,
    kind: Kind,
    target: usize,
    index: u64,
}

impl PlannedWrite {
    /// Derived from the pair's own write index, so two consecutive writes of
    /// one (account, kind) pair always store different values, whatever the
    /// number of accounts. (The old values came from residues of the global
    /// sequence; with twenty users each pair wrote one constant value.)
    fn value(&self) -> StoredValue {
        match self.kind {
            Kind::Favorite => StoredValue::Favorite(self.index.is_multiple_of(2)),
            Kind::Status => StoredValue::Status(["reading", "plan"][(self.index % 2) as usize]),
            Kind::Progress => StoredValue::Page(1 + (self.index % 14) as i64),
            Kind::WorkEdit => StoredValue::Description(format!(
                "komika-bench edit by account {} number {}",
                self.account, self.index
            )),
        }
    }

    fn mutation(&self, target: &Contested) -> String {
        match self.value() {
            StoredValue::Favorite(favorite) => format!(
                "mutation {{ setFavorite(seriesId:{},favorite:{favorite}) {{ id }} }}",
                json!(target.work)
            ),
            StoredValue::Status(status) => format!(
                "mutation {{ setLibraryStatus(seriesId:{},status:{}) {{ id }} }}",
                json!(target.work),
                json!(status)
            ),
            StoredValue::Page(page) => format!(
                "mutation {{ setProgress(chapterId:{},lastPageRead:{page},read:false) }}",
                json!(target.chapter)
            ),
            StoredValue::Description(text) => format!(
                "mutation {{ updateSeriesMetadata(input:{{seriesId:{},description:{}}}) {{ id }} }}",
                json!(target.work),
                json!(text)
            ),
        }
    }
}

/// The whole write schedule, as a pure function of its sizes.
///
/// Not mixed (the SQLite-comparable baseline): `setFavorite` only, users in turn.
/// Mixed: every tenth write is an admin metadata edit, admins in turn, admin `a`
/// always editing contested work `a` (at most one editor per work, so the stored
/// value is unambiguous; there are at most two admins, because the edit runs
/// in the two-permit maintenance lane). The other nine are user writes: users in turn, and in round
/// `r` user `u` sends kind `(r + u) % 3`, so every user sends every kind and a
/// pair returns every third round. A user always writes the same contested
/// work, `u % contested`. The schedule has no end: the window decides how
/// many of its writes are sent.
struct WritePlan {
    users: usize,
    admins: usize,
    contested: usize,
    mixed: bool,
    sequence: u64,
    user_writes: usize,
    admin_writes: usize,
    pair_writes: HashMap<(usize, Kind), u64>,
}

impl WritePlan {
    fn new(users: usize, admins: usize, contested: usize, mixed: bool) -> Self {
        assert!(users > 0 && contested > 0);
        Self {
            users,
            admins,
            contested,
            mixed,
            sequence: 0,
            user_writes: 0,
            admin_writes: 0,
            pair_writes: HashMap::new(),
        }
    }
}

impl Iterator for WritePlan {
    type Item = PlannedWrite;

    fn next(&mut self) -> Option<PlannedWrite> {
        let sequence = self.sequence;
        self.sequence += 1;
        let (account, kind, target) = if self.mixed && self.admins > 0 && sequence % 10 == 9 {
            let admin = self.admin_writes % self.admins;
            self.admin_writes += 1;
            (self.users + admin, Kind::WorkEdit, admin % self.contested)
        } else {
            let user = self.user_writes % self.users;
            let round = self.user_writes / self.users;
            self.user_writes += 1;
            let kind = if self.mixed {
                KINDS[(round + user) % 3]
            } else {
                Kind::Favorite
            };
            (user, kind, user % self.contested)
        };
        let index = self.pair_writes.entry((account, kind)).or_default();
        let write = PlannedWrite {
            sequence,
            account,
            kind,
            target,
            index: *index,
        };
        *index += 1;
        Some(write)
    }
}

/// The first `total` writes of the schedule.
fn plan_writes(
    total: u64,
    users: usize,
    admins: usize,
    contested: usize,
    mixed: bool,
) -> Vec<PlannedWrite> {
    WritePlan::new(users, admins, contested, mixed)
        .take(total as usize)
        .collect()
}

/// Requests the maintenance lane of `request_budget` admits at a time
/// (`RequestBudgets::default`). It does not queue: one more is answered 503.
const MAINTENANCE_PERMITS: usize = 2;

/// Admin accounts of the heavy profile. An admin's `updateSeriesMetadata` runs
/// in the maintenance lane and one account sends one request at a time, so
/// this many admins can never be refused there. One admin per contested work
/// (four by default) could: three edits in flight at once, the third gets 503
/// "Server busy" and the gate fails on a failed write that says nothing about
/// the database.
fn admin_accounts(contested: usize) -> usize {
    contested.min(MAINTENANCE_PERMITS)
}

/// (replayed, republished): the contested works the catalogue worker replays
/// (index 0, 2, ...) and the ones the rebuild worker perturbs, so that the
/// rebuild's Browse publication has to lock and rewrite them (index 1, 3, ...).
fn split_contested(contested: &[Contested]) -> (Vec<String>, Vec<String>) {
    let works = |odd: usize| {
        contested
            .iter()
            .enumerate()
            .filter(|(index, _)| index % 2 == odd)
            .map(|(_, contested)| contested.work.clone())
            .collect()
    };
    (works(0), works(1))
}

/// The accounts and works of one profile's write load.
struct WriteTargets {
    contested: Vec<Contested>,
    /// (bearer token, user id): users first, then admins.
    accounts: Vec<(String, uuid::Uuid)>,
}

/// Send one mutation. The latency clock runs from the send until the response
/// body is complete, exactly what `loadtest.mjs` measures; the body is parsed
/// after the clock has stopped. Returns (ms, http error, network error,
/// GraphQL error).
async fn send_write(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    mutation: &str,
) -> (f64, bool, bool, bool) {
    let request = client
        .post(endpoint)
        .bearer_auth(token)
        .json(&json!({"query":mutation}));
    let sent = Instant::now();
    match request.send().await {
        Ok(response) => {
            let http = !response.status().is_success();
            let body = response.bytes().await;
            let latency = sent.elapsed().as_secs_f64() * 1000.0;
            match body
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            {
                Some(answer) => (
                    latency,
                    http,
                    false,
                    answer
                        .get("errors")
                        .and_then(Value::as_array)
                        .is_some_and(|errors| !errors.is_empty()),
                ),
                None => (latency, http, true, false),
            }
        }
        Err(_) => (sent.elapsed().as_secs_f64() * 1000.0, false, true, false),
    }
}

/// Is the value this write stores in the database now? Read through the
/// instrument sessions, after the latency clock has stopped.
async fn verify_effect(
    instruments: &sqlx::PgPool,
    user: uuid::Uuid,
    target: &Contested,
    write: &PlannedWrite,
) -> Result<bool, sqlx::Error> {
    match write.value() {
        StoredValue::Favorite(favorite) => sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM user_library WHERE user_id=$1 AND series_id=$2 AND is_favorite=$3)")
            .bind(user).bind(&target.work).bind(favorite).fetch_one(instruments).await,
        StoredValue::Status(status) => sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM user_library WHERE user_id=$1 AND series_id=$2 AND status=$3)")
            .bind(user).bind(&target.work).bind(status).fetch_one(instruments).await,
        StoredValue::Page(page) => sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM canonical_progress WHERE user_id=$1 AND work_id=$2 AND chapter_id=$3 AND last_page_read=$4 AND read=false)")
            .bind(user).bind(&target.work).bind(&target.chapter).bind(page).fetch_one(instruments).await,
        StoredValue::Description(text) => sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM work WHERE id=$1 AND description_override=$2)")
            .bind(&target.work).bind(text).fetch_one(instruments).await,
    }
}

struct WriteOutcome {
    latency_ms: f64,
    queue_delay_ms: f64,
    http: bool,
    network: bool,
    gql: bool,
    correct: bool,
    instrument_error: bool,
    kind: Kind,
    request_start: Timestamp,
    activity: Vec<BackgroundActivity>,
}

/// Everything the write driver measured.
#[derive(Default)]
struct WriteResults {
    /// (latency ms, succeeded) per request.
    samples: Vec<(f64, bool)>,
    /// Scheduled arrival until the send: per-account ordering and the
    /// pre-send sample. Not part of the latency.
    queue_delays: Vec<f64>,
    /// Scheduled arrival until the response body is complete: queue delay plus
    /// latency. This is what an open-loop driver without per-account ordering
    /// (`loadtest.mjs`) would have measured for a request that had to wait
    /// behind a stalled one.
    arrival_latencies: Vec<f64>,
    http_errors: u64,
    net_errors: u64,
    gql_errors: u64,
    wrong_effects: u64,
    verified: BTreeMap<Kind, u64>,
    sent: BTreeMap<Kind, u64>,
    instrument_errors: u64,
    catalogue_overlap: u64,
    rebuild_overlap: u64,
    overlap_events: Vec<Value>,
    wall_seconds: f64,
}

/// One profile's write load, as the driver needs it.
struct WriteLoad<'a> {
    client: &'a reqwest::Client,
    endpoint: &'a str,
    instruments: &'a sqlx::PgPool,
    targets: &'a Arc<WriteTargets>,
    rate: u64,
    /// The shortest length of the load; the driver does not return earlier.
    duration: Duration,
    /// Set when the window ends: no write is sent after it.
    stop_sending: &'a AtomicBool,
    /// A request that starts after this instant is not an overlap sample.
    /// Unset means the window has no end (a finite plan in a test).
    window_end: &'a std::sync::OnceLock<Timestamp>,
}

/// Open-loop writes at `rate` per second from `started`, every request awaited,
/// until the plan ends or `stop_sending` is set. One account sends one request
/// at a time, so its effect is checked before its next write can replace it;
/// different accounts stay concurrent.
///
/// That ordering is where this driver differs from `loadtest.mjs` under a
/// stall: a write scheduled behind a stalled write of the same account waits
/// for its turn, and that wait is queue delay, not latency. `loadtest.mjs`
/// would have sent it at once and measured the stall in its latency. The
/// comparable number is `arrival_latencies` (queue delay plus latency).
async fn drive_writes(
    load: &WriteLoad<'_>,
    plan: impl Iterator<Item = PlannedWrite>,
    started: tokio::time::Instant,
) -> WriteResults {
    let gates: Vec<_> = load
        .targets
        .accounts
        .iter()
        .map(|_| Arc::new(tokio::sync::Semaphore::new(1)))
        .collect();
    let mut requests = Vec::new();
    for write in plan {
        let scheduled = started + Duration::from_secs_f64(write.sequence as f64 / load.rate as f64);
        tokio::time::sleep_until(scheduled).await;
        if load.stop_sending.load(Ordering::Relaxed) {
            break;
        }
        let client = load.client.clone();
        let endpoint = load.endpoint.to_owned();
        let instruments = load.instruments.clone();
        let targets = load.targets.clone();
        let gate = gates[write.account].clone();
        requests.push(tokio::spawn(async move {
            let _turn = gate.acquire_owned().await.unwrap();
            let (token, user) = &targets.accounts[write.account];
            let target = &targets.contested[write.target];
            let mutation = write.mutation(target);
            let request_start = chrono::Utc::now();
            let activity = background_activity(&instruments).await;
            let mut instrument_error = activity.is_err();
            let queue_delay_ms = scheduled.elapsed().as_secs_f64() * 1000.0;
            let (latency_ms, http, network, gql) =
                send_write(&client, &endpoint, token, &mutation).await;
            // Off the clock: the effect check.
            let correct = !http
                && !network
                && !gql
                && match verify_effect(&instruments, *user, target, &write).await {
                    Ok(correct) => correct,
                    Err(_) => {
                        instrument_error = true;
                        false
                    }
                };
            WriteOutcome {
                latency_ms,
                queue_delay_ms,
                http,
                network,
                gql,
                correct,
                instrument_error,
                kind: write.kind,
                request_start,
                activity: activity.unwrap_or_default(),
            }
        }));
    }
    // An open-loop load lasts its planned time even when every answer is early.
    tokio::time::sleep_until(started + load.duration).await;
    let mut results = WriteResults::default();
    // Every request has been sent, so in a profile the window has ended by now.
    let window_end = load.window_end.get().copied();
    for request in requests {
        let outcome = request.await.unwrap();
        let ok = !outcome.http && !outcome.network && !outcome.gql;
        *results.sent.entry(outcome.kind).or_default() += 1;
        if ok {
            if outcome.correct {
                *results.verified.entry(outcome.kind).or_default() += 1;
            } else {
                results.wrong_effects += 1;
            }
        }
        if window_end.is_none_or(|end| outcome.request_start < end) {
            for activity in outcome.activity {
                if activity.xact_start <= outcome.request_start {
                    if activity.application == CATALOGUE {
                        results.catalogue_overlap += 1;
                    } else {
                        results.rebuild_overlap += 1;
                    }
                    if results.overlap_events.len() < 200 {
                        results
                            .overlap_events
                            .push(json!({"kind":activity.application,
                            "user_write_started_at":outcome.request_start.to_rfc3339(),
                            "background_xact_started_at":activity.xact_start.to_rfc3339(),
                            "background_statement_started_at":activity.query_start.to_rfc3339()}));
                    }
                }
            }
        }
        results.http_errors += u64::from(outcome.http);
        results.net_errors += u64::from(outcome.network);
        results.gql_errors += u64::from(outcome.gql);
        results.instrument_errors += u64::from(outcome.instrument_error);
        results.samples.push((outcome.latency_ms, ok));
        results.queue_delays.push(outcome.queue_delay_ms);
        results
            .arrival_latencies
            .push(outcome.queue_delay_ms + outcome.latency_ms);
    }
    results.wall_seconds = started.elapsed().as_secs_f64();
    results
}

/// Which side of the gate a session is on, by its application name.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Side {
    /// A foreground writer of the measured handle: a user request's write.
    Foreground,
    /// A labelled leased background writer: catalogue replay or rebuild.
    Background,
    /// Everything else: readers, the measured handle's own leased writer,
    /// snapshot sessions, instruments, the perturbation, unknown sessions.
    Other,
}

fn side(application: &str) -> Side {
    match application {
        FOREGROUND => Side::Foreground,
        CATALOGUE | REBUILD => Side::Background,
        _ => Side::Other,
    }
}

/// One (waiter, blocker) row of a lock-wait sample.
#[derive(Clone, Debug)]
struct WaitSample {
    waiter_pid: i32,
    waiter: String,
    wait_event: String,
    /// `pg_locks.waitstart` of the waiter (NULL for a moment after the wait begins).
    wait_started: Option<Timestamp>,
    sampled_at: Timestamp,
    /// The waiter is queued for the background lease itself.
    lease: bool,
    blocker: Option<String>,
    waiter_query: String,
    blocker_query: String,
}

/// Backends waiting on a heavyweight lock, each with the sessions
/// `pg_blocking_pids()` names. Only waiting backends pay for the lock-manager
/// lookups, so an uncontended sample is one cheap scan of pg_stat_activity.
async fn sample_lock_waits(
    executor: impl sqlx::PgExecutor<'_>,
) -> Result<Vec<WaitSample>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT w.pid, COALESCE(w.application_name, ''), COALESCE(w.wait_event, ''), \
                l.waitstart, clock_timestamp(), COALESCE(l.lease, false), \
                b.application_name, left(COALESCE(w.query, ''), 160), \
                left(COALESCE(b.query, ''), 160) \
           FROM pg_stat_activity w \
           LEFT JOIN LATERAL ( \
                SELECT min(k.waitstart) AS waitstart, \
                       bool_or(k.locktype = 'advisory' AND k.objsubid = 1 \
                         AND ((k.classid::bigint << 32) | k.objid::bigint) = $1) AS lease \
                  FROM pg_locks k WHERE k.pid = w.pid AND NOT k.granted) l ON true \
           LEFT JOIN LATERAL unnest(pg_blocking_pids(w.pid)) AS blocked_by(pid) ON true \
           LEFT JOIN pg_stat_activity b ON b.pid = blocked_by.pid \
          WHERE w.datname = current_database() AND w.wait_event_type = 'Lock' \
            AND w.pid <> pg_backend_pid()",
    )
    .bind(LEASE_KEY)
    .fetch_all(executor)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(WaitSample {
                waiter_pid: row.try_get(0)?,
                waiter: row.try_get(1)?,
                wait_event: row.try_get(2)?,
                wait_started: row.try_get(3)?,
                sampled_at: row.try_get(4)?,
                lease: row.try_get(5)?,
                blocker: row.try_get(6)?,
                waiter_query: row.try_get(7)?,
                blocker_query: row.try_get(8)?,
            })
        })
        .collect()
}

/// One wait of one backend, from the first sample that saw it to the last.
#[derive(Debug)]
struct WaitEpisode {
    /// `pg_locks.waitstart`: when the wait began (server clock).
    started: Timestamp,
    waiter: String,
    blockers: BTreeSet<String>,
    lease: bool,
    wait_event: String,
    samples: u64,
    last_sampled: Timestamp,
    waited_ms: f64,
    waiter_query: String,
    blocker_query: String,
}

/// Episodes, samples and observed wait time of one direction.
#[derive(Clone, Debug, Default, PartialEq)]
struct WaitTotals {
    episodes: u64,
    samples: u64,
    max_wait_ms: f64,
    total_wait_ms: f64,
}

const WAIT_DIRECTIONS: [&str; 6] = [
    "foreground_waited_on_background",
    "background_waited_on_foreground",
    "foreground_waited_on_foreground",
    "background_waited_on_background",
    "background_lease_queue",
    "other",
];

/// The lock waits the observer saw, keyed by (waiting backend, wait start).
/// The wait time of an episode is what its last sample saw, so it is a lower
/// bound, short by at most one sampling interval; a wait that begins and ends
/// between two samples is not seen at all.
#[derive(Debug, Default)]
struct LockWaits {
    episodes: BTreeMap<(i32, i64), WaitEpisode>,
    /// Rows sampled in the instant before `pg_locks.waitstart` is set. Two such
    /// waits of one backend cannot be told apart, so they are counted here and
    /// are never an episode: the gate may miss a wait this way, never invent one.
    without_wait_start: u64,
}

impl LockWaits {
    fn record(&mut self, sample: &WaitSample) {
        let Some(started) = sample.wait_started else {
            self.without_wait_start += 1;
            return;
        };
        let episode = self
            .episodes
            .entry((sample.waiter_pid, started.timestamp_micros()))
            .or_insert_with(|| WaitEpisode {
                started,
                waiter: sample.waiter.clone(),
                blockers: BTreeSet::new(),
                lease: false,
                wait_event: sample.wait_event.clone(),
                samples: 0,
                last_sampled: sample.sampled_at - chrono::Duration::microseconds(1),
                waited_ms: 0.0,
                waiter_query: sample.waiter_query.clone(),
                blocker_query: String::new(),
            });
        // One sampling instant yields one row per blocker.
        if episode.last_sampled != sample.sampled_at {
            episode.samples += 1;
            episode.last_sampled = sample.sampled_at;
        }
        episode.lease |= sample.lease;
        if let Some(blocker) = &sample.blocker {
            if episode.blockers.insert(blocker.clone()) && side(blocker) != Side::Other {
                episode.blocker_query.clone_from(&sample.blocker_query);
            }
        }
        let waited = (sample.sampled_at - started)
            .num_microseconds()
            .unwrap_or(0) as f64;
        episode.waited_ms = episode.waited_ms.max(waited / 1000.0);
    }

    /// Queueing for the background lease is how background writers take turns:
    /// never contention between the sides. Otherwise a wait counts for a
    /// direction when ANY of its blockers is on the other side.
    fn direction(episode: &WaitEpisode) -> &'static str {
        let blocked_by = |wanted: Side| episode.blockers.iter().any(|name| side(name) == wanted);
        match side(&episode.waiter) {
            _ if episode.lease => "background_lease_queue",
            Side::Foreground if blocked_by(Side::Background) => "foreground_waited_on_background",
            Side::Background if blocked_by(Side::Foreground) => "background_waited_on_foreground",
            Side::Foreground if blocked_by(Side::Foreground) => "foreground_waited_on_foreground",
            Side::Background if blocked_by(Side::Background) => "background_waited_on_background",
            _ => "other",
        }
    }

    fn totals(&self) -> BTreeMap<&'static str, WaitTotals> {
        let mut totals: BTreeMap<_, _> = WAIT_DIRECTIONS
            .into_iter()
            .map(|direction| (direction, WaitTotals::default()))
            .collect();
        for episode in self.episodes.values() {
            let total = totals.get_mut(Self::direction(episode)).unwrap();
            total.episodes += 1;
            total.samples += episode.samples;
            total.max_wait_ms = total.max_wait_ms.max(episode.waited_ms);
            total.total_wait_ms += episode.waited_ms;
        }
        totals
    }

    /// The kinds of background work on the other side of a wait between the
    /// two sides: the blockers of a waiting foreground writer, or the waiting
    /// background writer itself. Empty for every other wait. The rebuild
    /// writer is placed by the instant the wait began.
    fn background_kinds(
        episode: &WaitEpisode,
        timeline: &[(Timestamp, &'static str)],
    ) -> BTreeSet<&'static str> {
        let kind = |application: &str| background_kind(application, episode.started, timeline);
        match Self::direction(episode) {
            "foreground_waited_on_background" => episode
                .blockers
                .iter()
                .filter(|name| side(name) == Side::Background)
                .map(|name| kind(name))
                .collect(),
            "background_waited_on_foreground" => [kind(&episode.waiter)].into(),
            _ => BTreeSet::new(),
        }
    }

    /// Episodes between the two sides per kind of background work:
    /// (foreground waited on it, it waited on foreground). An episode whose
    /// blockers are of two kinds counts once for each.
    fn by_background_work(
        &self,
        timeline: &[(Timestamp, &'static str)],
    ) -> BTreeMap<&'static str, (u64, u64)> {
        let mut counts: BTreeMap<_, _> = BACKGROUND_KINDS
            .into_iter()
            .map(|kind| (kind, (0, 0)))
            .collect();
        for episode in self.episodes.values() {
            let foreground_waited = Self::direction(episode) == "foreground_waited_on_background";
            for kind in Self::background_kinds(episode, timeline) {
                let count = counts.get_mut(kind).unwrap();
                if foreground_waited {
                    count.0 += 1;
                } else {
                    count.1 += 1;
                }
            }
        }
        counts
    }

    fn json(&self, timeline: &[(Timestamp, &'static str)]) -> Value {
        let mut out = json!({"note":"sampled about once a millisecond from pg_stat_activity, pg_locks and pg_blocking_pids(); wait times are lower bounds from pg_locks.waitstart, and a wait shorter than the sampling interval can be missed"});
        for (direction, total) in self.totals() {
            out[direction] = json!({"episodes":total.episodes,"samples":total.samples,
                "max_wait_ms":total.max_wait_ms,"total_wait_ms":total.total_wait_ms});
        }
        out["by_background_work"] = json!({"note":"episodes between the two sides, by what the background side was doing when the wait began: the catalogue writer is the replay; the rebuild writer is placed by time from the start of each rebuild call and the rebuild's own INFO events (client clock against the server's wait start, same machine). browse_publication is the whole Browse rebuild on the writer: tag sync, tag keys and the publication of browse_catalogue, tag and feed_genre_facet",
            "episodes":self.by_background_work(timeline).into_iter().map(|(kind,(foreground,background))|
                (kind.to_owned(),json!({"foreground_waited_on_it":foreground,"it_waited_on_foreground":background})))
                .collect::<serde_json::Map<_,_>>()});
        // Waits between the two sides first, then the rest, never the lease queue.
        out["samples_without_wait_start"] = json!(self.without_wait_start);
        let mut examples: Vec<_> = self
            .episodes
            .values()
            .filter(|episode| !episode.lease)
            .collect();
        examples.sort_by_key(|episode| match Self::direction(episode) {
            "foreground_waited_on_background" | "background_waited_on_foreground" => 0,
            "other" => 2,
            _ => 1,
        });
        // Reserve one actual episode per observed cross-side kind/direction
        // before filling the remaining slots. Common catalogue waits must not
        // crowd a later, rare Browse wait out of the bounded evidence array.
        let mut selected = Vec::new();
        let mut selected_indices = BTreeSet::new();
        let mut represented = BTreeSet::new();
        for (index, episode) in examples.iter().enumerate() {
            let direction = Self::direction(episode);
            let kinds = Self::background_kinds(episode, timeline);
            if kinds
                .iter()
                .any(|kind| !represented.contains(&(direction, *kind)))
            {
                represented.extend(kinds.into_iter().map(|kind| (direction, kind)));
                selected_indices.insert(index);
                selected.push(*episode);
            }
        }
        // At most five background kinds in two cross-side directions reserve
        // ten slots. Keep the existing hard bound even if that set grows later.
        selected.truncate(20);
        for (index, episode) in examples.into_iter().enumerate() {
            if selected.len() >= 20 {
                break;
            }
            if selected_indices.insert(index) {
                selected.push(episode);
            }
        }
        out["examples"] = selected
            .into_iter()
            .map(|episode| {
                json!({"direction":Self::direction(episode),"waiter":episode.waiter,
                    "background_work":Self::background_kinds(episode, timeline),
                    "wait_started_at":episode.started.to_rfc3339(),
                    "blockers":episode.blockers,"wait_event":episode.wait_event,
                    "observed_wait_ms":episode.waited_ms,"samples":episode.samples,
                    "waiter_statement":episode.waiter_query,"blocker_statement":episode.blocker_query})
            })
            .collect();
        out
    }
}

/// What the observer saw during the measurement.
#[derive(Default)]
struct Observation {
    /// (label, statement start, statement head) of background write statements
    /// that started, in a transaction that started, at or after the window
    /// start. The window's end is not known while observing; the summary
    /// leaves out what started after it.
    statements: BTreeSet<(String, Timestamp, String)>,
    waits: LockWaits,
    iterations: u64,
    errors: u64,
    first_error: Option<String>,
}

impl Observation {
    /// Distinct write statements of one background writer inside the window.
    fn statements_inside(&self, label: &str, window_end: Timestamp) -> u64 {
        self.statements
            .iter()
            .filter(|(kind, started, _)| kind == label && *started <= window_end)
            .count() as u64
    }
}

async fn observe_once(
    connection: &mut sqlx::PgConnection,
    seen: &mut Observation,
    window_start: Timestamp,
) -> Result<(), sqlx::Error> {
    for activity in background_activity(&mut *connection).await? {
        if starts_inside_window(&activity, window_start) {
            let head: String = activity.query.chars().take(60).collect();
            seen.statements
                .insert((activity.application, activity.query_start, head));
        }
    }
    for sample in sample_lock_waits(&mut *connection).await? {
        seen.waits.record(&sample);
    }
    Ok(())
}

/// pid to application name of every benchmark session that existed between a
/// profile's two server-log markers. The server log is cluster-wide: this map
/// tells a line of the benchmark or of the server under test from a line of
/// any other backend (an autovacuum worker that was cancelled logs 57014).
/// A session shorter than the sampling interval can be missed, and a pid the
/// operating system reuses for another backend inside the profile is taken
/// for the benchmark's; the first can only hide a line, the second only add one.
#[derive(Default)]
struct Census {
    sessions: HashMap<i32, String>,
    passes: u64,
    errors: u64,
}

async fn census_once(
    executor: impl sqlx::PgExecutor<'_>,
) -> Result<Vec<(i32, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT pid, application_name FROM pg_stat_activity \
         WHERE datname = current_database() AND application_name LIKE $1",
    )
    .bind(format!("{BENCH_APPLICATION}%"))
    .fetch_all(executor)
    .await
}

/// Sample the benchmark's sessions about every 50 ms until told to stop, and
/// once more after that.
async fn census(instruments: sqlx::PgPool, stop: Arc<AtomicBool>) -> Census {
    let mut seen = Census::default();
    loop {
        let last = stop.load(Ordering::Relaxed);
        match census_once(&instruments).await {
            Ok(sessions) => {
                seen.sessions.extend(sessions);
                seen.passes += 1;
            }
            Err(_) => seen.errors += 1,
        }
        if last {
            return seen;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The observer: its own session, one pass about every millisecond until told
/// to stop. A failed pass is counted, never ignored: a blind observer fails
/// the gate.
async fn observe(
    instruments: sqlx::PgPool,
    window_start: Timestamp,
    stop: Arc<AtomicBool>,
) -> Observation {
    let mut seen = Observation::default();
    let mut connection = None;
    while !stop.load(Ordering::Relaxed) {
        if connection.is_none() {
            match instruments.acquire().await {
                Ok(acquired) => connection = Some(acquired),
                Err(error) => {
                    seen.errors += 1;
                    seen.first_error.get_or_insert_with(|| error.to_string());
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                }
            }
        }
        let session = connection.as_mut().expect("acquired above");
        match observe_once(session, &mut seen, window_start).await {
            Ok(()) => seen.iterations += 1,
            Err(error) => {
                seen.errors += 1;
                seen.first_error.get_or_insert_with(|| error.to_string());
                connection = None;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    seen
}

/// WARN and ERROR events of the whole process while it is installed, and the
/// rebuild's own account of what it published. Background jobs log and swallow
/// some failures; without this they are invisible to the gate.
#[derive(Clone, Default)]
pub(crate) struct LogCapture(Arc<Mutex<CapturedLogs>>);

#[derive(Default)]
pub(crate) struct CapturedLogs {
    by_level_and_target: BTreeMap<(String, String), u64>,
    pub(crate) errors: u64,
    pub(crate) warnings: u64,
    database_warnings: u64,
    pub(crate) first_errors: Vec<String>,
    pub(crate) warn_events: Vec<Value>,
    warn_events_not_listed: u64,
    /// Rows each "catalogue snapshot published" INFO event reported, by table.
    published: BTreeMap<String, u64>,
    /// The same events one by one, and each "rebuilt browse catalogue", with
    /// the instant the capture received them.
    rebuild_events: Vec<RebuildEvent>,
    /// Each publication's actual turn/skip/hold metrics. The controller keeps
    /// using rebuild_events; these entries retain the full runtime evidence.
    snapshot_publications: Vec<Value>,
}

#[derive(Default)]
struct EventText {
    message: String,
    fields: Vec<(String, String)>,
}

impl EventText {
    fn put(&mut self, field: &tracing::field::Field, text: String) {
        if field.name() == "message" {
            self.message = text;
        } else {
            self.fields.push((field.name().to_owned(), text));
        }
    }
    fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, text)| text.as_str())
    }
}

impl tracing::field::Visit for EventText {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.put(field, format!("{value:?}"));
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.put(field, value.to_owned());
    }
}

/// The target of SQLx's statement log, which at WARN is its slow-statement alert.
const SLOW_STATEMENT_TARGET: &str = "sqlx::query";

/// Does a log event name a database failure, in its message or in any field?
/// Deliberately wide: a false alarm costs one look at the listed event, a miss
/// hides a failed background write. SQLx prints every server error as "error
/// returned from database" and every I/O loss as "error communicating with
/// database"; the remaining words cover the known log-and-continue sites
/// ("refresh failed", "sync failed") and errors shown only by their context.
/// An error logged without any of these words is not caught here: that is what
/// the required server log is for.
fn names_database_failure(text: &EventText) -> bool {
    const WORDS: &[&str] = &[
        "database",
        "sqlx",
        "postgres",
        "sqlstate",
        "deadlock",
        "lock timeout",
        "statement timeout",
        "canceling statement",
        "pool timed out",
        "closed pool",
        "serializ",
        "retry bound",
        "transaction",
        "refresh failed",
        "sync failed",
        "idle-in-transaction",
        "terminating connection",
        "lock not available",
        "55p03",
        "57014",
        "40p01",
        "40001",
        "25p03",
        "restart transaction",
    ];
    std::iter::once(text.message.as_str())
        .chain(text.fields.iter().map(|(_, value)| value.as_str()))
        .any(|value| {
            let value = value.to_ascii_lowercase();
            WORDS.iter().any(|word| value.contains(word))
        })
}

impl CapturedLogs {
    fn record(&mut self, level: tracing::Level, target: &str, text: EventText) {
        if level == tracing::Level::INFO {
            let at = chrono::Utc::now();
            if text.message == "catalogue snapshot published" {
                let parsed_changed = text
                    .field("changed")
                    .and_then(|changed| changed.parse::<u64>().ok());
                let changed = parsed_changed.unwrap_or(0);
                let table = text.field("table").unwrap_or("?").to_owned();
                let publication_table = text.field("table").filter(|table| !table.is_empty());
                *self.published.entry(table.clone()).or_default() += changed;
                let integer = |name| text.field(name).and_then(|value| value.parse::<u64>().ok());
                let turns = integer("turns");
                let busy_works = integer("busy_works_left_for_next_refresh");
                let longest_hold = integer("longest_writer_hold_ms");
                self.snapshot_publications.push(json!({
                    "timestamp":at.to_rfc3339(),"table":publication_table,"changed":parsed_changed,
                    "turns":turns,"busy_works_left_for_next_refresh":busy_works,
                    "longest_writer_hold_ms":longest_hold,
                    "fields_complete":publication_table.is_some() && parsed_changed.is_some()
                        && turns.is_some() && busy_works.is_some() && longest_hold.is_some()
                }));
                self.rebuild_events.push(RebuildEvent {
                    at,
                    mark: RebuildMark::Published { table, changed },
                });
            } else if text.message == "browse_catalogue: rebuilt browse catalogue" {
                self.rebuild_events.push(RebuildEvent {
                    at,
                    mark: RebuildMark::BrowseRebuilt,
                });
            }
            return;
        }
        if level != tracing::Level::ERROR && level != tracing::Level::WARN {
            return;
        }
        *self
            .by_level_and_target
            .entry((level.to_string(), target.to_owned()))
            .or_default() += 1;
        let shorten = |value: &str| value.chars().take(400).collect::<String>();
        if level == tracing::Level::ERROR {
            self.errors += 1;
            if self.first_errors.len() < 20 {
                let fields: Vec<_> = text
                    .fields
                    .iter()
                    .map(|(name, value)| format!("{name}={}", shorten(value)))
                    .collect();
                self.first_errors.push(format!(
                    "{target}: {} {}",
                    shorten(&text.message),
                    fields.join(" ")
                ));
            }
        } else {
            self.warnings += 1;
            // SQLx's slow-statement alert (target `sqlx::query`) quotes the
            // statement, and most statements contain a word such as
            // "transaction" or the name of a table. It reports a slow
            // statement, not a failed one: listed, never judged by its words.
            let database = target != SLOW_STATEMENT_TARGET && names_database_failure(&text);
            self.database_warnings += u64::from(database);
            if self.warn_events.len() < 500 {
                let fields: BTreeMap<_, _> = text
                    .fields
                    .iter()
                    .map(|(name, value)| (name.clone(), shorten(value)))
                    .collect();
                self.warn_events
                    .push(json!({"target":target,"message":shorten(&text.message),
                    "fields":fields,"names_a_database_failure":database}));
            } else {
                self.warn_events_not_listed += 1;
            }
        }
    }

    fn json(&self) -> Value {
        json!({"installed":true,"error_events":self.errors,"warn_events":self.warnings,
            "warn_events_naming_a_database_failure":self.database_warnings,
            "by_level_and_target":self.by_level_and_target.iter()
                .map(|((level,target),count)| json!({"level":level,"target":target,"count":count}))
                .collect::<Vec<_>>(),
            "first_errors":self.first_errors,"warnings":self.warn_events,
            "warnings_not_listed":self.warn_events_not_listed,
            "rows_published_by_rebuild":self.published,
            "snapshot_publications":self.snapshot_publications,
            "snapshot_publications_note":"actual product INFO fields, captured across this profile/cycle; timestamp is the client receive time and allows comparison with the measurement window. Missing/empty tables and missing or unparseable numeric fields are null, never zero; require fields_complete for runtime acceptance. longest_writer_hold_ms is the product transaction duration, not the foreground acquire probe."})
    }
}

impl LogCapture {
    /// Hand over what was captured so far and start again from nothing.
    pub(crate) fn take(&self) -> CapturedLogs {
        std::mem::take(&mut *self.0.lock().unwrap())
    }

    /// The rebuild's events captured so far; nothing is taken.
    fn rebuild_events(&self) -> Vec<RebuildEvent> {
        self.0.lock().unwrap().rebuild_events.clone()
    }

    pub(crate) fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync + 'static {
        use tracing_subscriber::layer::SubscriberExt;
        tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::INFO)
            .with(self.clone())
    }

    /// Process-wide. Fails (None) when another subscriber is already the global
    /// default; the gate then refuses to pass, because swallowed errors could
    /// not be seen.
    fn install() -> Option<Self> {
        let capture = Self::default();
        tracing::subscriber::set_global_default(capture.subscriber())
            .ok()
            .map(|()| capture)
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for LogCapture {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let metadata = event.metadata();
        let mut text = EventText::default();
        event.record(&mut text);
        self.0
            .lock()
            .unwrap()
            .record(*metadata.level(), metadata.target(), text);
    }
}

/// What the PostgreSQL server log says about one profile.
#[derive(Clone, Debug, Default, PartialEq)]
struct ServerLogScan {
    lines: u64,
    /// The line of the statement the benchmark made fail on purpose is in the file.
    probe_found: bool,
    /// That line carries the probe's own SQLSTATE in its prefix: `%e` is there.
    sqlstate_in_prefix: bool,
    /// That line carries the probe backend's pid as `[pid]` in its prefix, so
    /// a line can be attributed to a backend. Without it every SQLSTATE line
    /// is counted as the benchmark's.
    pid_in_prefix: bool,
    /// Primary log lines per SQLSTATE (55P03, 57014, 40P01, 25P03, 40001) of
    /// the benchmark's own sessions and the sessions of the server under test.
    sqlstates: BTreeMap<String, u64>,
    /// The same for every other backend of the cluster: reported, never judged.
    other_backends: BTreeMap<String, u64>,
    other_backend_examples: Vec<String>,
    /// ERROR/FATAL/PANIC primary lines outside the five SQLSTATE classes.
    other_error_lines: u64,
    other_error_examples: Vec<String>,
    foreign_other_error_lines: u64,
    foreign_other_error_examples: Vec<String>,
    still_waiting: u64,
    acquired: u64,
    /// Lock-wait lines for the background lease itself (background queueing).
    lease_waits: u64,
    foreground_waited_on_background: u64,
    background_waited_on_foreground: u64,
    /// Lock-wait lines that are neither the lease nor between the two sides,
    /// or whose sessions could not be identified.
    other_waits: u64,
    longest_acquired_ms: f64,
    examples: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
enum ServerLog {
    /// KOMIKA_PG_BENCH_SERVER_LOG is unset. The gate fails: without the file
    /// there is no server-side evidence for a timeout the application swallowed.
    NotProvided,
    /// A file was named but cannot serve as evidence for this profile.
    Unusable(String),
    Scanned(Box<ServerLogScan>),
}

const LOGGED_SQLSTATES: [(&str, &[&str]); 5] = [
    (
        "55P03",
        &[
            "canceling statement due to lock timeout",
            "could not obtain lock",
        ],
    ),
    (
        "57014",
        &[
            "canceling statement due to statement timeout",
            "canceling statement due to user request",
        ],
    ),
    ("40P01", &["deadlock detected"]),
    (
        "25P03",
        &["terminating connection due to idle-in-transaction timeout"],
    ),
    ("40001", &["could not serialize access"]),
];
/// SQLSTATEs whose presence in the server log fails the gate. 40001 is
/// reported only: a serialization failure that a retry recovered is allowed.
const GATED_SQLSTATES: [&str; 4] = ["55P03", "57014", "40P01", "25P03"];

/// (text before the severity word, is a primary line, message). A primary line
/// starts a log entry (LOG, ERROR, ...); DETAIL, CONTEXT, STATEMENT and the
/// like continue one and repeat its prefix, so they must not be counted again.
fn log_line_parts(line: &str) -> Option<(&str, bool, &str)> {
    const PRIMARY: [&str; 8] = [
        "LOG:", "ERROR:", "FATAL:", "PANIC:", "WARNING:", "NOTICE:", "INFO:", "DEBUG:",
    ];
    const CONTINUED: [&str; 6] = [
        "DETAIL:",
        "HINT:",
        "CONTEXT:",
        "STATEMENT:",
        "QUERY:",
        "LOCATION:",
    ];
    PRIMARY
        .iter()
        .map(|word| (word, true))
        .chain(CONTINUED.iter().map(|word| (word, false)))
        .filter_map(|(word, primary)| {
            let mut from = 0;
            while let Some(found) = line[from..].find(word) {
                let at = from + found;
                if at == 0 || line.as_bytes()[at - 1].is_ascii_whitespace() {
                    return Some((at, word.len(), primary));
                }
                from = at + word.len();
            }
            None
        })
        .min_by_key(|(at, _, _)| *at)
        .map(|(at, length, primary)| (&line[..at], primary, line[at + length..].trim()))
}

/// `process 123 still waiting for ShareLock on transaction 9 after 1000.1 ms`
/// or `process 123 acquired ...`: (pid, acquired, lock, milliseconds).
fn parse_lock_wait(message: &str) -> Option<(i32, bool, &str, f64)> {
    let rest = message.strip_prefix("process ")?;
    let (pid, rest) = rest.split_once(' ')?;
    let (acquired, rest) = match rest.strip_prefix("still waiting for ") {
        Some(rest) => (false, rest),
        None => (true, rest.strip_prefix("acquired ")?),
    };
    let (lock, after) = rest.rsplit_once(" after ")?;
    Some((
        pid.parse().ok()?,
        acquired,
        lock,
        after.trim().strip_suffix(" ms")?.trim().parse().ok()?,
    ))
}

/// The pids after "holding the lock:" in a lock-wait DETAIL line.
fn lock_holders(detail: &str) -> Vec<i32> {
    detail
        .split_once("holding the lock:")
        .map(|(_, rest)| rest.split('.').next().unwrap_or(""))
        .unwrap_or("")
        .split(',')
        .filter_map(|pid| pid.trim().parse().ok())
        .collect()
}

/// The backend pid of a log line whose prefix has it as `[pid]` (`[%p]`).
fn prefix_pid(prefix: &str) -> Option<i32> {
    prefix.split_whitespace().find_map(|token| {
        token
            .strip_prefix('[')?
            .strip_suffix(']')?
            .parse::<i32>()
            .ok()
    })
}

/// Read the part of the server log between this profile's two markers. The
/// markers are LOG lines the benchmark itself raised, so finding them proves
/// that this is the right file, that it was being written, and where the
/// profile's part is. `sessions` maps backend pids to application names.
///
/// The line of the SQLSTATE probe (`log_sqlstate_probe`, anywhere in the file:
/// it is written before the begin marker and carries the profile's own marker
/// text) proves how to read a line prefix: its SQLSTATE token and, with
/// `probe_pid`, its `[pid]` token.
fn scan_server_log(
    text: &str,
    marker: &str,
    sessions: &HashMap<i32, String>,
    probe_pid: Option<i32>,
) -> ServerLog {
    let lines: Vec<&str> = text.lines().collect();
    let marked = |line: &str, edge: &str| {
        log_line_parts(line).is_some_and(|(_, primary, message)| {
            primary && message == format!("{marker} {edge}").as_str()
        })
    };
    let Some(begin) = lines.iter().rposition(|line| marked(line, "begin")) else {
        return ServerLog::Unusable(
            "the begin marker of this profile is not in the file (wrong file, or the server \
             does not log to it)"
                .into(),
        );
    };
    let Some(end) = lines[begin..].iter().position(|line| marked(line, "end")) else {
        return ServerLog::Unusable("the end marker of this profile is not in the file".into());
    };
    let probe_text = format!("{marker} {SQLSTATE_PROBE}");
    let probe_prefix = lines.iter().rev().find_map(|line| {
        log_line_parts(line)
            .filter(|(_, primary, message)| *primary && *message == probe_text)
            .map(|(prefix, _, _)| prefix)
    });
    let mut scan = ServerLogScan {
        probe_found: probe_prefix.is_some(),
        sqlstate_in_prefix: probe_prefix.is_some_and(|prefix| {
            prefix
                .split_whitespace()
                .any(|token| token == PROBE_SQLSTATE)
        }),
        pid_in_prefix: probe_pid
            .zip(probe_prefix)
            .is_some_and(|(pid, prefix)| prefix_pid(prefix) == Some(pid)),
        ..Default::default()
    };
    let lease = format!(
        ",{},{},1]",
        LEASE_KEY >> 32,
        LEASE_KEY & i64::from(u32::MAX)
    );
    let body = &lines[begin + 1..begin + end];
    let example = |scan: &mut ServerLogScan, line: &str| {
        if scan.examples.len() < 20 {
            scan.examples.push(line.chars().take(300).collect());
        }
    };
    for (index, line) in body.iter().enumerate() {
        scan.lines += 1;
        let Some((prefix, true, message)) = log_line_parts(line) else {
            continue;
        };
        // A line counts for the gate unless it is known to come from a backend
        // that is neither the benchmark's nor the server's under test.
        // An empty session map attributes nothing, so it excludes nothing.
        let ours = !scan.pid_in_prefix
            || sessions.is_empty()
            || prefix_pid(prefix).is_none_or(|pid| sessions.contains_key(&pid));
        let mut known_sqlstate = false;
        for (code, messages) in LOGGED_SQLSTATES {
            if prefix.split_whitespace().any(|token| token == code)
                || messages.iter().any(|text| message.contains(text))
            {
                known_sqlstate = true;
                if ours {
                    *scan.sqlstates.entry(code.to_owned()).or_default() += 1;
                    example(&mut scan, line);
                } else {
                    *scan.other_backends.entry(code.to_owned()).or_default() += 1;
                    if scan.other_backend_examples.len() < 20 {
                        scan.other_backend_examples
                            .push(line.chars().take(300).collect());
                    }
                }
            }
        }
        let severe = ["ERROR:", "FATAL:", "PANIC:"]
            .iter()
            .any(|severity| line[prefix.len()..].starts_with(severity));
        if severe && !known_sqlstate {
            let (count, examples) = if ours {
                (&mut scan.other_error_lines, &mut scan.other_error_examples)
            } else {
                (
                    &mut scan.foreign_other_error_lines,
                    &mut scan.foreign_other_error_examples,
                )
            };
            *count += 1;
            if examples.len() < 20 {
                examples.push(line.chars().take(300).collect());
            }
        }
        let Some((waiter, acquired, lock, milliseconds)) = parse_lock_wait(message) else {
            continue;
        };
        let leased = lock.contains("advisory lock") && lock.contains(&lease);
        if acquired {
            scan.acquired += 1;
            if !leased {
                scan.longest_acquired_ms = scan.longest_acquired_ms.max(milliseconds);
            }
            continue;
        }
        scan.still_waiting += 1;
        if leased {
            scan.lease_waits += 1;
            continue;
        }
        example(&mut scan, line);
        // The entry's DETAIL line names the holders.
        let holders: Vec<i32> = body[index + 1..]
            .iter()
            .map_while(|next| log_line_parts(next).filter(|(_, primary, _)| !primary))
            .flat_map(|(_, _, detail)| lock_holders(detail))
            .collect();
        let side_of = |pid: &i32| sessions.get(pid).map_or(Side::Other, |name| side(name));
        let held_by = |wanted: Side| holders.iter().any(|pid| side_of(pid) == wanted);
        match side_of(&waiter) {
            Side::Foreground if held_by(Side::Background) => {
                scan.foreground_waited_on_background += 1
            }
            Side::Background if held_by(Side::Foreground) => {
                scan.background_waited_on_foreground += 1
            }
            _ => scan.other_waits += 1,
        }
    }
    for (code, _) in LOGGED_SQLSTATES {
        scan.sqlstates.entry(code.to_owned()).or_default();
        scan.other_backends.entry(code.to_owned()).or_default();
    }
    ServerLog::Scanned(Box::new(scan))
}

/// The SQLSTATE of the statement the benchmark makes fail on purpose. It is in
/// no class PostgreSQL uses, so nothing else in a log line can be mistaken for it.
const PROBE_SQLSTATE: &str = "KB001";
/// The words after the marker text in the probe's error message.
const SQLSTATE_PROBE: &str = "sqlstate probe";

/// Make one harmless statement fail with `PROBE_SQLSTATE`, so the server logs
/// an ERROR line for it. That line shows whether the log line prefix carries
/// the SQLSTATE (`%e`) and the backend pid (`[%p]`): the gate reads SQLSTATEs
/// from the prefix and must not take their absence for a clean run. Called
/// before a profile's statistics are settled and before its begin marker, so
/// its rollback is in no measured difference. Returns the pid of the backend
/// that ran it, or None when the statement did not fail as intended.
async fn log_sqlstate_probe(instruments: &sqlx::PgPool, marker: &str) -> Option<i32> {
    assert!(
        marker
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == ' '),
        "marker is inlined into SQL"
    );
    let mut connection = instruments.acquire().await.ok()?;
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *connection)
        .await
        .ok()?;
    let statement = format!(
        "DO $$ BEGIN RAISE EXCEPTION '{marker} {SQLSTATE_PROBE}' USING ERRCODE = '{PROBE_SQLSTATE}'; END $$"
    );
    match sqlx::raw_sql(&statement).execute(&mut *connection).await {
        Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some(PROBE_SQLSTATE) => {
            Some(pid)
        }
        _ => None,
    }
}

/// Put a LOG line into the server log (RAISE LOG is written whatever
/// log_min_messages is at its default), to delimit a profile there.
async fn log_marker(
    instruments: &sqlx::PgPool,
    marker: &str,
    edge: &str,
) -> Result<(), sqlx::Error> {
    assert!(
        marker
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == ' '),
        "marker is inlined into SQL"
    );
    let statement = format!("DO $$ BEGIN RAISE LOG '{marker} {edge}'; END $$");
    sqlx::raw_sql(&statement)
        .execute(instruments)
        .await
        .map(|_| ())
}

/// Scan the named server log for this profile's part, waiting a little for the
/// end marker to reach the file.
async fn read_server_log(
    path: &std::path::Path,
    marker: &str,
    sessions: &HashMap<i32, String>,
    probe_pid: Option<i32>,
) -> ServerLog {
    let mut last = ServerLog::Unusable("not read".into());
    for _ in 0..40 {
        last = match std::fs::read(path) {
            Ok(bytes) => scan_server_log(
                &String::from_utf8_lossy(&bytes),
                marker,
                sessions,
                probe_pid,
            ),
            Err(error) => ServerLog::Unusable(format!("cannot read the file: {}", error.kind())),
        };
        if matches!(last, ServerLog::Scanned(_)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    last
}

impl ServerLog {
    fn json(&self) -> Value {
        match self {
            ServerLog::NotProvided => json!("not provided"),
            ServerLog::Unusable(reason) => json!({"usable":false,"reason":reason}),
            ServerLog::Scanned(scan) => json!({"usable":true,"lines_scanned":scan.lines,
                "sqlstate_probe":{"sqlstate":PROBE_SQLSTATE,"line_found":scan.probe_found,
                    "note":"one statement the benchmark made fail on purpose before the profile; its ERROR line shows how to read a line prefix"},
                "sqlstate_in_log_line_prefix":scan.sqlstate_in_prefix,
                "pid_in_log_line_prefix":scan.pid_in_prefix,
                "sqlstates":scan.sqlstates,
                "sqlstates_note":"entries of the benchmark's sessions and the sessions of the server under test (application name komika-bench*, by the [pid] of the line); when pid_in_log_line_prefix is false every entry is counted here",
                "other_error_lines":scan.other_error_lines,"other_error_examples":scan.other_error_examples,
                "other_backends":{"sqlstates":scan.other_backends,"examples":scan.other_backend_examples,
                    "other_error_lines":scan.foreign_other_error_lines,"other_error_examples":scan.foreign_other_error_examples,
                    "note":"entries of any other backend of the cluster, for example an autovacuum worker that was cancelled (57014); reported, not judged"},
                "lock_wait_lines":{"still_waiting":scan.still_waiting,"acquired":scan.acquired,
                    "background_lease_queue":scan.lease_waits,
                    "foreground_waited_on_background":scan.foreground_waited_on_background,
                    "background_waited_on_foreground":scan.background_waited_on_foreground,
                    "other":scan.other_waits,"longest_acquired_ms":scan.longest_acquired_ms,
                    "note":"written only for waits longer than deadlock_timeout, and only with log_lock_waits=on"},
                "examples":scan.examples}),
        }
    }
}

/// Every number the pass rule reads. Nothing else decides `passed`.
#[derive(Clone, Debug)]
struct Measured {
    /// The heavy profile: all four mutation kinds are required.
    mixed: bool,
    write_requests: u64,
    write_ok: u64,
    wrong_effects: u64,
    /// Verified effects per kind, in `KINDS` order.
    verified_effects: [u64; 4],
    read_process_success: bool,
    read_requests: u64,
    read_ok: u64,
    application_lock_errors: u64,
    counters: db::DatabaseCounters,
    probe_failures: u64,
    background_error: Option<String>,
    proven_catalogue: u64,
    proven_rebuild: u64,
    /// Catalogue replays that began and ended inside the window.
    catalogue_units: u64,
    min_catalogue_units: u64,
    /// Rebuild cycles complete inside the window, each with a Browse publication that changed a row.
    rebuild_cycles: u64,
    /// KOMIKA_PG_BENCH_MIN_REBUILD_CYCLES.
    min_rebuild_cycles: u64,
    /// The window ended at KOMIKA_PG_BENCH_MAX_DURATION_SECS, not by its rule.
    window_capped: bool,
    catalogue_statements: u64,
    rebuild_statements: u64,
    /// Rows whose `year` the perturbations applied inside the window changed.
    perturbed_rows: u64,
    /// Rows the Browse publications of the counted cycles changed.
    browse_rows_published: u64,
    foreground_waited_on_background: u64,
    background_waited_on_foreground: u64,
    observer_iterations: u64,
    observer_errors: u64,
    instrument_errors: u64,
    statistics_valid: bool,
    server_deadlocks: i64,
    sessions_killed: i64,
    sessions_fatal: i64,
    foreign_sessions: u64,
    server_log: ServerLog,
    log_capture_installed: bool,
    error_events: u64,
    database_warn_events: u64,
    foreground_labels_valid: bool,
    background_ended_early: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct GateResult {
    passed: bool,
    /// User writes and background writes were seen waiting on each other.
    contention_exercised: bool,
    /// Every condition that failed; empty when passed.
    reasons: Vec<String>,
}

const NO_CONTENTION: &str = "no contention exercised";

/// The whole pass rule of one profile. `retries` (a recovered 40001 or 40P01
/// re-execution) and `transaction_restarts` are reported, not judged here.
fn evaluate_gate(measured: &Measured) -> GateResult {
    let m = measured;
    let mut reasons = Vec::new();
    let mut require = |holds: bool, reason: &str| {
        if !holds {
            reasons.push(reason.to_owned());
        }
    };
    // The gate itself: no lock error, no deadlock, on either side of the wire.
    require(m.application_lock_errors == 0, "application lock errors");
    require(m.counters.deadlocks == 0, "application deadlocks (40P01)");
    require(m.counters.lock_timeouts == 0, "lock timeouts (55P03)");
    require(m.counters.pool_timeouts == 0, "pool acquire timeouts");
    require(
        m.counters.statement_timeouts == 0,
        "statement timeouts (57014)",
    );
    require(
        m.counters.idle_transaction_terminations == 0,
        "idle-in-transaction terminations (25P03)",
    );
    require(m.counters.connection_lost == 0, "connections lost");
    require(m.counters.retries_exhausted == 0, "retries exhausted");
    require(
        m.statistics_valid,
        "server statistics not settled or database identity changed",
    );
    require(m.server_deadlocks == 0, "server deadlocks");
    require(m.sessions_killed == 0, "server sessions killed");
    require(
        m.sessions_fatal == 0,
        "server sessions ended by a fatal error",
    );
    require(
        m.foreign_sessions == 0,
        "another client session in the database",
    );
    // Every request did what it was sent to do.
    require(
        m.write_requests > 0 && m.write_ok == m.write_requests,
        "failed writes",
    );
    require(m.wrong_effects == 0, "wrong effects");
    let kinds = if m.mixed { 4 } else { 1 };
    require(
        m.verified_effects[..kinds].iter().all(|count| *count > 0),
        "a mutation kind has no verified effect",
    );
    require(m.read_process_success, "read driver failed");
    require(m.read_requests > 0, "no read requests");
    require(m.read_ok == m.read_requests, "failed reads");
    require(m.probe_failures == 0, "foreground probe failures");
    // The background work was real and ran inside the window.
    require(m.background_error.is_none(), "background worker error");
    require(m.proven_catalogue >= 1, "no proven catalogue overlap");
    require(m.proven_rebuild >= 1, "no proven rebuild overlap");
    require(
        m.catalogue_units >= m.min_catalogue_units.max(1),
        "too few catalogue units inside the window",
    );
    require(
        !m.window_capped,
        "the window reached its hard cap (KOMIKA_PG_BENCH_MAX_DURATION_SECS) before the rebuild cycles were complete",
    );
    require(
        m.rebuild_cycles >= m.min_rebuild_cycles.max(1),
        "too few complete rebuild cycles with a Browse publication inside the window",
    );
    require(
        m.catalogue_statements > 0,
        "no catalogue write statement observed inside the window",
    );
    require(
        m.rebuild_statements > 0,
        "no rebuild write statement observed inside the window",
    );
    require(
        m.perturbed_rows > 0,
        "the rebuild perturbation changed no row inside the window",
    );
    require(
        m.browse_rows_published > 0,
        "no Browse publication changed a row inside the window",
    );
    // Nothing was swallowed, and the instruments could see.
    require(m.log_capture_installed, "log capture not installed");
    require(m.error_events == 0, "ERROR log events");
    require(
        m.database_warn_events == 0,
        "WARN log events naming a database failure",
    );
    require(
        m.observer_iterations > 0 && m.observer_errors == 0,
        "lock-wait observer failed",
    );
    require(m.instrument_errors == 0, "an instrument query failed");
    require(
        m.foreground_labels_valid,
        "foreground session labels lost or incomplete",
    );
    require(
        !m.background_ended_early,
        "background worker ended before the window closed",
    );
    let mut contention = m.foreground_waited_on_background + m.background_waited_on_foreground >= 1;
    match &m.server_log {
        ServerLog::NotProvided => reasons.push("server log not provided".to_owned()),
        ServerLog::Unusable(reason) => reasons.push(format!("server log unusable: {reason}")),
        ServerLog::Scanned(scan) => {
            // Zero SQLSTATE entries mean nothing while the prefix is not known
            // to carry the SQLSTATE: the server's message text is the only
            // other sign, and it follows lc_messages.
            if !scan.probe_found {
                reasons.push("server log has no line for the SQLSTATE probe".to_owned());
            } else if !scan.sqlstate_in_prefix {
                reasons.push("server log line prefix has no SQLSTATE (%e)".to_owned());
            }
            if scan.other_error_lines > 0 {
                reasons.push("server log has other ERROR/FATAL/PANIC lines".to_owned());
            }
            for code in GATED_SQLSTATES {
                if scan.sqlstates.get(code).copied().unwrap_or(0) > 0 {
                    reasons.push(format!("server log has SQLSTATE {code}"));
                }
            }
            contention |=
                scan.foreground_waited_on_background + scan.background_waited_on_foreground >= 1;
        }
    }
    // A run in which the two sides never met proves nothing about them.
    if !contention {
        reasons.push(NO_CONTENTION.to_owned());
    }
    GateResult {
        passed: reasons.is_empty(),
        contention_exercised: contention,
        reasons,
    }
}

/// Keys of this output whose meaning differs from the same key in
/// `apps/server/bench/results/sqlite-contention-results.json`. Read before comparing numbers.
fn comparison_notes(rule: &WindowRule) -> Value {
    json!({
        "compared_with":"apps/server/bench/results/sqlite-contention-results.json",
        "matching_profile":format!("The top-level (baseline) block corresponds to `after` there: 10 reads/s, 2 setFavorite/s, rebuilds running. Its one account targets replayed work 0, so baseline measured waits can meet catalogue replay only; the heavy profile can meet both replay and Browse publication. SQLite measured a fixed window (workload.http_window_seconds in that file). PostgreSQL measures at least {} s and goes on until {} complete rebuild cycles with a Browse publication lie inside the window, up to {} s; the real length is `window.seconds` of each profile. Compare rates and latencies, not request counts. The `heavy` block has no SQLite counterpart.",
            rule.duration.as_secs(), rule.min_cycles, rule.cap.as_secs()),
        "keys":{
            "application_lock_error_events":"SQLite: log lines containing 'locked' or SQLITE_BUSY. PostgreSQL: the server's own counter of write-lane lock timeouts (55P03), deadlocks (40P01), exhausted retries and write-pool acquire timeouts.",
            "longest_sampled_writer_hold_ms":"SQLite: the longest run of consecutive busy probes, i.e. how long the single writer lock was held. PostgreSQL has no global writer lock: this is the longest pool-acquire plus BEGIN of the 5 ms foreground probe (the same value as longest_foreground_write_wait_ms). Not a hold time.",
            "busy_probes":"SQLite: probes that found the writer lock held (non-zero is normal). PostgreSQL: probes whose foreground BEGIN failed (any is a gate failure). See lock_waits for who really waited on whom.",
            "background_elapsed_ms":"SQLite: wall time of the rebuild loop. PostgreSQL: from the start of the measurement window until the rebuild worker finished the step that was in flight when the window closed.",
            "workload.background":"SQLite: refresh_feed_updates alone looped on the single writer. PostgreSQL: four rebuild calls per cycle, chaining to three Browse rebuilds and four search reconciles on one leased writer, catalogue metadata replay on a second leased writer, and the benchmark's own perturbation of work.year before each call that rebuilds Browse.",
            "reads":"Same driver, scenario and rate. The window differs: SQLite passed its fixed duration; PostgreSQL passes the hard cap as --duration and ends the load with --stop-file when the window ends. SQLite's loadtest.mjs looked for \"errors\" in the first body chunk only; it now looks in the whole body. SQLite served a minimal router (GraphQL handler only); PostgreSQL serves the real public router with admission, authentication and middleware. In the heavy profile every admin edit also clears the feed caches, as it does in production.",
            "writes.loadtest_arguments":"SQLite: loadtest.mjs with the facets operation overridden. PostgreSQL: a Rust driver (the JavaScript one has no bearer token or multi-user support).",
            "writes.results.p50..max,mean":"SQLite and PostgreSQL both run the clock from the send until the response body is complete, with the same percentile rule, but the two are NOT alike when a write stalls. SQLite's loadtest.mjs sends every request at its scheduled time, so a request scheduled during a stall is sent into it and its latency contains the stall. The PostgreSQL driver sends one request per account at a time (its effect is verified before the account's next write), so a write scheduled behind a stalled write of the same account waits unsent; that wait is in writes.queue_delay_ms, not in these numbers, and the baseline has one account. The like-for-like PostgreSQL number is writes.arrival_latency_ms (scheduled arrival until the response body is complete); it also contains the pre-send sample and the previous write's effect check, so it can only overstate. The effect check runs after the clock has stopped.",
            "writes.results.users":"SQLite: loadtest's implied users (ok/s times 5 s think time). PostgreSQL: accounts that sent writes.",
            "writes.results.gqlErrors":"SQLite: the first body chunk contains \"errors\". PostgreSQL: the parsed answer has a non-empty errors array.",
            "writes.results.bytesPerSec":"Not recorded by the Rust write driver."
        },
        "no_sqlite_counterpart":["passed","reasons","contention_exercised","lock_waits","counters","retries","server_statistics","server_log","log_events","proven_overlaps","overlap","wrong_effects","verified_effects","instruments","heavy"]
    })
}

/// The SQLite numbers for the matching profile, copied from the audit file so
/// one output shows both.
fn sqlite_baseline() -> Value {
    let source = "apps/server/bench/results/sqlite-contention-results.json";
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(source);
    match std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    {
        Some(file) => json!({"source":source,"profile":"after",
            "workload":file["workload"],"before":file["before"],"after":file["after"],
            "baseline_write_latency_caveat":file["baseline_write_latency_caveat"]}),
        None => json!({"source":source,"unavailable":"the file was not found or is not JSON"}),
    }
}

async fn session_limits(executor: impl sqlx::PgExecutor<'_>) -> Value {
    let row: (String, String, String) = sqlx::query_as(
        "SELECT current_setting('statement_timeout'), current_setting('lock_timeout'), current_setting('idle_in_transaction_session_timeout')"
    ).fetch_one(executor).await.unwrap();
    json!({"statement_timeout":row.0,"lock_timeout":row.1,"idle_in_transaction_session_timeout":row.2})
}

fn configured_session_limits(limits: config::DatabaseSessionLimits) -> Value {
    json!({"statement_timeout_ms":limits.statement_ms,"lock_timeout_ms":limits.lock_ms,
        "idle_in_transaction_session_timeout_ms":limits.idle_transaction_ms})
}

fn pool_retention(pool: &sqlx::PgPool) -> Value {
    let options = pool.options();
    json!({"idle_timeout_ms":options.get_idle_timeout().map(|duration| duration.as_millis() as u64),
        "max_lifetime_ms":options.get_max_lifetime().map(|duration| duration.as_millis() as u64)})
}

fn standalone_limits() -> Value {
    let limits = config::DatabaseLimits::from_env();
    let (staging_lane, staging_limits) = match catalog::REFRESH_STAGING_LANE {
        db::Lane::Readers => ("Readers", limits.readers),
        db::Lane::Foreground => ("Foreground", limits.foreground),
        db::Lane::Background => ("Background", limits.background),
    };
    json!({"source":"DatabaseLimits::from_env (standalone sessions)",
        "readers":configured_session_limits(limits.readers),
        "background":configured_session_limits(limits.background),
        "refresh_staging":{"lane":staging_lane,"limits":configured_session_limits(staging_limits)},
        "note":"readiness uses readers; perturbation uses background; refresh staging uses the product-selected lane, recorded separately"})
}

fn baseline_allows_heavy(baseline: &Value) -> bool {
    baseline["passed"] == true || baseline["reasons"] == json!([NO_CONTENTION])
}

/// What every profile of one benchmark run shares.
struct Bench<'a> {
    pool: &'a db::Database,
    instruments: &'a sqlx::PgPool,
    /// The works the user writes and the admin edits target.
    contested: &'a [Contested],
    /// The contested works the catalogue worker replays.
    replayed: &'a [String],
    /// Works whose `year` the rebuild worker flips before each Browse rebuild:
    /// the contested works that are not replayed and a slice of other works.
    perturbed: &'a [String],
    capture: Option<&'a LogCapture>,
    server_log: Option<&'a std::path::Path>,
    foreground_connections: u32,
    window: WindowRule,
    limits: config::DatabaseLimits,
    min_catalogue_units: u64,
}

/// One profile's load.
struct Load<'a> {
    /// Output path prefix for the read driver's own JSON.
    prefix: &'a std::path::Path,
    /// (token, id): `users` users first, then the admins.
    accounts: Vec<(String, uuid::Uuid)>,
    users: usize,
    rate: u64,
    mixed: bool,
}

/// The measurement window as it ran.
#[derive(Clone, Copy, Debug)]
struct Window {
    rule: WindowRule,
    started: Timestamp,
    ended: Timestamp,
    capped: bool,
}

/// Everything one profile collected, before any of it is judged or shaped.
/// `measure` and `summarize` are its only readers, and they do no I/O.
struct Collected {
    mixed: bool,
    rate: u64,
    accounts: usize,
    window: Window,
    writes: WriteResults,
    /// The read driver's own JSON file, or Null.
    read: Value,
    read_process_success: bool,
    observed: Observation,
    /// (passes, failed passes, sessions seen) of the session census.
    census: (u64, u64, usize),
    catalogue: BackgroundRun,
    rebuild: BackgroundRun,
    /// The deterministic samples: catalogue, rebuild.
    proofs: [Value; 2],
    /// (longest foreground acquire plus BEGIN, failed probes, probes).
    probe: (Duration, u64, u64),
    /// Before and after the profile.
    identity: [Value; 2],
    statistics: [Value; 2],
    counters: db::DatabaseCounters,
    application_lock_errors: u64,
    captured: Option<CapturedLogs>,
    server_log: ServerLog,
    marker: String,
    marker_written: bool,
    /// Foreground sessions carrying their label at the start and at the end.
    foreground_labelled: [i64; 2],
    foreground_connections: u32,
    background_ended_early: bool,
    foreign_sessions: Vec<Value>,
    contested_works: Vec<String>,
    replayed_works: Vec<String>,
    perturbed_works: usize,
    min_catalogue_units: u64,
}

impl Collected {
    fn rebuild_events(&self) -> &[RebuildEvent] {
        self.captured
            .as_ref()
            .map_or(&[], |logs| logs.rebuild_events.as_slice())
    }

    /// After minus before of one `pg_stat_database` column.
    fn statistics_delta(&self, key: &str) -> i64 {
        let value = |sample: &Value| sample[key].as_i64().expect("a sampled statistics column");
        value(&self.statistics[1]) - value(&self.statistics[0])
    }
}

/// The numbers the pass rule reads, each from the one place it is collected.
fn measure(c: &Collected) -> Measured {
    let (from, to) = (c.window.started, c.window.ended);
    let events = c.rebuild_events();
    let cycles = counted_cycles(&c.rebuild.cycles, events, from, to);
    let read_results = &c.read["results"][0];
    let wait_totals = c.observed.waits.totals();
    let verified = |kind: Kind| c.writes.verified.get(&kind).copied().unwrap_or(0);
    let proven = |proof: &Value| u64::from(proof["proven"] == true);
    Measured {
        mixed: c.mixed,
        write_requests: c.writes.samples.len() as u64,
        write_ok: c.writes.samples.iter().filter(|(_, ok)| *ok).count() as u64,
        wrong_effects: c.writes.wrong_effects,
        verified_effects: KINDS.map(verified),
        read_process_success: c.read_process_success,
        read_requests: read_results["requests"].as_u64().unwrap_or(0),
        read_ok: read_results["ok"].as_u64().unwrap_or(0),
        application_lock_errors: c.application_lock_errors,
        counters: c.counters,
        probe_failures: c.probe.1,
        background_error: c.catalogue.error.clone().or(c.rebuild.error.clone()),
        proven_catalogue: proven(&c.proofs[0]),
        proven_rebuild: proven(&c.proofs[1]),
        catalogue_units: c.catalogue.units_inside(from, to) as u64,
        min_catalogue_units: c.min_catalogue_units,
        rebuild_cycles: cycles.len() as u64,
        min_rebuild_cycles: c.window.rule.min_cycles,
        window_capped: c.window.capped,
        catalogue_statements: c.observed.statements_inside(CATALOGUE, to),
        rebuild_statements: c.observed.statements_inside(REBUILD, to),
        perturbed_rows: c
            .rebuild
            .perturbations
            .iter()
            .filter(|perturbation| perturbation.at >= from && perturbation.at <= to)
            .map(|perturbation| perturbation.changed)
            .sum(),
        browse_rows_published: cycles
            .iter()
            .map(|cycle| browse_publications(events, cycle.began, cycle.ended).1)
            .sum(),
        foreground_waited_on_background: wait_totals["foreground_waited_on_background"].episodes,
        background_waited_on_foreground: wait_totals["background_waited_on_foreground"].episodes,
        observer_iterations: c.observed.iterations,
        observer_errors: c.observed.errors,
        // A census that never ran, or failed a pass, cannot attribute a log line.
        instrument_errors: c.writes.instrument_errors + c.census.1 + u64::from(c.census.0 == 0),
        statistics_valid: c.identity[0] == c.identity[1]
            && c.statistics[0]["settled"] == true
            && c.statistics[1]["settled"] == true,
        server_deadlocks: c.statistics_delta("deadlocks"),
        sessions_killed: c.statistics_delta("sessions_killed"),
        sessions_fatal: c.statistics_delta("sessions_fatal"),
        foreign_sessions: c.foreign_sessions.len() as u64,
        server_log: c.server_log.clone(),
        log_capture_installed: c.captured.is_some(),
        error_events: c.captured.as_ref().map_or(0, |logs| logs.errors),
        database_warn_events: c.captured.as_ref().map_or(0, |logs| logs.database_warnings),
        foreground_labels_valid: c
            .foreground_labelled
            .iter()
            .all(|count| *count == i64::from(c.foreground_connections)),
        background_ended_early: c.background_ended_early
            || c.catalogue.ended < to
            || c.rebuild.ended < to,
    }
}

/// One profile's output block: the verdict and every measurement behind it.
fn summarize(c: Collected) -> Value {
    let measured = measure(&c);
    let gate = evaluate_gate(&measured);
    let (from, to) = (c.window.started, c.window.ended);
    let (rule, rate, counters) = (c.window.rule, c.rate, c.counters);
    let events = c.rebuild_events();
    let timeline = rebuild_timeline(&c.rebuild.step_marks, events);
    let inside = |at: Timestamp| at >= from && at <= to;
    let milliseconds =
        |began: Timestamp, ended: Timestamp| ended.signed_duration_since(began).num_milliseconds();
    let mut write_results = latency_result(
        c.writes.samples.clone(),
        rate,
        c.writes.wall_seconds,
        c.accounts,
    );
    write_results["httpErrors"] = json!(c.writes.http_errors);
    write_results["netErrors"] = json!(c.writes.net_errors);
    write_results["gqlErrors"] = json!(c.writes.gql_errors);
    let by_kind = |counts: &BTreeMap<Kind, u64>| -> Value {
        KINDS
            .into_iter()
            .map(|kind| {
                (
                    kind.name().to_owned(),
                    json!(counts.get(&kind).copied().unwrap_or(0)),
                )
            })
            .collect()
    };
    let run = |run: &BackgroundRun, units: u64, sampled: u64, statements: u64| {
        json!({"started_at":run.began.to_rfc3339(),"ended_at":run.ended.to_rfc3339(),"completed_runs":run.units.len(),
            "units_inside_window":units,"user_writes_started_with_open_transaction":sampled,"observed_statements_inside_window":statements})
    };
    let mut rebuild_json = run(
        &c.rebuild,
        c.rebuild.units_inside(from, to) as u64,
        c.writes.rebuild_overlap,
        measured.rebuild_statements,
    );
    let perturbations_inside: Vec<&Perturbation> = c
        .rebuild
        .perturbations
        .iter()
        .filter(|perturbation| inside(perturbation.at))
        .collect();
    rebuild_json["perturbation"] = json!({"what":"work.year flipped by one on a bounded slice of works before each rebuild call that rebuilds Browse, on the benchmark's own session; not restored",
        "works":c.perturbed_works,"applied":c.rebuild.perturbations.len(),
        "applied_inside_window":perturbations_inside.len(),
        "rows_touched_inside_window":perturbations_inside.iter().map(|perturbation| perturbation.touched).sum::<u64>(),
        "rows_changed_inside_window":measured.perturbed_rows,
        "note":"touched and changed differ for a work with a pinned year: the work_pin_year trigger restores the pin"});
    rebuild_json["cycles_counted"] = json!(measured.rebuild_cycles);
    rebuild_json["browse_rows_published_by_counted_cycles"] = json!(measured.browse_rows_published);
    rebuild_json["cycles"] = c
        .rebuild
        .cycles
        .iter()
        .map(|cycle| {
            let (publications, rows) = browse_publications(events, cycle.began, cycle.ended);
            let complete = cycle.began >= from && cycle.ended <= to;
            json!({"began_at":cycle.began.to_rfc3339(),"ended_at":cycle.ended.to_rfc3339(),
                "duration_ms":milliseconds(cycle.began, cycle.ended),
                "steps_ms":REBUILD_STEPS.into_iter().zip(cycle.steps)
                    .map(|(name,(began,ended))| (name.to_owned(), json!(milliseconds(began, ended))))
                    .collect::<serde_json::Map<_,_>>(),
                "complete_inside_window":complete,
                "browse_publications":publications,"browse_rows_published":rows,
                "counted":complete && rows > 0})
        })
        .collect();
    let mut catalogue_json = run(
        &c.catalogue,
        measured.catalogue_units,
        c.writes.catalogue_overlap,
        measured.catalogue_statements,
    );
    catalogue_json["min_units_inside_window"] = json!(c.min_catalogue_units);
    let background_error = measured.background_error.clone();
    let delta: Value = STATISTICS
        .into_iter()
        .map(|key| (key.to_owned(), json!(c.statistics_delta(key))))
        .collect();
    let [catalogue_proof, rebuild_proof] = c.proofs.clone();
    let [identity_before, identity_after] = c.identity.clone();
    let [stats_before, stats_after] = c.statistics.clone();
    let longest_wait_ms = c.probe.0.as_secs_f64() * 1000.0;
    json!({
        "passed":gate.passed,
        "reasons":gate.reasons,
        "reason":gate.reasons.join("; "),
        "contention_exercised":gate.contention_exercised,
        "window":{"started_at":from.to_rfc3339(),"ended_at":to.to_rfc3339(),
            "seconds":milliseconds(from, to) as f64 / 1000.0,
            "configured_duration_seconds":rule.duration.as_secs(),
            "min_rebuild_cycles":rule.min_cycles,"max_duration_seconds":rule.cap.as_secs(),
            "capped":c.window.capped,"background_worker_ended_early":measured.background_ended_early,"rebuild_cycles_counted":measured.rebuild_cycles,
            "note":"the window ends when the configured duration has passed AND the wanted number of rebuild cycles is complete inside it, each with a Browse publication that changed a row; reaching the hard cap ends it and fails the gate. A cycle is one pass of the rebuild worker over its four rebuild calls; overlap.rebuild.cycles has each one's duration"},
        "retries":{"note":"recovered re-executions; reported, not a gate condition",
            "total":counters.retries,
            "by_kind":{"restart_transaction":counters.transaction_restarts,
                "serialization_failure_or_deadlock":counters.retries.saturating_sub(counters.transaction_restarts)},
            "serialization_failures_observed":counters.serialization_failures,
            "deadlocks_observed":counters.deadlocks,
            "exhausted":counters.retries_exhausted},
        "application_lock_error_events":c.application_lock_errors,
        "longest_sampled_writer_hold_ms":longest_wait_ms,
        "longest_foreground_write_wait_ms":longest_wait_ms,
        "background_elapsed_ms":milliseconds(from, c.rebuild.ended),
        "background_runs":(c.catalogue.units.len() + c.rebuild.units.len()) as u64,
        "background_error":background_error,"busy_probes":c.probe.1,"foreground_probes":c.probe.2,
        "database_deadlocks":c.statistics_delta("deadlocks"),"counters":counters,
        "lock_waits":c.observed.waits.json(&timeline),
        "instruments":{"observer_passes":c.observed.iterations,"observer_errors":c.observed.errors,
            "observer_first_error":c.observed.first_error,"request_instrument_errors":c.writes.instrument_errors,
            "census_passes":c.census.0,"census_errors":c.census.1,"census_sessions_seen":c.census.2,
            "foreground_connections_expected":c.foreground_connections,
            "foreground_labels_valid":measured.foreground_labels_valid,
            "foreground_sessions_labelled":c.foreground_labelled[0],
            "foreground_sessions_labelled_at_end":c.foreground_labelled[1],
            "note":"instrument sessions are separate from the measured pools; a foreground connection replaced during the profile loses its label and its waits are then counted as other"},
        "server_statistics":{"scope":"database-wide pg_stat_database row, not attributable to this benchmark; valid only for an isolated scratch database",
            "identity_before":identity_before,"identity_after":identity_after,"valid":measured.statistics_valid,
            "other_client_sessions_at_end":c.foreign_sessions,
            "before":stats_before,"after":stats_after,"delta":delta},
        "server_log":c.server_log.json(),
        "server_log_marker":{"text":c.marker,"written":c.marker_written,
            "note":"LOG lines '<text> begin' and '<text> end' delimit this profile in the server log; the ERROR line '<text> sqlstate probe' before them is the benchmark's own probe"},
        "log_events":c.captured.as_ref().map_or_else(|| json!({"installed":false}), CapturedLogs::json),
        "wrong_effects":c.writes.wrong_effects,"verified_effects":by_kind(&c.writes.verified),
        "proven_overlaps":{"catalogue":measured.proven_catalogue,"rebuild":measured.proven_rebuild,"samples":[catalogue_proof,rebuild_proof]},
        "overlap":{"window_started_at":from.to_rfc3339(),"window_ended_at":to.to_rfc3339(),
            "note":"request-start samples are measurements of the free-running workload, not proof of overlap; see proven_overlaps and lock_waits",
            "contested_works":c.contested_works,"replayed_works":c.replayed_works,
            "catalogue":catalogue_json,
            "rebuild":rebuild_json,
            "events":c.writes.overlap_events,
            "statements":c.observed.statements.iter().filter(|(_,at,_)| *at <= to).take(500)
                .map(|(kind,at,head)|json!({"kind":kind,"started_at":at.to_rfc3339(),"statement":head})).collect::<Vec<_>>()},
        "reads":{"loadtest_arguments":c.read["args"],"results":c.read["results"][0],"process_success":c.read_process_success},
        "writes":{"loadtest_arguments":{"rate":rate,"duration":milliseconds(from, to) as f64 / 1000.0,
                "scenario":if c.mixed {"mixed-mutations"}else{"setFavorite"}},
            "results":write_results,"sent_by_kind":by_kind(&c.writes.sent),
            "queue_delay_ms":spread(c.writes.queue_delays.clone()),
            "arrival_latency_ms":spread(c.writes.arrival_latencies.clone())},
    })
}

async fn profile(bench: &Bench<'_>, load: Load<'_>, proof: ProofAccounts<'_>) -> Value {
    let instruments = bench.instruments;
    // An explicit test-only constructor retains labelled sessions through long
    // profiles without injected keepalives or relabelling. Actual connection loss
    // still loses the label and explicitly fails the gate at the profile edge.
    let measured_pool = db::connect_existing_for_benchmark_with(
        bench
            .pool
            .connect_options()
            .as_ref()
            .clone()
            .application_name(MAIN),
        bench.limits,
    )
    .await
    .expect("fresh measured handle for the profile");
    let pool = &measured_pool;
    let (router, covers, ready) = make_router(pool).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let (rate, mixed, rule) = (load.rate, load.mixed, bench.window);
    let mut foreground_session = pool.foreground().write().acquire().await.unwrap();
    let foreground_limits = session_limits(&mut *foreground_session).await;
    drop(foreground_session);
    let mut background_session = pool.background().write().acquire().await.unwrap();
    let background_limits = session_limits(&mut *background_session).await;
    drop(background_session);
    let effective_limits = json!({"measured":{
        "readers":session_limits(pool.read()).await,
        "foreground":foreground_limits,"background":background_limits,
        "readiness":session_limits(&ready.0).await},
        "connection_retention":{"source":"actual SQLx pool options; explicit cfg(test) benchmark constructor",
            "readers":pool_retention(pool.read()),
            "foreground":pool_retention(pool.foreground().write()),
            "background":pool_retention(pool.background().write())},
        "background_handles":{"read_connections":4,"foreground_connections":1,
            "readers":configured_session_limits(config::DatabaseLimits::default().readers),
            "writer":configured_session_limits(config::DatabaseLimits::default().background),
            "connection_retention":{"idle_timeout_ms":null,"max_lifetime_ms":null},
            "retention_source":"same explicit cfg(test) benchmark constructor; only labelled measured handles"},
        "standalone":standalone_limits(),
        "instruments":session_limits(instruments).await});
    let marker = format!("komika-bench-marker {}", uuid::Uuid::new_v4().simple());
    let identity_before = server_identity(instruments).await;
    // Settle first: counters still pending from earlier work would otherwise be
    // flushed inside this profile and counted as its own.
    let stats_before = settled_server_stats(
        instruments,
        Duration::from_secs(11),
        Duration::from_secs(45),
    )
    .await;
    let foreground_labelled = label_foreground(pool, bench.foreground_connections).await;
    // From before the begin marker until after the end marker: every session
    // whose lines can be in this profile's part of the server log.
    let census_stop = Arc::new(AtomicBool::new(false));
    let census_job = tokio::spawn(census(instruments.clone(), census_stop.clone()));
    // Keep the probe adjacent to the begin marker, after statistics settle and
    // labels are applied, so its line cannot be separated by the settle delay.
    let probe_pid = match bench.server_log {
        Some(_) => log_sqlstate_probe(instruments, &marker).await,
        None => None,
    };
    let marker_written = log_marker(instruments, &marker, "begin").await.is_ok();
    if let Some(capture) = bench.capture {
        capture.take();
    }
    let before = db::database_counters();
    let errors_before = db::lock_error_count();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let endpoint = format!("{base}/graphql");
    let stop_background = Arc::new(AtomicBool::new(false));

    // Part 1, deterministic: one proven overlap per background kind.
    let contested_works: Vec<String> = bench
        .contested
        .iter()
        .map(|contested| contested.work.clone())
        .collect();
    let background = start_background_with_proofs(
        pool,
        &client,
        &endpoint,
        bench.replayed,
        bench.perturbed,
        &proof,
        &stop_background,
    )
    .await;

    // Part 2, the measurement: both workers now run free against the HTTP load.
    let targets = Arc::new(WriteTargets {
        contested: bench.contested.to_vec(),
        accounts: load.accounts,
    });
    let accounts = targets.accounts.len();
    let plan = WritePlan::new(
        load.users,
        accounts - load.users,
        bench.contested.len(),
        mixed,
    );
    let stop = Arc::new(AtomicBool::new(false));
    let foreground = pool.foreground();
    let probe_stop = stop.clone();
    let probe = tokio::spawn(async move {
        let mut maximum = Duration::ZERO;
        let mut failures = 0u64;
        let mut probes = 0u64;
        while !probe_stop.load(Ordering::Relaxed) {
            let started = Instant::now();
            let result = foreground.begin().await;
            maximum = maximum.max(started.elapsed());
            match result {
                Ok(tx) => {
                    tx.rollback().await.unwrap();
                }
                Err(error) => {
                    db::record_database_error(&error);
                    failures += 1;
                }
            }
            probes += 1;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        (maximum, failures, probes)
    });
    let read_path = load.prefix.with_extension("read.json");
    // The read driver gets the hard cap as its duration and stops when this
    // file appears, which is when the window ends.
    let stop_path = load.prefix.with_extension("read.stop");
    let _ = std::fs::remove_file(&stop_path);
    let mut child = tokio::process::Command::new("node");
    child
        .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/loadtest.mjs"))
        .args([
            "--target",
            &base,
            "--scenario",
            "mixed",
            "--rate",
            "10",
            "--duration",
            &(rule.cap.max(rule.duration).as_secs() + 60).to_string(),
            "--warmup",
            "0",
            "--max-p95",
            "60000",
            "--stop-file",
        ])
        .arg(&stop_path)
        .arg("--json")
        .arg(&read_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true);
    let mut child = child
        .spawn()
        .expect("start existing loadtest.mjs read profile");
    let started = tokio::time::Instant::now();
    let window_start = chrono::Utc::now();
    let stop_sending = Arc::new(AtomicBool::new(false));
    let window_end = Arc::new(std::sync::OnceLock::new());
    let observer = tokio::spawn(observe(instruments.clone(), window_start, stop.clone()));
    let controller = tokio::spawn(run_window(
        WindowControl {
            rule,
            window_start,
            cycles: background.rebuild_cycles.clone(),
            capture: bench.capture.cloned(),
            stop_sending: stop_sending.clone(),
            window_end: window_end.clone(),
            stop_file: stop_path.clone(),
            workers_finished: background.finished.clone(),
        },
        started,
    ));
    let writes = drive_writes(
        &WriteLoad {
            client: &client,
            endpoint: &endpoint,
            instruments,
            targets: &targets,
            rate,
            duration: rule.duration,
            stop_sending: &stop_sending,
            window_end: &window_end,
        },
        plan,
        started,
    )
    .await;
    let (window_ended, capped, background_ended_early) = controller.await.unwrap();
    stop_background.store(true, Ordering::Relaxed);
    let read_status = child.wait().await.unwrap();
    let _ = std::fs::remove_file(&stop_path);
    stop.store(true, Ordering::Relaxed);
    stop_background.store(true, Ordering::Relaxed);
    let observed = observer.await.unwrap();
    let catalogue_run = await_background(background.catalogue).await;
    let rebuild_run = await_background(background.rebuild).await;
    let probe = probe.await.unwrap();
    pool.flush_deferred().await;
    let marker_written = marker_written && log_marker(instruments, &marker, "end").await.is_ok();
    census_stop.store(true, Ordering::Relaxed);
    let census = census_job.await.unwrap();
    let captured = bench.capture.map(LogCapture::take);
    let foreground_labelled_at_end = labelled_sessions(instruments, FOREGROUND).await;
    let foreign_sessions = other_sessions(instruments, false).await;
    let stats_after = settled_server_stats(
        instruments,
        Duration::from_secs(11),
        Duration::from_secs(45),
    )
    .await;
    let identity_after = server_identity(instruments).await;
    let counters = db::database_counters().since(before);
    let application_lock_errors = db::lock_error_count().saturating_sub(errors_before);
    let server_log = match bench.server_log {
        None => ServerLog::NotProvided,
        Some(_) if !marker_written => {
            ServerLog::Unusable("the run markers could not be written to the server log".into())
        }
        Some(_) if probe_pid.is_none() => {
            ServerLog::Unusable("the SQLSTATE probe statement could not be run".into())
        }
        Some(path) => read_server_log(path, &marker, &census.sessions, probe_pid).await,
    };
    let read: Value = std::fs::read(&read_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null);
    let mut result = summarize(Collected {
        mixed,
        rate,
        accounts,
        window: Window {
            rule,
            started: window_start,
            ended: window_ended,
            capped,
        },
        writes,
        read,
        read_process_success: read_status.success(),
        observed,
        census: (census.passes, census.errors, census.sessions.len()),
        catalogue: catalogue_run,
        rebuild: rebuild_run,
        proofs: [background.catalogue_proof, background.rebuild_proof],
        probe,
        identity: [identity_before, identity_after],
        statistics: [stats_before, stats_after],
        counters,
        application_lock_errors,
        captured,
        server_log,
        marker,
        marker_written,
        foreground_labelled: [foreground_labelled, foreground_labelled_at_end],
        foreground_connections: bench.foreground_connections,
        background_ended_early,
        foreign_sessions,
        contested_works,
        replayed_works: bench.replayed.to_vec(),
        perturbed_works: bench.perturbed.len(),
        min_catalogue_units: bench.min_catalogue_units,
    });
    result["effective_session_limits"] = effective_limits;
    server.abort();
    let _ = server.await;
    ready.0.close().await;
    covers.close().await;
    measured_pool.close().await;
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicitly supplied imported PostgreSQL scratch database and loopback sockets"]
async fn postgres_writer_overlap_benchmark() {
    use std::str::FromStr;
    let url = std::env::var("KOMIKA_PG_BENCH_URL")
        .expect("KOMIKA_PG_BENCH_URL existing scratch database");
    // Never print the URL: it carries the password.
    let options = sqlx::postgres::PgConnectOptions::from_str(&url)
        .unwrap_or_else(|_| panic!("KOMIKA_PG_BENCH_URL is not a PostgreSQL connection URL"))
        .application_name(MAIN);
    // Server statistics are per database, not per benchmark: any other session
    // in this database would be counted as ours. Refuse to start beside one.
    let (identity, limits) = {
        use sqlx::Connection;
        let mut preflight = sqlx::PgConnection::connect_with(&options)
            .await
            .expect("connect to the benchmark database");
        let others = other_sessions(&mut preflight, true).await;
        assert!(
            others.is_empty(),
            "the benchmark database must have no other client session at start; found {others:?}"
        );
        let identity = server_identity(&mut preflight).await;
        preflight.close().await.unwrap();
        // The production default pools: sixteen readers, eight foreground writers.
        (identity, config::DatabaseLimits::default())
    };
    // WARN and ERROR events from here on, for the whole process.
    let capture = LogCapture::install();
    let pool = db::connect_existing_with(options.application_name("komika-bench-setup"), limits)
        .await
        .expect("connect existing migrated PostgreSQL benchmark database");
    let database = identity["database"].as_str().unwrap().to_owned();
    assert!(
        !database.starts_with("komika_app_test_") && database != "komika_test_template",
        "benchmark must not use the disposable test database"
    );
    let output = std::path::PathBuf::from(
        std::env::var("KOMIKA_PG_BENCH_OUTPUT").expect("KOMIKA_PG_BENCH_OUTPUT JSON output path"),
    );
    let server_log = std::env::var_os("KOMIKA_PG_BENCH_SERVER_LOG")
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from);
    let duration = knob("KOMIKA_PG_BENCH_DURATION_SECS", 20);
    // The window lasts at least `duration` and until this many rebuild cycles
    // are complete inside it; the cap ends it and fails the gate.
    let rule = WindowRule {
        duration: Duration::from_secs(duration),
        cap: Duration::from_secs(knob("KOMIKA_PG_BENCH_MAX_DURATION_SECS", 900).max(duration)),
        min_cycles: knob("KOMIKA_PG_BENCH_MIN_REBUILD_CYCLES", 2),
    };
    let rate = knob("KOMIKA_PG_BENCH_HEAVY_WRITES_PER_SEC", 50);
    let users = knob("KOMIKA_PG_BENCH_USERS", 20).max(20) as usize;
    let contested_count = knob("KOMIKA_PG_BENCH_CONTESTED_WORKS", 4).clamp(2, 16) as i64;
    let perturbed_count = knob("KOMIKA_PG_BENCH_PERTURBED_WORKS", 64).min(1024) as i64;
    let proof_wait = Duration::from_secs(knob("KOMIKA_PG_BENCH_PROOF_WAIT_SECS", 300));
    // The observer keeps one session; requests share the rest for their
    // pre-send sample and their effect check.
    let instruments =
        instrument_pool(&pool, knob("KOMIKA_PG_BENCH_OBSERVER_SESSIONS", 6) as u32).await;
    // The contested works carry the measured workload: user writes target them
    // and admins edit them. The catalogue worker replays every second one
    // (index 0, 2, ...); the rebuild republishes the others, because they are
    // perturbed. One work cannot be both: the replay writes the stored year
    // back and projects the Browse row itself, so the rebuild would find
    // nothing to publish for it. The last work is only for the deterministic
    // samples: no background transaction of this benchmark locks it.
    let works: Vec<(String,String)> = sqlx::query_as("SELECT DISTINCT ON (w.id) w.id,c.external_id FROM work w JOIN work_external_id e ON e.work_id=w.id AND e.provider='mangadex' JOIN source_series ss ON ss.work_id=w.id AND ss.source_type='mangadex' JOIN chapter c ON c.source_series_id=ss.id JOIN browse_catalogue b ON b.work_id=w.id WHERE NOT COALESCE(w.is_nsfw_override,w.is_nsfw) AND c.lang='en' AND c.external_id IS NOT NULL AND c.external_id <> '' AND c.chapter_key IS NOT NULL ORDER BY w.id,c.id LIMIT $1")
        .bind(contested_count + 1).fetch_all(&instruments).await.unwrap();
    let Some(((proof_work, _), contested)) = works.split_last().filter(|(_, rest)| rest.len() >= 2)
    else {
        panic!("scratch copy needs three public MangaDex works with English chapters and browse projection");
    };
    let contested: Vec<Contested> = contested
        .iter()
        .map(|(work, chapter)| Contested {
            work: work.clone(),
            chapter: chapter.clone(),
        })
        .collect();
    let (replayed, republished) = split_contested(&contested);
    // Rows the Browse rebuild must republish: the contested works that are not
    // replayed and a bounded slice of other catalogued works, never the proof
    // work and never a replayed work.
    let excluded: Vec<&str> = contested
        .iter()
        .map(|contested| contested.work.as_str())
        .chain([proof_work.as_str()])
        .collect();
    let mut perturbed: Vec<String> = sqlx::query_scalar(
        "SELECT work_id FROM browse_catalogue WHERE work_id <> ALL($1) ORDER BY work_id LIMIT $2",
    )
    .bind(&excluded)
    .bind(perturbed_count)
    .fetch_all(&instruments)
    .await
    .unwrap();
    perturbed.extend(republished);
    // Use the database clock so the stored finite expiry and its retained
    // horizon share one time base. Reject unrepresentable knobs before creating
    // any account; never wrap a huge duration into an already-expired fixture.
    let session_fixture_started: Timestamp = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let (session_fixture_seconds, session_fixture_expires) =
        session_fixture_expiry(session_fixture_started, &rule, proof_wait)
            .expect("benchmark session horizon must fit a finite timestamp");
    let mut accounts = Vec::new();
    // Users, then four accounts for the deterministic samples (two kinds, two
    // profiles), then the admins for the metadata edits. `updateSeriesMetadata`
    // runs in the maintenance lane, which has two permits and answers 503 to a
    // third concurrent request; one account sends one request at a time, so
    // two admins can never be refused there.
    let admins = admin_accounts(contested.len());
    for index in 0..users + 4 + admins {
        let id = uuid::Uuid::new_v4();
        let name = format!("pg_bench_{}", id.simple());
        let token = format!("pg-benchmark-{}", uuid::Uuid::new_v4());
        sqlx::query("INSERT INTO users(id,username,email,password_hash,is_admin,created_at) VALUES($1,$2,$3,'unused',$4,now())")
            .bind(id).bind(&name).bind(format!("{name}@example.invalid")).bind(index >= users + 4).execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO sessions(token,user_id,created_at,expires_at) VALUES($1,$2,now(),$3)",
        )
        .bind(auth::hash_token(&token))
        .bind(id)
        .bind(session_fixture_expires)
        .execute(&pool)
        .await
        .unwrap();
        accounts.push((token, id));
    }
    let ids: Vec<uuid::Uuid> = accounts.iter().map(|(_, id)| *id).collect();
    let proof_accounts = |first: usize| ProofAccounts {
        work: proof_work,
        catalogue: (
            accounts[users + first].0.as_str(),
            accounts[users + first].1,
        ),
        rebuild: (
            accounts[users + first + 1].0.as_str(),
            accounts[users + first + 1].1,
        ),
        wait: proof_wait,
    };
    let bench = Bench {
        pool: &pool,
        instruments: &instruments,
        contested: &contested,
        replayed: &replayed,
        perturbed: &perturbed,
        capture: capture.as_ref(),
        server_log: server_log.as_deref(),
        foreground_connections: limits.foreground_connections,
        window: rule,
        limits,
        min_catalogue_units: 1000,
    };
    // The SQLite-comparable profile: one user, setFavorite at 2/s.
    let baseline = profile(
        &bench,
        Load {
            prefix: &output.with_extension("baseline"),
            accounts: accounts[..1].to_vec(),
            users: 1,
            rate: 2,
            mixed: false,
        },
        proof_accounts(0),
    )
    .await;
    let heavy = if baseline_allows_heavy(&baseline) {
        profile(
            &bench,
            Load {
                prefix: &output.with_extension("heavy"),
                accounts: accounts[..users]
                    .iter()
                    .chain(&accounts[users + 4..])
                    .cloned()
                    .collect(),
                users,
                rate,
                mixed: true,
            },
            proof_accounts(2),
        )
        .await
    } else {
        json!({"passed":false,"skipped":true,"reasons":["heavy profile skipped after failed baseline"],
            "reason":"heavy profile skipped after failed baseline", "baseline_reasons":baseline["reasons"]})
    };
    // Same keys as the historical `after` block at top level, with heavy profile
    // alongside it. Retain failed-run measurements before asserting the gate.
    let identity_after = server_identity(&instruments).await;
    let mut result = baseline.clone();
    result["session_fixtures"] = json!({
        "horizon_started_at":session_fixture_started.to_rfc3339(),
        "expires_at":session_fixture_expires.to_rfc3339(),
        "lifetime_seconds":session_fixture_seconds,
        "setup_drain_margin_seconds":SESSION_FIXTURE_MARGIN_SECS,
        "formula":"2 * max(profile cap, minimum duration) + 4 * proof deadline + setup/drain margin",
        "note":"finite scratch-only bearer sessions minted once before both profiles; no renewal during traffic; unusually long draining can still exhaust this explicit horizon"});
    result["heavy"] = heavy.clone();
    result["heavy"]["sqlite_baseline"] = json!({"source":"apps/server/bench/results/sqlite-contention-results.json","profile":null,
        "note":"no heavy profile was measured on SQLite; compare the top-level block only"});
    result["sqlite_baseline"] = sqlite_baseline();
    result["comparison_notes"] = comparison_notes(&rule);
    result["database"] = json!({"identity_at_start":identity,"identity_at_end":identity_after,"unchanged":identity==identity_after,
        "other_client_sessions_at_start":0,
        "effective_limits":{"baseline":baseline["effective_session_limits"],"heavy":heavy["effective_session_limits"]},
        "pool_limits":{"read_connections":limits.read_connections,"foreground_connections":limits.foreground_connections,"background_connections":1,
            "instrument_sessions":instruments.options().get_max_connections()}});
    result["workload"] = json!({"background":["refresh_feed_updates","refresh_feed_series_updates","refresh_browse_catalogue","refresh_work_fts","upsert_work_from_mangadex (stored metadata of the replayed works)","benchmark perturbation of work.year before each rebuild call that rebuilds Browse"],
        "read_requests_per_second":10,"baseline_writes_per_second":2,"heavy_writes_per_second":rate,"users":users,"admins":admins,"min_catalogue_units_inside_window":bench.min_catalogue_units,
        "contested_works":contested.len(),"contested_works_requested":knob("KOMIKA_PG_BENCH_CONTESTED_WORKS", 4),"contested_works_minimum":2,"replayed_works":replayed.len(),"perturbed_works":perturbed.len(),
        "baseline_contention":"one account targets replayed work 0; catalogue replay waits suffice for baseline, while the heavy profile must show replay and Browse publication waits",
        "duration_seconds":duration,"min_rebuild_cycles":rule.min_cycles,"max_duration_seconds":rule.cap.as_secs(),"probe_interval_ms":5,
        "window":"at least duration_seconds, and until min_rebuild_cycles rebuild cycles are complete inside it, each with a Browse publication that changed a row; max_duration_seconds ends it and fails the gate. The real length is window.seconds of each profile",
        "rebuild_cycle":"one pass of the rebuild worker over its four calls. refresh_feed_updates chains into refresh_feed_series_updates, which chains into refresh_browse_catalogue and refresh_work_fts, so one cycle publishes Browse three times and reconciles search four times",
        "heavy_mix":"nine of ten writes are user writes (setFavorite, setLibraryStatus, setProgress in rotation) on the contested works; every tenth is an admin updateSeriesMetadata on one of the first two contested works (two admins: the maintenance lane has two permits)",
        "contested_split":"the catalogue worker replays contested works 0, 2, ...; the rebuild worker perturbs, and so republishes, contested works 1, 3, ... A replayed work is never perturbed: the replay writes the stored year back and projects the Browse row itself",
        "what_the_gate_proves":"with real catalogue persistence and real rebuilds running, the measured user writes and reads all succeeded and stored the right values, no lock error, deadlock, timeout, lost connection or swallowed database error occurred on either side of the wire, the wanted number of rebuild cycles ran from start to end inside the window and published Browse rows, and user writes and background writes were seen waiting on each other at least once",
        "what_it_does_not_prove":["the network catalogue sync: the catalogue unit is the real persistence function replaying stored metadata, with no upstream fetch","that every lock path was exercised: contention is sampled, not exhaustive; lock_waits.by_background_work says which background work was met","production load, hardware or data volume: local loopback against an imported copy; production staging remains Phase 6"],
        "limitations":["loopback with real public router; no upstream fetch/sync HTTP","local hardware and imported copy; production staging remains Phase 6","counters and log capture are process-wide; run this benchmark alone","server statistics are database-wide, not per-benchmark; the database must have no other client session","the server log is cluster-wide: SQLSTATE lines are attributed by backend pid, and nothing else should be active in the cluster","the deterministic overlap samples hold the background transaction right after its BEGIN, before its first statement","on a quiet copy a rebuild publishes nothing and opens no writer transaction; the rebuild's writes come from the benchmark's own perturbation of work.year","lock waits are sampled about once a millisecond; shorter waits can be missed, and the server log reports only waits longer than deadlock_timeout","the scratch database is changed and not restored: work.year on the perturbed works, description_override on the edited works"]});
    std::fs::write(&output, serde_json::to_vec_pretty(&result).unwrap()).unwrap();
    pool.flush_deferred().await;
    sqlx::query("DELETE FROM users WHERE id=ANY($1)")
        .bind(&ids)
        .execute(&pool)
        .await
        .unwrap();
    instruments.close().await;
    pool.close().await;
    assert_eq!(
        identity, identity_after,
        "the server restarted or its statistics were reset during the benchmark"
    );
    assert_eq!(
        baseline["passed"], true,
        "baseline overlap gate failed ({}); see JSON output",
        baseline["reason"]
    );
    assert_eq!(
        heavy["passed"], true,
        "heavy overlap gate failed ({}); see JSON output",
        heavy["reason"]
    );
}

/// Quiet scratch-copy measurement for the Phase 4 preflight, not a gate.
/// Session-only slow statement logging follows inherited connect options into
/// the product's standalone staging sessions. It requires permission to set
/// log_min_duration_statement on this isolated database's measurement role.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicitly supplied migrated PostgreSQL scratch clone"]
async fn postgres_rebuild_cycle_timing() {
    use std::str::FromStr;
    let url = std::env::var("KOMIKA_PG_BENCH_URL").expect("KOMIKA_PG_BENCH_URL scratch clone");
    let options = sqlx::postgres::PgConnectOptions::from_str(&url)
        .unwrap_or_else(|_| panic!("KOMIKA_PG_BENCH_URL is not a PostgreSQL connection URL"))
        .application_name("komika-bench-cycle-timing")
        .options([("log_min_duration_statement", "1000")]);
    let output = std::path::PathBuf::from(
        std::env::var("KOMIKA_PG_BENCH_OUTPUT").expect("KOMIKA_PG_BENCH_OUTPUT timing JSON path"),
    );
    let log_path = std::path::PathBuf::from(
        std::env::var_os("KOMIKA_PG_BENCH_SERVER_LOG")
            .expect("KOMIKA_PG_BENCH_SERVER_LOG timing log"),
    );
    assert!(
        log_path.is_file(),
        "timing requires a readable plain-text server log"
    );
    // Guard before the pool constructor: that constructor may ensure extension
    // functions, so even its writes must be restricted to an explicit clone.
    let identity = {
        use sqlx::Connection;
        let mut preflight = sqlx::PgConnection::connect_with(&options)
            .await
            .expect("connect timing clone with session-only slow statement logging");
        let identity = server_identity(&mut preflight).await;
        assert!(
            timing_scratch_database(identity["database"].as_str().unwrap()),
            "timing refuses databases without an explicit Phase 4 scratch prefix"
        );
        assert!(
            other_sessions(&mut preflight, true).await.is_empty(),
            "timing clone must have no other client session"
        );
        preflight.close().await.unwrap();
        identity
    };
    let capture =
        LogCapture::install().expect("timing requires process-wide error/warning capture");
    let limits = config::DatabaseLimits::default();
    let pool = db::connect_existing_with(options, limits)
        .await
        .expect("connect existing migrated timing clone");
    let instruments = instrument_pool(&pool, 2).await;
    let mut dataset = serde_json::Map::new();
    for table in [
        "work",
        "chapter",
        "suwayomi_chapter",
        "browse_catalogue",
        "feed_updates",
        "feed_series_updates",
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&instruments)
            .await
            .unwrap();
        dataset.insert(table.into(), json!(count));
    }
    let works: Vec<String> = sqlx::query_scalar(
        "SELECT b.work_id FROM browse_catalogue b JOIN work w ON w.id = b.work_id WHERE NOT EXISTS (SELECT 1 FROM work_field_pin p WHERE p.work_id=w.id AND p.field='year') ORDER BY b.work_id LIMIT $1"
    ).bind(knob("KOMIKA_PG_BENCH_PERTURBED_WORKS", 64).min(1024) as i64)
        .fetch_all(&instruments).await.unwrap();
    assert!(
        !works.is_empty(),
        "timing clone needs an unpinned Browse work to republish"
    );
    let works_count = works.len();
    let mut perturber = Perturber::connect(&pool, works).await;
    let perturbation = perturber.apply().await.expect("perturb timing clone");
    assert!(perturbation.1 > 0, "timing perturbation must change a row");
    let marker = format!("komika-bench-timing {}", uuid::Uuid::new_v4().simple());
    capture.take();
    let before = db::database_counters();
    let census_stop = Arc::new(AtomicBool::new(false));
    let census_job = tokio::spawn(census(instruments.clone(), census_stop.clone()));
    let probe_pid = log_sqlstate_probe(&instruments, &marker).await;
    let begin_written = log_marker(&instruments, &marker, "begin").await.is_ok();
    let began = chrono::Utc::now();
    let started = Instant::now();
    let mut calls = Vec::new();
    let mut failure = None;
    for (step, name) in REBUILD_STEPS.into_iter().enumerate() {
        let call_began = chrono::Utc::now();
        let call_started = Instant::now();
        let result = match step {
            0 => catalog::refresh_feed_updates(&pool).await,
            1 => catalog::refresh_feed_series_updates(&pool).await,
            2 => catalog::refresh_browse_catalogue(&pool).await,
            _ => catalog::refresh_work_fts(&pool).await,
        };
        let error = result.err().map(|error| format!("{error:#}"));
        calls.push(json!({"call":name,"started_at":call_began.to_rfc3339(),
            "ended_at":chrono::Utc::now().to_rfc3339(),"seconds":call_started.elapsed().as_secs_f64(),
            "error":error}));
        if error.is_some() {
            failure = error;
            break;
        }
    }
    let seconds = started.elapsed().as_secs_f64();
    let ended = chrono::Utc::now();
    let end_written = log_marker(&instruments, &marker, "end").await.is_ok();
    census_stop.store(true, Ordering::Relaxed);
    let census = census_job.await.unwrap();
    let server_log = read_server_log(&log_path, &marker, &census.sessions, probe_pid).await;
    let counters = db::database_counters().since(before);
    let logs = capture.take();
    let mut reasons = timing_failure_reasons(&counters, &logs, &server_log);
    if !begin_written || !end_written {
        reasons.push("timing log markers failed".into());
    }
    if census.errors > 0 || census.passes == 0 {
        reasons.push("timing census failed".into());
    }
    if failure.is_some() || calls.len() != 4 {
        reasons.push("rebuild call failed".into());
    }
    let passed = reasons.is_empty();
    let mut background = pool.background().write().acquire().await.unwrap();
    let effective_writer = session_limits(&mut *background).await;
    drop(background);
    let result = json!({"measurement_only":true,"gate":false,"passed":passed,"reasons":reasons,"counters":counters,"log_events":logs.json(),"server_log":server_log.json(),
        "error":failure,"identity":identity,"dataset_rows":dataset,
        "started_at":began.to_rfc3339(),"ended_at":ended.to_rfc3339(),"seconds":seconds,
        "calls":calls,"recommended_gate_max_duration_seconds":if passed {Some((seconds*3.5).ceil() as u64)} else {None},
        "effective_limits":{"readers":session_limits(pool.read()).await,"writer":effective_writer,
            "standalone":standalone_limits()},
        "session_log_min_duration_statement_ms":1000,"server_log_marker":marker,
        "perturbation":{"works":works_count,"rows_touched":perturbation.0,"rows_changed":perturbation.1,
            "note":"one work.year perturbation before the cycle, on this explicit scratch clone only; the gate perturbs before each of its three Browse-containing calls"},
        "note":"one cycle of the gate's actual four product calls; error evidence is retained before failing. No catalogue replay or HTTP load runs, so gate contention may increase durations. Requires quiet machine, migrated imported clone, VACUUM ANALYZE, and no DATABASE_* overrides"});
    std::fs::write(output, serde_json::to_vec_pretty(&result).unwrap()).unwrap();
    {
        use sqlx::Connection;
        perturber.connection.close().await.unwrap();
    }
    instruments.close().await;
    pool.close().await;
    assert_eq!(
        result["passed"], true,
        "timing rebuild failed; see retained JSON/log evidence"
    );
}

fn timing_failure_reasons(
    counters: &db::DatabaseCounters,
    logs: &CapturedLogs,
    server_log: &ServerLog,
) -> Vec<String> {
    let mut reasons = Vec::new();
    if counters.lock_timeouts
        + counters.statement_timeouts
        + counters.deadlocks
        + counters.idle_transaction_terminations
        + counters.pool_timeouts
        + counters.retries_exhausted
        + counters.connection_lost
        > 0
    {
        reasons.push("database error observed during timing cycle".into());
    }
    if logs.errors > 0 || logs.database_warnings > 0 {
        reasons.push("application database ERROR/WARN during timing cycle".into());
    }
    match server_log {
        ServerLog::Scanned(scan)
            if scan.probe_found && scan.sqlstate_in_prefix && scan.pid_in_prefix =>
        {
            if scan.other_error_lines > 0
                || GATED_SQLSTATES
                    .iter()
                    .any(|code| scan.sqlstates.get(*code).copied().unwrap_or(0) > 0)
            {
                reasons.push("server ERROR/FATAL/PANIC during timing cycle".into());
            }
        }
        _ => reasons.push("timing server log evidence unavailable".into()),
    }
    reasons
}

fn timing_scratch_database(name: &str) -> bool {
    [
        "komika_p4_measure_",
        "komika_phase4_measure_",
        "komika_phase4_bench_",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix) && name.len() > prefix.len())
}

/// The helper statements behind the hygiene checks, on the disposable test
/// database: they must parse and answer on this PostgreSQL version. This is not
/// the benchmark; it takes no measurement.
#[tokio::test]
async fn hygiene_statements_answer_on_the_test_database() {
    let pool = db::Database::connect("sqlite::memory:").await.unwrap();
    let instruments = instrument_pool(&pool, 2).await;
    let identity = server_identity(&instruments).await;
    assert!(identity["database"]
        .as_str()
        .is_some_and(|name| !name.is_empty()));
    assert!(identity["postmaster_started_at"].is_string());
    assert_eq!(identity, server_identity(pool.read()).await);
    // A reader holds a connection: it is another client session of this database,
    // and it is not one of the benchmark's own (wrong application name).
    let held = pool.read().acquire().await.unwrap();
    for own in [true, false] {
        let others = other_sessions(pool.write(), own).await;
        assert!(!others.is_empty(), "own={own}: {others:?}");
    }
    drop(held);
    let labelled = benchmark_background_pool(&pool, CATALOGUE).await;
    // The foreground writers get their own name, and keep it in the pool.
    assert_eq!(label_foreground(&pool, 3).await, 3);
    assert_eq!(labelled_sessions(&instruments, FOREGROUND).await, 3);
    let named = other_sessions(pool.write(), true).await;
    for name in [CATALOGUE, FOREGROUND, OBSERVER] {
        assert!(
            named
                .iter()
                .any(|session| session["application_name"] == name),
            "{name}: {named:?}"
        );
    }
    // Instrument, labelled and foreground sessions are the benchmark's own.
    assert!(other_sessions(pool.write(), false)
        .await
        .iter()
        .all(|session| !session["application_name"]
            .as_str()
            .unwrap()
            .starts_with(BENCH_APPLICATION)));
    // No transaction is open on the labelled writer; one is after BEGIN, and it
    // owns the background lease.
    assert!(background_transaction(&pool, CATALOGUE).await.is_empty());
    let open = labelled.begin().await.unwrap();
    let rows = background_transaction(&pool, CATALOGUE).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].2.as_str(), rows[0].3),
        ("idle in transaction", true)
    );
    // Idle in a transaction is not a statement, and nobody waits on a lock.
    assert!(background_activity(&instruments)
        .await
        .unwrap()
        .iter()
        .all(|activity| activity.application == CATALOGUE
            && activity.xact_start <= activity.query_start));
    assert!(sample_lock_waits(&instruments).await.unwrap().is_empty());
    open.rollback().await.unwrap();
    // The run marker is accepted by the server (it goes to the server log).
    log_marker(&instruments, "komika-bench-marker hygiene", "begin")
        .await
        .unwrap();
    // The SQLSTATE probe fails with its own SQLSTATE on a known backend, and
    // leaves its session usable.
    let probe_pid = log_sqlstate_probe(&instruments, "komika-bench-marker hygiene")
        .await
        .expect("the probe statement fails with its own SQLSTATE");
    assert!(probe_pid > 0);
    // The census sees the benchmark's sessions by name, and only those.
    let sessions: HashMap<i32, String> = census_once(&instruments)
        .await
        .unwrap()
        .into_iter()
        .collect();
    assert!(sessions.contains_key(&probe_pid), "{sessions:?}");
    assert!(sessions
        .values()
        .all(|name| name.starts_with(BENCH_APPLICATION)));
    for name in [CATALOGUE, FOREGROUND, OBSERVER] {
        assert!(sessions.values().any(|seen| seen == name), "{name}");
    }
    let stopped = Arc::new(AtomicBool::new(true));
    let counted = census(instruments.clone(), stopped).await;
    assert_eq!((counted.passes, counted.errors), (1, 0));
    assert_eq!(counted.sessions.len(), sessions.len());
    // A quiet database settles within the bound.
    let stats = settled_server_stats(
        &instruments,
        Duration::from_millis(600),
        Duration::from_secs(20),
    )
    .await;
    for key in STATISTICS {
        assert!(stats[key].is_i64(), "{key}: {stats}");
    }
    assert!(stats["samples"].as_u64().unwrap() >= 2);
    labelled.close().await;
    instruments.close().await;
    pool.close().await;
}

/// The deterministic overlap sample (second check, R2-1), exactly as a profile
/// takes it, against the real router on the disposable test database. The old
/// gate counted a request that merely STARTED while a background transaction was
/// open. Here the background transaction must still be the same open transaction
/// after the user write has returned and its row is stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlap_proof_needs_a_verified_user_write_inside_the_open_background_transaction() {
    let pool = db::Database::connect("sqlite::memory:").await.unwrap();
    let mut works = Vec::new();
    // Three roles, three works. The perturbed work must not be the replayed
    // one: in the merged tree a rebuild opens a writer transaction only for a
    // row that differs, and the catalogue worker writes the replayed work's
    // stored year back within milliseconds and projects its Browse row itself.
    // With one work in both roles the rebuild finds nothing to publish, opens
    // no transaction, and its sample waits for a BEGIN that never comes.
    for key in ["md-bench-replay", "md-bench-proof", "md-bench-perturbed"] {
        works.push(
            catalog::upsert_work_from_mangadex(
                &pool,
                key,
                &catalog::WorkInput {
                    primary_title: Some(key.into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
        );
    }
    let mut accounts = Vec::new();
    for name in ["catalogue", "rebuild", "never", "ended"] {
        let id = uuid::Uuid::new_v4();
        let token = format!("overlap-proof-{}", uuid::Uuid::new_v4());
        sqlx::query("INSERT INTO users(id,username,email,password_hash,is_admin,created_at) VALUES($1,$2,$3,'unused',false,now())")
            .bind(id).bind(format!("proof_{name}")).bind(format!("proof_{name}@example.invalid")).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO sessions(token,user_id,created_at,expires_at) VALUES($1,$2,now(),now()+interval '1 hour')")
            .bind(auth::hash_token(&token)).bind(id).execute(&pool).await.unwrap();
        accounts.push((token, id));
    }
    let (router, covers, ready) = make_router(&pool).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/graphql", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let proof = ProofAccounts {
        work: &works[1],
        catalogue: (accounts[0].0.as_str(), accounts[0].1),
        rebuild: (accounts[1].0.as_str(), accounts[1].1),
        wait: Duration::from_secs(30),
    };
    let background = start_background_with_proofs(
        &pool,
        &client,
        &endpoint,
        &works[..1],
        &works[2..],
        &proof,
        &stop,
    )
    .await;
    let at = |value: &Value| {
        chrono::DateTime::parse_from_rfc3339(value.as_str().expect("timestamp")).unwrap()
    };
    for (label, sample) in [
        (CATALOGUE, &background.catalogue_proof),
        (REBUILD, &background.rebuild_proof),
    ] {
        assert_eq!(sample["proven"], true, "{sample}");
        assert_eq!(sample["kind"], label);
        let background = &sample["background"];
        assert_eq!(background["state_while_held"], "idle in transaction");
        assert_eq!(background["held_background_lease"], true);
        assert_eq!(background["same_transaction_still_open"], true);
        let write = &sample["user_write"];
        assert_eq!(write["effect_absent_before"], true);
        assert_eq!(write["effect_verified"], true);
        // Server clock: the transaction opened, was seen open, the user row was
        // stored, and the same transaction was seen open again after that.
        assert!(
            at(&background["xact_started_at"]) <= at(&background["open_checked_before_write_at"])
        );
        assert!(
            at(&background["open_checked_before_write_at"]) <= at(&write["effect_verified_at"])
        );
        assert!(at(&write["effect_verified_at"]) <= at(&background["open_checked_after_write_at"]));
        assert!(at(&write["sent_at"]) <= at(&write["returned_at"]));
        assert!(at(&write["returned_at"]) <= at(&sample["released_at"]));
    }
    assert_ne!(
        background.catalogue_proof["background"]["pid"],
        background.rebuild_proof["background"]["pid"]
    );
    // No background transaction reaches the pause: nothing is proven, the pause
    // is released, and no user write is sent on its behalf.
    let points = locks::pause_points::PausePoints::default();
    let never = points.arm("transaction-open");
    let refused = prove_overlap(
        &ProofWrite {
            pool: &pool,
            client: &client,
            endpoint: &endpoint,
            token: accounts[2].0.as_str(),
            user: accounts[2].1,
            work: &works[1],
            before_verify: None,
        },
        "komika-bench-absent",
        &never,
        Duration::from_millis(200),
        None,
    )
    .await;
    assert_eq!(refused["proven"], false, "{refused}");
    assert!(refused["reason"]
        .as_str()
        .unwrap()
        .contains("no background transaction opened"));
    tokio::time::timeout(Duration::from_secs(2), points.hit("transaction-open"))
        .await
        .expect("a failed proof must release its pause");
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_library WHERE user_id=$1")
        .bind(accounts[2].1)
        .fetch_one(pool.read())
        .await
        .unwrap();
    assert_eq!(rows, 0);
    let points = locks::pause_points::PausePoints::default();
    let unopened = points.arm("transaction-open");
    let (sender, mut finished) = tokio::sync::watch::channel(false);
    let dead_write = ProofWrite {
        pool: &pool,
        client: &client,
        endpoint: &endpoint,
        token: accounts[2].0.as_str(),
        user: accounts[2].1,
        work: &works[1],
        before_verify: None,
    };
    let dead_proof = prove_overlap(
        &dead_write,
        "komika-bench-dead",
        &unopened,
        Duration::from_secs(300),
        Some(&mut finished),
    );
    let (dead, ()) = tokio::join!(dead_proof, async {
        drop(sender);
    });
    assert_eq!(dead["proven"], false);
    assert_eq!(
        dead["reason"],
        "background worker ended before its transaction opened"
    );
    tokio::time::timeout(Duration::from_secs(2), points.hit("transaction-open"))
        .await
        .expect("dead-worker proof must release its pause");
    // A row that existed before the write proves nothing: the catalogue account
    // already has its favourite, so a second sample with it is refused.
    let points = locks::pause_points::PausePoints::default();
    let again = points.arm("transaction-open");
    let holder = benchmark_background_pool(&pool, "komika-bench-again").await;
    let held = tokio::spawn(async move {
        let result = database::TRANSACTION_OPEN_PAUSE
            .scope(points, async { holder.begin().await?.rollback().await })
            .await;
        holder.close().await;
        result
    });
    let stale = prove_overlap(
        &ProofWrite {
            pool: &pool,
            client: &client,
            endpoint: &endpoint,
            token: accounts[0].0.as_str(),
            user: accounts[0].1,
            work: &works[1],
            before_verify: None,
        },
        "komika-bench-again",
        &again,
        Duration::from_secs(30),
        None,
    )
    .await;
    held.await.unwrap().unwrap();
    assert_eq!(stale["proven"], false, "{stale}");
    assert_eq!(stale["user_write"]["effect_absent_before"], false);
    // Third check, weak test: the negative overlap case. The background
    // transaction is open when the user write is sent, and ENDS after the
    // mutation is answered but before its effect is verified. Everything else
    // holds (row absent before, mutation succeeded, effect stored, lease
    // owned), and the sample is still refused.
    let points = locks::pause_points::PausePoints::default();
    let opened = points.arm("transaction-open");
    let holder = benchmark_background_pool(&pool, "komika-bench-ended").await;
    let end = Arc::new(tokio::sync::Notify::new());
    let ended = end.clone();
    let held = tokio::spawn(async move {
        database::TRANSACTION_OPEN_PAUSE
            .scope(points, async {
                tokio::select! {
                    // Stopped at the pause right after its real BEGIN ...
                    transaction = holder.begin() => drop(transaction),
                    // ... until told to end: dropping the future drops the open
                    // transaction, and the pool's release hook rolls it back.
                    () = ended.notified() => {}
                }
            })
            .await;
        holder.close().await;
    });
    let proof_points = locks::pause_points::PausePoints::default();
    let before_verify = proof_points.arm("proof-before-verify");
    let write = ProofWrite {
        pool: &pool,
        client: &client,
        endpoint: &endpoint,
        token: accounts[3].0.as_str(),
        user: accounts[3].1,
        work: &works[1],
        before_verify: Some(&proof_points),
    };
    let (too_late, ()) = tokio::join!(
        prove_overlap(
            &write,
            "komika-bench-ended",
            &opened,
            Duration::from_secs(30),
            None,
        ),
        async {
            // The mutation has been answered; now the background transaction ends.
            before_verify.reached().await;
            end.notify_one();
            tokio::time::timeout(Duration::from_secs(10), async {
                while !background_transaction(&pool, "komika-bench-ended")
                    .await
                    .is_empty()
                {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("the held background transaction did not end");
            before_verify.release();
        }
    );
    held.await.unwrap();
    assert_eq!(too_late["proven"], false, "{too_late}");
    assert_eq!(
        too_late["reason"],
        "the background transaction ended before the user write was verified"
    );
    assert_eq!(too_late["user_write"]["effect_absent_before"], true);
    assert_eq!(too_late["user_write"]["effect_verified"], true);
    assert_eq!(too_late["background"]["held_background_lease"], true);
    assert_eq!(too_late["background"]["same_transaction_still_open"], false);
    assert!(too_late["background"]["open_checked_after_write_at"].is_null());
    stop.store(true, Ordering::Relaxed);
    let catalogue_run = await_background(background.catalogue).await;
    let rebuild_run = await_background(background.rebuild).await;
    for run in [&catalogue_run, &rebuild_run] {
        assert_eq!(run.error, None);
        assert!(!run.units.is_empty());
        assert!(run.began <= run.ended);
    }
    // The rebuild worker's cycles are what it shared while it ran: four calls
    // in order, three perturbations per cycle, and every perturbation really
    // changed the perturbed work's year (nothing writes it back).
    assert_eq!(
        rebuild_run.cycles,
        *background.rebuild_cycles.lock().unwrap()
    );
    assert_eq!(rebuild_run.units.len(), rebuild_run.cycles.len());
    // Shutdown may finish only a prefix of the final cycle. Complete cycles
    // still have all four calls; its unfinished suffix must not be counted.
    let complete_calls = rebuild_run.cycles.len() * 4;
    assert!(rebuild_run.step_marks.len() >= complete_calls);
    let final_prefix = rebuild_run.step_marks.len() - complete_calls;
    assert!(final_prefix < 4);
    assert_eq!(
        rebuild_run.perturbations.len(),
        rebuild_run.cycles.len() * 3 + final_prefix.min(3)
    );
    assert_eq!(
        rebuild_run
            .step_marks
            .iter()
            .map(|(_, name)| *name)
            .collect::<Vec<_>>(),
        REBUILD_STEPS
            .into_iter()
            .cycle()
            .take(rebuild_run.step_marks.len())
            .collect::<Vec<_>>()
    );
    assert!(rebuild_run
        .perturbations
        .iter()
        .all(|perturbation| (perturbation.touched, perturbation.changed) == (1, 1)));
    for cycle in &rebuild_run.cycles {
        assert!(cycle.began <= cycle.steps[0].0 && cycle.steps[3].1 <= cycle.ended);
        assert!(cycle
            .steps
            .windows(2)
            .all(|pair| pair[0].0 <= pair[0].1 && pair[0].1 <= pair[1].0));
    }
    assert!(catalogue_run.cycles.is_empty() && catalogue_run.perturbations.is_empty());
    // A replayed work in the perturbed slice would be refused before anything starts.
    assert!(roles_are_disjoint(&works[..1], &works[2..]));
    assert!(!roles_are_disjoint(&works[..1], &works[..1]));
    assert!(!roles_are_disjoint(&works[..2], &works[1..]));
    server.abort();
    let _ = server.await;
    pool.flush_deferred().await;
    ready.0.close().await;
    covers.close().await;
    pool.close().await;
}

/// The lock-wait observer's statement on a real server: who waits, who blocks,
/// since when, and whether it is only the background lease queue. Both
/// directions between the sides, with real sessions labelled as a profile
/// labels them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lock_wait_observer_attributes_real_waits_to_their_sides() {
    let pool = db::Database::connect("sqlite::memory:").await.unwrap();
    let instruments = instrument_pool(&pool, 2).await;
    sqlx::raw_sql(
        "CREATE TABLE bench_wait(id bigint PRIMARY KEY, value bigint NOT NULL); \
         INSERT INTO bench_wait VALUES (1, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(label_foreground(&pool, 2).await, 2);
    let foreground = pool.foreground();
    let catalogue = benchmark_background_pool(&pool, CATALOGUE).await;
    let rebuild = benchmark_background_pool(&pool, REBUILD).await;
    // Sample until the ledger holds a timed wait in `direction`.
    async fn until(instruments: &sqlx::PgPool, waits: &mut LockWaits, direction: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                for sample in sample_lock_waits(instruments).await.unwrap() {
                    waits.record(&sample);
                }
                let total = &waits.totals()[direction];
                if total.episodes > 0 && total.max_wait_ms > 0.0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {direction} wait was observed: {waits:?}"));
    }
    const TOUCH: &str = "UPDATE bench_wait SET value = value + 1 WHERE id = 1";
    let mut waits = LockWaits::default();

    // A user request holds the row; the catalogue writer waits for it.
    let mut user = foreground.begin().await.unwrap();
    sqlx::query(TOUCH).execute(&mut *user).await.unwrap();
    let worker = catalogue.clone();
    let job = tokio::spawn(async move {
        let mut transaction = worker.begin().await?;
        sqlx::query(TOUCH).execute(&mut *transaction).await?;
        transaction.commit().await
    });
    until(&instruments, &mut waits, "background_waited_on_foreground").await;
    // Meanwhile the rebuild writer queues for the lease the catalogue writer
    // holds: a background-on-background wait that is NOT contention.
    let worker = rebuild.clone();
    let queued = tokio::spawn(async move { worker.begin().await?.rollback().await });
    until(&instruments, &mut waits, "background_lease_queue").await;
    user.commit().await.unwrap();
    job.await.unwrap().unwrap();
    queued.await.unwrap().unwrap();

    // The rebuild writer holds the row; a user request waits for it.
    let mut held = rebuild.begin().await.unwrap();
    sqlx::query(TOUCH).execute(&mut *held).await.unwrap();
    let request = tokio::spawn(async move {
        let mut transaction = foreground.begin().await?;
        sqlx::query(TOUCH).execute(&mut *transaction).await?;
        transaction.commit().await
    });
    until(&instruments, &mut waits, "foreground_waited_on_background").await;
    held.commit().await.unwrap();
    request.await.unwrap().unwrap();

    let totals = waits.totals();
    assert_eq!(
        totals["background_waited_on_foreground"].episodes, 1,
        "{waits:?}"
    );
    assert_eq!(
        totals["foreground_waited_on_background"].episodes, 1,
        "{waits:?}"
    );
    assert!(totals["background_lease_queue"].episodes >= 1, "{waits:?}");
    for quiet in [
        "foreground_waited_on_foreground",
        "background_waited_on_background",
        "other",
    ] {
        assert_eq!(totals[quiet], WaitTotals::default(), "{quiet}: {waits:?}");
    }
    // Each wait was a row lock held by the named side. The timeline places
    // the rebuild writer in its Browse rebuild from long before these waits.
    let long_ago = chrono::Utc::now() - chrono::Duration::hours(1);
    let timeline = [(long_ago, "browse_publication")];
    assert_eq!(
        waits.by_background_work(&timeline),
        [
            ("browse_publication", (1, 0)),
            ("catalogue_replay", (0, 1)),
            ("feed_rebuild", (0, 0)),
            ("other", (0, 0)),
            ("search_reconcile", (0, 0)),
        ]
        .into_iter()
        .collect()
    );
    let json = waits.json(&timeline);
    assert_eq!(
        json["by_background_work"]["episodes"]["catalogue_replay"],
        json!({"foreground_waited_on_it":0,"it_waited_on_foreground":1})
    );
    let examples = json["examples"].as_array().unwrap();
    assert_eq!(examples.len(), 2, "{json}");
    assert_eq!(examples[0]["wait_event"], "transactionid");
    assert!(examples.iter().any(
        |example| example["waiter"] == CATALOGUE && example["blockers"] == json!([FOREGROUND])
    ));
    assert!(examples
        .iter()
        .any(|example| example["waiter"] == FOREGROUND && example["blockers"] == json!([REBUILD])));
    let value: i64 = sqlx::query_scalar("SELECT value FROM bench_wait WHERE id = 1")
        .fetch_one(&instruments)
        .await
        .unwrap();
    assert_eq!(value, 4);
    catalogue.close().await;
    rebuild.close().await;
    instruments.close().await;
    pool.close().await;
}

/// The write driver and its effect checks, as a profile runs them, against the
/// real router on the disposable test database: every kind of the mix is
/// accepted by the real resolvers and every stored value is the planned one.
/// No measurement is taken.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_mix_is_accepted_by_the_real_router_and_every_effect_verifies() {
    let pool = db::Database::connect("sqlite::memory:").await.unwrap();
    let instruments = instrument_pool(&pool, 4).await;
    let mut contested = Vec::new();
    for key in ["md-bench-mix-a", "md-bench-mix-b"] {
        let work = catalog::upsert_work_from_mangadex(
            &pool,
            key,
            &catalog::WorkInput {
                primary_title: Some(key.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let source: String = sqlx::query_scalar(
            "SELECT id FROM source_series WHERE work_id=$1 AND source_type='mangadex'",
        )
        .bind(&work)
        .fetch_one(pool.read())
        .await
        .unwrap();
        let chapter = format!("{key}-chapter");
        sqlx::query("INSERT INTO chapter (id, source_series_id, external_id, number, lang, chapter_key, created_at) VALUES ($1,$2,$3,'1','en','1',now())")
            .bind(format!("c_{key}")).bind(&source).bind(&chapter).execute(&pool).await.unwrap();
        contested.push(Contested { work, chapter });
    }
    // Fourth check, R4-2: never more admins than the maintenance lane admits.
    assert_eq!(
        [1, 2, 3, 4, 16].map(admin_accounts),
        [1, MAINTENANCE_PERMITS, 2, 2, 2]
    );
    let (users, admins) = (3usize, admin_accounts(contested.len()));
    let mut accounts = Vec::new();
    // One admin more than the write mix uses, for the refused request below.
    for index in 0..users + admins + 1 {
        let id = uuid::Uuid::new_v4();
        let token = format!("write-mix-{}", uuid::Uuid::new_v4());
        sqlx::query("INSERT INTO users(id,username,email,password_hash,is_admin,created_at) VALUES($1,$2,$3,'unused',$4,now())")
            .bind(id).bind(format!("mix_{index}")).bind(format!("mix_{index}@example.invalid")).bind(index >= users).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO sessions(token,user_id,created_at,expires_at) VALUES($1,$2,now(),now()+interval '1 hour')")
            .bind(auth::hash_token(&token)).bind(id).execute(&pool).await.unwrap();
        accounts.push((token, id));
    }
    let (router, covers, ready) = make_router(&pool).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/graphql", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let targets = Arc::new(WriteTargets {
        contested: contested.clone(),
        accounts: accounts.clone(),
    });
    let plan = plan_writes(80, users, admins, contested.len(), true);
    let never_stopped = AtomicBool::new(false);
    let no_window_end = std::sync::OnceLock::new();
    let results = drive_writes(
        &WriteLoad {
            client: &client,
            endpoint: &endpoint,
            instruments: &instruments,
            targets: &targets,
            rate: 200,
            duration: Duration::from_millis(400),
            stop_sending: &never_stopped,
            window_end: &no_window_end,
        },
        plan.clone().into_iter(),
        tokio::time::Instant::now(),
    )
    .await;
    assert_eq!(results.samples.len(), 80);
    assert_eq!(
        (results.http_errors, results.net_errors, results.gql_errors),
        (0, 0, 0)
    );
    assert!(results
        .samples
        .iter()
        .all(|(latency, ok)| *ok && *latency > 0.0));
    assert_eq!(results.wrong_effects, 0);
    assert_eq!(results.instrument_errors, 0);
    assert_eq!(results.sent, results.verified);
    for kind in KINDS {
        assert!(results.verified[&kind] > 0, "{kind:?}");
    }
    assert_eq!(results.verified[&Kind::WorkEdit], 8);
    assert_eq!(results.queue_delays.len(), 80);
    // Scheduled arrival to complete answer: queue delay plus latency, per request.
    assert_eq!(results.arrival_latencies.len(), 80);
    for (index, arrival) in results.arrival_latencies.iter().enumerate() {
        assert_eq!(
            *arrival,
            results.queue_delays[index] + results.samples[index].0
        );
    }
    assert!(results.wall_seconds >= 0.4);
    // The last write of every (account, kind) pair is what the database holds.
    let mut last: BTreeMap<(usize, Kind), &PlannedWrite> = BTreeMap::new();
    for write in &plan {
        last.insert((write.account, write.kind), write);
    }
    assert_eq!(last.len(), users * 3 + admins);
    for write in last.values() {
        assert!(
            verify_effect(
                &instruments,
                accounts[write.account].1,
                &contested[write.target],
                write
            )
            .await
            .unwrap(),
            "{write:?}"
        );
    }
    // A value that was never written is not reported as stored.
    let never = PlannedWrite {
        index: 1_000_001,
        ..plan[9].clone()
    };
    assert_eq!(never.kind, Kind::WorkEdit);
    assert!(!verify_effect(
        &instruments,
        accounts[never.account].1,
        &contested[never.target],
        &never
    )
    .await
    .unwrap());
    // The schedule has no end of its own: the driver sends until the window's
    // stop signal, sends nothing after it, and still awaits what it sent.
    let stopping = Arc::new(AtomicBool::new(false));
    let ended = Arc::new(std::sync::OnceLock::new());
    let signal = (stopping.clone(), ended.clone());
    let stopper = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        signal.1.set(chrono::Utc::now()).unwrap();
        signal.0.store(true, Ordering::Relaxed);
    });
    let endless = drive_writes(
        &WriteLoad {
            client: &client,
            endpoint: &endpoint,
            instruments: &instruments,
            targets: &targets,
            rate: 100,
            duration: Duration::from_millis(100),
            stop_sending: &stopping,
            window_end: &ended,
        },
        WritePlan::new(users, admins, contested.len(), true),
        tokio::time::Instant::now(),
    )
    .await;
    stopper.await.unwrap();
    let sent = endless.samples.len();
    assert!((10..=40).contains(&sent), "{sent} writes in about 300 ms");
    assert!(endless.samples.iter().all(|(_, ok)| *ok));
    assert_eq!((endless.wrong_effects, endless.instrument_errors), (0, 0));
    // Fourth check, R4-2, the premise: the maintenance lane admits two
    // `updateSeriesMetadata` requests and answers 503 to a third. Two admin
    // edits are held in flight behind a row lock; the edit of a third admin is
    // refused at admission. This is why a profile has at most two admins.
    let edit = |account: usize, target: usize, number: u64| {
        let write = PlannedWrite {
            sequence: 0,
            account,
            kind: Kind::WorkEdit,
            target,
            index: number,
        };
        client
            .post(&endpoint)
            .bearer_auth(&accounts[account].0)
            .json(&json!({"query":write.mutation(&contested[target])}))
            .send()
    };
    let mut holder = instruments.begin().await.unwrap();
    sqlx::query("SELECT id FROM work WHERE id = ANY($1) FOR UPDATE")
        .bind(
            contested
                .iter()
                .map(|contested| contested.work.clone())
                .collect::<Vec<_>>(),
        )
        .fetch_all(&mut *holder)
        .await
        .unwrap();
    let held = [
        tokio::spawn(edit(users, 0, 900)),
        tokio::spawn(edit(users + 1, 1, 900)),
    ];
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let waiting: BTreeSet<i32> = sample_lock_waits(&instruments)
                .await
                .unwrap()
                .iter()
                .map(|sample| sample.waiter_pid)
                .collect();
            if waiting.len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("two admin edits waiting behind the row locks");
    let refused = edit(users + admins, 0, 901).await.unwrap();
    assert_eq!(refused.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    holder.commit().await.unwrap();
    for request in held {
        let answer = request.await.unwrap().unwrap();
        assert!(answer.status().is_success());
        let body: Value = answer.json().await.unwrap();
        assert!(body.get("errors").is_none(), "{body}");
    }
    server.abort();
    let _ = server.await;
    pool.flush_deferred().await;
    ready.0.close().await;
    covers.close().await;
    instruments.close().await;
    pool.close().await;
}

/// The premise of the rebuild perturbation, on the real functions: a quiet
/// Browse rebuild publishes nothing; after the perturbation it must republish
/// exactly the perturbed row, and the log capture sees the rebuild say so.
#[tokio::test]
async fn perturbation_gives_the_browse_rebuild_a_row_to_publish() {
    let pool = db::Database::connect("sqlite::memory:").await.unwrap();
    let work = catalog::upsert_work_from_mangadex(
        &pool,
        "md-bench-perturbed",
        &catalog::WorkInput {
            primary_title: Some("Perturbed".into()),
            year: Some(2010),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let years = || async {
        let row: (Option<i64>, Option<i64>) = sqlx::query_as(
            "SELECT (SELECT year::bigint FROM work WHERE id=$1), \
                    (SELECT year::bigint FROM browse_catalogue WHERE work_id=$1)",
        )
        .bind(&work)
        .fetch_one(pool.read())
        .await
        .unwrap();
        row
    };
    assert_eq!(years().await, (Some(2010), Some(2010)));
    // This test runs on one thread, so the scoped subscriber sees every event
    // of the rebuild.
    let capture = LogCapture::default();
    let _scoped = tracing::subscriber::set_default(capture.subscriber());
    let published = |logs: &CapturedLogs| logs.published.get("browse_catalogue").copied();
    // Let the first rebuild settle whatever the incremental projection left.
    catalog::refresh_browse_catalogue(&pool).await.unwrap();
    capture.take();
    catalog::refresh_browse_catalogue(&pool).await.unwrap();
    let quiet = capture.take();
    assert_eq!(
        published(&quiet),
        Some(0),
        "a quiet rebuild publishes nothing"
    );
    assert_eq!(
        (quiet.errors, quiet.warnings),
        (0, 0),
        "{:?}",
        quiet.warn_events
    );
    let mut perturber = Perturber::connect(&pool, vec![work.clone()]).await;
    for expected in [2011, 2010, 2011] {
        assert_eq!(perturber.apply().await.unwrap(), (1, 1));
        // The source row moved; the projection has not been touched.
        let (source, projected) = years().await;
        assert_eq!(source, Some(expected));
        assert_ne!(projected, source);
        catalog::refresh_browse_catalogue(&pool).await.unwrap();
        assert_eq!(years().await, (Some(expected), Some(expected)));
        let logs = capture.take();
        assert_eq!(published(&logs), Some(1), "the rebuild republished the row");
        assert_eq!(
            (logs.errors, logs.database_warnings),
            (0, 0),
            "{:?} {:?}",
            logs.first_errors,
            logs.warn_events
        );
    }
    // A work that is not in the slice is left alone.
    let mut nobody = Perturber::connect(&pool, vec!["w_absent".into()]).await;
    assert_eq!(nobody.apply().await.unwrap(), (0, 0));
    // Fourth check, R4-5: a pinned year. The UPDATE touches the row, the
    // `work_pin_year` trigger restores the pin, and nothing changed: the
    // rebuild has nothing to publish, and the perturbation says so.
    sqlx::query(
        "INSERT INTO work_field_pin (work_id, field, value, source_value, pinned_at) \
         VALUES ($1, 'year', '2011', NULL, now())",
    )
    .bind(&work)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(perturber.apply().await.unwrap(), (1, 0));
    assert_eq!(years().await, (Some(2011), Some(2011)));
    catalog::refresh_browse_catalogue(&pool).await.unwrap();
    assert_eq!(published(&capture.take()), Some(0));
    {
        use sqlx::Connection;
        perturber.connection.close().await.unwrap();
        nobody.connection.close().await.unwrap();
    }
    pool.close().await;
}

/// Fourth check, R4-4 and R4-5, on the real functions: one cycle of the
/// rebuild worker in the merged tree, as the window counts it. The first
/// rebuild call chains into the others, so one cycle publishes Browse three
/// times; each publication is reported with the rows it changed, and the cycle
/// counts only for a window that contains all of it.
#[tokio::test]
async fn one_rebuild_cycle_publishes_browse_three_times_and_is_counted() {
    let pool = db::Database::connect("sqlite::memory:").await.unwrap();
    let work = catalog::upsert_work_from_mangadex(
        &pool,
        "md-bench-cycle",
        &catalog::WorkInput {
            primary_title: Some("Cycle".into()),
            year: Some(2010),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    // This test runs on one thread, so the scoped subscriber sees every event
    // of the worker, which is awaited here and not spawned.
    let capture = LogCapture::default();
    let _scoped = tracing::subscriber::set_default(capture.subscriber());
    // Let a first rebuild settle whatever the incremental projection left.
    catalog::refresh_feed_updates(&pool).await.unwrap();
    capture.take();
    let rebuild = benchmark_background_pool(&pool, REBUILD).await;
    let perturber = Perturber::connect(&pool, vec![work.clone()]).await;
    let progress = Arc::new(Mutex::new(Vec::new()));
    let before = chrono::Utc::now();
    // A test-only cycle bound lets one full cycle finish. The production stop
    // signal now stops between steps, rather than requiring a whole cycle.
    let run = rebuild_worker(
        rebuild,
        locks::pause_points::PausePoints::default(),
        Arc::new(AtomicBool::new(false)),
        perturber,
        progress.clone(),
        Some(1),
    )
    .await;
    let after = chrono::Utc::now();
    assert_eq!(run.error, None);
    assert_eq!(run.cycles.len(), 1);
    assert_eq!(*progress.lock().unwrap(), run.cycles);
    assert_eq!(
        run.step_marks
            .iter()
            .map(|(_, step)| *step)
            .collect::<Vec<_>>(),
        REBUILD_STEPS
    );
    assert_eq!(
        run.perturbations
            .iter()
            .map(|perturbation| (perturbation.touched, perturbation.changed))
            .collect::<Vec<_>>(),
        [(1, 1); 3]
    );
    // What the capture holds is still there for the window controller to read.
    let events = capture.rebuild_events();
    let logs = capture.take();
    assert_eq!(events, logs.rebuild_events);
    assert_eq!(
        (logs.errors, logs.database_warnings),
        (0, 0),
        "{:?} {:?}",
        logs.first_errors,
        logs.warn_events
    );
    // Three Browse publications, one perturbed row each.
    assert_eq!(browse_publications(&events, before, after), (3, 3));
    assert_eq!(logs.published.get("browse_catalogue"), Some(&3));
    let cycle = &run.cycles[0];
    assert_eq!(
        browse_publications(&events, cycle.began, cycle.ended),
        (3, 3)
    );
    assert_eq!(counted_cycles(&run.cycles, &events, before, after), [cycle]);
    // A window that begins after the cycle began, or ends before it ended,
    // does not contain it; and a cycle nobody saw publish Browse is not counted.
    let instant = chrono::Duration::microseconds(1);
    assert!(counted_cycles(&run.cycles, &events, cycle.began + instant, after).is_empty());
    assert!(counted_cycles(&run.cycles, &events, before, cycle.ended - instant).is_empty());
    assert!(counted_cycles(&run.cycles, &[], before, after).is_empty());
    // Where the rebuild writer was, in order: the chain twice, then the two
    // direct calls.
    let mut kinds: Vec<&str> = rebuild_timeline(&run.step_marks, &events)
        .into_iter()
        .map(|(_, kind)| kind)
        .collect();
    kinds.dedup();
    assert_eq!(
        kinds,
        [
            "feed_rebuild",
            "browse_publication",
            "search_reconcile",
            "feed_rebuild",
            "browse_publication",
            "search_reconcile",
            "browse_publication",
            "search_reconcile"
        ]
    );
    pool.close().await;
}

/// Fourth check, R4-8, and the window's read driver: `loadtest.mjs` against a
/// loopback server that answers every request in two chunks, `data` first and
/// `errors` after it, as async-graphql does, with the chunk boundary inside
/// the key. The old driver looked for `"errors"` in the first chunk only and
/// counted every one of these as a success. `--stop-file` ends a run long before its `--duration`.
#[tokio::test]
async fn read_driver_finds_graphql_errors_after_the_first_chunk_and_stops_on_its_stop_file() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                // One request per connection: read the head and the declared body.
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let Ok(read) = socket.read(&mut buffer).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&request);
                    if let Some(head_end) = text.find("\r\n\r\n") {
                        let length = text[..head_end]
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if request.len() >= head_end + 4 + length {
                            break;
                        }
                    }
                }
                // The key itself is cut in two by the chunk boundary.
                let first = format!(
                    "{{\"data\":{{\"genreFacets\":\"{}\"}},\"err",
                    "x".repeat(2000)
                );
                let second = "ors\":[{\"message\":\"late\"}]}";
                let head = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                            transfer-encoding: chunked\r\nconnection: close\r\n\r\n";
                let chunk = |body: &str| format!("{:x}\r\n{body}\r\n", body.len());
                let _ = socket
                    .write_all(format!("{head}{}", chunk(&first)).as_bytes())
                    .await;
                let _ = socket.flush().await;
                tokio::time::sleep(Duration::from_millis(30)).await;
                let _ = socket
                    .write_all(format!("{}0\r\n\r\n", chunk(second)).as_bytes())
                    .await;
                let _ = socket.shutdown().await;
            });
        }
    });
    let scratch = std::env::temp_dir().join(format!(
        "komika-bench-read-driver-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&scratch).unwrap();
    let (output, stop_file) = (scratch.join("read.json"), scratch.join("read.stop"));
    let spawned = tokio::process::Command::new("node")
        .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/loadtest.mjs"))
        .args(["--target", &target, "--scenario", "facets", "--rate", "20"])
        .args(["--duration", "120", "--warmup", "0", "--max-p95", "60000"])
        .arg("--stop-file")
        .arg(&stop_file)
        .arg("--json")
        .arg(&output)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn();
    let Ok(mut child) = spawned else {
        // The benchmark itself needs node; a machine without it cannot run
        // this check, and that is said here, not passed off as a result.
        eprintln!("SKIPPED: node is not installed, loadtest.mjs was not exercised");
        server.abort();
        let _ = std::fs::remove_dir_all(&scratch);
        return;
    };
    tokio::time::sleep(Duration::from_millis(1500)).await;
    std::fs::write(&stop_file, b"stop").unwrap();
    let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
        .await
        .expect("the read driver stops soon after its stop file appears, not after 120 s")
        .unwrap();
    assert!(status.success());
    let report: Value = serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
    let results = &report["results"][0];
    let requests = results["requests"].as_u64().unwrap();
    assert!(
        (5..=120).contains(&requests),
        "about 1.5 s at 20 requests per second: {results}"
    );
    // Every answer carried `errors` after its first chunk.
    assert_eq!(results["gqlErrors"], requests, "{results}");
    assert_eq!(results["ok"], 0, "{results}");
    assert_eq!(results["httpErrors"], 0, "{results}");
    assert_eq!(results["netErrors"], 0, "{results}");
    server.abort();
    let _ = std::fs::remove_dir_all(&scratch);
}

#[cfg(test)]
mod instrument_tests {
    use super::*;

    #[test]
    fn session_fixture_outlives_both_caps_and_all_four_proofs() {
        let started = chrono::DateTime::parse_from_rfc3339("2026-10-11T22:39:50Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let rule = WindowRule {
            duration: Duration::from_secs(20),
            cap: Duration::from_secs(3600),
            min_cycles: 2,
        };
        let proof_wait = Duration::from_secs(900);
        let (seconds, expiry) = session_fixture_expiry(started, &rule, proof_wait).unwrap();
        assert_eq!(seconds, 97_200);
        assert_eq!(expiry, started + chrono::TimeDelta::hours(27));
        let latest_windows_and_proofs = started + chrono::TimeDelta::hours(3);
        assert!(expiry > latest_windows_and_proofs);
        assert_eq!(
            (expiry - latest_windows_and_proofs).num_seconds(),
            SESSION_FIXTURE_MARGIN_SECS as i64
        );
        // The effective cap must also cover a larger configured minimum.
        let larger_minimum = WindowRule {
            duration: Duration::from_secs(10_000),
            ..rule
        };
        assert_eq!(
            session_fixture_expiry(started, &larger_minimum, proof_wait)
                .unwrap()
                .0,
            110_000
        );
    }

    #[test]
    fn session_fixture_rejects_overflow_and_nonfinite_expiry() {
        let started = chrono::DateTime::parse_from_rfc3339("2026-10-11T22:39:50Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let mut rule = WindowRule {
            duration: Duration::from_secs(20),
            cap: Duration::from_secs(3600),
            min_cycles: 2,
        };
        assert!(session_fixture_expiry(started, &rule, Duration::MAX).is_none());
        rule.cap = Duration::MAX;
        assert!(session_fixture_expiry(started, &rule, Duration::ZERO).is_none());
        rule.cap = Duration::from_secs(u64::MAX / 2);
        assert!(session_fixture_expiry(started, &rule, Duration::from_secs(1)).is_none());
        assert!(session_fixture_expiry(started, &rule, Duration::ZERO).is_none());
        rule.cap = Duration::from_secs(u64::MAX / 3);
        assert!(session_fixture_expiry(started, &rule, Duration::ZERO).is_none());
        // Integer arithmetic can fit while chrono's finite duration/date does not.
        rule.cap = Duration::from_secs(i64::MAX as u64 / 4);
        assert!(session_fixture_expiry(started, &rule, Duration::ZERO).is_none());
        rule.cap = Duration::from_secs(3600);
        assert!(session_fixture_expiry(Timestamp::MAX_UTC, &rule, Duration::ZERO).is_none());
    }

    fn passing() -> Measured {
        Measured {
            mixed: true,
            write_requests: 1000,
            write_ok: 1000,
            wrong_effects: 0,
            verified_effects: [300, 300, 300, 100],
            read_process_success: true,
            read_requests: 201,
            read_ok: 201,
            application_lock_errors: 0,
            // Recovered retries and restarts are allowed.
            counters: db::DatabaseCounters {
                serialization_failures: 3,
                retries: 5,
                transaction_restarts: 2,
                ..Default::default()
            },
            probe_failures: 0,
            background_error: None,
            proven_catalogue: 1,
            proven_rebuild: 1,
            catalogue_units: 1000,
            min_catalogue_units: 1000,
            rebuild_cycles: 2,
            min_rebuild_cycles: 2,
            window_capped: false,
            catalogue_statements: 40,
            rebuild_statements: 12,
            perturbed_rows: 68,
            browse_rows_published: 396,
            foreground_waited_on_background: 7,
            background_waited_on_foreground: 0,
            observer_iterations: 9000,
            observer_errors: 0,
            instrument_errors: 0,
            statistics_valid: true,
            server_deadlocks: 0,
            sessions_killed: 0,
            sessions_fatal: 0,
            foreign_sessions: 0,
            server_log: scanned(|_| {}),
            log_capture_installed: true,
            error_events: 0,
            database_warn_events: 0,
            foreground_labels_valid: true,
            background_ended_early: false,
        }
    }

    /// A scanned server log whose probe line proved its prefix, then changed.
    fn scanned(change: impl FnOnce(&mut ServerLogScan)) -> ServerLog {
        let mut scan = ServerLogScan {
            probe_found: true,
            sqlstate_in_prefix: true,
            pid_in_prefix: true,
            ..Default::default()
        };
        change(&mut scan);
        ServerLog::Scanned(Box::new(scan))
    }

    // Third check, weak test: nothing evaluated `passed`. Every condition of
    // the pass rule is broken alone and must fail with its own reason.
    #[test]
    fn gate_fails_on_each_condition_alone_with_its_reason() {
        let pass = evaluate_gate(&passing());
        assert_eq!(
            pass,
            GateResult {
                passed: true,
                contention_exercised: true,
                reasons: vec![]
            }
        );
        type Break = fn(&mut Measured);
        let conditions: &[(&str, Break)] = &[
            ("application lock errors", |m| m.application_lock_errors = 1),
            ("application deadlocks (40P01)", |m| {
                m.counters.deadlocks = 1
            }),
            ("lock timeouts (55P03)", |m| m.counters.lock_timeouts = 1),
            ("pool acquire timeouts", |m| m.counters.pool_timeouts = 1),
            ("statement timeouts (57014)", |m| {
                m.counters.statement_timeouts = 1
            }),
            ("idle-in-transaction terminations (25P03)", |m| {
                m.counters.idle_transaction_terminations = 1
            }),
            ("connections lost", |m| m.counters.connection_lost = 1),
            ("retries exhausted", |m| m.counters.retries_exhausted = 1),
            (
                "server statistics not settled or database identity changed",
                |m| m.statistics_valid = false,
            ),
            ("server deadlocks", |m| m.server_deadlocks = 1),
            ("server sessions killed", |m| m.sessions_killed = 1),
            ("server sessions ended by a fatal error", |m| {
                m.sessions_fatal = 1
            }),
            ("another client session in the database", |m| {
                m.foreign_sessions = 1
            }),
            ("failed writes", |m| m.write_ok -= 1),
            ("failed writes", |m| (m.write_requests, m.write_ok) = (0, 0)),
            ("wrong effects", |m| m.wrong_effects = 1),
            ("a mutation kind has no verified effect", |m| {
                m.verified_effects[0] = 0
            }),
            ("a mutation kind has no verified effect", |m| {
                m.verified_effects[3] = 0
            }),
            ("read driver failed", |m| m.read_process_success = false),
            ("no read requests", |m| {
                (m.read_requests, m.read_ok) = (0, 0)
            }),
            ("failed reads", |m| m.read_ok -= 1),
            ("foreground probe failures", |m| m.probe_failures = 1),
            ("background worker error", |m| {
                m.background_error = Some("rebuild failed".into())
            }),
            ("no proven catalogue overlap", |m| m.proven_catalogue = 0),
            ("no proven rebuild overlap", |m| m.proven_rebuild = 0),
            ("too few catalogue units inside the window", |m| {
                m.catalogue_units = 0
            }),
            (
                "the window reached its hard cap (KOMIKA_PG_BENCH_MAX_DURATION_SECS) before the rebuild cycles were complete",
                |m| m.window_capped = true,
            ),
            (
                "too few complete rebuild cycles with a Browse publication inside the window",
                |m| m.rebuild_cycles = 1,
            ),
            (
                "too few complete rebuild cycles with a Browse publication inside the window",
                |m| (m.rebuild_cycles, m.min_rebuild_cycles) = (0, 0),
            ),
            (
                "too few complete rebuild cycles with a Browse publication inside the window",
                |m| m.min_rebuild_cycles = 3,
            ),
            (
                "no catalogue write statement observed inside the window",
                |m| m.catalogue_statements = 0,
            ),
            (
                "no rebuild write statement observed inside the window",
                |m| m.rebuild_statements = 0,
            ),
            (
                "the rebuild perturbation changed no row inside the window",
                |m| m.perturbed_rows = 0,
            ),
            (
                "no Browse publication changed a row inside the window",
                |m| m.browse_rows_published = 0,
            ),
            ("log capture not installed", |m| {
                m.log_capture_installed = false
            }),
            ("ERROR log events", |m| m.error_events = 1),
            ("WARN log events naming a database failure", |m| {
                m.database_warn_events = 1
            }),
            ("lock-wait observer failed", |m| m.observer_errors = 1),
            ("lock-wait observer failed", |m| m.observer_iterations = 0),
            ("an instrument query failed", |m| m.instrument_errors = 1),
            ("foreground session labels lost or incomplete", |m| m.foreground_labels_valid = false),
            ("background worker ended before the window closed", |m| m.background_ended_early = true),
            ("server log has other ERROR/FATAL/PANIC lines", |m| m.server_log = scanned(|scan| scan.other_error_lines = 1)),
            (NO_CONTENTION, |m| m.foreground_waited_on_background = 0),
            // Fourth check, R4-1: no server log is no evidence, and a log whose
            // prefix is not known to carry the SQLSTATE proves nothing by being clean.
            ("server log not provided", |m| {
                m.server_log = ServerLog::NotProvided
            }),
            ("server log unusable: no markers", |m| {
                m.server_log = ServerLog::Unusable("no markers".into())
            }),
            ("server log has no line for the SQLSTATE probe", |m| {
                m.server_log = scanned(|scan| {
                    (scan.probe_found, scan.sqlstate_in_prefix) = (false, false)
                })
            }),
            ("server log line prefix has no SQLSTATE (%e)", |m| {
                m.server_log = scanned(|scan| scan.sqlstate_in_prefix = false)
            }),
            ("server log has SQLSTATE 55P03", |m| {
                m.server_log = scanned(|scan| {
                    scan.sqlstates.insert("55P03".into(), 1);
                })
            }),
            ("server log has SQLSTATE 57014", |m| {
                m.server_log = scanned(|scan| {
                    scan.sqlstates.insert("57014".into(), 2);
                })
            }),
            ("server log has SQLSTATE 40P01", |m| {
                m.server_log = scanned(|scan| {
                    scan.sqlstates.insert("40P01".into(), 1);
                })
            }),
            ("server log has SQLSTATE 25P03", |m| {
                m.server_log = scanned(|scan| {
                    scan.sqlstates.insert("25P03".into(), 1);
                })
            }),
        ];
        for (reason, change) in conditions {
            let mut broken = passing();
            change(&mut broken);
            let gate = evaluate_gate(&broken);
            assert!(!gate.passed, "{reason}");
            assert_eq!(gate.reasons, [*reason], "{reason}");
            assert_eq!(
                gate.contention_exercised,
                *reason != NO_CONTENTION,
                "{reason}"
            );
        }
        // More cycles than wanted is fine, and so is a prefix without a pid:
        // every SQLSTATE line is then counted as the benchmark's.
        let mut more = passing();
        more.rebuild_cycles = 5;
        more.server_log = scanned(|scan| scan.pid_in_prefix = false);
        assert!(evaluate_gate(&more).passed);
        // SQLSTATE lines of other backends are reported, never judged.
        let mut elsewhere = passing();
        elsewhere.server_log = scanned(|scan| {
            scan.other_backends.insert("57014".into(), 3);
            scan.other_backends.insert("55P03".into(), 1);
        });
        assert!(evaluate_gate(&elsewhere).passed);
        // Two broken conditions give two reasons, in the rule's order.
        let mut both = passing();
        both.counters.lock_timeouts = 2;
        both.read_ok = 0;
        assert_eq!(
            evaluate_gate(&both).reasons,
            ["lock timeouts (55P03)", "failed reads"]
        );
    }

    #[test]
    fn gate_contention_comes_from_either_direction_or_from_attributed_server_log_waits() {
        // Either observed direction is enough.
        let mut reverse = passing();
        reverse.foreground_waited_on_background = 0;
        reverse.background_waited_on_foreground = 1;
        assert!(evaluate_gate(&reverse).passed);
        // No observed wait: the run is refused, whatever else is clean.
        let mut never_met = passing();
        never_met.foreground_waited_on_background = 0;
        let gate = evaluate_gate(&never_met);
        assert_eq!(
            (gate.passed, gate.contention_exercised, gate.reasons),
            (false, false, vec![NO_CONTENTION.to_owned()])
        );
        // A clean server log without lock-wait lines does not change that.
        never_met.server_log = scanned(|_| {});
        assert_eq!(evaluate_gate(&never_met).reasons, [NO_CONTENTION]);
        // Lease queueing and unattributed waits in the server log are not
        // contention between the sides.
        never_met.server_log = scanned(|scan| {
            scan.still_waiting = 9;
            scan.lease_waits = 6;
            scan.other_waits = 3;
            scan.acquired = 9;
        });
        assert_eq!(evaluate_gate(&never_met).reasons, [NO_CONTENTION]);
        // A server-log wait between the two sides is.
        for direction in 0..2 {
            never_met.server_log = scanned(|scan| {
                if direction == 0 {
                    scan.foreground_waited_on_background = 1;
                } else {
                    scan.background_waited_on_foreground = 1;
                }
            });
            let gate = evaluate_gate(&never_met);
            assert!(gate.passed && gate.contention_exercised, "{direction}");
        }
        // A recovered serialization failure in the server log is reported only.
        let mut recovered = passing();
        recovered.server_log = scanned(|scan| {
            scan.sqlstates.insert("40001".into(), 4);
        });
        assert!(evaluate_gate(&recovered).passed);
        // The baseline profile needs only its one mutation kind.
        let mut baseline = passing();
        baseline.mixed = false;
        baseline.verified_effects = [40, 0, 0, 0];
        assert!(evaluate_gate(&baseline).passed);
        baseline.mixed = true;
        assert_eq!(
            evaluate_gate(&baseline).reasons,
            ["a mutation kind has no verified effect"]
        );
    }

    // Third check, F4: with twenty users the old values were constant per
    // (user, kind) pair, so a write that did nothing still "verified".
    #[test]
    fn planned_writes_change_the_stored_value_for_any_user_count() {
        for users in [1usize, 7, 20, 23, 60] {
            for (admins, contested) in [(4usize, 4usize), (1, 1), (3, 5)] {
                let plan = plan_writes(1000, users, admins, contested, true);
                assert_eq!(plan.len(), 1000);
                let mut pairs: BTreeMap<(usize, Kind), Vec<&PlannedWrite>> = BTreeMap::new();
                for (sequence, write) in plan.iter().enumerate() {
                    assert_eq!(write.sequence, sequence as u64);
                    assert!(write.target < contested);
                    assert_eq!(write.kind == Kind::WorkEdit, write.account >= users);
                    assert!(write.account < users + admins);
                    pairs
                        .entry((write.account, write.kind))
                        .or_default()
                        .push(write);
                }
                let (mut consecutive, mut changed) = (0u64, 0u64);
                for ((account, kind), writes) in &pairs {
                    for (index, write) in writes.iter().enumerate() {
                        // The pair's own index, and always the same row.
                        assert_eq!(write.index, index as u64);
                        assert_eq!(write.target, writes[0].target, "{account} {kind:?}");
                    }
                    for pair in writes.windows(2) {
                        consecutive += 1;
                        changed += u64::from(pair[0].value() != pair[1].value());
                    }
                }
                assert!(consecutive > 0);
                assert!(
                    changed * 10 >= consecutive * 9,
                    "users={users}: {changed} of {consecutive} consecutive writes change the value"
                );
                // In fact every one does.
                assert_eq!(changed, consecutive, "users={users}");
                // One editor per contested work, so an edit's value is unambiguous.
                let mut editors: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
                for write in plan.iter().filter(|write| write.kind == Kind::WorkEdit) {
                    editors
                        .entry(write.target)
                        .or_default()
                        .insert(write.account);
                }
                if admins <= contested {
                    assert!(editors.values().all(|accounts| accounts.len() == 1));
                }
                assert_eq!(
                    plan.iter()
                        .filter(|write| write.kind == Kind::WorkEdit)
                        .count(),
                    100
                );
                if users <= 23 {
                    // Every user sends every user kind, more than once.
                    for user in 0..users {
                        for kind in &KINDS[..3] {
                            assert!(pairs[&(user, *kind)].len() >= 2, "{users} {user} {kind:?}");
                        }
                    }
                }
            }
        }
        // The stated cases, at the default heavy size.
        for users in [20usize, 23] {
            let plan = plan_writes(1000, users, 4, 4, true);
            let mut previous: HashMap<(usize, Kind), StoredValue> = HashMap::new();
            let (mut repeated, mut total) = (0, 0);
            for write in &plan {
                if let Some(before) = previous.insert((write.account, write.kind), write.value()) {
                    total += 1;
                    repeated += usize::from(before == write.value());
                }
            }
            assert!(total > 800 && repeated == 0, "users={users}");
        }
        // The baseline: one kind, alternating, one account.
        let baseline = plan_writes(40, 1, 0, 4, false);
        assert!(baseline
            .iter()
            .all(|write| write.kind == Kind::Favorite && write.account == 0 && write.target == 0));
        assert!(baseline
            .windows(2)
            .all(|pair| pair[0].value() != pair[1].value()));
        // The mutation text carries the planned value and target.
        let target = Contested {
            work: "w_1".into(),
            chapter: "chapter-1".into(),
        };
        let texts: Vec<String> = plan_writes(10, 1, 1, 1, true)
            .iter()
            .map(|write| write.mutation(&target))
            .collect();
        assert_eq!(
            texts[0],
            "mutation { setFavorite(seriesId:\"w_1\",favorite:true) { id } }"
        );
        assert_eq!(
            texts[1],
            "mutation { setLibraryStatus(seriesId:\"w_1\",status:\"reading\") { id } }"
        );
        assert_eq!(
            texts[2],
            "mutation { setProgress(chapterId:\"chapter-1\",lastPageRead:1,read:false) }"
        );
        assert_eq!(
            texts[9],
            "mutation { updateSeriesMetadata(input:{seriesId:\"w_1\",description:\"komika-bench edit by account 1 number 0\"}) { id } }"
        );
    }

    fn wait(
        waiter: (i32, &str),
        blocker: Option<&str>,
        started_ms: Option<i64>,
        sampled_ms: i64,
        lease: bool,
    ) -> WaitSample {
        let at = |ms: i64| chrono::DateTime::from_timestamp_millis(1_800_000_000_000 + ms).unwrap();
        WaitSample {
            waiter_pid: waiter.0,
            waiter: waiter.1.into(),
            wait_event: if lease { "advisory" } else { "transactionid" }.into(),
            wait_started: started_ms.map(at),
            sampled_at: at(sampled_ms),
            lease,
            blocker: blocker.map(str::to_owned),
            waiter_query: "UPDATE work SET ...".into(),
            blocker_query: "INSERT INTO work ...".into(),
        }
    }

    #[test]
    fn lock_wait_examples_keep_a_late_browse_blocker_after_twenty_catalogue_waits() {
        let at = |ms: i64| chrono::DateTime::from_timestamp_millis(1_800_000_000_000 + ms).unwrap();
        let mut waits = LockWaits::default();
        for ms in 0..30 {
            waits.record(&wait(
                (10, FOREGROUND),
                Some(CATALOGUE),
                Some(ms),
                ms + 1,
                false,
            ));
        }
        let mut browse = wait((10, FOREGROUND), Some(REBUILD), Some(100), 105, false);
        browse.blocker_query = "UPDATE browse_catalogue SET year = $1 WHERE work_id = $2".into();
        waits.record(&browse);
        waits.record(&wait(
            (20, REBUILD),
            Some(FOREGROUND),
            Some(110),
            112,
            false,
        ));
        let timeline = [(at(90), "browse_publication")];
        let totals_before = waits.totals();
        let kinds_before = waits.by_background_work(&timeline);
        let json = waits.json(&timeline);
        let examples = json["examples"].as_array().unwrap();
        assert_eq!(examples.len(), 20);
        assert_eq!(waits.totals(), totals_before);
        assert_eq!(waits.by_background_work(&timeline), kinds_before);
        assert_eq!(json["foreground_waited_on_background"]["episodes"], 31);
        assert_eq!(json["background_waited_on_foreground"]["episodes"], 1);
        for (kind, direction, blocker) in [
            (
                "catalogue_replay",
                "foreground_waited_on_background",
                CATALOGUE,
            ),
            (
                "browse_publication",
                "foreground_waited_on_background",
                REBUILD,
            ),
            (
                "browse_publication",
                "background_waited_on_foreground",
                FOREGROUND,
            ),
        ] {
            assert!(examples.iter().any(|example| {
                example["direction"] == direction
                    && example["background_work"]
                        .as_array()
                        .unwrap()
                        .contains(&json!(kind))
                    && example["blockers"]
                        .as_array()
                        .unwrap()
                        .contains(&json!(blocker))
            }));
        }
        let retained_browse = examples
            .iter()
            .find(|example| {
                example["direction"] == "foreground_waited_on_background"
                    && example["blockers"]
                        .as_array()
                        .unwrap()
                        .contains(&json!(REBUILD))
            })
            .unwrap();
        assert_eq!(retained_browse["wait_started_at"], at(100).to_rfc3339());
        assert_eq!(retained_browse["observed_wait_ms"], 5.0);
        assert_eq!(retained_browse["blocker_statement"], browse.blocker_query);
    }

    #[test]
    fn lock_wait_ledger_counts_episodes_by_direction_and_keeps_the_lease_queue_apart() {
        let mut waits = LockWaits::default();
        let samples = [
            // One foreground wait on the catalogue writer, seen three times.
            wait((10, FOREGROUND), Some(CATALOGUE), Some(0), 2, false),
            wait((10, FOREGROUND), Some(CATALOGUE), Some(0), 5, false),
            wait((10, FOREGROUND), Some(CATALOGUE), Some(0), 9, false),
            // The same backend waits again later: a second episode.
            wait((10, FOREGROUND), Some(REBUILD), Some(100), 104, false),
            // The rebuild writer waits on two foreground sessions: one episode,
            // one sample (two rows of the same instant).
            wait((20, REBUILD), Some(FOREGROUND), Some(50), 61, false),
            wait((20, REBUILD), Some(FOREGROUND), Some(50), 61, false),
            // Foreground behind foreground, and behind foreground AND background.
            wait((11, FOREGROUND), Some(FOREGROUND), Some(10), 13, false),
            wait((12, FOREGROUND), Some(FOREGROUND), Some(20), 22, false),
            wait((12, FOREGROUND), Some(CATALOGUE), Some(20), 22, false),
            // The lease queue: never contention, whoever holds the lease.
            wait((21, CATALOGUE), Some(REBUILD), Some(0), 900, true),
            wait((30, MAIN), Some(CATALOGUE), Some(0), 400, true),
            // Scaffolding, readers and unknown sessions are `other`.
            wait((10, FOREGROUND), Some(PERTURB), Some(300), 307, false),
            wait((40, PERTURB), Some(CATALOGUE), Some(310), 311, false),
            wait((31, MAIN), Some(REBUILD), Some(320), 325, false),
            wait((13, FOREGROUND), None, Some(330), 331, false),
            // No wait start yet: not an episode.
            wait((14, FOREGROUND), Some(CATALOGUE), None, 340, false),
        ];
        for sample in &samples {
            waits.record(sample);
        }
        let totals = waits.totals();
        let expect = |direction: &str, episodes: u64, samples: u64, max: f64, total: f64| {
            assert_eq!(
                totals[direction],
                WaitTotals {
                    episodes,
                    samples,
                    max_wait_ms: max,
                    total_wait_ms: total
                },
                "{direction}"
            );
        };
        expect(
            "foreground_waited_on_background",
            3,
            5,
            9.0,
            9.0 + 4.0 + 2.0,
        );
        expect("background_waited_on_foreground", 1, 1, 11.0, 11.0);
        expect("foreground_waited_on_foreground", 1, 1, 3.0, 3.0);
        expect("background_waited_on_background", 0, 0, 0.0, 0.0);
        expect("background_lease_queue", 2, 2, 900.0, 1300.0);
        expect("other", 4, 4, 7.0, 7.0 + 1.0 + 5.0 + 1.0);
        assert_eq!(waits.without_wait_start, 1);
        assert_eq!(totals.len(), WAIT_DIRECTIONS.len());
        // Fourth check, R4-6: which background work was on the other side. The
        // rebuild writer is placed by the instant the wait began.
        let at = |ms: i64| chrono::DateTime::from_timestamp_millis(1_800_000_000_000 + ms).unwrap();
        let timeline = [
            (at(15), "feed_rebuild"),
            (at(45), "browse_publication"),
            (at(200), "search_reconcile"),
        ];
        assert_eq!(
            waits.by_background_work(&timeline),
            [
                // Backend 10 behind the catalogue writer, and backend 12 behind
                // a foreground AND the catalogue writer.
                ("catalogue_replay", (2, 0)),
                ("feed_rebuild", (0, 0)),
                // The rebuild writer waited from 50 ms on, backend 10 behind
                // it from 100 ms on: both during the Browse rebuild.
                ("browse_publication", (1, 1)),
                ("search_reconcile", (0, 0)),
                ("other", (0, 0)),
            ]
            .into_iter()
            .collect()
        );
        // The same waits against another timeline: the rebuild writer is in
        // its feed rebuild at 50 ms and in the search reconcile at 100 ms.
        let other_timeline = [(at(40), "feed_rebuild"), (at(90), "search_reconcile")];
        let moved = waits.by_background_work(&other_timeline);
        assert_eq!(
            (
                moved["feed_rebuild"],
                moved["search_reconcile"],
                moved["browse_publication"]
            ),
            ((0, 1), (1, 0), (0, 0))
        );
        // A rebuild wait from before the first mark cannot be placed.
        assert_eq!(waits.by_background_work(&[])["other"], (1, 1));
        assert_eq!(background_kind(CATALOGUE, at(0), &[]), "catalogue_replay");
        assert_eq!(background_kind(MAIN, at(999), &timeline), "other");
        assert_eq!(background_kind(REBUILD, at(14), &timeline), "other");
        assert_eq!(background_kind(REBUILD, at(15), &timeline), "feed_rebuild");
        assert_eq!(
            background_kind(REBUILD, at(199), &timeline),
            "browse_publication"
        );
        let json = waits.json(&timeline);
        assert_eq!(
            json["by_background_work"]["episodes"]["browse_publication"],
            json!({"foreground_waited_on_it":1,"it_waited_on_foreground":1})
        );
        assert_eq!(
            json["examples"][0]["background_work"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(json["foreground_waited_on_background"]["episodes"], 3);
        assert_eq!(json["foreground_waited_on_background"]["max_wait_ms"], 9.0);
        let examples = json["examples"].as_array().unwrap();
        // Waits between the sides come first; the lease queue is never listed.
        assert_eq!(examples.len(), 9);
        assert!(examples[..4].iter().all(|example| example["direction"]
            .as_str()
            .unwrap()
            .ends_with("ground")
            && example["direction"] != "foreground_waited_on_foreground"));
        assert!(examples
            .iter()
            .all(|example| example["direction"] != "background_lease_queue"));
        assert_eq!(side(FOREGROUND), Side::Foreground);
        assert_eq!(side(CATALOGUE), Side::Background);
        assert_eq!(side(REBUILD), Side::Background);
        for other in [MAIN, SNAPSHOT, OBSERVER, PERTURB, "", "psql"] {
            assert_eq!(side(other), Side::Other, "{other}");
        }
    }

    const LOG: &str = "\
2026-10-11 10:00:00.000 UTC [90] 65.1 40P01 ERROR:  deadlock detected
2026-10-11 10:00:00.100 UTC [100] 65.2 00000 LOG:  komika-bench-marker other begin
2026-10-11 10:00:00.150 UTC [150] 65.0 KB001 ERROR:  komika-bench-marker abc sqlstate probe
2026-10-11 10:00:00.150 UTC [150] 65.0 KB001 CONTEXT:  PL/pgSQL function inline_code_block line 1 at RAISE
2026-10-11 10:00:00.150 UTC [150] 65.0 KB001 STATEMENT:  DO $$ BEGIN RAISE EXCEPTION 'komika-bench-marker abc sqlstate probe' USING ERRCODE = 'KB001'; END $$
2026-10-11 10:00:00.200 UTC [100] 65.2 00000 LOG:  komika-bench-marker abc begin
2026-10-11 10:00:00.200 UTC [100] 65.2 00000 CONTEXT:  PL/pgSQL function inline_code_block line 1 at RAISE
2026-10-11 10:00:00.200 UTC [100] 65.2 00000 STATEMENT:  DO $$ BEGIN RAISE LOG 'komika-bench-marker abc begin'; END $$
2026-10-11 10:00:01.000 UTC [201] 65.3 00000 LOG:  process 201 still waiting for ShareLock on transaction 900 after 1000.123 ms
2026-10-11 10:00:01.000 UTC [201] 65.3 00000 DETAIL:  Process holding the lock: 301. Wait queue: 201.
2026-10-11 10:00:01.000 UTC [201] 65.3 00000 CONTEXT:  while updating tuple (0,1) in relation \"work\"
2026-10-11 10:00:01.000 UTC [201] 65.3 00000 STATEMENT:  UPDATE work SET description_override = $1 WHERE id = $2
2026-10-11 10:00:01.500 UTC [201] 65.3 00000 LOG:  process 201 acquired ShareLock on transaction 900 after 1500.250 ms
2026-10-11 10:00:02.000 UTC [302] 65.4 00000 LOG:  process 302 still waiting for ExclusiveLock on advisory lock [16384,1147899,1307981997,1] after 1000.050 ms
2026-10-11 10:00:02.000 UTC [302] 65.4 00000 DETAIL:  Process holding the lock: 301. Wait queue: 302.
2026-10-11 10:00:09.000 UTC [302] 65.4 00000 LOG:  process 302 acquired ExclusiveLock on advisory lock [16384,1147899,1307981997,1] after 8000.000 ms
2026-10-11 10:00:03.000 UTC [301] 65.5 00000 LOG:  process 301 still waiting for ShareLock on transaction 903 after 1000.200 ms
2026-10-11 10:00:03.000 UTC [301] 65.5 00000 DETAIL:  Processes holding the lock: 202, 201. Wait queue: 301.
2026-10-11 10:00:03.000 UTC [301] 65.5 00000 STATEMENT:  SELECT id FROM work WHERE id = $1 FOR NO KEY UPDATE
2026-10-11 10:00:04.000 UTC [204] 65.6 00000 LOG:  process 204 still waiting for ShareLock on transaction 901 after 1000.100 ms
2026-10-11 10:00:04.000 UTC [204] 65.6 00000 DETAIL:  Process holding the lock: 999. Wait queue: 204.
2026-10-11 10:00:04.500 UTC [201] 65.3 00000 LOG:  process 201 still waiting for ShareLock on transaction 902 after 1000.300 ms
2026-10-11 10:00:04.500 UTC [201] 65.3 00000 DETAIL:  Process holding the lock: 202. Wait queue: 201.
2026-10-11 10:00:05.000 UTC [202] 65.7 55P03 ERROR:  canceling statement due to lock timeout
2026-10-11 10:00:05.000 UTC [202] 65.7 55P03 STATEMENT:  UPDATE work SET year = 40001 WHERE id = 'deadlock detected'
2026-10-11 10:00:05.500 UTC [777] 65.a 57014 ERROR:  canceling autovacuum task
2026-10-11 10:00:05.500 UTC [777] 65.a 57014 CONTEXT:  while scanning relation \"public.work\"
2026-10-11 10:00:06.000 UTC [203] 65.8 40001 ERROR:  could not serialize access due to concurrent update
2026-10-11 10:00:06.000 UTC [203] 65.8 40001 STATEMENT:  UPDATE example SET value = 2
2026-10-11 10:00:07.000 UTC [100] 65.2 00000 LOG:  komika-bench-marker abc end
2026-10-11 10:00:08.000 UTC [205] 65.9 25P03 FATAL:  terminating connection due to idle-in-transaction timeout
";

    #[test]
    fn server_log_scan_reads_only_its_profile_and_attributes_lock_waits() {
        let sessions: HashMap<i32, String> = [
            (100, OBSERVER),
            (150, OBSERVER),
            (201, FOREGROUND),
            (202, FOREGROUND),
            (203, MAIN),
            (301, CATALOGUE),
            (302, REBUILD),
        ]
        .into_iter()
        .map(|(pid, name)| (pid, name.to_owned()))
        .collect();
        let ServerLog::Scanned(scan) =
            scan_server_log(LOG, "komika-bench-marker abc", &sessions, Some(150))
        else {
            panic!("the markers are in the text");
        };
        // Fourth check, R4-1: the probe line, written before the begin marker,
        // proves the prefix carries the SQLSTATE and the backend pid.
        assert!(scan.probe_found && scan.sqlstate_in_prefix && scan.pid_in_prefix);
        assert_eq!(scan.lines, 23);
        // Fourth check, R4-3: the cancelled autovacuum worker (pid 777) is no
        // session of the benchmark. Its 57014 is reported apart and not judged.
        assert_eq!(scan.other_backends["57014"], 1);
        assert_eq!(scan.other_backends.values().sum::<u64>(), 1);
        assert_eq!(scan.other_backend_examples.len(), 1);
        assert!(scan.other_backend_examples[0].contains("canceling autovacuum task"));
        // One per log entry, not per line; nothing from outside the markers, and
        // nothing from statement text.
        assert_eq!(
            scan.sqlstates,
            [
                ("25P03", 0),
                ("40001", 1),
                ("40P01", 0),
                ("55P03", 1),
                ("57014", 0)
            ]
            .into_iter()
            .map(|(code, count)| (code.to_owned(), count))
            .collect()
        );
        assert_eq!((scan.still_waiting, scan.acquired), (5, 2));
        assert_eq!(scan.lease_waits, 1);
        assert_eq!(scan.foreground_waited_on_background, 1);
        assert_eq!(scan.background_waited_on_foreground, 1);
        // An unknown holder, and foreground behind foreground.
        assert_eq!(scan.other_waits, 2);
        // The lease queue's eight seconds are not a lock wait between writers.
        assert_eq!(scan.longest_acquired_ms, 1500.25);
        assert!(scan.examples.iter().any(|line| line.contains("55P03")));
        // The profile fails on the lock timeout, and contention was exercised.
        let mut measured = passing();
        measured.foreground_waited_on_background = 0;
        measured.server_log = ServerLog::Scanned(scan.clone());
        let gate = evaluate_gate(&measured);
        assert_eq!(gate.reasons, ["server log has SQLSTATE 55P03"]);
        assert!(gate.contention_exercised);
        let json = ServerLog::Scanned(scan.clone()).json();
        assert_eq!(json["usable"], true);
        assert_eq!(json["sqlstate_in_log_line_prefix"], true);
        assert_eq!(json["pid_in_log_line_prefix"], true);
        assert_eq!(json["sqlstates"]["57014"], 0);
        assert_eq!(json["other_backends"]["sqlstates"]["57014"], 1);
        // A line can be left out only when its backend is known to be foreign.
        // Without a proven `[pid]` in the prefix (no probe pid, or another
        // backend's), every SQLSTATE line counts as the benchmark's.
        for unproven in [None, Some(151)] {
            let ServerLog::Scanned(unattributed) =
                scan_server_log(LOG, "komika-bench-marker abc", &sessions, unproven)
            else {
                panic!()
            };
            assert!(unattributed.probe_found && unattributed.sqlstate_in_prefix);
            assert!(!unattributed.pid_in_prefix);
            assert_eq!(unattributed.sqlstates["57014"], 1);
            assert_eq!(unattributed.other_backends["57014"], 0);
            let mut measured = passing();
            measured.server_log = ServerLog::Scanned(unattributed);
            assert_eq!(
                evaluate_gate(&measured).reasons,
                [
                    "server log has SQLSTATE 55P03",
                    "server log has SQLSTATE 57014"
                ]
            );
        }
        // Without the pid map nothing can be attributed, and nothing left out.
        let ServerLog::Scanned(blind) =
            scan_server_log(LOG, "komika-bench-marker abc", &HashMap::new(), Some(150))
        else {
            panic!()
        };
        assert_eq!(blind.sqlstates["57014"], 1);
        assert_eq!(
            (
                blind.foreground_waited_on_background,
                blind.background_waited_on_foreground,
                blind.other_waits,
                blind.lease_waits
            ),
            (0, 0, 4, 1)
        );
        // Another profile's part of the same file.
        let ServerLog::Scanned(wider) = scan_server_log(
            &LOG.replace("abc end", "other end"),
            "komika-bench-marker other",
            &sessions,
            Some(150),
        ) else {
            panic!()
        };
        assert_eq!((wider.lines, wider.sqlstates["55P03"]), (27, 1));
        // That profile's own probe is not in the file: nothing about the prefix
        // is proven, so nothing is left out and the gate refuses the log.
        assert!(!wider.probe_found && !wider.sqlstate_in_prefix && !wider.pid_in_prefix);
        assert_eq!(wider.sqlstates["57014"], 1);
        let mut measured = passing();
        measured.server_log = ServerLog::Scanned(wider);
        assert_eq!(
            evaluate_gate(&measured).reasons[0],
            "server log has no line for the SQLSTATE probe"
        );
        // A wrong file, or a profile whose end has not been written.
        assert!(matches!(
            scan_server_log(LOG, "komika-bench-marker missing", &sessions, Some(150)),
            ServerLog::Unusable(_)
        ));
        assert!(matches!(
            scan_server_log(
                &LOG.replace("abc end", "abc en"),
                "komika-bench-marker abc",
                &sessions,
                Some(150)
            ),
            ServerLog::Unusable(_)
        ));
        // The marker inside a STATEMENT line is not a marker.
        assert!(matches!(
            scan_server_log(
                "x STATEMENT:  komika-bench-marker abc begin\nx STATEMENT:  komika-bench-marker abc end\n",
                "komika-bench-marker abc",
                &sessions,
                Some(150)
            ),
            ServerLog::Unusable(_)
        ));
        // A prefix without %e: counted by message text, said so, and refused by
        // the gate, because the text follows lc_messages and a clean scan of
        // such a log proves nothing.
        let plain = "\
[9] ERROR:  komika-bench-marker p sqlstate probe
[1] LOG:  komika-bench-marker p begin
[2] ERROR:  deadlock detected
[2] DETAIL:  Process 2 waits for ShareLock on transaction 5; blocked by process 3.
[3] ERROR:  canceling statement due to statement timeout
[4] FATAL:  terminating connection due to idle-in-transaction timeout
[1] LOG:  komika-bench-marker p end
";
        let ServerLog::Scanned(plain) =
            scan_server_log(plain, "komika-bench-marker p", &sessions, None)
        else {
            panic!()
        };
        assert!(plain.probe_found && !plain.sqlstate_in_prefix);
        let mut measured = passing();
        measured.server_log = ServerLog::Scanned(plain.clone());
        assert_eq!(
            evaluate_gate(&measured).reasons[0],
            "server log line prefix has no SQLSTATE (%e)"
        );
        assert_eq!(
            (
                plain.sqlstates["40P01"],
                plain.sqlstates["57014"],
                plain.sqlstates["25P03"]
            ),
            (1, 1, 1)
        );
        assert_eq!(
            parse_lock_wait("process 7 still waiting for ShareLock on transaction 9 after 12.5 ms"),
            Some((7, false, "ShareLock on transaction 9", 12.5))
        );
        assert_eq!(
            parse_lock_wait("process 7 detected deadlock while waiting"),
            None
        );
        assert_eq!(
            lock_holders("Processes holding the lock: 4, 5. Wait queue: 6, 7."),
            [4, 5]
        );
        assert_eq!(ServerLog::NotProvided.json(), json!("not provided"));
        assert_eq!(
            prefix_pid("2026-10-11 10:00:00.000 UTC [4321] 65.1 55P03 "),
            Some(4321)
        );
        assert_eq!(prefix_pid("2026-10-11 10:00:00.000 UTC 65.1 55P03 "), None);
        assert_eq!(prefix_pid("[x] [] [12a] "), None);
    }

    #[test]
    fn publication_turn_skip_and_hold_fields_survive_capture_and_summary() {
        let capture = LogCapture::default();
        tracing::subscriber::with_default(capture.subscriber(), || {
            // Same field names and integer types as publish_snapshot's INFO.
            // Nonzero skips must survive, rather than becoming an assumed zero.
            tracing::info!(
                table = "browse_catalogue",
                changed = 9u64,
                turns = 4u64,
                busy_works_left_for_next_refresh = 7u64,
                longest_writer_hold_ms = 137u128,
                "catalogue snapshot published"
            );
            // An older/incomplete event must reveal missing metrics as null.
            tracing::info!(
                table = "tag",
                changed = 0u64,
                "catalogue snapshot published"
            );
            // Bad core and extra fields cannot manufacture complete evidence.
            tracing::info!(
                table = "",
                changed = "invalid",
                turns = "invalid",
                busy_works_left_for_next_refresh = 3u64,
                longest_writer_hold_ms = "invalid",
                "catalogue snapshot published"
            );
        });
        let events = capture.rebuild_events();
        assert_eq!(
            browse_publications(&events, events[0].at, events[2].at),
            (1, 9)
        );
        let logs = capture.take();
        assert_eq!(logs.published["browse_catalogue"], 9);
        let publications = logs.json()["snapshot_publications"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(publications.len(), 3);
        assert_eq!(
            publications[0],
            json!({
                "timestamp":events[0].at.to_rfc3339(),"table":"browse_catalogue","changed":9,
                "turns":4,"busy_works_left_for_next_refresh":7,"longest_writer_hold_ms":137,
                "fields_complete":true
            })
        );
        assert_eq!(publications[1]["fields_complete"], false);
        for field in [
            "turns",
            "busy_works_left_for_next_refresh",
            "longest_writer_hold_ms",
        ] {
            assert!(
                publications[1][field].is_null(),
                "missing {field} became a value"
            );
        }
        assert_eq!(publications[2]["fields_complete"], false);
        assert_eq!(publications[2]["busy_works_left_for_next_refresh"], 3);
        for field in ["table", "changed", "turns", "longest_writer_hold_ms"] {
            assert!(
                publications[2][field].is_null(),
                "invalid {field} became a value"
            );
        }
        let mut collected = collected();
        collected.captured = Some(logs);
        let summary = summarize(collected);
        assert_eq!(
            summary["log_events"]["snapshot_publications"],
            json!(publications)
        );
        assert_eq!(
            summary["log_events"]["rows_published_by_rebuild"]["browse_catalogue"],
            9
        );
        assert!(capture.take().snapshot_publications.is_empty());
    }

    #[test]
    fn log_capture_counts_errors_and_database_warnings_and_lists_every_warning() {
        let capture = LogCapture::default();
        tracing::subscriber::with_default(capture.subscriber(), || {
            // The known log-and-continue shapes of a failed background write.
            tracing::warn!(error = %sqlx::Error::PoolTimedOut, "tags: Suwayomi genre sync skipped");
            tracing::warn!(error = %anyhow::anyhow!("boom"), "browse_catalogue: refresh failed");
            tracing::warn!(error = %anyhow::anyhow!("page 3").context(db::RetryExhausted), "sync step skipped");
            // Not a database failure: listed, not gated.
            tracing::warn!(
                error = "connection refused",
                "upstream chapter list unavailable"
            );
            tracing::error!(error = %"internal", "internal error");
            tracing::info!(
                table = "browse_catalogue",
                changed = 3u64,
                "catalogue snapshot published"
            );
            tracing::info!(
                table = "browse_catalogue",
                changed = 4u64,
                "catalogue snapshot published"
            );
            tracing::info!(
                table = "tag",
                changed = 0u64,
                "catalogue snapshot published"
            );
            tracing::info!("database unavailable is only INFO here");
            tracing::debug!("database error at debug level is not captured");
            // Fourth check, R4-10: SQLx's slow-statement alert quotes the
            // statement. Its words do not make it a database failure.
            tracing::warn!(
                target: "sqlx::query",
                summary = "UPDATE work SET ...",
                db.statement = "UPDATE work SET year = 1 /* database transaction deadlock */",
                elapsed_secs = 1.5,
                "slow statement: execution time exceeded alert threshold"
            );
            tracing::info!(total = 9u64, "browse_catalogue: rebuilt browse catalogue");
        });
        // Nothing is captured once the subscriber is gone.
        tracing::error!("outside the capture");
        // The rebuild's own events, in order, each with the instant it arrived.
        let events = capture.rebuild_events();
        assert_eq!(
            events
                .iter()
                .map(|event| event.mark.clone())
                .collect::<Vec<_>>(),
            [
                RebuildMark::Published {
                    table: "browse_catalogue".into(),
                    changed: 3
                },
                RebuildMark::Published {
                    table: "browse_catalogue".into(),
                    changed: 4
                },
                RebuildMark::Published {
                    table: "tag".into(),
                    changed: 0
                },
                RebuildMark::BrowseRebuilt,
            ]
        );
        assert!(events.windows(2).all(|pair| pair[0].at <= pair[1].at));
        assert_eq!(
            browse_publications(&events, events[0].at, events[3].at),
            (2, 7)
        );
        let logs = capture.take();
        assert_eq!(logs.rebuild_events, events);
        assert!(capture.rebuild_events().is_empty());
        assert_eq!(
            (logs.errors, logs.warnings, logs.database_warnings),
            (1, 5, 3)
        );
        let slow = &logs.warn_events[4];
        assert_eq!(slow["target"], "sqlx::query");
        assert_eq!(slow["names_a_database_failure"], false);
        // The same words from any other target do name a database failure.
        let elsewhere = LogCapture::default();
        tracing::subscriber::with_default(elsewhere.subscriber(), || {
            tracing::warn!(
                db.statement = "UPDATE work SET year = 1 /* database transaction deadlock */",
                "slow statement: execution time exceeded alert threshold"
            );
        });
        assert_eq!(elsewhere.take().database_warnings, 1);
        let logs = CapturedLogs {
            warn_events: logs.warn_events[..4].to_vec(),
            warnings: 4,
            ..logs
        };
        assert_eq!(logs.warn_events.len(), 4);
        assert_eq!(logs.first_errors.len(), 1);
        assert!(logs.first_errors[0].contains("internal error"));
        assert_eq!(
            logs.published,
            [("browse_catalogue".to_owned(), 7), ("tag".to_owned(), 0)]
                .into_iter()
                .collect()
        );
        let json = logs.json();
        assert_eq!(json["warn_events_naming_a_database_failure"], 3);
        let listed = json["warnings"].as_array().unwrap();
        assert_eq!(listed[0]["names_a_database_failure"], true);
        assert!(listed[0]["fields"]["error"]
            .as_str()
            .unwrap()
            .contains("pool timed out"));
        assert_eq!(listed[3]["message"], "upstream chapter list unavailable");
        assert_eq!(listed[3]["names_a_database_failure"], false);
        let target = module_path!();
        assert_eq!(
            json["by_level_and_target"],
            json!([{"level":"ERROR","target":target,"count":1},{"level":"WARN","target":target,"count":4},
                {"level":"WARN","target":"sqlx::query","count":1}])
        );
        // `take` starts again from nothing.
        assert_eq!(capture.take().warnings, 0);
        // The listing is bounded; the counts the gate reads are not.
        tracing::subscriber::with_default(capture.subscriber(), || {
            for _ in 0..505 {
                tracing::warn!("work_fts: refresh failed");
            }
            for _ in 0..25 {
                tracing::error!("internal error");
            }
        });
        let many = capture.take();
        assert_eq!(
            (many.warnings, many.database_warnings, many.errors),
            (505, 505, 25)
        );
        assert_eq!(
            (many.warn_events.len(), many.warn_events_not_listed),
            (500, 5)
        );
        assert_eq!(many.first_errors.len(), 20);
    }

    #[test]
    fn output_names_every_key_that_differs_from_the_sqlite_file_and_carries_its_numbers() {
        let sqlite = sqlite_baseline();
        assert_eq!(sqlite["profile"], "after");
        // The real file of this repository: 41 writes, none failed, after the fix.
        assert_eq!(sqlite["after"]["writes"]["results"]["requests"], 41);
        assert_eq!(sqlite["after"]["writes"]["results"]["ok"], 41);
        assert_eq!(sqlite["after"]["application_lock_error_events"], 0);
        assert_eq!(sqlite["before"]["application_lock_error_events"], 35);
        assert_eq!(sqlite["workload"]["write_requests_per_second"], 2);
        let rule = WindowRule {
            duration: Duration::from_secs(45),
            cap: Duration::from_secs(600),
            min_cycles: 3,
        };
        let notes = comparison_notes(&rule);
        // Fourth check, R4-7: the window's numbers are the run's, not a fixed
        // "20 s", and the write latency is not called alike any more.
        let matching = notes["matching_profile"].as_str().unwrap();
        assert!(!matching.contains("20 s"), "{matching}");
        for part in ["at least 45 s", "3 complete rebuild cycles", "up to 600 s"] {
            assert!(matching.contains(part), "{part}: {matching}");
        }
        let latency = notes["keys"]["writes.results.p50..max,mean"]
            .as_str()
            .unwrap();
        assert!(
            latency.contains("NOT alike when a write stalls"),
            "{latency}"
        );
        assert!(latency.contains("writes.arrival_latency_ms"), "{latency}");
        assert!(!latency.contains("SQLite and PostgreSQL alike"));
        for key in [
            "application_lock_error_events",
            "longest_sampled_writer_hold_ms",
            "busy_probes",
            "background_elapsed_ms",
            "reads",
            "writes.loadtest_arguments",
            "writes.results.p50..max,mean",
            "writes.results.users",
        ] {
            assert!(
                notes["keys"][key]
                    .as_str()
                    .is_some_and(|note| note.contains("SQLite")),
                "{key}"
            );
        }
        // Every top-level key the SQLite `after` block has is either explained or
        // means the same (reads and writes are explained through their parts).
        for key in sqlite["after"].as_object().unwrap().keys() {
            assert!(
                notes["keys"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .any(|noted| noted == key || noted.starts_with(&format!("{key}."))),
                "{key}"
            );
        }
        // loadtest.mjs's percentile rule: a[min(n - 1, floor(p * n))].
        let samples = (1..=10).map(|ms| (f64::from(ms), ms != 3)).collect();
        let summary = latency_result(samples, 5, 2.0, 3);
        assert_eq!(
            (
                &summary["p50"],
                &summary["p90"],
                &summary["p99"],
                &summary["max"]
            ),
            (&json!(6.0), &json!(10.0), &json!(10.0), &json!(10.0))
        );
        assert_eq!(
            (&summary["requests"], &summary["ok"]),
            (&json!(10), &json!(9))
        );
        assert_eq!(
            (&summary["rps"], &summary["okRps"]),
            (&json!(5.0), &json!(4.5))
        );
        assert_eq!(latency_result(vec![], 5, 2.0, 3)["p95"], 0.0);
        assert_eq!(spread(vec![3.0, 1.0, 2.0])["max"], 3.0);
        // What the observer counts as a background write statement.
        for write in [
            "INSERT INTO public.browse_catalogue (work_id) SELECT $1",
            "  update work SET year = 1",
            "DELETE FROM public.tag WHERE id = $1",
            "WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x",
            "LOCK TABLE work IN SHARE MODE",
            // Fourth check, R4-4: what the merged rebuild runs on its writer.
            "SELECT id FROM work WHERE id = $1 FOR NO KEY UPDATE",
            "SELECT id FROM public.work WHERE id = ANY($1) ORDER BY id FOR NO KEY UPDATE SKIP LOCKED",
            "select id from source_series where id = $1 for update",
            "INSERT INTO public.feed_series_updates (work_id) SELECT $1 WHERE (NOT $2 OR EXISTS (SELECT 1)) ON CONFLICT(work_id) DO UPDATE SET title = excluded.title",
            "DELETE FROM public.browse_catalogue WHERE work_id = $1 AND browse_catalogue.title IS NOT DISTINCT FROM $2",
            "INSERT INTO work_search_alias (id,work_id,normalized_title,raw_title,kind) SELECT 'alias:' || a.id",
            "DELETE FROM work_search_alias s WHERE s.work_id = $1",
        ] {
            assert!(is_write_statement(write), "{write}");
        }
        for read in [
            "SELECT 1",
            "SELECT pg_try_advisory_lock($1, $2)",
            "SELECT pg_advisory_xact_lock_shared($1, $2)",
            "BEGIN",
            "COMMIT",
            "SELECT e.work_id FROM work_external_id e",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            // The old search reconcile: a function that no longer exists is
            // not a write by its name.
            "SELECT public.rebuild_work_search_alias()",
            // The search reconcile's read, and the publication's authority read.
            "WITH expected AS MATERIALIZED ( SELECT 'alias:' || a.id AS id, a.work_id FROM work_alias a",
            "SELECT w.id, md5(concat_ws('|', w.xmin::text)) FROM public.work w WHERE w.id = ANY($1)",
        ] {
            assert!(!is_write_statement(read), "{read}");
        }
        // Fourth check, R4-5: a statement of a transaction that was opened
        // before the window (the one a deterministic sample held) is not a
        // statement of the measurement, even when it starts inside the window.
        let at = |ms: i64| chrono::DateTime::from_timestamp_millis(1_800_000_000_000 + ms).unwrap();
        let activity = |xact_ms: i64, query_ms: i64, query: &str| BackgroundActivity {
            application: REBUILD.into(),
            xact_start: at(xact_ms),
            query_start: at(query_ms),
            query: query.into(),
        };
        let insert = "INSERT INTO public.browse_catalogue (work_id) SELECT $1";
        assert!(starts_inside_window(&activity(100, 105, insert), at(100)));
        assert!(!starts_inside_window(&activity(90, 105, insert), at(100)));
        assert!(!starts_inside_window(&activity(90, 95, insert), at(100)));
        assert!(!starts_inside_window(
            &activity(100, 105, "SELECT 1"),
            at(100)
        ));
        // The window's end is applied when the statements are counted.
        let mut observed = Observation::default();
        for (label, ms) in [
            (REBUILD, 105),
            (REBUILD, 300),
            (CATALOGUE, 200),
            (REBUILD, 301),
        ] {
            observed
                .statements
                .insert((label.to_owned(), at(ms), insert.to_owned()));
        }
        assert_eq!(observed.statements_inside(REBUILD, at(300)), 2);
        assert_eq!(observed.statements_inside(CATALOGUE, at(300)), 1);
        assert_eq!(observed.statements_inside(CATALOGUE, at(199)), 0);
    }

    fn at(ms: i64) -> Timestamp {
        chrono::DateTime::from_timestamp_millis(1_800_000_000_000 + ms).unwrap()
    }

    // Fourth check, R4-4: the window ends when its duration has passed AND the
    // cycles are there; the cap ends it otherwise, and that is a failure.
    #[test]
    fn window_ends_after_its_duration_and_its_cycles_or_at_the_cap() {
        let rule = WindowRule {
            duration: Duration::from_secs(20),
            cap: Duration::from_secs(900),
            min_cycles: 2,
        };
        let decide = |seconds: u64, cycles: u64| {
            window_decision(Duration::from_secs(seconds), &rule, cycles)
        };
        // The cycles alone do not end it, nor does the duration alone.
        assert_eq!(decide(19, 5), WindowEnd::Continue);
        assert_eq!(decide(20, 1), WindowEnd::Continue);
        assert_eq!(decide(899, 1), WindowEnd::Continue);
        assert_eq!(decide(20, 2), WindowEnd::Complete);
        assert_eq!(decide(431, 2), WindowEnd::Complete);
        assert_eq!(decide(900, 1), WindowEnd::Capped);
        assert_eq!(decide(5000, 0), WindowEnd::Capped);
        // Cycles that complete in the last instant still complete the window.
        assert_eq!(decide(900, 2), WindowEnd::Complete);
        // A cap below the duration cannot end the window before its duration.
        let short_cap = WindowRule {
            cap: Duration::from_secs(5),
            ..rule
        };
        let decide = |seconds: u64, cycles: u64| {
            window_decision(Duration::from_secs(seconds), &short_cap, cycles)
        };
        assert_eq!(decide(6, 0), WindowEnd::Continue);
        assert_eq!(decide(20, 0), WindowEnd::Capped);
        assert_eq!(decide(20, 2), WindowEnd::Complete);
    }

    #[test]
    fn contested_works_are_split_between_replay_and_republication() {
        let contested: Vec<Contested> = ["w_a", "w_b", "w_c", "w_d", "w_e"]
            .into_iter()
            .map(|work| Contested {
                work: work.into(),
                chapter: format!("{work}-chapter"),
            })
            .collect();
        let (replayed, republished) = split_contested(&contested);
        assert_eq!(replayed, ["w_a", "w_c", "w_e"]);
        assert_eq!(republished, ["w_b", "w_d"]);
        assert!(roles_are_disjoint(&replayed, &republished));
        // One contested work is replayed; nothing of it is republished.
        let (replayed, republished) = split_contested(&contested[..1]);
        assert_eq!((replayed.len(), republished.len()), (1, 0));
        // With the default four works and two admins, admin 0 edits a replayed
        // work and admin 1 a republished one; the users write all four.
        let plan = plan_writes(1000, 20, admin_accounts(4), 4, true);
        let edited: BTreeSet<usize> = plan
            .iter()
            .filter(|write| write.kind == Kind::WorkEdit)
            .map(|write| write.target)
            .collect();
        assert_eq!(edited, [0, 1].into());
        let editors: BTreeSet<usize> = plan
            .iter()
            .filter(|write| write.kind == Kind::WorkEdit)
            .map(|write| write.account)
            .collect();
        assert_eq!(editors.len(), MAINTENANCE_PERMITS);
        let written: BTreeSet<usize> = plan.iter().map(|write| write.target).collect();
        assert_eq!(written, [0, 1, 2, 3].into());
        // The endless schedule and its finite prefix are the same schedule.
        assert_eq!(
            WritePlan::new(20, 2, 4, true)
                .take(1000)
                .collect::<Vec<_>>(),
            plan
        );
    }

    #[test]
    fn timing_rejects_swallowed_downstream_errors_and_missing_server_evidence() {
        let mut counters = db::DatabaseCounters::default();
        let logs = CapturedLogs::default();
        assert!(timing_failure_reasons(&counters, &logs, &scanned(|_| {})).is_empty());
        counters.statement_timeouts = 1;
        assert_eq!(
            timing_failure_reasons(&counters, &logs, &scanned(|_| {})),
            ["database error observed during timing cycle"]
        );
        assert_eq!(
            timing_failure_reasons(
                &db::DatabaseCounters::default(),
                &CapturedLogs {
                    database_warnings: 1,
                    ..Default::default()
                },
                &scanned(|_| {})
            ),
            ["application database ERROR/WARN during timing cycle"]
        );
        assert_eq!(
            timing_failure_reasons(
                &db::DatabaseCounters::default(),
                &logs,
                &scanned(|scan| {
                    scan.sqlstates.insert("57014".into(), 1);
                })
            ),
            ["server ERROR/FATAL/PANIC during timing cycle"]
        );
        assert_eq!(
            timing_failure_reasons(
                &db::DatabaseCounters::default(),
                &logs,
                &ServerLog::NotProvided
            ),
            ["timing server log evidence unavailable"]
        );
    }

    #[test]
    fn baseline_failure_skips_heavy_except_no_contention_alone() {
        assert!(baseline_allows_heavy(&json!({"passed":true,"reasons":[]})));
        assert!(baseline_allows_heavy(
            &json!({"passed":false,"reasons":[NO_CONTENTION]})
        ));
        for reasons in [
            json!([]),
            json!(["background worker error"]),
            json!([NO_CONTENTION, "failed writes"]),
            json!([NO_CONTENTION, NO_CONTENTION]),
        ] {
            assert!(!baseline_allows_heavy(
                &json!({"passed":false,"reasons":reasons})
            ));
        }
        for name in [
            "komika_real",
            "postgres",
            "komika_test_template",
            "komika_app_test_x",
            "komika_phase4_measure_",
            "komika_p4_measure_",
        ] {
            assert!(!timing_scratch_database(name), "{name}");
        }
        for name in [
            "komika_p4_measure_170355",
            "komika_phase4_measure_new",
            "komika_phase4_bench_new",
        ] {
            assert!(timing_scratch_database(name), "{name}");
        }
    }

    #[tokio::test]
    async fn window_notices_dead_workers_and_stops_both_drivers() {
        let dir =
            std::env::temp_dir().join(format!("komika-bench-window-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let stop_file = dir.join("read.stop");
        let stop_sending = Arc::new(AtomicBool::new(false));
        let ended = Arc::new(std::sync::OnceLock::new());
        let (catalogue_sender, catalogue) = tokio::sync::watch::channel(false);
        let (rebuild_sender, rebuild) = tokio::sync::watch::channel(false);
        let control = WindowControl {
            rule: WindowRule {
                duration: Duration::from_secs(20),
                cap: Duration::from_secs(900),
                min_cycles: 2,
            },
            window_start: chrono::Utc::now(),
            cycles: Arc::new(Mutex::new(Vec::new())),
            capture: None,
            stop_sending: stop_sending.clone(),
            window_end: ended.clone(),
            stop_file: stop_file.clone(),
            workers_finished: [catalogue, rebuild],
        };
        let window = tokio::spawn(run_window(control, tokio::time::Instant::now()));
        // Channel closure is how a panicked or cancelled task is detected.
        drop(rebuild_sender);
        let (at, capped, worker_ended) = tokio::time::timeout(Duration::from_secs(2), window)
            .await
            .unwrap()
            .unwrap();
        assert!(worker_ended);
        assert!(!capped);
        assert_eq!(ended.get(), Some(&at));
        assert!(stop_sending.load(Ordering::Relaxed));
        assert_eq!(std::fs::read(stop_file).unwrap(), b"window ended\n");
        drop(catalogue_sender);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn window_completion_keeps_healthy_workers_separate_from_failure() {
        let dir =
            std::env::temp_dir().join(format!("komika-bench-window-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let (catalogue_sender, catalogue) = tokio::sync::watch::channel(false);
        let (rebuild_sender, rebuild) = tokio::sync::watch::channel(false);
        let result = run_window(
            WindowControl {
                rule: WindowRule {
                    duration: Duration::ZERO,
                    cap: Duration::from_secs(1),
                    min_cycles: 2,
                },
                window_start: chrono::Utc::now(),
                cycles: Arc::new(Mutex::new(Vec::new())),
                capture: None,
                stop_sending: Arc::new(AtomicBool::new(false)),
                window_end: Arc::new(std::sync::OnceLock::new()),
                stop_file: dir.join("stop"),
                workers_finished: [catalogue, rebuild],
            },
            tokio::time::Instant::now(),
        )
        .await;
        assert_eq!((result.1, result.2), (false, false));
        drop((catalogue_sender, rebuild_sender));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn zero_row_publications_do_not_count_as_rebuild_cycles() {
        let mut c = collected();
        for event in &mut c.captured.as_mut().unwrap().rebuild_events {
            if event.at >= at(30_000) && event.at <= at(55_000) {
                if let RebuildMark::Published { changed, .. } = &mut event.mark {
                    *changed = 0;
                }
            }
        }
        let measured = measure(&c);
        assert_eq!(measured.rebuild_cycles, 1);
        assert_eq!(measured.browse_rows_published, 198);
        assert_eq!(
            evaluate_gate(&measured).reasons,
            ["too few complete rebuild cycles with a Browse publication inside the window"]
        );
        let output = summarize(c);
        assert_eq!(output["overlap"]["rebuild"]["cycles"][2]["counted"], false);
    }

    #[test]
    fn server_log_reports_other_severe_errors_by_backend_without_counting_continuations() {
        let text = "2026-10-11 [1] KB001 ERROR: komika-bench-marker other sqlstate probe\n\
2026-10-11 [1] 00000 LOG: komika-bench-marker other begin\n\
2026-10-11 [2] 23505 ERROR: duplicate key\n\
2026-10-11 [2] 23505 DETAIL: ERROR: continued message\n\
2026-10-11 [2] 23505 STATEMENT: SELECT 1\n\
2026-10-11 [3] 57P01 FATAL: terminating connection\n\
2026-10-11 [9] XX000 PANIC: another backend failure\n\
2026-10-11 [2] 40001 ERROR: could not serialize access\n\
2026-10-11 [2] 00000 LOG: unexpected EOF on client connection with an open transaction\n\
2026-10-11 [1] 00000 LOG: komika-bench-marker other end";
        let sessions = [
            (1, OBSERVER.into()),
            (2, SNAPSHOT.into()),
            (3, PERTURB.into()),
        ]
        .into();
        let ServerLog::Scanned(scan) =
            scan_server_log(text, "komika-bench-marker other", &sessions, Some(1))
        else {
            panic!("scan");
        };
        assert_eq!(scan.other_error_lines, 2);
        assert_eq!(scan.other_error_examples.len(), 2);
        assert_eq!(scan.foreign_other_error_lines, 1);
        assert_eq!(scan.sqlstates["40001"], 1);
        let mut measured = passing();
        measured.server_log = ServerLog::Scanned(scan);
        assert_eq!(
            evaluate_gate(&measured).reasons,
            ["server log has other ERROR/FATAL/PANIC lines"]
        );
    }

    /// What a passing heavy profile collects, with a window from 0 to 60 s.
    fn collected() -> Collected {
        let cycle = |began: i64, ended: i64| RebuildCycle {
            began: at(began),
            ended: at(ended),
            steps: [
                (at(began), at(began + 10)),
                (at(began + 10), at(began + 20)),
                (at(began + 20), at(began + 30)),
                (at(began + 30), at(ended)),
            ],
        };
        let cycles = vec![
            // Began before the window.
            cycle(-2000, 9000),
            cycle(9000, 30_000),
            cycle(30_000, 55_000),
            // Ended after the window.
            cycle(55_000, 75_000),
        ];
        let published = |ms: i64, table: &str, changed: u64| RebuildEvent {
            at: at(ms),
            mark: RebuildMark::Published {
                table: table.into(),
                changed,
            },
        };
        let rebuild_events = vec![
            published(3000, "browse_catalogue", 66),
            published(12_000, "feed_series_updates", 0),
            published(15_000, "browse_catalogue", 66),
            RebuildEvent {
                at: at(16_000),
                mark: RebuildMark::BrowseRebuilt,
            },
            published(22_000, "browse_catalogue", 66),
            published(27_000, "browse_catalogue", 66),
            published(40_000, "browse_catalogue", 0),
            published(47_000, "browse_catalogue", 5),
            published(58_000, "browse_catalogue", 66),
        ];
        let perturbation = |ms: i64, changed: u64| Perturbation {
            at: at(ms),
            touched: 66,
            changed,
        };
        let statistics = |deadlocks: i64| {
            json!({"settled":true,"samples":3,"settle_wait_ms":11000,"deadlocks":deadlocks,
                "xact_rollback":7,"sessions_killed":1,"sessions_fatal":2,"sessions_abandoned":0,
                "conflicts":0,"temp_files":2})
        };
        let identity = json!({"database":"scratch","postmaster_started_at":"2026-10-11T00:00:00+00:00","statistics_reset_at":null});
        let writes = WriteResults {
            samples: vec![(5.0, true); 8],
            queue_delays: vec![1.0; 8],
            arrival_latencies: vec![6.0; 8],
            verified: KINDS.into_iter().map(|kind| (kind, 2)).collect(),
            sent: KINDS.into_iter().map(|kind| (kind, 2)).collect(),
            catalogue_overlap: 3,
            rebuild_overlap: 1,
            wall_seconds: 60.0,
            ..Default::default()
        };
        let mut observed = Observation {
            iterations: 100,
            ..Default::default()
        };
        let statement = "INSERT INTO public.browse_catalogue".to_owned();
        for (label, ms) in [(CATALOGUE, 1000), (REBUILD, 2000), (REBUILD, 61_000)] {
            observed
                .statements
                .insert((label.to_owned(), at(ms), statement.clone()));
        }
        observed.waits.record(&wait(
            (10, FOREGROUND),
            Some(CATALOGUE),
            Some(5000),
            5002,
            false,
        ));
        observed.waits.record(&wait(
            (20, REBUILD),
            Some(FOREGROUND),
            Some(20_500),
            20_600,
            false,
        ));
        Collected {
            mixed: true,
            rate: 50,
            accounts: 22,
            window: Window {
                rule: WindowRule {
                    duration: Duration::from_secs(20),
                    cap: Duration::from_secs(900),
                    min_cycles: 2,
                },
                started: at(0),
                ended: at(60_000),
                capped: false,
            },
            writes,
            read: json!({"args":{"rate":10},"results":[{"requests":600,"ok":600}]}),
            read_process_success: true,
            observed,
            census: (1200, 0, 31),
            catalogue: BackgroundRun {
                began: at(-5000),
                ended: at(60_010),
                // Began before the window; inside it; ended after it.
                units: vec![(at(-10), at(5)), (at(10), at(20)), (at(59_990), at(60_005))],
                perturbations: Vec::new(),
                step_marks: Vec::new(),
                cycles: Vec::new(),
                error: None,
            },
            rebuild: BackgroundRun {
                began: at(-3000),
                ended: at(75_000),
                units: cycles
                    .iter()
                    .map(|cycle| (cycle.began, cycle.ended))
                    .collect(),
                perturbations: vec![
                    perturbation(-2100, 66),
                    perturbation(9100, 64),
                    perturbation(14_000, 64),
                    perturbation(59_000, 66),
                    perturbation(61_000, 66),
                ],
                step_marks: cycles
                    .iter()
                    .flat_map(|cycle| REBUILD_STEPS.into_iter().zip(cycle.steps))
                    .map(|(step, (began, _))| (began, step))
                    .collect(),
                cycles,
                error: None,
            },
            proofs: [
                json!({"kind":CATALOGUE,"proven":true}),
                json!({"kind":REBUILD,"proven":true}),
            ],
            probe: (Duration::from_millis(12), 0, 4000),
            identity: [identity.clone(), identity],
            statistics: [statistics(4), statistics(4)],
            counters: db::DatabaseCounters {
                serialization_failures: 2,
                retries: 3,
                transaction_restarts: 1,
                ..Default::default()
            },
            application_lock_errors: 0,
            captured: Some(CapturedLogs {
                published: [("browse_catalogue".to_owned(), 335)].into(),
                rebuild_events,
                ..Default::default()
            }),
            server_log: scanned(|_| {}),
            marker: "komika-bench-marker wiring".into(),
            marker_written: true,
            foreground_labelled: [8, 8],
            foreground_connections: 8,
            background_ended_early: false,
            foreign_sessions: Vec::new(),
            contested_works: ["w_a", "w_b", "w_c", "w_d"].map(String::from).to_vec(),
            replayed_works: ["w_a", "w_c"].map(String::from).to_vec(),
            perturbed_works: 66,
            min_catalogue_units: 1,
        }
    }

    // Fourth check, R4-9: `profile()` had no test. Its judging half is now
    // `measure` and `summarize`, pure functions of what it collected. This
    // reads every measured number from a known collection, then changes one
    // collected thing at a time and expects exactly the reason it stands for.
    #[test]
    fn collected_measurements_reach_the_gate_and_the_output_from_the_right_place() {
        let measured = measure(&collected());
        assert_eq!(evaluate_gate(&measured).reasons, Vec::<String>::new());
        assert!(measured.mixed);
        assert_eq!((measured.write_requests, measured.write_ok), (8, 8));
        assert_eq!(measured.verified_effects, [2, 2, 2, 2]);
        assert_eq!((measured.read_requests, measured.read_ok), (600, 600));
        assert_eq!((measured.proven_catalogue, measured.proven_rebuild), (1, 1));
        // Only what lies wholly inside the window: one catalogue unit of
        // three, two rebuild cycles of four, with the rows their own Browse
        // publications changed (66 + 66 + 66, and 0 + 5).
        assert_eq!(measured.catalogue_units, 1);
        assert_eq!(
            (measured.rebuild_cycles, measured.min_rebuild_cycles),
            (2, 2)
        );
        assert_eq!(measured.browse_rows_published, 203);
        // Three of five perturbations were applied inside the window, and a
        // pinned year is touched, not changed: 64 + 64 + 66.
        assert_eq!(measured.perturbed_rows, 194);
        // The rebuild statement that started after the window end is left out.
        assert_eq!(
            (measured.catalogue_statements, measured.rebuild_statements),
            (1, 1)
        );
        assert_eq!(
            (
                measured.foreground_waited_on_background,
                measured.background_waited_on_foreground
            ),
            (1, 1)
        );
        assert_eq!(
            (measured.observer_iterations, measured.instrument_errors),
            (100, 0)
        );
        // Differences, not totals: the server already had 4 deadlocks, one
        // killed session and two fatal ones before the profile.
        assert_eq!(
            (
                measured.server_deadlocks,
                measured.sessions_killed,
                measured.sessions_fatal
            ),
            (0, 0, 0)
        );
        assert_eq!(measured.counters.retries, 3);
        assert!(measured.statistics_valid && measured.log_capture_installed);

        type Change = fn(&mut Collected);
        let cases: &[(&[&str], Change)] = &[
            (&["failed reads"], |c| {
                c.read["results"][0]["ok"] = json!(599)
            }),
            (&["read driver failed"], |c| c.read_process_success = false),
            (&["no read requests"], |c| c.read = Value::Null),
            (&["failed writes"], |c| c.writes.samples[3].1 = false),
            (&["wrong effects"], |c| c.writes.wrong_effects = 1),
            (&["a mutation kind has no verified effect"], |c| {
                c.writes.verified.remove(&Kind::WorkEdit);
            }),
            (&["application lock errors"], |c| {
                c.application_lock_errors = 1
            }),
            (&["lock timeouts (55P03)"], |c| c.counters.lock_timeouts = 1),
            (&["foreground probe failures"], |c| c.probe.1 = 1),
            (&["background worker error"], |c| {
                c.catalogue.error = Some("replay failed".into())
            }),
            (&["background worker error"], |c| {
                c.rebuild.error = Some("rebuild failed".into())
            }),
            (&["no proven catalogue overlap"], |c| {
                c.proofs[0]["proven"] = json!(false)
            }),
            (&["no proven rebuild overlap"], |c| {
                c.proofs[1]["proven"] = json!(false)
            }),
            (&["too few catalogue units inside the window"], |c| c.min_catalogue_units = 2),
            (&["too few catalogue units inside the window"], |c| {
                c.catalogue.units.remove(1);
            }),
            (
                &["the window reached its hard cap (KOMIKA_PG_BENCH_MAX_DURATION_SECS) before the rebuild cycles were complete"],
                |c| c.window.capped = true,
            ),
            // One cycle less: the third one's last instant is outside.
            (
                &["too few complete rebuild cycles with a Browse publication inside the window"],
                |c| c.window.ended = at(54_999),
            ),
            (
                &["too few complete rebuild cycles with a Browse publication inside the window"],
                |c| c.window.rule.min_cycles = 3,
            ),
            // A cycle nobody saw publish Browse is not a counted cycle.
            (
                &["too few complete rebuild cycles with a Browse publication inside the window"],
                |c| {
                    c.captured
                        .as_mut()
                        .unwrap()
                        .rebuild_events
                        .retain(|event| event.at != at(40_000) && event.at != at(47_000))
                },
            ),
            // The publications of the counted cycles changed nothing; the ones
            // of the two cycles that cross the window's edges do not count.
            (
                &["too few complete rebuild cycles with a Browse publication inside the window", "no Browse publication changed a row inside the window"],
                |c| {
                    for event in &mut c.captured.as_mut().unwrap().rebuild_events {
                        if let RebuildMark::Published { changed, .. } = &mut event.mark {
                            if event.at > at(9000) && event.at < at(55_000) {
                                *changed = 0;
                            }
                        }
                    }
                },
            ),
            // Only perturbations before and after the window changed a row.
            (
                &["the rebuild perturbation changed no row inside the window"],
                |c| {
                    for perturbation in &mut c.rebuild.perturbations[1..4] {
                        perturbation.changed = 0;
                    }
                },
            ),
            (
                &["no catalogue write statement observed inside the window"],
                |c| c.observed.statements.retain(|(label, _, _)| label != CATALOGUE),
            ),
            // The only rebuild statement left started after the window end.
            (
                &["no rebuild write statement observed inside the window"],
                |c| c.observed.statements.retain(|(_, started, _)| *started != at(2000)),
            ),
            (
                &[
                    "too few complete rebuild cycles with a Browse publication inside the window",
                    "no Browse publication changed a row inside the window",
                    "log capture not installed",
                ],
                |c| c.captured = None,
            ),
            (&["ERROR log events"], |c| {
                c.captured.as_mut().unwrap().errors = 1
            }),
            (&["WARN log events naming a database failure"], |c| {
                c.captured.as_mut().unwrap().database_warnings = 1
            }),
            (&["lock-wait observer failed"], |c| c.observed.errors = 1),
            (&["an instrument query failed"], |c| {
                c.writes.instrument_errors = 1
            }),
            (&["foreground session labels lost or incomplete"], |c| c.foreground_labelled[1] = 7),
            (&["foreground session labels lost or incomplete"], |c| c.foreground_labelled[0] = 0),
            (&["background worker ended before the window closed"], |c| c.background_ended_early = true),
            (&["background worker ended before the window closed"], |c| c.rebuild.ended = at(59_999)),
            (&["an instrument query failed"], |c| c.census.1 = 1),
            (&["an instrument query failed"], |c| c.census = (0, 0, 0)),
            (
                &["server statistics not settled or database identity changed"],
                |c| c.identity[1]["postmaster_started_at"] = json!("later"),
            ),
            (
                &["server statistics not settled or database identity changed"],
                |c| c.statistics[0]["settled"] = json!(false),
            ),
            (&["server deadlocks"], |c| {
                c.statistics[1]["deadlocks"] = json!(5)
            }),
            (&["server sessions killed"], |c| {
                c.statistics[1]["sessions_killed"] = json!(2)
            }),
            (&["server sessions ended by a fatal error"], |c| {
                c.statistics[1]["sessions_fatal"] = json!(3)
            }),
            (&["another client session in the database"], |c| {
                c.foreign_sessions.push(json!({"pid":1}))
            }),
            (&["server log not provided"], |c| {
                c.server_log = ServerLog::NotProvided
            }),
            (&[NO_CONTENTION], |c| c.observed.waits = LockWaits::default()),
        ];
        for (reasons, change) in cases {
            let mut changed = collected();
            change(&mut changed);
            assert_eq!(evaluate_gate(&measure(&changed)).reasons, *reasons);
        }

        // The output block: the verdict and where each number is shown.
        let output = summarize(collected());
        assert_eq!(output["passed"], true, "{}", output["reason"]);
        assert_eq!(output["reasons"], json!([]));
        assert_eq!(output["contention_exercised"], true);
        let window = &output["window"];
        assert_eq!(window["seconds"], 60.0);
        assert_eq!(
            (
                &window["configured_duration_seconds"],
                &window["min_rebuild_cycles"],
                &window["max_duration_seconds"]
            ),
            (&json!(20), &json!(2), &json!(900))
        );
        assert_eq!(window["capped"], false);
        assert_eq!(window["rebuild_cycles_counted"], 2);
        let rebuild = &output["overlap"]["rebuild"];
        let cycles = rebuild["cycles"].as_array().unwrap();
        assert_eq!(
            cycles
                .iter()
                .map(|cycle| (
                    cycle["duration_ms"].as_i64().unwrap(),
                    cycle["complete_inside_window"].as_bool().unwrap(),
                    cycle["browse_publications"].as_u64().unwrap(),
                    cycle["browse_rows_published"].as_u64().unwrap(),
                    cycle["counted"].as_bool().unwrap()
                ))
                .collect::<Vec<_>>(),
            [
                (11_000, false, 1, 66, false),
                (21_000, true, 3, 198, true),
                (25_000, true, 2, 5, true),
                (20_000, false, 1, 66, false)
            ]
        );
        assert_eq!(
            cycles[1]["steps_ms"],
            json!({"refresh_feed_updates":10,"refresh_feed_series_updates":10,
                "refresh_browse_catalogue":10,"refresh_work_fts":20_970})
        );
        assert_eq!(rebuild["cycles_counted"], 2);
        assert_eq!(rebuild["browse_rows_published_by_counted_cycles"], 203);
        assert_eq!(rebuild["completed_runs"], 4);
        assert_eq!(rebuild["units_inside_window"], 2);
        assert_eq!(rebuild["observed_statements_inside_window"], 1);
        assert_eq!(rebuild["user_writes_started_with_open_transaction"], 1);
        let perturbation = &rebuild["perturbation"];
        assert_eq!(
            (
                &perturbation["works"],
                &perturbation["applied"],
                &perturbation["applied_inside_window"],
                &perturbation["rows_touched_inside_window"],
                &perturbation["rows_changed_inside_window"]
            ),
            (&json!(66), &json!(5), &json!(3), &json!(198), &json!(194))
        );
        let catalogue = &output["overlap"]["catalogue"];
        assert_eq!(
            (
                &catalogue["completed_runs"],
                &catalogue["units_inside_window"],
                &catalogue["observed_statements_inside_window"],
                &catalogue["user_writes_started_with_open_transaction"]
            ),
            (&json!(3), &json!(1), &json!(1), &json!(3))
        );
        assert_eq!(output["overlap"]["statements"].as_array().unwrap().len(), 2);
        assert_eq!(
            output["overlap"]["contested_works"],
            json!(["w_a", "w_b", "w_c", "w_d"])
        );
        assert_eq!(output["overlap"]["replayed_works"], json!(["w_a", "w_c"]));
        // The rebuild writer waited from 20.5 s on: after the Browse rebuild of
        // that cycle reported itself done at 16 s, so in the search reconcile.
        let met = &output["lock_waits"]["by_background_work"]["episodes"];
        assert_eq!(
            met["catalogue_replay"],
            json!({"foreground_waited_on_it":1,"it_waited_on_foreground":0})
        );
        assert_eq!(
            met["search_reconcile"],
            json!({"foreground_waited_on_it":0,"it_waited_on_foreground":1})
        );
        assert_eq!(met["browse_publication"]["foreground_waited_on_it"], 0);
        assert_eq!(
            output["lock_waits"]["examples"].as_array().unwrap().len(),
            2
        );
        // The keys the orchestrator reads to accept a run.
        assert_eq!(output["server_log"]["usable"], true);
        assert_eq!(output["server_log"]["sqlstate_in_log_line_prefix"], true);
        assert_eq!(
            output["log_events"]["rows_published_by_rebuild"]["browse_catalogue"],
            335
        );
        assert_eq!(output["instruments"]["foreground_sessions_labelled"], 8);
        assert_eq!(
            output["instruments"]["foreground_sessions_labelled_at_end"],
            8
        );
        assert_eq!(output["instruments"]["census_sessions_seen"], 31);
        let writes = &output["writes"];
        assert_eq!(
            (
                &writes["results"]["requests"],
                &writes["results"]["ok"],
                &writes["results"]["httpErrors"],
                &writes["results"]["users"]
            ),
            (&json!(8), &json!(8), &json!(0), &json!(22))
        );
        assert_eq!(writes["arrival_latency_ms"]["max"], 6.0);
        assert_eq!(writes["queue_delay_ms"]["max"], 1.0);
        assert_eq!(writes["loadtest_arguments"]["duration"], 60.0);
        assert_eq!(writes["sent_by_kind"]["updateSeriesMetadata"], 2);
        assert_eq!(output["verified_effects"]["setProgress"], 2);
        assert_eq!(output["reads"]["results"]["requests"], 600);
        assert_eq!(output["reads"]["loadtest_arguments"]["rate"], 10);
        assert_eq!(output["reads"]["process_success"], true);
        assert_eq!(output["server_statistics"]["delta"]["deadlocks"], 0);
        assert_eq!(output["server_statistics"]["delta"]["temp_files"], 0);
        assert_eq!(output["server_statistics"]["valid"], true);
        assert_eq!(output["database_deadlocks"], 0);
        // From the window start until the rebuild worker finished its last cycle.
        assert_eq!(output["background_elapsed_ms"], 75_000);
        assert_eq!(output["background_runs"], 7);
        assert_eq!(
            (&output["busy_probes"], &output["foreground_probes"]),
            (&json!(0), &json!(4000))
        );
        assert_eq!(output["longest_foreground_write_wait_ms"], 12.0);
        assert_eq!(
            output["retries"]["by_kind"],
            json!({"restart_transaction":1,"serialization_failure_or_deadlock":2})
        );
        assert_eq!(output["proven_overlaps"]["samples"][1]["kind"], REBUILD);
        assert_eq!(
            output["server_log_marker"]["text"],
            "komika-bench-marker wiring"
        );
        // A failed profile keeps its measurements and says why.
        let mut failed = collected();
        failed.window.capped = true;
        failed.server_log = ServerLog::NotProvided;
        let output = summarize(failed);
        assert_eq!(output["passed"], false);
        assert_eq!(output["server_log"], "not provided");
        assert_eq!(output["window"]["capped"], true);
        assert_eq!(output["reasons"].as_array().unwrap().len(), 2);
        assert!(output["reason"]
            .as_str()
            .unwrap()
            .ends_with("; server log not provided"));
        assert_eq!(output["window"]["rebuild_cycles_counted"], 2);
    }
}
