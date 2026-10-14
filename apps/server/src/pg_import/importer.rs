use anyhow::{ensure, Result};
use serde::Serialize;
use sqlx::{Connection, PgConnection};
use std::{collections::BTreeMap, path::Path, time::Instant};

use super::{
    copy::{self, TableReport},
    plan::{quote, tables},
    source::{self, Precheck},
};

pub(crate) static POSTGRES: sqlx::migrate::Migrator = sqlx::migrate!("./migrations_pg");
const STATE_DDL:&str="CREATE TABLE public.komika_import_state (singleton boolean PRIMARY KEY CHECK (singleton), source_identity text NOT NULL, completed boolean NOT NULL DEFAULT false, schema_hashes jsonb NOT NULL); CREATE TABLE public.komika_import_table (table_name text PRIMARY KEY, status text NOT NULL DEFAULT 'done' CHECK (status = 'done'), report jsonb NOT NULL)";

#[derive(Serialize)]
pub(super) struct Report {
    pub precheck: Precheck,
    pub tables: Vec<TableReport>,
    pub rows: u64,
    pub bytes_sent: u64,
    pub seconds: f64,
    pub peak_memory_bytes: Option<u64>,
    pub complete: bool,
}

/// Read-only marker check, suitable for the Phase 3 startup guard. Missing state
/// is false; a marker alone is never accepted as data-verification evidence.
pub async fn import_complete(conn: &mut PgConnection) -> Result<bool> {
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public.komika_import_state') IS NOT NULL")
            .fetch_one(&mut *conn)
            .await?;
    if !exists {
        return Ok(false);
    }
    Ok(
        sqlx::query_scalar("SELECT completed FROM public.komika_import_state WHERE singleton=true")
            .fetch_optional(conn)
            .await?
            .unwrap_or(false),
    )
}

pub(super) async fn run(path: &Path, url: &str, stop_after: Option<usize>) -> Result<Report> {
    let mut target = super::connect(url).await?;
    super::target_definition(&mut target).await?;
    // Serialize cooperating imports for the entire run, across table commits.
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(192837465, 180002)")
        .fetch_one(&mut target)
        .await?;
    ensure!(locked, "another importer owns the target");
    let report = run_with_target(path, &mut target, stop_after).await;
    // Give the lock back explicitly, on failure too: PostgreSQL releases a
    // session's locks only some time after the client hangs up, and a retry
    // that follows at once must not find "another importer".
    let _ = sqlx::query("SELECT pg_advisory_unlock(192837465, 180002)")
        .execute(&mut target)
        .await;
    // Everything is committed by now. PostgreSQL may hang up first after the
    // Terminate message, and then closing the socket reports "not connected"
    // (seen on macOS under load): that says nothing about the import.
    super::hang_up(target).await;
    report
}

/// A resume whose source is not the one PostgreSQL pinned. Startup turns this
/// into its one checked restart; the command line reports it as it is.
#[derive(Debug)]
pub(super) struct PinnedSourceDiffers;
impl std::fmt::Display for PinnedSourceDiffers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "resume source differs from the pinned source snapshot")
    }
}
impl std::error::Error for PinnedSourceDiffers {}

// Startup retains this connection's importer lease through standalone verification
// and archival, so another cooperating CLI cannot intervene between stages.
pub(super) async fn run_with_target(
    path: &Path,
    target: &mut PgConnection,
    stop_after: Option<usize>,
) -> Result<Report> {
    run_with_migrator(path, target, stop_after, &POSTGRES).await
}

/// The migrator is a parameter only so that a test can leave a target at the
/// migration level of an older release. Production always passes `POSTGRES`.
pub(super) async fn run_with_migrator(
    path: &Path,
    target: &mut PgConnection,
    stop_after: Option<usize>,
    migrator: &sqlx::migrate::Migrator,
) -> Result<Report> {
    let start = Instant::now();
    super::target_definition(target).await?;
    // Refuse finished/foreign targets before even opening/scanning the source.
    let previous = target_state(&mut *target).await?;
    let fresh = previous.is_none();
    let mut source = source::open(path).await?;
    if let Some(previous) = &previous {
        let cheap = source::identity(path, &mut source, &Precheck::default()).await?;
        let mut pinned: BTreeMap<String, serde_json::Value> = serde_json::from_str(previous)?;
        let mut current: BTreeMap<String, serde_json::Value> = serde_json::from_str(&cheap)?;
        pinned.remove("snapshot_data_sha256");
        current.remove("snapshot_data_sha256");
        if pinned != current {
            return Err(PinnedSourceDiffers.into());
        }
        super::schema::verify(&mut *target).await?;
    }
    let precheck = source::precheck(&mut source).await?;
    if !precheck.passed() {
        for line in precheck_failure_lines(&precheck) {
            eprintln!("{line}");
        }
        return Err(source::PrecheckFailure(precheck).into());
    }
    let identity = source::identity(path, &mut source, &precheck).await?;
    if let Some(previous) = &previous {
        if previous != &identity {
            return Err(PinnedSourceDiffers.into());
        }
    }
    // SQLx opens a nested savepoint for the baseline; outer transaction includes
    // its migration history, state ownership, and empty-database recheck.
    let mut tx = target.begin().await?;
    lock_application_tables(&mut tx).await?;
    let rechecked = target_state(&mut tx).await?;
    ensure!(rechecked == previous, "target changed during preflight");
    if !fresh {
        super::schema::verify(&mut tx).await?;
    }
    super::schema::search_contract(&mut tx).await?;
    migrator.run_direct(&mut *tx).await?;
    if fresh {
        super::schema::foreign_key_triggers(&mut tx).await?;
        sqlx::raw_sql(STATE_DDL).execute(&mut *tx).await?;
        let fingerprint = super::schema::fingerprint(&mut tx).await?;
        sqlx::query(
            "INSERT INTO public.komika_import_state (singleton,source_identity,schema_hashes) VALUES (true,$1,$2)",
        )
        .bind(identity)
        .bind(sqlx::types::Json(fingerprint))
        .execute(&mut *tx)
        .await?;
    } else {
        // An unfinished import of an older release: the migrator has just
        // applied the migrations that release did not have.
        pin_schema_again(&mut tx).await?;
    }
    tx.commit().await?;
    let done: Vec<(String, String)> =
        sqlx::query_as("SELECT table_name,report::text FROM public.komika_import_table")
            .fetch_all(&mut *target)
            .await?;
    let mut done: BTreeMap<String, TableReport> = done
        .into_iter()
        .map(|(name, json)| Ok((name, serde_json::from_str(&json)?)))
        .collect::<Result<_>>()?;
    let mut reports = Vec::new();
    let mut newly_loaded = 0;
    for table in tables() {
        if let Some(mut prior) = done.remove(&table.name) {
            let actual: i64 = sqlx::query_scalar(&format!(
                "SELECT count(*) FROM public.{}",
                quote(&table.name)
            ))
            .fetch_one(&mut *target)
            .await?;
            ensure!(
                actual as u64 == prior.rows && precheck.rows[&table.name] == prior.rows,
                "{} completed table count changed",
                table.name
            );
            prior.resumed = true;
            reports.push(prior);
            continue;
        }
        let actual: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM public.{}",
            quote(&table.name)
        ))
        .fetch_one(&mut *target)
        .await?;
        ensure!(
            actual == 0,
            "{} unfinished table contains unowned rows",
            table.name
        );
        let report = copy::load(&mut source, &mut *target, &table).await?;
        eprintln!(
            "sqlite import: table {} complete: {} rows, {:.3}s, {} COPY bytes",
            report.table, report.rows, report.seconds, report.bytes_sent
        );
        reports.push(report);
        newly_loaded += 1;
        // Internal test hook only; never sourced from an operator environment.
        if stop_after == Some(newly_loaded) {
            // Abrupt process exit without Rust destructors, only in the test
            // executable. Production CLI always passes None for stop_after.
            #[cfg(test)]
            if std::env::var_os("KOMIKA_PG_TEST_CHILD").is_some() {
                std::process::exit(86);
            }
            anyhow::bail!("test interruption after {newly_loaded} committed tables");
        }
    }
    ensure!(done.is_empty(), "unknown table in importer checkpoint");
    // Verify all retained data before touching sequences/completion. A resumed
    // table with same count but changed values is caught here as well.
    let mut tx = target.begin().await?;
    let lock_tables = tables()
        .iter()
        .map(|t| format!("public.{}", quote(&t.name)))
        .chain(std::iter::once("public.work_search_alias".into()))
        .collect::<Vec<_>>()
        .join(",");
    sqlx::query(&format!(
        "LOCK TABLE {lock_tables} IN ACCESS EXCLUSIVE MODE"
    ))
    .execute(&mut *tx)
    .await?;
    super::verifier::compare_tables_at_applied_level(&mut source, &mut tx).await?;
    for table in tables() {
        sqlx::query(&format!(
            "ALTER TABLE public.{} ENABLE TRIGGER USER",
            quote(&table.name)
        ))
        .execute(&mut *tx)
        .await?;
    }
    copy::counts(&mut source, &mut tx).await?;
    super::verifier::constraints(&mut tx).await?;
    // setval is not transactional. A failed final step may advance sequences,
    // but never marks completion; every resume repeats these exact resets.
    for table in tables() {
        for c in table.columns.iter().filter(|c| c.identity) {
            let max: Option<i64> = sqlx::query_scalar(&format!(
                "SELECT max({}) FROM public.{}",
                quote(&c.name),
                quote(&table.name)
            ))
            .fetch_one(&mut *tx)
            .await?;
            ensure!(
                max != Some(i64::MAX),
                "{}.{} exhausts identity sequence",
                table.name,
                c.name
            );
            sqlx::query(&format!("SELECT setval(pg_get_serial_sequence('public.{}','{}'), greatest(1,coalesce(max({}),1)),coalesce(max({})>=1,false)) FROM public.{}",table.name,c.name,quote(&c.name),quote(&c.name),quote(&table.name))).execute(&mut *tx).await?;
        }
    }
    sqlx::query("SELECT public.rebuild_work_search_alias()")
        .execute(&mut *tx)
        .await?;
    super::verifier::search(&mut tx).await?;
    sqlx::query("UPDATE public.komika_import_state SET completed=true WHERE singleton=true")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    source.close().await?;
    Ok(Report {
        rows: reports.iter().map(|r| r.rows).sum(),
        bytes_sent: reports
            .iter()
            .filter(|r| !r.resumed)
            .map(|r| r.bytes_sent)
            .sum(),
        seconds: start.elapsed().as_secs_f64(),
        peak_memory_bytes: peak_memory(),
        precheck,
        tables: reports,
        complete: true,
    })
}

/// Pin the schema as it is now. Only called inside a transaction that holds
/// ACCESS EXCLUSIVE on every application table and has already proven, under
/// those locks, that the schema was exactly the pinned one before the
/// migrator ran. The only difference is therefore what this server's own
/// migrations did, and the pinned-schema proof carries over to the new level.
async fn pin_schema_again(tx: &mut PgConnection) -> Result<()> {
    super::schema::foreign_key_triggers(&mut *tx).await?;
    let fingerprint = super::schema::fingerprint(&mut *tx).await?;
    sqlx::query("UPDATE public.komika_import_state SET schema_hashes=$1 WHERE singleton=true")
        .bind(sqlx::types::Json(fingerprint))
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// A target that holds this importer's state and was migrated by an older
/// release is brought to the migration level of this server, so that the
/// final verification can compare it with the latest schema. True when a
/// migration was applied. Refused, with nothing changed, when an applied
/// migration is unknown or changed, or when the schema is not the pinned one.
pub(super) async fn bring_up_to_date(target: &mut PgConnection) -> Result<bool> {
    let applied = super::verifier::applied_migrations(target).await?;
    let embedded = POSTGRES
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
        .count();
    if applied == embedded {
        return Ok(false);
    }
    let mut tx = target.begin().await?;
    let migrated = async {
        lock_application_tables(&mut tx).await?;
        super::schema::verify(&mut tx).await?;
        POSTGRES.run_direct(&mut *tx).await?;
        pin_schema_again(&mut tx).await
    }
    .await;
    match migrated {
        Ok(()) => {
            tx.commit().await?;
            Ok(true)
        }
        Err(error) => {
            // A dropped transaction would keep its locks until the
            // connection is used again.
            let _ = tx.rollback().await;
            Err(error)
        }
    }
}

/// What an operator needs to repair the source: every failing rule by name.
/// A rule name holds a table, a column or index, and the kind of failure. It
/// never holds a stored value; row keys stay in the JSON report as hashes.
pub(super) fn precheck_failure_lines(precheck: &Precheck) -> Vec<String> {
    let failing = precheck
        .rules
        .iter()
        .filter(|(_, rule)| rule.failures != 0)
        .collect::<Vec<_>>();
    let mut lines = vec![format!(
        "sqlite import: precheck failed: {} failing rules",
        failing.len()
    )];
    lines.extend(failing.into_iter().map(|(name, rule)| {
        format!(
            "sqlite import: error: precheck rule failed: {name} ({} rows)",
            rule.failures
        )
    }));
    lines
}

// Read-only ownership check, repeated inside the baseline transaction after the
// source pre-check. Full source data identity still requires the counted scan.
pub(super) async fn target_state(target: &mut PgConnection) -> Result<Option<String>> {
    ensure!(
        !import_complete(target).await?,
        "target import is already complete; refusing a second import"
    );
    let existing: Vec<(String, String)> = sqlx::query_as("SELECT n.nspname,c.relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind IN ('r','p','f','m') AND n.nspname <> 'information_schema' AND n.nspname !~ '^pg_' ORDER BY 1,2").fetch_all(&mut *target).await?;
    if existing.is_empty() {
        return Ok(None);
    }
    if unowned_empty(target).await? {
        return Ok(None);
    }
    let mut expected = tables()
        .into_iter()
        .map(|t| ("public".to_owned(), t.name))
        .collect::<Vec<_>>();
    expected.extend(
        [
            "work_search_alias",
            "_sqlx_migrations",
            "komika_import_state",
            "komika_import_table",
        ]
        .into_iter()
        .map(|s| ("public".into(), s.into())),
    );
    if existing
        .iter()
        .any(|(_, name)| name == "komika_startup_import")
    {
        expected.push(("public".into(), "komika_startup_import".into()));
    }
    expected.extend(postgres_only_present(&existing));
    expected.sort();
    ensure!(
        existing == expected,
        "target is not empty or an importer-owned unfinished baseline"
    );
    let identity: String = sqlx::query_scalar(
        "SELECT source_identity FROM public.komika_import_state WHERE singleton=true",
    )
    .fetch_one(target)
    .await?;
    Ok(Some(identity))
}

/// The tables of `plan::POSTGRES_ONLY` that the target has. A migration after the
/// baseline creates such a table, so a target has it or not with its migration level,
/// and both are the importer's own schema. That the table belongs to the level the
/// target really has is proven where the schema is: by the pinned fingerprint of an
/// unfinished import (`schema::verify`) and by the indexes of the applied migrations
/// (`verifier::schema_at_applied_level`), which include the table's primary key.
fn postgres_only_present(existing: &[(String, String)]) -> Vec<(String, String)> {
    super::plan::POSTGRES_ONLY
        .iter()
        .filter(|table| {
            existing
                .iter()
                .any(|(schema, name)| schema == "public" && name == table.name)
        })
        .map(|table| ("public".to_owned(), table.name.to_owned()))
        .collect()
}

// A previous normal server start is deliberately outside this inventory. Its
// marker prevents adoption even if every application row was later removed.
pub(super) async fn unowned_empty(target: &mut PgConnection) -> Result<bool> {
    let existing: Vec<(String, String)> = sqlx::query_as("SELECT n.nspname,c.relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind IN ('r','p','f','m') AND n.nspname <> 'information_schema' AND n.nspname !~ '^pg_' ORDER BY 1,2").fetch_all(&mut *target).await?;
    let mut expected = tables()
        .into_iter()
        .map(|t| ("public".to_owned(), t.name))
        .collect::<Vec<_>>();
    expected.extend(
        ["work_search_alias", "_sqlx_migrations"]
            .into_iter()
            .map(|n| ("public".into(), n.into())),
    );
    let application = existing
        .iter()
        .filter(|(schema, n)| schema != "public" || n != "komika_startup_import")
        .cloned()
        .collect::<Vec<_>>();
    if application.is_empty() {
        return Ok(true);
    }
    expected.extend(postgres_only_present(&existing));
    expected.sort();
    if application != expected {
        return Ok(false);
    }
    for (schema, name) in &application {
        if name == "_sqlx_migrations" {
            continue;
        }
        let occupied: bool = sqlx::query_scalar(&format!(
            "SELECT EXISTS(SELECT 1 FROM {}.{})",
            quote(schema),
            quote(name)
        ))
        .fetch_one(&mut *target)
        .await?;
        if occupied {
            return Ok(false);
        }
    }
    // Validate the column/index contract of the migration level this target
    // really has (an older release may have migrated it; the migrator then
    // adds what is missing inside the adoption transaction).
    super::verifier::schema_at_applied_level(target).await?;
    Ok(true)
}

pub(super) async fn lock_application_tables(target: &mut PgConnection) -> Result<()> {
    let existing: Vec<String> = sqlx::query_scalar("SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relkind IN ('r','p') ORDER BY c.relname").fetch_all(&mut *target).await?;
    if !existing.is_empty() {
        let names = existing
            .iter()
            .map(|n| format!("public.{}", quote(n)))
            .collect::<Vec<_>>()
            .join(",");
        sqlx::query(&format!("LOCK TABLE {names} IN ACCESS EXCLUSIVE MODE"))
            .execute(target)
            .await?;
    }
    Ok(())
}

pub(crate) fn peak_memory() -> Option<u64> {
    // Linux VmHWM is cheap and measures this process, including SQLite's buffers.
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmHWM:")
                .and_then(|s| s.split_whitespace().next()?.parse::<u64>().ok())
                .map(|kb| kb * 1024)
        })
}
