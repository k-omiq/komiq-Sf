//! First-start SQLite cutover. See docs/audits/postgres-startup-import.md.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{ConnectOptions, Connection, PgConnection, SqliteConnection};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    future::Future,
    io::{Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
    time::Instant,
};

const COMPONENTS: &[&str] = &["", "-wal", "-shm", "-journal"];

/// Every operator line written by this file goes through here, so the tests
/// read exactly what the desktop supervisor and `docker compose logs` read.
fn say(line: String) {
    #[cfg(test)]
    tests::capture(&line);
    eprintln!("{line}");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Empty,
    Unfinished,
    Complete,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    Nothing,
    Warn,
    Import,
    Resume,
    Refuse,
}

fn decision(source_exists: bool, target: Target, pending: bool) -> Decision {
    if pending {
        return Decision::Resume;
    }
    if !source_exists {
        return if target == Target::Unfinished {
            Decision::Refuse
        } else {
            Decision::Nothing
        };
    }
    match target {
        Target::Empty | Target::Unfinished => Decision::Import,
        Target::Complete | Target::Other => Decision::Warn,
    }
}

/// What differs between the unfinished attempt on disk and this start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    Source,
    Binary,
    Plan,
    Work,
    /// The record and the disk agree, but PostgreSQL pinned another copy.
    Pinned,
}

impl Change {
    fn reason(self) -> &'static str {
        match self {
            Self::Source => "the SQLite file changed since the unfinished import began",
            Self::Binary => {
                "this server's database migrations differ from the ones that prepared the unfinished import"
            }
            Self::Plan => "the import plan changed since the unfinished import began",
            Self::Work => "the prepared work copy is missing or changed",
            Self::Pinned => {
                "PostgreSQL holds the unfinished import of another prepared copy than the one in this directory"
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    /// Application data without the importer's marker.
    Unrelated,
    /// Importer state that this startup record did not create.
    NotOurs,
    /// Nothing is at the original name.
    SourceGone,
    /// Something is at the original name and cannot be imported.
    SourceUnusable,
    Changed(Change),
    /// A Rename record without its own completed import in PostgreSQL.
    RenameWithoutImport,
}

/// The file at the original name, read and checked BEFORE anything is
/// removed, reset or started empty (rule 1 of fix round 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    /// A regular, readable file with a SQLite header that passes preflight.
    Valid,
    /// Nothing has the name, and no companion file is left either.
    Missing,
    /// A dangling link, a directory, a permission error, damaged bytes, a
    /// database of a newer release: never read as "gone".
    Unusable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    Resume,
    /// Empty target: begin again from Copy.
    Restart,
    /// Our own unfinished import: reset the target, then begin again from Copy.
    Reset(Change),
    /// The SQLite file is gone, no work file exists and PostgreSQL is empty.
    StartEmpty,
    Refuse(Refusal),
}

/// The one restart rule for a start that finds an unfinished startup record.
/// `owned` is the database-side proof that this record created the importer
/// state. `Reset` is the only outcome that removes PostgreSQL rows; it needs an
/// unfinished (never a complete) import, that proof, a record that is not a
/// command-line resume, a stage before Rename and a VALID SQLite file.
/// `Restart` and `Reset` are the only outcomes after which a work file is
/// removed; both need a valid SQLite file. `StartEmpty` needs that no recorded
/// work file exists (`leftovers` false). Every other state is refused and
/// changes nothing.
fn pending(
    stage: Stage,
    target: Target,
    owned: bool,
    cli_resume: bool,
    source: Source,
    change: Option<Change>,
    leftovers: bool,
) -> Pending {
    if stage == Stage::Rename {
        // Verification already passed once. Only that same completed import
        // may be archived; nothing is restarted or reset from here.
        return if target == Target::Complete && owned {
            Pending::Resume
        } else {
            Pending::Refuse(Refusal::RenameWithoutImport)
        };
    }
    match target {
        Target::Other => return Pending::Refuse(Refusal::Unrelated),
        Target::Unfinished | Target::Complete if !owned => {
            return Pending::Refuse(Refusal::NotOurs)
        }
        _ => {}
    }
    match source {
        Source::Unusable => return Pending::Refuse(Refusal::SourceUnusable),
        Source::Missing if target == Target::Empty && !leftovers => return Pending::StartEmpty,
        Source::Missing => return Pending::Refuse(Refusal::SourceGone),
        Source::Valid => {}
    }
    match target {
        Target::Empty => Pending::Restart,
        Target::Unfinished => match change {
            None => Pending::Resume,
            Some(change) if cli_resume => Pending::Refuse(Refusal::Changed(change)),
            Some(change) => Pending::Reset(change),
        },
        // A completed import is never reset. Only a changed SQLite file makes
        // the remaining verification and rename impossible up front.
        Target::Complete => match change {
            Some(Change::Source) => Pending::Refuse(Refusal::Changed(Change::Source)),
            _ => Pending::Resume,
        },
        Target::Other => Pending::Refuse(Refusal::Unrelated),
    }
}

/// The stage at which a record that is resumed goes on. A Verify record says
/// that PostgreSQL held the completed import; when PostgreSQL holds this
/// record's UNFINISHED import instead (a database restored to an earlier
/// point), the Import stage has to run again. A Rename record is never moved
/// back: `pending` refuses it without its completed import.
fn resume_stage(stage: Stage, target: Target) -> Stage {
    if stage == Stage::Verify && target == Target::Unfinished {
        Stage::Import
    } else {
        stage
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
enum Stage {
    Copy,
    Migrate,
    Repair,
    Import,
    Verify,
    Rename,
    Done,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Migrate => "migrate",
            Self::Repair => "repair",
            Self::Import => "import",
            Self::Verify => "verify",
            Self::Rename => "rename",
            Self::Done => "cleanup",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Fingerprint {
    bytes: u64,
    sha256: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct Journal {
    version: u32,
    target: String,
    original: BTreeMap<String, Fingerprint>,
    #[serde(default)]
    inventory_ready: bool,
    stage: Stage,
    archive_stamp: Option<String>,
    work: Option<Fingerprint>,
    #[serde(default)]
    cli_resume: bool,
    #[serde(default)]
    created: Vec<String>,
    #[serde(default)]
    temporary: String,
    #[serde(default)]
    probes: Vec<String>,
    /// SQLite and PostgreSQL migration sets of the binary that pinned the
    /// source inventory, and the import plan it used. Empty in older records.
    #[serde(default)]
    prepared_migrations: String,
    #[serde(default)]
    prepared_plan: String,
    /// Row counts of a few user-facing tables in the snapshot of this attempt.
    #[serde(default)]
    rows: BTreeMap<String, u64>,
    /// The staged copy that a start moved aside instead of deleting it.
    #[serde(default)]
    kept: Option<Kept>,
}

/// A byte-exact copy of the SQLite database that was kept under
/// `<original name>.import-kept-<stamp>` because it was not redundant.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Kept {
    stamp: String,
    /// False until every staged component has its kept name.
    moved: bool,
    /// Every recorded -wal and -journal was copied completely too.
    complete: bool,
    /// The row counts that the record held for this copy.
    rows: BTreeMap<String, u64>,
    /// Every data component that had a staged name when the move was
    /// recorded, with its bytes. `moved` is set only when each of them is at
    /// its kept name with exactly these bytes.
    #[serde(default)]
    files: BTreeMap<String, Fingerprint>,
}

impl Journal {
    /// A no-write Copy reservation. The random names are recorded before any
    /// of those files is created.
    fn fresh(target: String, cli_resume: bool) -> Self {
        Self {
            version: 1,
            target,
            original: BTreeMap::new(),
            inventory_ready: false,
            stage: Stage::Copy,
            archive_stamp: None,
            work: None,
            cli_resume,
            created: work_suffixes(),
            temporary: format!(".import-work-state.next-{}", uuid::Uuid::new_v4()),
            probes: ["a", "b"]
                .iter()
                .map(|end| format!(".import-probe-{}-{end}", uuid::Uuid::new_v4()))
                .collect(),
            prepared_migrations: String::new(),
            prepared_plan: String::new(),
            rows: BTreeMap::new(),
            kept: None,
        }
    }

    /// Back to a no-write Copy reservation, keeping the recorded file names.
    /// Only `begin_again` calls this, after the staged copy is safe.
    /// A command-line identity is derived from PostgreSQL state that no longer
    /// exists after a restart, so such a record gets its own identity.
    fn restart(&mut self) {
        if self.cli_resume {
            self.target = uuid::Uuid::new_v4().to_string();
            self.cli_resume = false;
        }
        self.original.clear();
        self.inventory_ready = false;
        self.stage = Stage::Copy;
        self.archive_stamp = None;
        self.work = None;
        self.prepared_migrations.clear();
        self.prepared_plan.clear();
        // `kept` stays: the next snapshot is compared with the kept copy.
        self.rows.clear();
    }
}

fn migrations_fingerprint() -> String {
    let mut hash = Sha256::new();
    for (engine, migrator) in [
        ("sqlite", &super::source::SQLITE),
        ("postgres", &super::importer::POSTGRES),
    ] {
        hash.update(engine.as_bytes());
        for migration in migrator
            .iter()
            .filter(|m| !m.migration_type.is_down_migration())
        {
            hash.update(migration.version.to_be_bytes());
            hash.update((migration.checksum.len() as u64).to_be_bytes());
            hash.update(&migration.checksum);
        }
    }
    hex::encode(hash.finalize())
}

fn plan_fingerprint() -> String {
    hex::encode(Sha256::digest(include_bytes!("plan.json")))
}

pub(super) fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn source_path(explicit: Option<PathBuf>, covers: &str) -> Result<Option<PathBuf>> {
    if let Some(path) = explicit {
        ensure!(
            path.is_absolute(),
            "KOMIKA_SQLITE_IMPORT_PATH must be absolute"
        );
        return Ok(Some(path));
    }
    let options = sqlx::sqlite::SqliteConnectOptions::from_str(covers)
        .map_err(|_| anyhow::anyhow!("invalid covers SQLite URL [redacted]"))?;
    let file = options.get_filename();
    if file == Path::new(":memory:") || file.to_string_lossy().starts_with("file:sqlx-in-memory-") {
        return Ok(None);
    }
    let parent = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(Some(std::path::absolute(parent.join("komika.sqlite3"))?))
}

/// Called with the covers data-directory lease held, before db::init and listeners.
pub(crate) async fn run(url: &str, covers: &str) -> Result<()> {
    run_configured(url, covers, crate::config::sqlite_import_path()).await
}

/// The same lock file and the same lock call as `db::DataDirectoryLock`, taken
/// by directory. That type only accepts a database URL, and a path must never
/// be turned into a URL here.
fn lock_source_directory(directory: &Path) -> Result<File> {
    let lock_path = directory.join(".komiq-server.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("open data-directory lock {}", lock_path.display()))?;
    file.try_lock().with_context(|| {
        format!(
            "cannot lock {}; another server may be using this data directory",
            lock_path.display()
        )
    })?;
    Ok(file)
}

async fn run_configured(url: &str, covers: &str, explicit: Option<PathBuf>) -> Result<()> {
    let mut entered_runner = false;
    let result = async {
        if let Some(path) = source_path(explicit, covers)? {
            // Covers must never be accepted as an explicitly named legacy main DB.
            let cover_options = sqlx::sqlite::SqliteConnectOptions::from_str(covers)
                .map_err(|_| anyhow::anyhow!("invalid covers SQLite URL [redacted]"))?;
            // An entry that cannot be examined is refused by the runner below,
            // with its path; here it only cannot be compared.
            if path.try_exists().unwrap_or(false)
                && cover_options.get_filename().try_exists().unwrap_or(false)
            {
                ensure!(
                    path.canonicalize()? != cover_options.get_filename().canonicalize()?,
                    "import path overlaps covers database"
                );
            }
            let _source_directory_lock = if path.try_exists().unwrap_or(false)
                || sibling(&path, ".import-work-state")
                    .try_exists()
                    .unwrap_or(false)
            {
                let directory = path.parent().unwrap_or(Path::new("."));
                let covers_directory = cover_options
                    .get_filename()
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                if directory.canonicalize()? != covers_directory.canonicalize()? {
                    Some(lock_source_directory(directory)?)
                } else {
                    None
                }
            } else {
                None
            };
            entered_runner = true;
            run_path(&path, url, None).await?;
        } else {
            let mut target = super::connect(url).await?;
            let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(192837465,180002)")
                .fetch_one(&mut target)
                .await?;
            ensure!(locked, "another importer owns the target");
            ensure!(
                inspect(&mut target).await? != Target::Unfinished,
                "unfinished import has no SQLite source; refusing to serve"
            );
            mark_normal_start(&mut target).await?;
            // The start mark is durable; a close that fails changes nothing.
            let _ = release(target).await;
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        let message = if entered_runner {
            super::cli::safe_error(&error, url)
        } else {
            format!("decision stage: {}", super::cli::safe_error(&error, url))
        };
        say(format!("sqlite import: error: {message}"));
        // Return only the redacted error: main's termination formatter must not
        // print a driver error chain (which can quote credentials or whole rows).
        anyhow::bail!("sqlite import: {message}");
    }
    Ok(())
}

async fn inspect(target: &mut PgConnection) -> Result<Target> {
    let state_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public.komika_import_state') IS NOT NULL")
            .fetch_one(&mut *target)
            .await?;
    if state_exists {
        if super::import_complete(target).await? {
            return Ok(Target::Complete);
        }
        super::importer::target_state(target).await?;
        super::schema::verify(target).await?;
        checkpoint_ownership(target).await?;
        return Ok(Target::Unfinished);
    }
    Ok(if super::importer::unowned_empty(target).await? {
        Target::Empty
    } else {
        Target::Other
    })
}

// A marker is not enough: every existing row must be in a checkpointed
// table, with its recorded count; uncommitted tables and derived search are empty.
async fn checkpoint_ownership(target: &mut PgConnection) -> Result<()> {
    let rows: Vec<(String, sqlx::types::Json<super::copy::TableReport>)> =
        sqlx::query_as("SELECT table_name,report FROM public.komika_import_table")
            .fetch_all(&mut *target)
            .await?;
    let mut done: BTreeMap<_, _> = rows
        .into_iter()
        .map(|(name, report)| (name, report.0))
        .collect();
    for table in super::plan::tables() {
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM public.{}",
            super::plan::quote(&table.name)
        ))
        .fetch_one(&mut *target)
        .await?;
        let expected = match done.remove(&table.name) {
            Some(report) => {
                ensure!(
                    report.table == table.name,
                    "checkpoint table identity differs"
                );
                report.rows
            }
            None => 0,
        };
        ensure!(
            count as u64 == expected,
            "{} contains unowned or changed rows",
            table.name
        );
    }
    ensure!(done.is_empty(), "unknown table in importer checkpoint");
    let search: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public.work_search_alias)")
        .fetch_one(&mut *target)
        .await?;
    ensure!(!search, "unfinished import contains unowned search rows");
    // The importer writes nothing into a table that only PostgreSQL has.
    for name in postgres_only_tables(target).await? {
        let occupied: bool = sqlx::query_scalar(&format!(
            "SELECT EXISTS(SELECT 1 FROM public.{})",
            super::plan::quote(name)
        ))
        .fetch_one(&mut *target)
        .await?;
        ensure!(!occupied, "unfinished import contains unowned {name} rows");
    }
    Ok(())
}

/// The tables of `plan::POSTGRES_ONLY` that the target has: those the migrations it has
/// applied create.
async fn postgres_only_tables(target: &mut PgConnection) -> Result<Vec<&'static str>> {
    let mut present = Vec::new();
    for table in super::plan::POSTGRES_ONLY {
        let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(format!("public.{}", super::plan::quote(table.name)))
            .fetch_one(&mut *target)
            .await?;
        if exists {
            present.push(table.name);
        }
    }
    Ok(present)
}

/// The identity a startup record wrote into PostgreSQL. A random value that
/// only the record which created it also holds.
async fn startup_binding(target: &mut PgConnection) -> Result<Option<String>> {
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public.komika_startup_import') IS NOT NULL")
            .fetch_one(&mut *target)
            .await?;
    if !exists {
        return Ok(None);
    }
    Ok(sqlx::query_scalar(
        "SELECT import_id FROM public.komika_startup_import WHERE singleton=true",
    )
    .fetch_optional(target)
    .await?)
}

// Stable ownership lives in PostgreSQL, never in the connection spelling.
async fn target_identity(target: &mut PgConnection) -> Result<Option<String>> {
    let binding: bool =
        sqlx::query_scalar("SELECT to_regclass('public.komika_startup_import') IS NOT NULL")
            .fetch_one(&mut *target)
            .await?;
    if binding {
        return startup_binding(target).await;
    }
    let state: bool =
        sqlx::query_scalar("SELECT to_regclass('public.komika_import_state') IS NOT NULL")
            .fetch_one(&mut *target)
            .await?;
    if state {
        // A command-line import has no startup binding. Its identity is
        // derived from state that any data directory can read, so it is never
        // accepted as proof of ownership for a reset.
        let pinned: String = sqlx::query_scalar(
            "SELECT source_identity FROM public.komika_import_state WHERE singleton=true",
        )
        .fetch_one(target)
        .await?;
        return Ok(Some(hex::encode(Sha256::digest(pinned.as_bytes()))));
    }
    Ok(None)
}

/// Does PostgreSQL say that this record created the importer state it holds?
fn owns(journal: &Journal, binding: Option<&String>, identity: Option<&String>) -> bool {
    if journal.cli_resume {
        binding.is_none() && identity == Some(&journal.target)
    } else {
        binding == Some(&journal.target)
    }
}

/// Publish this record's identity in PostgreSQL. A missing or stale binding is
/// replaced only while the database holds no importer state and no application
/// row, so one binding value never stands for two startup records.
async fn bind_target(target: &mut PgConnection, journal: &Journal) -> Result<()> {
    if journal.cli_resume {
        // Creating the binding table would change the schema that the
        // command-line import pinned.
        ensure!(
            startup_binding(target).await?.is_none()
                && target_identity(target).await?.as_ref() == Some(&journal.target),
            "startup import target binding changed"
        );
        return Ok(());
    }
    if startup_binding(target).await?.as_ref() == Some(&journal.target) {
        return Ok(());
    }
    let mut tx = target.begin().await?;
    let bound = bind_in_transaction(&mut tx, journal).await;
    finish_transaction(tx, bound).await
}

/// Commit on success. On failure roll back NOW: a dropped sqlx transaction is
/// only rolled back when its connection is used again, and until then it would
/// keep its exclusive locks.
async fn finish_transaction(
    tx: sqlx::Transaction<'_, sqlx::Postgres>,
    result: Result<()>,
) -> Result<()> {
    match result {
        Ok(()) => Ok(tx.commit().await?),
        Err(error) => {
            let _ = tx.rollback().await;
            Err(error)
        }
    }
}

async fn bind_in_transaction(tx: &mut PgConnection, journal: &Journal) -> Result<()> {
    sqlx::query("CREATE TABLE IF NOT EXISTS public.komika_startup_import (singleton boolean PRIMARY KEY CHECK(singleton), import_id text NOT NULL)").execute(&mut *tx).await?;
    sqlx::query("LOCK TABLE public.komika_startup_import IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    ensure!(
        inspect(&mut *tx).await? == Target::Empty,
        "startup import target binding changed"
    );
    sqlx::query("DELETE FROM public.komika_startup_import")
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO public.komika_startup_import VALUES(true,$1)")
        .bind(&journal.target)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// Checked restart: remove what this record's unfinished import put into
/// PostgreSQL, and nothing else. Everything is re-read inside one transaction
/// that first takes exclusive locks, so a second server waits and then finds
/// nothing left to reset. The result is the migrated schema with zero rows,
/// user triggers enabled again, and the binding kept.
async fn reset_target(target: &mut PgConnection, journal: &Journal) -> Result<()> {
    ensure!(
        !journal.cli_resume,
        "a command-line import is never reset by the server"
    );
    let present: bool = sqlx::query_scalar(
        "SELECT to_regclass('public.komika_startup_import') IS NOT NULL \
         AND to_regclass('public.komika_import_state') IS NOT NULL \
         AND to_regclass('public.komika_import_table') IS NOT NULL",
    )
    .fetch_one(&mut *target)
    .await?;
    ensure!(present, "PostgreSQL holds no unfinished startup import");
    let mut tx = target.begin().await?;
    let reset = reset_in_transaction(&mut tx, journal).await;
    finish_transaction(tx, reset).await
}

async fn reset_in_transaction(tx: &mut PgConnection, journal: &Journal) -> Result<()> {
    sqlx::query("LOCK TABLE public.komika_startup_import, public.komika_import_state, public.komika_import_table IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    let bound: Option<String> = sqlx::query_scalar(
        "SELECT import_id FROM public.komika_startup_import WHERE singleton=true",
    )
    .fetch_optional(&mut *tx)
    .await?;
    ensure!(
        bound.as_ref() == Some(&journal.target),
        "the unfinished import in PostgreSQL belongs to another startup record"
    );
    super::importer::lock_application_tables(&mut *tx).await?;
    // Refuses a completed import and every table outside the importer's own
    // inventory, including the marker of a normal server start.
    ensure!(
        super::importer::target_state(&mut *tx).await?.is_some(),
        "PostgreSQL holds no unfinished import"
    );
    // Every row is a row of a checkpointed importer table, with its count.
    checkpoint_ownership(&mut *tx).await?;
    let tables = super::plan::tables();
    // A table that references a truncated one must be in the same statement, even
    // when it is empty: the tables that only PostgreSQL has reference `users`.
    let postgres_only = postgres_only_tables(&mut *tx).await?;
    let names = tables
        .iter()
        .map(|table| table.name.as_str())
        .chain(postgres_only)
        .map(|name| format!("public.{}", super::plan::quote(name)))
        .chain(std::iter::once("public.work_search_alias".to_owned()))
        .collect::<Vec<_>>()
        .join(",");
    sqlx::query(&format!("TRUNCATE TABLE {names} RESTART IDENTITY"))
        .execute(&mut *tx)
        .await?;
    for table in &tables {
        // Table loads leave user triggers disabled until completion.
        sqlx::query(&format!(
            "ALTER TABLE public.{} ENABLE TRIGGER USER",
            super::plan::quote(&table.name)
        ))
        .execute(&mut *tx)
        .await?;
    }
    sqlx::raw_sql("DROP TABLE public.komika_import_table; DROP TABLE public.komika_import_state")
        .execute(&mut *tx)
        .await?;
    ensure!(
        inspect(&mut *tx).await? == Target::Empty,
        "reset did not leave an empty database"
    );
    Ok(())
}

async fn mark_normal_start(target: &mut PgConnection) -> Result<()> {
    if inspect(target).await? != Target::Complete {
        // Written BEFORE handing off to db::init/provisioning/background jobs.
        sqlx::query("CREATE TABLE IF NOT EXISTS public.komika_server_started (singleton boolean PRIMARY KEY CHECK(singleton))").execute(&mut *target).await?;
        sqlx::query("INSERT INTO public.komika_server_started VALUES(true) ON CONFLICT DO NOTHING")
            .execute(target)
            .await?;
    }
    Ok(())
}

// Internal test-only fault injection: production always passes None. A boundary
// error leaves exactly the same durable state as death without destructors.
fn boundary(stage: Stage, stop: Option<Stage>) -> Result<()> {
    ensure!(
        stop != Some(stage),
        "test interruption at {} boundary",
        stage.name()
    );
    Ok(())
}

/// Points inside the "begin again" sequence at which a test lets the process
/// die. Production never sets one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Crash {
    /// The kept name is recorded; no staged file has moved yet.
    KeepRecorded,
    /// A staged sidecar has its kept name, the main file not yet.
    KeepHalfMoved,
    /// Every component has its kept name; the record does not say so yet.
    KeepMoved,
    /// The staged copy is safe; PostgreSQL is not reset yet.
    BeforeReset,
    /// PostgreSQL is reset; no work file is removed yet.
    AfterReset,
    /// Work files are removed; the record still describes the old attempt.
    WorkDiscarded,
    /// The record is a Copy reservation again; no file of the new attempt
    /// exists yet.
    Reserved,
}

fn crash(point: Crash, at: Option<Crash>) -> Result<()> {
    ensure!(at != Some(point), "test interruption at {point:?}");
    Ok(())
}

/// Test-only fault injection. Production always uses `Faults::NONE`.
#[derive(Clone, Copy)]
struct Faults {
    stop: Option<Stage>,
    probe: fn(&Path, &Journal) -> Result<()>,
    /// Runs at the start of every stage, before that stage's own checks.
    before_stage: fn(&Path, Stage),
    /// Runs when the staged copy is safe, immediately before the checked reset.
    before_reset: fn(&str),
    crash: Option<Crash>,
}

impl Faults {
    const NONE: Self = Self {
        stop: None,
        probe: capability_probe,
        before_stage: |_, _| {},
        before_reset: |_| {},
        crash: None,
    };
}

struct Progress {
    stage: &'static str,
    /// Manual steps, appended to an error that no later start can repair.
    advice: String,
    announced: bool,
    /// The process-wide SQLite temporary directory points at the work directory.
    temporary: bool,
    /// Warnings about unrecorded leftover files, held back until the two
    /// warnings that matter most (kept copy, smaller database) are out: a
    /// supervisor keeps only the first warnings of a start.
    later: Vec<String>,
}

impl Progress {
    /// First progress line. Printed when an import, a resume or an archive
    /// completion is really attempted, and always before the first file is
    /// hashed (a large file takes longer than a supervisor waits for a line).
    fn announce(&mut self) {
        if !self.announced {
            self.announced = true;
            say("sqlite import: preparing (checking source and target)".into());
        }
    }

    /// Say the warnings that were held back.
    fn say_later(&mut self) {
        for line in self.later.drain(..) {
            say(line);
        }
    }
}

async fn run_path(path: &Path, url: &str, stop: Option<Stage>) -> Result<()> {
    run_path_with(
        path,
        url,
        Faults {
            stop,
            ..Faults::NONE
        },
    )
    .await
}

#[cfg(test)]
async fn run_path_with_probe(
    path: &Path,
    url: &str,
    stop: Option<Stage>,
    probe: fn(&Path, &Journal) -> Result<()>,
) -> Result<()> {
    run_path_with(
        path,
        url,
        Faults {
            stop,
            probe,
            ..Faults::NONE
        },
    )
    .await
}

async fn run_path_with(path: &Path, url: &str, faults: Faults) -> Result<()> {
    let mut progress = Progress {
        stage: "decision",
        advice: String::new(),
        announced: false,
        temporary: false,
        later: Vec::new(),
    };
    let mut connection = None;
    let result = cutover(path, url, faults, &mut progress, &mut connection).await;
    // On every way out, a failed one too.
    progress.say_later();
    if let Some(connection) = connection.take() {
        // Every way out of the cutover gives the importer lock back at once.
        let _ = release(connection).await;
    }
    if progress.temporary {
        // The covers database and every later SQLite user get the default back.
        if let Err(error) = sqlite_temporary_directory(None).await {
            say(format!(
                "sqlite import: warning: could not restore the default SQLite temporary directory: {}",
                super::cli::safe_error(&error, url)
            ));
        }
    }
    result.map_err(|error| {
        anyhow::anyhow!(
            "{} stage: {}{}",
            progress.stage,
            super::cli::safe_error(&error, url),
            progress.advice
        )
    })
}

/// Give the importer lock back and close the connection.
async fn release(target: PgConnection) -> Result<()> {
    release_with(target, |target| async move { Ok(target.close().await?) }).await
}

/// The unlock is a statement of its own because PostgreSQL releases a
/// session's locks only some time after the client hangs up, and the next
/// start (or the server itself) must not find "another importer" in that gap.
/// The hang-up is a parameter so that a test can keep the session open and
/// see that the lock is free before it.
async fn release_with<F, Fut>(mut target: PgConnection, hang_up: F) -> Result<()>
where
    F: FnOnce(PgConnection) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let _ = sqlx::query("SELECT pg_advisory_unlock(192837465,180002)")
        .execute(&mut target)
        .await;
    hang_up(target).await
}

fn load(journal_path: &Path) -> Result<Option<Journal>> {
    if !exists_at(journal_path)? {
        return Ok(None);
    }
    ensure!(
        fs::symlink_metadata(journal_path)?.file_type().is_file(),
        "startup import record must be regular"
    );
    let journal: Journal = serde_json::from_slice(&fs::read(journal_path)?)
        .map_err(|_| anyhow::anyhow!("invalid startup import record"))?;
    ensure!(
        journal.version == 1,
        "startup import record belongs to another target or version"
    );
    ensure!(
        if journal.inventory_ready {
            journal.original.contains_key("")
                && journal
                    .original
                    .keys()
                    .all(|k| COMPONENTS.contains(&k.as_str()))
        } else {
            journal.stage == Stage::Copy
                && journal.original.is_empty()
                && journal.work.is_none()
                && journal.archive_stamp.is_none()
        },
        "invalid source inventory in startup record"
    );
    validate_record(&journal)?;
    Ok(Some(journal))
}

/// Compare the unfinished attempt with this start: the SQLite files, the
/// binary's migration sets, the import plan and the pinned work copy. What
/// PostgreSQL pinned is compared by the importer itself, see the Import stage.
fn changed(path: &Path, journal: &Journal) -> Result<Option<Change>> {
    if !journal.inventory_ready {
        // Nothing was pinned yet; this start pins the current files.
        return Ok(None);
    }
    if inventory(path)? != journal.original {
        return Ok(Some(Change::Source));
    }
    if journal.prepared_migrations != migrations_fingerprint() {
        return Ok(Some(Change::Binary));
    }
    if journal.prepared_plan != plan_fingerprint() {
        return Ok(Some(Change::Plan));
    }
    if matches!(journal.stage, Stage::Import | Stage::Verify) && !work_is_pinned(path, journal) {
        return Ok(Some(Change::Work));
    }
    Ok(None)
}

fn work_is_pinned(path: &Path, journal: &Journal) -> bool {
    journal
        .work
        .as_ref()
        .is_some_and(|pin| fingerprint(&sibling(path, ".import-work")).is_ok_and(|now| now == *pin))
}

/// Is anything at this name? A dangling link counts. An entry that cannot be
/// examined is an error, never "nothing".
fn exists_at(path: &Path) -> Result<bool> {
    Ok(kind_at(path)?.is_some())
}

fn kind_at(path: &Path) -> Result<Option<fs::FileType>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata.file_type())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(anyhow::anyhow!("cannot read {}: {error}", path.display())),
    }
}

/// What is at the original name right now. Only `Missing` means "gone".
#[derive(Clone, Debug, PartialEq, Eq)]
enum Present {
    Missing,
    /// The name cannot even be examined: the OS error.
    Unreadable(String),
    /// Something has the name and is not a readable regular file: what it is.
    Blocked(String),
    File,
}

fn present(path: &Path) -> Present {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Present::Missing,
        Err(error) => return Present::Unreadable(error.to_string()),
    };
    let kind = metadata.file_type();
    if kind.is_symlink() {
        return Present::Blocked(match fs::metadata(path) {
            Ok(_) => {
                "it is a symbolic link; the import needs the database file itself at this name"
                    .into()
            }
            Err(error) => format!("it is a symbolic link whose target cannot be read: {error}"),
        });
    }
    if kind.is_dir() {
        return Present::Blocked("it is a directory, not a file".into());
    }
    if !kind.is_file() {
        return Present::Blocked("it is not a regular file".into());
    }
    match File::open(path) {
        Ok(_) => Present::File,
        Err(error) => Present::Blocked(format!("it cannot be opened: {error}")),
    }
}

/// Rule 1 of fix round 4. The second value says why a file is unusable.
fn source_state(path: &Path, here: &Present) -> (Source, String) {
    match here {
        Present::Missing => {
            for suffix in &COMPONENTS[1..] {
                let companion = sibling(path, suffix);
                // A companion without its main file is no deliberate removal.
                if exists_at(&companion).unwrap_or(true) {
                    return (
                        Source::Unusable,
                        format!(
                            "the main file is missing while {} is still there",
                            companion.display()
                        ),
                    );
                }
            }
            (Source::Missing, String::new())
        }
        Present::Unreadable(error) => (Source::Unusable, format!("it cannot be read: {error}")),
        Present::Blocked(what) => (Source::Unusable, what.clone()),
        Present::File => match super::preflight::check(path) {
            Ok(()) => (Source::Valid, String::new()),
            Err(error) => (
                Source::Unusable,
                format!(
                    "it is not a SQLite database that this server can import ({})",
                    super::cli::safe_error(&error, "")
                ),
            ),
        },
    }
}

/// Recorded data and lease files of the import that exist now.
fn recorded_work(path: &Path, journal: &Journal) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for name in &journal.created {
        let file = sibling(path, name);
        if exists_at(&file)? {
            found.push(file);
        }
    }
    Ok(found)
}

/// Recorded private files that exist now, for an operator who must remove them.
fn work_files(path: &Path, journal: &Journal) -> String {
    let names = journal
        .created
        .iter()
        .chain(&journal.probes)
        .chain(std::iter::once(&journal.temporary))
        .map(|name| sibling(path, name))
        .filter(|file| fs::symlink_metadata(file).is_ok())
        .map(|file| file.display().to_string())
        .collect::<Vec<_>>();
    if names.is_empty() {
        "(no work files exist)".into()
    } else {
        names.join(", ")
    }
}

// ---- the staged copy: the last good copy is never deleted ----

/// Components of a SQLite database that hold data. The -shm index does not.
const DATA_COMPONENTS: &[&str] = &["", "-wal", "-journal"];

/// The byte-exact copy of the original that an import keeps beside it. A
/// command-line resume has no VACUUM step: its work copy IS that byte copy.
fn staged_base(journal: &Journal) -> &'static str {
    if journal.cli_resume {
        ".import-work"
    } else {
        ".import-work-source"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Staged {
    Absent,
    /// A file at the staged name that is provably not the recorded original:
    /// an interrupted copy.
    Torn,
    /// The bytes of the recorded original main file. `complete`: with every
    /// recorded -wal and -journal, byte for byte.
    Copy {
        complete: bool,
    },
}

fn staged(path: &Path, journal: &Journal) -> Result<Staged> {
    Ok(staged_with(path, journal)?.0)
}

/// A `-wal` or `-journal` at a staged name that is no part of the staged
/// copy: a file of zero bytes, where the recorded original had no such
/// component. Nothing of the owner's database is in it. SQLite makes such a
/// file itself: opened read-only, a database in write-ahead-log mode that was
/// closed cleanly (one file, the usual source) gets an empty `-wal` and an
/// index beside it, and a read-only connection cannot remove them again.
///
/// The rule is made here, where the copy is judged, and not where the copy is
/// opened: it then holds whoever opened the file (this server, an older build
/// of it, an owner looking at the copy with a read-only tool), and SQLite's
/// `immutable` flag, which would create nothing, must not be used on a copy
/// that has a write-ahead log, because it ignores the log.
///
/// A sidecar with one byte in it, and every sidecar that the original had, is
/// a component and counts as before. Without a recorded original nothing is
/// ruled out.
fn left_by_sqlite(file: &Path, suffix: &str, journal: &Journal) -> Result<bool> {
    if suffix.is_empty()
        || !journal.original.contains_key("")
        || journal.original.contains_key(suffix)
    {
        return Ok(false);
    }
    match fs::symlink_metadata(file) {
        Ok(metadata) => Ok(metadata.file_type().is_file() && metadata.len() == 0),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(anyhow::anyhow!(
            "cannot examine {}: {error}",
            file.display()
        )),
    }
}

/// The same, with the bytes of every regular data component that has a staged
/// name (empty unless the answer is `Copy`).
fn staged_with(path: &Path, journal: &Journal) -> Result<(Staged, BTreeMap<String, Fingerprint>)> {
    let base = sibling(path, staged_base(journal));
    let mut files = BTreeMap::new();
    let Some(kind) = kind_at(&base)? else {
        return Ok((Staged::Absent, files));
    };
    if !kind.is_file() {
        return Ok((Staged::Torn, files));
    }
    let read = |file: &Path| {
        fingerprint(file)
            .map_err(|error| anyhow::anyhow!("cannot read {}: {error}", file.display()))
    };
    let main = read(&base)?;
    let recorded = journal.original.get("");
    if recorded.is_some_and(|recorded| main != *recorded) {
        return Ok((Staged::Torn, files));
    }
    files.insert(String::new(), main);
    // Without a recorded original nothing can be compared, so the file cannot
    // be shown to be an interrupted copy, and it cannot be called exact.
    let mut complete = recorded.is_some();
    for suffix in &DATA_COMPONENTS[1..] {
        let file = sibling(&base, suffix);
        if left_by_sqlite(&file, suffix, journal)? {
            continue;
        }
        let found = match kind_at(&file)? {
            Some(kind) if kind.is_file() => Some(read(&file)?),
            Some(_) => {
                complete = false;
                None
            }
            None => None,
        };
        if found.as_ref() != journal.original.get(*suffix) {
            complete = false;
        }
        if let Some(found) = found {
            files.insert((*suffix).to_owned(), found);
        }
    }
    Ok((Staged::Copy { complete }, files))
}

/// Condition (ii): the file now at the original name is readable, passes the
/// header and preflight check, and every data component of the staged copy
/// has a byte-identical file at the original name.
fn staged_is_redundant(path: &Path, journal: &Journal) -> Result<bool> {
    if source_state(path, &present(path)).0 != Source::Valid {
        return Ok(false);
    }
    let base = sibling(path, staged_base(journal));
    for suffix in DATA_COMPONENTS {
        let copy = sibling(&base, suffix);
        if left_by_sqlite(&copy, suffix, journal)? {
            continue;
        }
        match kind_at(&copy)? {
            None => {}
            Some(kind) if !kind.is_file() => return Ok(false),
            Some(_) => {
                let original = sibling(path, suffix);
                let regular = kind_at(&original)?.is_some_and(|kind| kind.is_file());
                if !regular || fingerprint(&original)? != fingerprint(&copy)? {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

/// Proof that whatever still has a staged name is not the last copy of the
/// owner's database. It is made in exactly three places: after a verified
/// cutover (`after_cutover`), by the read-only rule (`disposable_staged`) and
/// by the rule that moves a copy aside (`protect_staged_as`, which
/// `protect_staged` and `begin_again` call). `discard_work`, `remove_work` and
/// `remove_staged` delete a staged file only against this value.
struct Disposable(());

impl Disposable {
    /// Condition (i): the import of exactly that content is complete and
    /// verified, and the record says so durably.
    fn after_cutover(journal: &Journal) -> Result<Self> {
        ensure!(
            journal.stage == Stage::Done,
            "the staged copy is kept until the cutover is complete"
        );
        Ok(Self(()))
    }
}

/// The rule in its read-only form, for the Copy stage: an interrupted copy
/// and a redundant copy may be replaced; anything else stops the import.
fn disposable_staged(path: &Path, journal: &Journal) -> Result<Disposable> {
    ensure!(
        journal.kept.as_ref().is_none_or(|kept| kept.moved),
        "an earlier start did not finish moving the staged copy aside"
    );
    let base = sibling(path, staged_base(journal));
    match staged(path, journal)? {
        Staged::Absent | Staged::Torn => Ok(Disposable(())),
        Staged::Copy { .. } if staged_is_redundant(path, journal)? => Ok(Disposable(())),
        Staged::Copy { .. } => anyhow::bail!(
            "{} is a copy of the SQLite database as it was when the import began and differs from the file now at {}; it is never overwritten or deleted",
            base.display(),
            path.display()
        ),
    }
}

/// The rule of fix round 4. The staged copy is deleted later only when it is
/// an interrupted copy or redundant; in every other case it is moved to its
/// kept name here, before PostgreSQL is reset and before any file is removed.
fn protect_staged(
    path: &Path,
    journal_path: &Path,
    journal: &mut Journal,
    crash_at: Option<Crash>,
) -> Result<Disposable> {
    let keep = keep_decision(path, journal)?;
    protect_staged_as(path, journal_path, journal, crash_at, keep)
}

/// What the rule says about the staged copy, before anything is moved.
enum Keep {
    /// Nothing has the staged name, or an interrupted or redundant copy.
    No,
    /// An earlier start died while it moved the copy aside.
    Unfinished,
    /// A copy that is not redundant: it gets a kept name. With the bytes of
    /// its data components.
    Needed {
        complete: bool,
        files: BTreeMap<String, Fingerprint>,
    },
}

fn keep_decision(path: &Path, journal: &Journal) -> Result<Keep> {
    if journal.kept.as_ref().is_some_and(|kept| !kept.moved) {
        return Ok(Keep::Unfinished);
    }
    Ok(match staged_with(path, journal)? {
        (Staged::Absent | Staged::Torn, _) => Keep::No,
        (Staged::Copy { .. }, _) if staged_is_redundant(path, journal)? => Keep::No,
        (Staged::Copy { complete }, files) => Keep::Needed { complete, files },
    })
}

fn protect_staged_as(
    path: &Path,
    journal_path: &Path,
    journal: &mut Journal,
    crash_at: Option<Crash>,
    keep: Keep,
) -> Result<Disposable> {
    match keep {
        Keep::No => {}
        Keep::Unfinished => finish_keep(path, journal_path, journal, crash_at)?,
        Keep::Needed { complete, files } => {
            journal.kept = Some(Kept {
                stamp: free_kept_stamp(path)?,
                moved: false,
                complete,
                rows: journal.rows.clone(),
                files,
            });
            // The name is durable before the first rename, like the archive
            // name: a later start finishes under the same name.
            save(journal_path, journal)?;
            crash(Crash::KeepRecorded, crash_at)?;
            finish_keep(path, journal_path, journal, crash_at)?;
        }
    }
    Ok(Disposable(()))
}

/// Where one data component of a copy is whose move to its kept name was
/// recorded and not finished.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Place {
    /// Still at its staged name.
    Staged(PathBuf),
    /// Already at its kept name.
    Kept(PathBuf),
    /// At neither, and the file at the original name has exactly its bytes:
    /// somebody put it back.
    Original(PathBuf),
    Missing,
}

/// Every data component of a copy that is being moved aside, at BOTH names.
/// A component that was recorded is always in the answer; another one only
/// when something has its staged or kept name.
fn keep_pieces(path: &Path, journal: &Journal, kept: &Kept) -> Result<Vec<(&'static str, Place)>> {
    let from = sibling(path, staged_base(journal));
    let to = kept_path(path, &kept.stamp)?;
    let mut pieces = Vec::new();
    for suffix in DATA_COMPONENTS {
        let staged = sibling(&from, suffix);
        let aside = sibling(&to, suffix);
        // A record of fix round 4 has no list: it needs the main file.
        let recorded = if kept.files.is_empty() {
            suffix.is_empty()
        } else {
            kept.files.contains_key(*suffix)
        };
        // What SQLite left at a staged name is no piece of the copy: it gets
        // no kept name and goes with the work files.
        let place = if exists_at(&staged)? && !left_by_sqlite(&staged, suffix, journal)? {
            Place::Staged(staged)
        } else if exists_at(&aside)? {
            Place::Kept(aside)
        } else if !recorded {
            continue;
        } else {
            let original = sibling(path, suffix);
            let back = kept.files.get(*suffix).is_some_and(|bytes| {
                matches!(kind_at(&original), Ok(Some(kind)) if kind.is_file()) && {
                    say_scan(std::slice::from_ref(&original));
                    fingerprint(&original).is_ok_and(|now| now == *bytes)
                }
            });
            if back {
                Place::Original(original)
            } else {
                Place::Missing
            }
        };
        pieces.push((*suffix, place));
    }
    Ok(pieces)
}

fn component_name(suffix: &str) -> &'static str {
    match suffix {
        "" => "the main file",
        "-wal" => "the write-ahead log",
        _ => "the rollback journal",
    }
}

/// The exact renames that put the listed pieces back at the original name.
fn renames_back(path: &Path, pieces: &[(&'static str, &PathBuf)]) -> String {
    let mut text = String::new();
    for (index, (suffix, file)) in pieces.iter().enumerate() {
        text.push_str(&format!(
            "{}{} to {}",
            if index == 0 { "rename " } else { ", " },
            file.display(),
            sibling(path, suffix).display()
        ));
    }
    text
}

/// What has to leave the original name before a copy is renamed there, as
/// the first words of the instruction: empty, or "move A, B out of the data
/// directory, then ". A rename replaces whatever has the target name without
/// a word, and a `-wal`, `-shm` or `-journal` that stays beside the renamed
/// main file would be read by SQLite as a part of it. A name that cannot be
/// examined is listed too.
fn clear_first(path: &Path) -> String {
    let occupied = COMPONENTS
        .iter()
        .map(|suffix| sibling(path, suffix))
        .filter(|file| !matches!(exists_at(file), Ok(false)))
        .map(|file| file.display().to_string())
        .collect::<Vec<_>>();
    if occupied.is_empty() {
        String::new()
    } else {
        format!(
            "move {} out of the data directory, then ",
            occupied.join(", ")
        )
    }
}

/// A copy whose move was interrupted and of which a recorded component is no
/// longer at its staged or kept name. Nothing is moved, announced or deleted
/// on the strength of such a set: `None` when the set is whole (or when
/// nothing of it is left at those names at all).
fn incomplete_keep(path: &Path, pieces: &[(&'static str, Place)]) -> Option<anyhow::Error> {
    let here = pieces
        .iter()
        .filter_map(|(suffix, place)| match place {
            Place::Staged(file) | Place::Kept(file) => Some((*suffix, file)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let elsewhere = pieces
        .iter()
        .any(|(_, place)| matches!(place, Place::Original(_) | Place::Missing));
    if here.is_empty() || !elsewhere {
        return None;
    }
    let list = pieces
        .iter()
        .map(|(suffix, place)| match place {
            Place::Staged(file) | Place::Kept(file) => {
                format!("{} is at {}", component_name(suffix), file.display())
            }
            Place::Original(file) => {
                format!(
                    "{} was put back at {}",
                    component_name(suffix),
                    file.display()
                )
            }
            Place::Missing => format!("{} is missing", component_name(suffix)),
        })
        .collect::<Vec<_>>()
        .join("; ");
    let lost = pieces.iter().any(|(_, place)| *place == Place::Missing);
    // The rest can be put back only beside the very files it belongs to, and
    // only at names that are free.
    let free = here
        .iter()
        .all(|(suffix, _)| matches!(exists_at(&sibling(path, suffix)), Ok(false)));
    let finish = if !lost && free {
        format!(
            " To finish putting that database back, {} and start again.",
            renames_back(path, &here)
        )
    } else {
        String::new()
    };
    let files = here
        .iter()
        .map(|(_, file)| file.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Some(anyhow::anyhow!(
        "an earlier start was interrupted while it moved the copy of the SQLite database as it was when that import began to its kept name, and that copy is not complete now: {list}. Nothing was changed, and the server never deletes these files.{finish} To go on without that copy, move {files} out of the data directory and start again"
    ))
}

fn valid_stamp(stamp: &str) -> bool {
    stamp.len() == 16 && chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M%SZ").is_ok()
}

fn kept_path(path: &Path, stamp: &str) -> Result<PathBuf> {
    ensure!(valid_stamp(stamp), "invalid kept-copy timestamp");
    Ok(sibling(path, &format!(".import-kept-{stamp}")))
}

/// A UTC name under which nothing exists yet. A second keep in the same
/// second gets the next second; an existing file is never replaced.
fn free_kept_stamp(path: &Path) -> Result<String> {
    let mut time = chrono::Utc::now();
    for _ in 0..3600 {
        let stamp = time.format("%Y%m%dT%H%M%SZ").to_string();
        let base = kept_path(path, &stamp)?;
        let mut free = true;
        for suffix in COMPONENTS {
            free &= !exists_at(&sibling(&base, suffix))?;
        }
        if free {
            return Ok(stamp);
        }
        time += chrono::TimeDelta::seconds(1);
    }
    anyhow::bail!("no free name for the kept copy of the SQLite database")
}

/// Move every staged data component to the recorded kept name with the
/// no-replace primitive of the archive rename. Main last: while it has its
/// staged name the move is visibly unfinished.
fn finish_keep(
    path: &Path,
    journal_path: &Path,
    journal: &mut Journal,
    crash_at: Option<Crash>,
) -> Result<()> {
    let kept = journal
        .kept
        .clone()
        .context("no kept-copy name is recorded")?;
    let to = kept_path(path, &kept.stamp)?;
    let pieces = keep_pieces(path, journal, &kept)?;
    if !pieces
        .iter()
        .any(|(_, place)| matches!(place, Place::Staged(_) | Place::Kept(_)))
    {
        // Nothing of that copy has a staged or a kept name any more: the
        // owner put it back at the original name, or moved it out as a
        // refusal told them. Nothing is left to move, to announce or to
        // delete, and the record stops speaking of a kept copy.
        journal.kept = None;
        save(journal_path, journal)?;
        return Ok(());
    }
    if let Some(refusal) = incomplete_keep(path, &pieces) {
        return Err(refusal);
    }
    for suffix in ["-wal", "-journal", ""] {
        let Some((_, Place::Staged(file))) = pieces.iter().find(|(piece, _)| *piece == suffix)
        else {
            continue;
        };
        let destination = sibling(&to, suffix);
        move_no_replace(file, &destination).map_err(|error| {
            anyhow::anyhow!(
                "cannot keep {} as {}: {error}",
                file.display(),
                destination.display()
            )
        })?;
        if !suffix.is_empty() {
            crash(Crash::KeepHalfMoved, crash_at)?;
        }
    }
    sync_directory(path)?;
    // Before the record says "moved" and before anyone gets the proof: every
    // recorded component, the main file included, is at its kept name with
    // its recorded bytes.
    for (suffix, _) in &pieces {
        let file = sibling(&to, suffix);
        ensure!(
            matches!(kind_at(&file)?, Some(kind) if kind.is_file()),
            "{} of the kept copy is not a regular file at {}; nothing was deleted",
            component_name(suffix),
            file.display()
        );
        if let Some(bytes) = kept.files.get(*suffix) {
            ensure!(
                fingerprint(&file)? == *bytes,
                "{} does not hold the bytes of the copy that was being kept under that name; nothing was deleted. If it is another file, move it out of the data directory and start again",
                file.display()
            );
        }
    }
    crash(Crash::KeepMoved, crash_at)?;
    // Said before the record is updated: a start that dies in between says
    // it again, and the owner never misses it.
    say(kept_warning(path, &kept));
    if let Some(kept) = journal.kept.as_mut() {
        kept.moved = true;
    }
    save(journal_path, journal)?;
    Ok(())
}

fn kept_warning(path: &Path, kept: &Kept) -> String {
    let base = sibling(path, &format!(".import-kept-{}", kept.stamp));
    let sidecars = DATA_COMPONENTS[1..]
        .iter()
        .map(|suffix| sibling(&base, suffix))
        .filter(|file| fs::symlink_metadata(file).is_ok())
        .map(|file| format!(" and {}", file.display()))
        .collect::<String>();
    format!(
        "sqlite import: warning: the copy of the SQLite database that an earlier import attempt made differs from the file now at {}; {} of the database as it was then is kept at {}{sidecars}. The server never deletes it: you may delete it once you have checked your data",
        path.display(),
        if kept.complete {
            "an exact copy"
        } else {
            "a copy of the main file (without its complete write-ahead log)"
        },
        base.display()
    )
}

/// Decision 2: the file that is imported now holds fewer rows than the copy
/// that was kept. A warning only; the import goes on.
fn smaller_warning(path: &Path, journal: &Journal) -> Option<String> {
    let kept = journal.kept.as_ref().filter(|kept| kept.moved)?;
    let counts = kept
        .rows
        .iter()
        .map(|(table, before)| {
            (
                table,
                journal.rows.get(table).copied().unwrap_or(0),
                *before,
            )
        })
        .collect::<Vec<_>>();
    if !counts.iter().any(|(_, now, before)| now < before) {
        return None;
    }
    Some(format!(
        "sqlite import: warning: the database being imported is smaller than the copy kept at {} ({})",
        sibling(path, &format!(".import-kept-{}", kept.stamp)).display(),
        counts
            .iter()
            .map(|(table, now, before)| format!("{table}: {now} now, {before} before"))
            .collect::<Vec<_>>()
            .join("; ")
    ))
}

/// Every data file of every kept database set in the directory:
/// `<name>.import-kept-<UTC>` with or without `-wal` or `-journal`. A sidecar
/// counts without its main file too. This decides whether the server may
/// start with an empty database, so a directory that cannot be listed is an
/// error; a directory that does not exist holds nothing.
fn kept_on_disk(path: &Path) -> Result<Vec<PathBuf>> {
    let Some(base) = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return Ok(Vec::new());
    };
    let prefix = format!("{base}.import-kept-");
    let directory = path.parent().unwrap_or(Path::new("."));
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => anyhow::bail!("cannot read {}: {error}", directory.display()),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry
            .map_err(|error| anyhow::anyhow!("cannot read {}: {error}", directory.display()))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        if DATA_COMPONENTS
            .iter()
            .any(|suffix| rest.strip_suffix(suffix).is_some_and(valid_stamp))
        {
            found.push(directory.join(name.as_ref()));
        }
    }
    found.sort();
    Ok(found)
}

/// One kept set as the way back: its main file, and the exact renames that
/// put the set at the original name. It is the set that this record made, or
/// else the newest one on disk. "Newest" says nothing about what it holds, so
/// every message that offers it also says its rows (`kept_facts`).
fn kept_restore(
    path: &Path,
    journal: Option<&Journal>,
    kept: &[PathBuf],
) -> Option<(PathBuf, String)> {
    let name = |stamp: &str| sibling(path, &format!(".import-kept-{stamp}"));
    let recorded = journal
        .and_then(|journal| journal.kept.as_ref())
        .filter(|record| record.moved)
        .map(|record| name(&record.stamp))
        .filter(|main| kept.contains(main));
    let main = recorded.or_else(|| {
        kept_copies(path)
            .into_iter()
            .map(PathBuf::from)
            .rfind(|main| kept.contains(main))
    })?;
    let pieces = DATA_COMPONENTS
        .iter()
        .map(|suffix| (*suffix, sibling(&main, suffix)))
        .filter(|(_, file)| kept.contains(file))
        .collect::<Vec<_>>();
    let pieces = pieces
        .iter()
        .map(|(suffix, file)| (*suffix, file))
        .collect::<Vec<_>>();
    Some((
        main,
        format!("{}{}", clear_first(path), renames_back(path, &pieces)),
    ))
}

/// What the kept sets hold, for a refusal that leaves the choice between them
/// to the owner: for every main file in `kept`, the rows of the key tables,
/// or that it is empty.
///
/// The counts of the set that this record made are in the record. Any other
/// set is read from its main file with SQLite's `immutable` flag, which takes
/// no lock and creates no file beside it. That flag ignores a write-ahead log
/// and a rollback journal, so a set that has one is not counted (its main
/// file alone may not be what the set holds). A file that cannot be read as a
/// database says so. Nothing here can fail a start.
async fn kept_facts(
    path: &Path,
    journal: Option<&Journal>,
    kept: &[PathBuf],
) -> Vec<(PathBuf, String)> {
    let recorded = journal
        .and_then(|journal| journal.kept.as_ref())
        .filter(|record| record.moved && !record.rows.is_empty())
        .and_then(|record| Some((kept_path(path, &record.stamp).ok()?, &record.rows)));
    let mains = kept_copies(path)
        .into_iter()
        .map(PathBuf::from)
        .filter(|main| kept.contains(main))
        .collect::<Vec<_>>();
    let mut facts = Vec::new();
    for main in mains {
        let rows = match &recorded {
            Some((file, rows)) if *file == main => Ok((*rows).clone()),
            _ if DATA_COMPONENTS[1..]
                .iter()
                .any(|suffix| kept.contains(&sibling(&main, suffix))) =>
            {
                Err("rows not counted: it has a write-ahead log or a rollback journal")
            }
            _ => {
                // Before every file that is read: counting the rows of a
                // large database takes time, and this line is what a
                // supervisor reads as work in progress.
                say_scan(std::slice::from_ref(&main));
                let counted = async {
                    let options = sqlite_options(&main)
                        .read_only(true)
                        .immutable(true)
                        .foreign_keys(false);
                    let mut conn = SqliteConnection::connect_with(&options).await?;
                    let rows = key_rows(&mut conn).await;
                    let _ = conn.close().await;
                    rows
                }
                .await;
                counted.map_err(|_| "rows not counted: it could not be read as a database")
            }
        };
        let text = match rows {
            Ok(rows) if rows.values().all(|count| *count == 0) => {
                format!("empty: no rows in {}", KEY_TABLES.join(", "))
            }
            Ok(rows) => rows
                .iter()
                .map(|(table, count)| format!("{table}: {count}"))
                .collect::<Vec<_>>()
                .join(", "),
            Err(why) => why.to_owned(),
        };
        facts.push((main, text));
    }
    facts
}

/// "K (what it holds)" for a kept main file, the plain name for any other.
fn with_facts(file: &Path, facts: &[(PathBuf, String)]) -> String {
    match facts.iter().find(|(main, _)| main == file) {
        Some((_, text)) => format!("{} ({text})", file.display()),
        None => file.display().to_string(),
    }
}

/// Kept copies of earlier starts that are still on disk (main files only).
fn kept_copies(path: &Path) -> Vec<String> {
    let Some(base) = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return Vec::new();
    };
    let prefix = format!("{base}.import-kept-");
    let mut found = fs::read_dir(path.parent().unwrap_or(Path::new(".")))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.strip_prefix(&prefix).is_some_and(valid_stamp)
        })
        .map(|entry| entry.path().display().to_string())
        .collect::<Vec<_>>();
    found.sort();
    found
}

/// What the owner can restore the SQLite database from, for a refusal. This
/// only reads. `valid` says whether the file at the original name is usable.
struct Copies {
    /// Sentences for the message; empty or starting with a space.
    note: String,
    /// The exact renames that put the staged copy back at the original name.
    restore: Option<String>,
    /// The same for a kept copy: (which one and what it holds, the renames).
    kept_restore: Option<(String, String)>,
    /// More than one kept copy is on disk.
    kept_choice: bool,
    /// Every file of every kept set: what an owner must move away before the
    /// server may start with an empty database.
    kept: Vec<String>,
}

/// The line that precedes a recovery scan which hashes whole files. It has
/// the shape of the source inventory line (it IS an inventory: sizes first,
/// then every byte), which a supervisor already reads as work in progress, so
/// a healthy scan of a large file is not taken for a hung start.
fn say_scan(files: &[PathBuf]) {
    let bytes = files
        .iter()
        .filter_map(|file| fs::symlink_metadata(file).ok())
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
        .sum::<u64>();
    say(format!(
        "sqlite import: source inventory start: {bytes} bytes"
    ));
}

async fn copies(path: &Path, journal: &Journal, valid: bool, progress: &mut Progress) -> Copies {
    let base = sibling(path, staged_base(journal));
    let mut note = String::new();
    let mut restore = None;
    let only_copy = " Do not delete that copy unless you have another one: it may be the only one.";
    let moving = journal.kept.as_ref().filter(|kept| !kept.moved);
    if let Some(kept) = moving {
        // An interrupted move: the copy is in two places. Every recorded
        // component is looked for at both names, so that the renames below
        // put back the whole database and not the part that had not moved.
        // Looking may hash a file, so the first line comes before it.
        progress.announce();
        match keep_pieces(path, journal, kept) {
            Ok(pieces) => {
                let here = pieces
                    .iter()
                    .filter_map(|(suffix, place)| match place {
                        Place::Staged(file) | Place::Kept(file) => Some((*suffix, file)),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                // A file at a kept name is a piece of the copy only with the
                // bytes that were recorded for it: the name alone proves
                // nothing, and the renames below would hand it to the owner
                // as a part of their database.
                let moved = pieces
                    .iter()
                    .filter_map(|(suffix, place)| match place {
                        Place::Kept(file) => Some((file, kept.files.get(*suffix)?)),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if !moved.is_empty() {
                    say_scan(
                        &moved
                            .iter()
                            .map(|(file, _)| (*file).clone())
                            .collect::<Vec<_>>(),
                    );
                }
                let foreign = moved
                    .into_iter()
                    .filter(|(file, bytes)| fingerprint(file).ok().as_ref() != Some(*bytes))
                    .map(|(file, _)| file)
                    .collect::<Vec<_>>();
                if !here.is_empty() {
                    let whole = here.len() == pieces.len() && foreign.is_empty();
                    note = format!(
                        " {} of the database as it was when the import began was being moved aside when an earlier start stopped, and is now at these names: {}.",
                        if whole && kept.complete {
                            "An exact copy"
                        } else if whole {
                            "A copy of the main file (without its complete write-ahead log)"
                        } else {
                            "A part of the copy"
                        },
                        pieces
                            .iter()
                            .map(|(suffix, place)| match place {
                                Place::Kept(file) if foreign.contains(&file) => format!(
                                    "{} at {} (not the bytes that were recorded)",
                                    component_name(suffix),
                                    file.display()
                                ),
                                Place::Staged(file) | Place::Kept(file) =>
                                    format!("{} at {}", component_name(suffix), file.display()),
                                Place::Original(file) => format!(
                                    "{} put back at {}",
                                    component_name(suffix),
                                    file.display()
                                ),
                                Place::Missing => format!("{} missing", component_name(suffix)),
                            })
                            .collect::<Vec<_>>()
                            .join("; ")
                    );
                    if !valid {
                        note.push_str(only_copy);
                    }
                    // Only a whole set is offered as the way back.
                    if whole {
                        restore = Some(format!(
                            "{}{}",
                            clear_first(path),
                            renames_back(path, &here)
                        ));
                    }
                }
            }
            Err(error) => {
                note = format!(
                    " The copy that was being moved aside could not be checked ({}).",
                    super::cli::safe_error(&error, "")
                );
            }
        }
    } else if !matches!(kind_at(&base), Ok(None)) {
        // The staged copy is hashed below.
        progress.announce();
        say_scan(
            &DATA_COMPONENTS
                .iter()
                .map(|suffix| sibling(&base, suffix))
                .collect::<Vec<_>>(),
        );
        match staged(path, journal) {
            Ok(Staged::Copy { complete }) => {
                let mut renames = vec![format!("rename {} to {}", base.display(), path.display())];
                for suffix in &DATA_COMPONENTS[1..] {
                    let file = sibling(&base, suffix);
                    if fs::symlink_metadata(&file).is_ok()
                        && !left_by_sqlite(&file, suffix, journal).unwrap_or(false)
                    {
                        renames.push(format!(
                            "{} to {}",
                            file.display(),
                            sibling(path, suffix).display()
                        ));
                    }
                }
                note = format!(
                    " {} of the database as it was when the import began is at {}.",
                    if complete {
                        "An exact copy"
                    } else {
                        "A copy of the main file (without its complete write-ahead log)"
                    },
                    base.display()
                );
                if !valid {
                    note.push_str(only_copy);
                }
                restore = Some(format!("{}{}", clear_first(path), renames.join(", ")));
            }
            Ok(_) => {}
            Err(error) => {
                note = format!(
                    " {} could not be checked ({}).",
                    base.display(),
                    super::cli::safe_error(&error, "")
                );
            }
        }
    }
    // Kept sets of earlier starts, every file of them. The pieces of an
    // unfinished move are named above already.
    let (kept, unlisted) = match kept_on_disk(path) {
        Ok(found) => (found, String::new()),
        Err(error) => (
            Vec::new(),
            format!(
                " Kept copies could not be listed ({}).",
                super::cli::safe_error(&error, "")
            ),
        ),
    };
    note.push_str(&unlisted);
    let moving_base = moving.and_then(|kept| kept_path(path, &kept.stamp).ok());
    let earlier = kept
        .iter()
        .filter(|file| {
            !moving_base.as_ref().is_some_and(|base| {
                DATA_COMPONENTS
                    .iter()
                    .any(|suffix| sibling(base, suffix) == **file)
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    if !earlier.is_empty() {
        // A kept copy may be read to count its rows.
        progress.announce();
    }
    let facts = kept_facts(path, Some(journal), &earlier).await;
    if !earlier.is_empty() {
        note.push_str(&format!(
            " Kept by earlier starts (each is the SQLite database as it was when an earlier import attempt began): {}.",
            earlier
                .iter()
                .map(|file| with_facts(file, &facts))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let kept_restore = kept_restore(path, Some(journal), &earlier)
        .map(|(main, renames)| (with_facts(&main, &facts), renames));
    Copies {
        note,
        restore,
        kept_restore,
        kept_choice: facts.len() > 1,
        kept: kept.iter().map(|file| file.display().to_string()).collect(),
    }
}

/// Staged and work copies that exist without a startup record.
fn unrecorded_copies(path: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for base in [".import-work-source", ".import-work"] {
        for suffix in DATA_COMPONENTS {
            let file = sibling(path, &format!("{base}{suffix}"));
            if exists_at(&file)? {
                found.push(file);
            }
        }
    }
    Ok(found)
}

/// ", delete A, B" for an instruction that ends with a new import of the file
/// at the original name: every file with a work name that is not renamed.
/// The data copies come first, then the lock and index files of the work
/// names, because a start that imports refuses beside any of them. Only work
/// files are ever named for deletion, never a kept copy.
fn delete_work(path: &Path, copies: &[PathBuf], renamed: &[PathBuf]) -> String {
    let mut files = copies.to_vec();
    for name in work_suffixes() {
        let file = sibling(path, &name);
        if !files.contains(&file) && !matches!(exists_at(&file), Ok(false)) {
            files.push(file);
        }
    }
    let files = files
        .iter()
        .filter(|file| !renamed.contains(file))
        .map(|file| file.display().to_string())
        .collect::<Vec<_>>();
    if files.is_empty() {
        String::new()
    } else {
        format!(", delete {}", files.join(", "))
    }
}

/// No SQLite file, no record, an empty PostgreSQL database, and copies of an
/// earlier attempt: starting now would turn the import off for good while the
/// only copy of the owner's database may be one of these files.
async fn unrecorded_refusal(path: &Path, copies: &[PathBuf], kept: &[PathBuf]) -> anyhow::Error {
    let source = path.display();
    let list = copies
        .iter()
        .chain(kept)
        .map(|file| file.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let staged = sibling(path, ".import-work-source");
    let import = if copies.contains(&staged) {
        let mut renames = vec![format!("rename {} to {source}", staged.display())];
        let mut renamed = vec![staged.clone()];
        for suffix in &DATA_COMPONENTS[1..] {
            let file = sibling(&staged, suffix);
            if copies.contains(&file) {
                renames.push(format!(
                    "{} to {}",
                    file.display(),
                    sibling(path, suffix).display()
                ));
                renamed.push(file);
            }
        }
        let delete = delete_work(path, copies, &renamed);
        format!(
            " {} is a copy of the SQLite database as it was when that import began, and it may be the only one. To import it, {}{}{delete} and start again. Or, to",
            staged.display(),
            clear_first(path),
            renames.join(", ")
        )
    } else if let Some((newest, renames)) = kept_restore(path, None, kept) {
        let facts = kept_facts(path, None, kept).await;
        let others = facts
            .iter()
            .filter(|(main, _)| *main != newest)
            .map(|(main, _)| with_facts(main, &facts))
            .collect::<Vec<_>>();
        let others = if others.is_empty() {
            String::new()
        } else {
            format!(
                " The other kept copies, which are imported the same way: {}.",
                others.join(", ")
            )
        };
        let newest = with_facts(&newest, &facts);
        let delete = delete_work(path, copies, &[]);
        format!(
            " A file named {source}.import-kept-<time> is the SQLite database as it was when an earlier import attempt began, kept aside by the server, and it may be the only copy. To import the newest one, {newest}, {renames}{delete} and start again.{others} Or, to"
        )
    } else {
        " To".to_owned()
    };
    anyhow::anyhow!(
        "the SQLite file {source} is missing, and files of an earlier import attempt are here without their record: {list}. Nothing was changed.{import} start with an empty database, move the listed files out of the data directory and start again"
    )
}

const EMPTY_DATABASE: &str = "empty the PostgreSQL database (drop it and create it again)";

/// A state that the server must not repair by itself. Nothing was changed
/// when this is returned. The message names the record file, the work files,
/// every copy of the SQLite database that exists, and the exact manual steps.
async fn refusal(
    path: &Path,
    journal: &Journal,
    status: Target,
    why: Refusal,
    unusable: &str,
    progress: &mut Progress,
) -> anyhow::Error {
    let record = sibling(path, ".import-work-state");
    let record = record.display();
    let files = work_files(path, journal);
    let source = path.display();
    let valid = !matches!(why, Refusal::SourceGone | Refusal::SourceUnusable)
        && present(path) == Present::File;
    let Copies {
        note,
        restore,
        kept_restore,
        kept_choice,
        kept,
    } = copies(path, journal, valid, progress).await;
    let forget = format!("delete {record} and the work files {files}");
    // A way out that ends with an empty database and no SQLite file must also
    // take the kept copies away: the server does not start empty beside them.
    let forget_all = if kept.is_empty() {
        forget.clone()
    } else {
        format!(
            "{forget}, move {} out of the data directory",
            kept.join(", ")
        )
    };
    let or_kept = kept_restore
        .as_ref()
        .map_or_else(String::new, |(which, renames)| {
            format!(
                " Or, to import the kept copy {which} instead, {renames} and start again{}.",
                if kept_choice {
                    "; any other kept copy is imported the same way"
                } else {
                    ""
                }
            )
        });
    let message = match why {
        Refusal::Unrelated => format!(
            "this PostgreSQL database already holds other data, while {record} records an unfinished import of {source}. Nothing was changed.{note} If this is the right database, {forget} and start again: the server then starts and leaves {source} in place. If it is the wrong database, correct DATABASE_URL"
        ),
        Refusal::NotOurs if status == Target::Complete => format!(
            "this PostgreSQL database holds a completed import that {record} did not make. Nothing was changed.{note} If this is the right database, {forget} and start again: the server then starts and leaves {source} in place. If it is the wrong database, correct DATABASE_URL"
        ),
        Refusal::NotOurs => format!(
            "this PostgreSQL database holds an unfinished import that {record} did not start. Nothing was changed.{note} If this is the wrong database, correct DATABASE_URL. To import {source} into this database, {EMPTY_DATABASE} and start again"
        ),
        Refusal::SourceGone => {
            // The staged copy is the way back. Without one, a kept copy is
            // offered beside "put the file back", as what it is.
            let back = match &restore {
                Some(renames) => format!("{renames} and start again."),
                None => format!("put {source} back and start again.{or_kept}"),
            };
            match status {
                Target::Complete => format!(
                    "the SQLite file {source} named by the unfinished import is missing, and the data PostgreSQL received was not yet verified against it. Nothing was changed.{note} To continue, {back} Or, to keep what PostgreSQL holds without that check, {forget} and start again. Or, to start with an empty database, {EMPTY_DATABASE}, {forget_all} and start again"
                ),
                Target::Unfinished => format!(
                    "the SQLite file {source} named by the unfinished import is missing, and PostgreSQL holds only part of its data. Nothing was changed.{note} To continue, {back} Or, to start with an empty database, {EMPTY_DATABASE}, {forget_all} and start again"
                ),
                _ => format!(
                    "the SQLite file {source} named by an unfinished import is missing. Nothing was changed.{note} To continue, {back} Or, to start with an empty database, {forget_all} and start again"
                ),
            }
        }
        Refusal::SourceUnusable => {
            let or_restore = restore.or(kept_restore.map(|(_, renames)| renames)).map_or_else(String::new, |renames| {
                format!(" (if the file is damaged: {renames})")
            });
            let and_empty = if status == Target::Unfinished {
                format!(", {EMPTY_DATABASE}")
            } else {
                String::new()
            };
            format!(
                "the SQLite file {source} named by an unfinished import cannot be used: {unusable}. Nothing was changed: no file was removed and PostgreSQL was not touched.{note} To continue, correct that{or_restore} and start again. Or, to give up this import, move {source} out of the data directory, {forget_all}{and_empty} and start again"
            )
        }
        Refusal::Changed(Change::Source) if status == Target::Complete => format!(
            "{source} changed after PostgreSQL received its data, so the import can no longer be verified and the file is left in place. Nothing was changed.{note} To keep what PostgreSQL holds, move {source} to another directory, {forget} and start again. Or, only if PostgreSQL holds nothing that you need, {EMPTY_DATABASE} and start again: that imports the current file from the beginning"
        ),
        Refusal::Changed(change) => format!(
            "the unfinished command-line import cannot continue: {}. Nothing was changed.{note} The server never resets an import that the command line started. To import {source} from the beginning, {EMPTY_DATABASE} and start again",
            change.reason()
        ),
        Refusal::RenameWithoutImport => format!(
            "{record} says that verification passed, but this PostgreSQL database does not hold that completed import. Nothing was changed.{note} Restore the PostgreSQL backup and start again. Or, to import again: move every file named {source}.imported-* (with its -wal, -shm and -journal siblings) back to its original name, {forget}, {EMPTY_DATABASE} and start again"
        ),
    };
    anyhow::anyhow!(message)
}

/// Appended to stage errors when PostgreSQL holds a completed import, or a
/// command-line import, that the server must never reset. Two ways out, the
/// one that keeps PostgreSQL first, and what each of them discards.
fn stuck(path: &Path, journal: &Journal, complete: bool) -> String {
    let source = path.display();
    if !complete {
        return format!(
            " (the SQLite file {source} was not changed. This import was started with the import-sqlite-to-postgres command, and the server never resets it. If this error repeats: finish it with that command; or, only if PostgreSQL holds nothing that you need, {EMPTY_DATABASE} and start again, which discards the partly imported rows (they are still in {source}) and imports the file from the beginning)"
        );
    }
    format!(
        " (the SQLite file {source} was not changed. If this error repeats there are two ways out. 1, to keep what PostgreSQL holds now: stop the server, move {source} and any file beside it named {source}-wal, {source}-shm or {source}-journal out of the data directory, delete {} and the work files {}, and start; this discards nothing in PostgreSQL, and whatever is only in the SQLite file stays in the file you moved. 2, only if PostgreSQL holds nothing that you need: {EMPTY_DATABASE} and start again; this discards everything PostgreSQL holds now and imports {source} from the beginning)",
        sibling(path, ".import-work-state").display(),
        work_files(path, journal)
    )
}

/// The archive rename of a Done record was interrupted (a failed journal sync
/// moved the files back, possibly only some of them): the pinned work copy is
/// still there, the main file is at its original name, and every recorded
/// component is found with its recorded bytes at the original or the archive
/// name. Any other file at the original name is not this state.
fn interrupted_archive(path: &Path, archive: &Path, journal: &Journal) -> Result<bool> {
    let work = sibling(path, ".import-work");
    if !fs::symlink_metadata(&work).is_ok_and(|m| m.file_type().is_file())
        || !work_is_pinned(path, journal)
    {
        return Ok(false);
    }
    for suffix in COMPONENTS {
        let found = [sibling(path, suffix), sibling(archive, suffix)]
            .into_iter()
            .find(|file| fs::symlink_metadata(file).is_ok());
        let same = match (journal.original.get(*suffix), found) {
            (None, None) => true,
            (Some(expected), Some(file)) => {
                fs::symlink_metadata(&file)?.file_type().is_file()
                    && fingerprint(&file)? == *expected
            }
            _ => false,
        };
        if !same {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Finish the interrupted archive rename of a Done record. Nothing is renamed
/// before the verifier passed in this process.
async fn finish_archive(
    path: &Path,
    url: &str,
    journal: &Journal,
    progress: &mut Progress,
    target: &mut PgConnection,
) -> Result<()> {
    require_private_directory(path)?;
    sqlite_temporary_directory(Some(path.parent().unwrap_or(Path::new(".")))).await?;
    progress.temporary = true;
    for stage in [Stage::Verify, Stage::Rename] {
        say(format!("sqlite import: {} start", stage.name()));
        let clock = Instant::now();
        if stage == Stage::Verify {
            super::importer::bring_up_to_date(&mut *target).await?;
            let report = super::verifier::run(&sibling(path, ".import-work"), url).await?;
            ensure!(report.complete, "verifier did not pass");
            say(format!(
                "sqlite import: verifier PASS: {} tables",
                report.tables.len()
            ));
        } else if let Err(error) = archive(path, journal, None) {
            if let Err(undo) = rollback_archive(path, journal) {
                say(format!(
                    "sqlite import: warning: archived SQLite components were not moved back: {}",
                    super::cli::safe_error(&undo, url)
                ));
            }
            return Err(error);
        }
        say(format!(
            "sqlite import: {} complete: {:.3}s",
            stage.name(),
            clock.elapsed().as_secs_f64()
        ));
    }
    Ok(())
}

/// Work files of a Done record. The cutover is complete and verified, so the
/// staged copy goes too (condition (i)).
fn discard_done(path: &Path, journal: &Journal) -> Result<()> {
    discard_work(path, journal, Some(Disposable::after_cutover(journal)?))
}

/// A Done record is never downgraded. With a file at the original name, only
/// an interrupted archive rename is finished; in every other case the record
/// and the file are left alone and the server starts on PostgreSQL.
///
/// `true` when the archive rename was finished here: verification passed in
/// this process and the file has its archive name.
async fn settle_done(
    path: &Path,
    url: &str,
    journal: &Journal,
    progress: &mut Progress,
    target: &mut PgConnection,
) -> bool {
    let archive = archive_path(path, journal).ok();
    if fs::symlink_metadata(sibling(path, ".import-work")).is_ok() {
        // The work copy and every original component are hashed next.
        progress.announce();
        // Exactly what `interrupted_archive` reads: of every component the
        // file at the original name, or else the archived one.
        let mut scanned = vec![sibling(path, ".import-work")];
        for suffix in COMPONENTS {
            let found = [
                Some(sibling(path, suffix)),
                archive.as_ref().map(|a| sibling(a, suffix)),
            ]
            .into_iter()
            .flatten()
            .find(|file| fs::symlink_metadata(file).is_ok());
            scanned.extend(found);
        }
        say_scan(&scanned);
    }
    let interrupted = archive
        .as_ref()
        .is_some_and(|archive| interrupted_archive(path, archive, journal).unwrap_or(false));
    if interrupted {
        progress.announce();
        match finish_archive(path, url, journal, progress, target).await {
            Ok(()) => {
                if let Err(error) = discard_done(path, journal) {
                    say(format!("sqlite import: warning: cutover succeeded; work cleanup will retry on next start: {}", super::cli::safe_error(&error, url)));
                }
                return true;
            }
            Err(error) => say(format!(
                "sqlite import: warning: could not finish the earlier archive rename: {}",
                super::cli::safe_error(&error, url)
            )),
        }
    }
    // The server now serves PostgreSQL, so the work copy can never again prove
    // anything about this file. Removing it keeps later starts cheap.
    if let Err(error) = discard_done(path, journal) {
        say(format!(
            "sqlite import: warning: cutover succeeded; work cleanup will retry on next start: {}",
            super::cli::safe_error(&error, url)
        ));
    }
    let name = archive
        .as_ref()
        .and_then(|archive| archive.file_name())
        .map_or_else(
            || "not recorded".into(),
            |name| name.to_string_lossy().into_owned(),
        );
    say(format!(
        "sqlite import: warning: PostgreSQL already holds the imported data; leaving {} in place (earlier archive: {name})",
        path.display()
    ));
    false
}

/// Begin again from Copy. The order is the safety rule of fix round 4: the
/// staged copy is made safe first, PostgreSQL is reset next, and only then is
/// any work file removed. A refused reset therefore leaves every file as it
/// was (a copy that had to be kept has its kept name by then). The caller has
/// already read and checked the file at the original name.
///
/// Returns the warnings about unrecorded leftover files. The caller says them
/// after the Copy stage, so that the kept-copy and the smaller-database
/// warning come first.
async fn begin_again(
    path: &Path,
    journal_path: &Path,
    journal: &mut Journal,
    reset: bool,
    target: &mut PgConnection,
    url: &str,
    faults: Faults,
) -> Result<Vec<String>> {
    let keep = keep_decision(path, journal)?;
    if matches!(keep, Keep::Needed { .. }) && journal.rows.is_empty() {
        // A kill between the snapshot and the Copy checkpoint left the record
        // without the row counts of the copy that is kept now. They are read
        // from that copy, so that the smaller-database warning can be given.
        journal.rows = staged_rows(path, journal, !reset).await;
    }
    let proof = protect_staged_as(path, journal_path, journal, faults.crash, keep)?;
    crash(Crash::BeforeReset, faults.crash)?;
    if reset {
        (faults.before_reset)(url);
        reset_target(target, journal).await?;
    }
    crash(Crash::AfterReset, faults.crash)?;
    remove_work(path, journal, Some(proof))?;
    let later = unrecorded(path, Some(journal))?;
    crash(Crash::WorkDiscarded, faults.crash)?;
    journal.restart();
    save(journal_path, journal)?;
    crash(Crash::Reserved, faults.crash)?;
    Ok(later)
}

/// Row counts of the staged copy of a startup import, read without changing
/// one of its bytes, exactly as `snapshot` reads it: a read-only connection
/// (which also reads a committed write-ahead log), or, for a copy with a
/// rollback journal, a playback in the derived work copy. The work copy is
/// only used when the caller says so: at the Copy stage, where it is not
/// pinned, PostgreSQL holds nothing of it and the next Copy stage replaces it.
/// Never a reason to stop: without the counts only the second warning is
/// missing, and that is what an empty answer means.
async fn staged_rows(path: &Path, journal: &Journal, may_use_work: bool) -> BTreeMap<String, u64> {
    if journal.cli_resume {
        return BTreeMap::new();
    }
    let staging = sibling(path, ".import-work-source");
    let counted = async {
        if exists_at(&sibling(&staging, "-journal"))? {
            ensure!(
                may_use_work && journal.stage == Stage::Copy,
                "the staged copy has a rollback journal"
            );
            let work = sibling(path, ".import-work");
            remove_database(path, ".import-work", journal)?;
            create_private(&work)?.sync_all()?;
            let mut parts = Vec::new();
            for suffix in DATA_COMPONENTS {
                if exists_at(&sibling(&staging, suffix))? {
                    parts.push((*suffix).to_owned());
                }
            }
            recover_into_work(&staging, &work, &parts).await
        } else {
            let options = sqlite_options(&staging).read_only(true).foreign_keys(false);
            let mut conn = SqliteConnection::connect_with(&options).await?;
            let rows = key_rows(&mut conn).await;
            let _ = conn.close().await;
            rows
        }
    }
    .await;
    counted.unwrap_or_default()
}

/// Everything between the decision and the first stage: the archive probe,
/// the source inventory of a fresh reservation, and the binding.
async fn prepare(
    path: &Path,
    journal_path: &Path,
    journal: &mut Journal,
    target: &mut PgConnection,
    faults: Faults,
    source_is_file: bool,
) -> Result<()> {
    // The probe is recorded and finished before binding/baseline/COPY writes.
    (faults.probe)(path, journal)
        .context("archive capability probe failed before PostgreSQL writes")?;
    if !journal.inventory_ready {
        journal.original = inventory(path)?;
        super::preflight::check(path)?;
        journal.inventory_ready = true;
        journal.prepared_migrations = migrations_fingerprint();
        journal.prepared_plan = plan_fingerprint();
        save(journal_path, journal)?;
    } else if source_is_file {
        // Downgrading the binary must refuse a newer source on resume too.
        super::preflight::check(path)?;
    }
    bind_target(target, journal).await
}

async fn cutover(
    path: &Path,
    url: &str,
    faults: Faults,
    progress: &mut Progress,
    connection: &mut Option<PgConnection>,
) -> Result<()> {
    let start = Instant::now();
    // Read once, before anything else. A name that cannot even be examined is
    // refused with the path and the OS error; it is never read as "no file".
    let here = present(path);
    if let Present::Unreadable(error) = &here {
        anyhow::bail!(
            "the SQLite file {} cannot be read: {error}. Nothing was changed. Correct that and start again",
            path.display()
        );
    }
    let has_work = here != Present::Missing || exists_at(&sibling(path, ".import-work-state"))?;
    let resolved = if has_work {
        resolve_source(path)?
    } else {
        path.to_owned()
    };
    let path = resolved.as_path();
    let journal_path = sibling(path, ".import-work-state");
    let mut journal = load(&journal_path)?;
    // Known without PostgreSQL: an unfinished record with something at the
    // original name (or whose rename is under way) means work. The line
    // precedes the connection and every source byte, so a supervisor can
    // extend its start timeout.
    if journal.as_ref().is_some_and(|j| {
        j.stage == Stage::Rename || (j.stage != Stage::Done && here != Present::Missing)
    }) {
        progress.announce();
    }
    // The caller owns the connection, so that every return below releases it.
    let target = connection.insert(super::connect(url).await?);
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(192837465,180002)")
        .fetch_one(&mut *target)
        .await?;
    ensure!(locked, "another importer owns the target");
    let mut status = inspect(&mut *target).await?;
    let binding = startup_binding(&mut *target).await?;
    let identity = target_identity(&mut *target).await?;

    if journal.as_ref().is_some_and(|j| j.stage == Stage::Done) {
        let mut record = journal.take().expect("checked above");
        if status == Target::Complete && identity.as_ref() == Some(&record.target) {
            let mut archived_now = false;
            if here != Present::Missing {
                archived_now = settle_done(path, url, &record, progress, &mut *target).await;
            } else if let Err(error) = discard_done(path, &record) {
                say(format!("sqlite import: warning: cutover succeeded; work cleanup will retry on next start: {}", super::cli::safe_error(&error, url)));
            }
            mark_normal_start(&mut *target).await?;
            if archived_now {
                // This start printed the verify and rename lines of the
                // cutover and finished it: it ends with the line that every
                // import that ends well ends with.
                say(format!(
                    "sqlite import: complete: {:.3}s total",
                    start.elapsed().as_secs_f64()
                ));
            }
            return Ok(());
        }
        progress.stage = "recovery";
        let archive = archive_path(path, &record)?;
        ensure!(
            here == Present::File && status == Target::Empty,
            "archive {} has no matching completed PostgreSQL binding; restore the PostgreSQL backup, or rename the archive back to {} to import again into an EMPTY database",
            archive.display(),
            path.display()
        );
        // Operator restored the original name. Start a fresh import only
        // after proving the target empty; never reuse old PG state.
        progress.announce();
        require_private_directory(path)?;
        ensure!(
            inventory(path)? == record.original,
            "the SQLite file at {} is not the archived original; to import this file into the empty database, delete {} and start again (the earlier archive is left untouched)",
            path.display(),
            journal_path.display()
        );
        // The same order as every restart: the file is checked first, a
        // leftover staged copy goes only by the rule, the record goes last.
        super::preflight::check(path)?;
        let proof = protect_staged(path, &journal_path, &mut record, faults.crash)?;
        discard_work(path, &record, Some(proof))?;
        fs::remove_file(&journal_path)?;
        sync_directory(path)?;
        progress.stage = "decision";
    }

    // At most one checked restart per process start.
    let mut restarted = false;
    if let Some(record) = journal.as_mut() {
        ensure!(
            work_suffixes()
                .iter()
                .all(|name| record.created.contains(name)),
            "startup record {} lacks work-file ownership; leaving unrecorded files alone",
            journal_path.display()
        );
        let owned = owns(record, binding.as_ref(), identity.as_ref());
        // Rule 1: the file at the original name is read and checked before
        // the decision. During a rename it may rightly be archived already.
        let (source, unusable) = if record.stage == Stage::Rename {
            (Source::Valid, String::new())
        } else {
            source_state(path, &here)
        };
        let comparable = owned
            && source == Source::Valid
            && record.stage != Stage::Rename
            && matches!(status, Target::Unfinished | Target::Complete);
        let change = if comparable {
            changed(path, record)?
        } else {
            None
        };
        // What an empty start would leave behind unimported: recorded work
        // files, and every kept database set (the one this record made and
        // those of earlier starts).
        let leftovers = !recorded_work(path, record)?.is_empty() || !kept_on_disk(path)?.is_empty();
        // A copy whose move aside was interrupted and is no longer whole is
        // never moved further, announced or deleted. With a usable file at
        // the original name the start would go on and do one of these.
        if source == Source::Valid {
            if let Some(kept) = record.kept.as_ref().filter(|kept| !kept.moved) {
                if let Some(refusal) = incomplete_keep(path, &keep_pieces(path, record, kept)?) {
                    progress.stage = "recovery";
                    return Err(refusal);
                }
            }
        }
        // One decision per process start: this code runs once.
        match pending(
            record.stage,
            status,
            owned,
            record.cli_resume,
            source,
            change,
            leftovers,
        ) {
            Pending::Resume => {
                require_private_directory(path)?;
                let stage = resume_stage(record.stage, status);
                if stage != record.stage {
                    // PostgreSQL was put back to a point before the import
                    // was complete. The import is resumed: that compares the
                    // copy PostgreSQL pinned with the one in this directory
                    // and restarts once, by the reset rules, when they differ.
                    record.stage = stage;
                    save(&journal_path, record)?;
                }
            }
            Pending::Refuse(why) => {
                progress.stage = "recovery";
                return Err(refusal(path, record, status, why, &unusable, progress).await);
            }
            Pending::StartEmpty => {
                progress.stage = "recovery";
                // No recorded work file exists (`leftovers` is false): only
                // empty scratch names and the record itself are removed.
                discard_work(path, record, None)?;
                // The record must be gone before the server writes anything.
                fs::remove_file(&journal_path)?;
                sync_directory(path)?;
                say("sqlite import: warning: the SQLite file named by an unfinished import is gone; starting with an empty database".into());
                mark_normal_start(&mut *target).await?;
                return Ok(());
            }
            Pending::Restart => {
                require_private_directory(path)?;
                progress.announce();
                if record.inventory_ready {
                    say("sqlite import: restarting from the beginning: the earlier attempt stopped before PostgreSQL held any imported data".into());
                }
                let later = begin_again(
                    path,
                    &journal_path,
                    record,
                    false,
                    &mut *target,
                    url,
                    faults,
                )
                .await?;
                progress.later.extend(later);
                restarted = true;
            }
            Pending::Reset(change) => {
                require_private_directory(path)?;
                progress.announce();
                say(format!(
                    "sqlite import: restarting from the beginning: {}",
                    change.reason()
                ));
                let later =
                    begin_again(path, &journal_path, record, true, &mut *target, url, faults)
                        .await?;
                progress.later.extend(later);
                status = Target::Empty;
                restarted = true;
            }
        }
    } else {
        match decision(here != Present::Missing, status, false) {
            Decision::Nothing => {
                // No file and no record. Copies of an attempt whose record
                // was deleted by hand are never removed, and the server does
                // not start with an empty database beside them.
                // A kept set counts as well: it may be the only good copy.
                let mut copies = unrecorded_copies(path)?;
                if !copies.is_empty() || !kept_on_disk(path)?.is_empty() {
                    // Name them as every other message does.
                    let resolved = resolve_source(path)?;
                    copies = unrecorded_copies(&resolved)?;
                    if status == Target::Empty {
                        let kept = kept_on_disk(&resolved)?;
                        if !kept.is_empty() {
                            // A kept copy may be read to count its rows.
                            progress.announce();
                        }
                        return Err(unrecorded_refusal(&resolved, &copies, &kept).await);
                    }
                    // An occupied database starts. Kept copies stay for good
                    // and are not mentioned on every start; work files are.
                }
                for copy in &copies {
                    say(format!(
                        "sqlite import: warning: unrecorded file {}; leaving it alone",
                        copy.display()
                    ));
                }
                mark_normal_start(&mut *target).await?;
                return Ok(());
            }
            Decision::Warn => {
                mark_normal_start(&mut *target).await?;
                say("sqlite import: warning: PostgreSQL already holds data or a completed import; leaving SQLite in place".into());
                return Ok(());
            }
            Decision::Refuse => anyhow::bail!(
                "unfinished import has no SQLite source; refusing to serve. Put the SQLite file back at {} and start again, or {EMPTY_DATABASE} to start with an empty database",
                path.display()
            ),
            Decision::Import | Decision::Resume => {}
        }
        if let Present::Blocked(what) = &here {
            // Never "gone": no empty start, no normal-start marker.
            anyhow::bail!(
                "the SQLite file {} cannot be used: {what}. Nothing was changed. Correct that and start again; or, to start without importing it, move it out of the data directory",
                path.display()
            );
        }
        // An unfinished import with a startup binding belongs to a record this
        // directory does not have. It is never adopted and never reset.
        ensure!(
            !(status == Target::Unfinished && binding.is_some()),
            "this PostgreSQL database holds an unfinished import, and {} is missing here (it was deleted, or the import was started from another data directory). Nothing was changed. To import {} into this database, {EMPTY_DATABASE} and start again",
            journal_path.display(),
            path.display()
        );
        progress.announce();
        require_private_directory(path)?;
        ensure_work_absent(path)?;
        let record = if status == Target::Unfinished {
            Journal::fresh(
                identity
                    .clone()
                    .context("unfinished import has no source identity")?,
                true,
            )
        } else {
            Journal::fresh(uuid::Uuid::new_v4().to_string(), false)
        };
        save(&journal_path, &record)?;
        journal = Some(record);
    }

    let mut journal = journal.context("startup import record is missing")?;
    progress.announce();
    // From here on SQLite's own temporary files go to the work directory.
    sqlite_temporary_directory(Some(path.parent().unwrap_or(Path::new(".")))).await?;
    progress.temporary = true;
    let source_is_file = here == Present::File;
    prepare(
        path,
        &journal_path,
        &mut journal,
        &mut *target,
        faults,
        source_is_file,
    )
    .await?;
    if status == Target::Complete || journal.cli_resume {
        progress.advice = stuck(path, &journal, status == Target::Complete);
    }
    let work = sibling(path, ".import-work");
    let mut verified_here = false;
    while journal.stage != Stage::Done {
        let current = journal.stage;
        progress.stage = current.name();
        (faults.before_stage)(path, current);
        say(format!("sqlite import: {} start", current.name()));
        let clock = Instant::now();
        if current != Stage::Rename {
            ensure!(
                inventory(path)? == journal.original,
                "original SQLite files changed since snapshot; refusing cutover"
            );
        }
        if matches!(current, Stage::Import | Stage::Verify | Stage::Rename) {
            ensure!(
                work_is_pinned(path, &journal),
                "prepared work snapshot changed or is unpinned"
            );
        }
        match current {
            Stage::Copy => {
                // A staged copy that an earlier attempt left is settled by
                // the rule before any file of this attempt takes its name.
                protect_staged(path, &journal_path, &mut journal, faults.crash)?;
                if journal.cli_resume {
                    // Preserve physical identity (free pages/header/WAL) for
                    // CLI checkpoints; its source already passed preparation.
                    snapshot_cli(path, &journal.original, &journal).await?;
                    journal.work = Some(fingerprint(&work)?);
                } else {
                    let rows = snapshot(path, &journal.original, &journal).await?;
                    journal.rows = rows;
                    if let Some(warning) = smaller_warning(path, &journal) {
                        say(warning);
                    }
                }
                // The two warnings that matter most are out.
                progress.say_later();
            }
            Stage::Migrate => migrate(&work).await?,
            Stage::Repair => {
                repair(&work).await?;
                journal.work = Some(fingerprint(&work)?);
            }
            Stage::Import => {
                if !super::import_complete(&mut *target).await? {
                    let result = super::importer::run_with_target(&work, &mut *target, None).await;
                    let differs = result.as_ref().err().is_some_and(|error| {
                        error
                            .downcast_ref::<super::importer::PinnedSourceDiffers>()
                            .is_some()
                    });
                    if differs && !restarted {
                        // The record and the disk agree, but PostgreSQL
                        // pinned another work copy or source snapshot (a
                        // restored data directory, for example). This is a
                        // change like a changed work copy: one checked
                        // restart under the reset rules, after rule 1.
                        progress.stage = "recovery";
                        if journal.cli_resume {
                            return Err(refusal(
                                path,
                                &journal,
                                Target::Unfinished,
                                Refusal::Changed(Change::Pinned),
                                "",
                                progress,
                            )
                            .await);
                        }
                        let (source, unusable) = source_state(path, &present(path));
                        if source != Source::Valid {
                            let why = if source == Source::Missing {
                                Refusal::SourceGone
                            } else {
                                Refusal::SourceUnusable
                            };
                            return Err(refusal(
                                path,
                                &journal,
                                Target::Unfinished,
                                why,
                                &unusable,
                                progress,
                            )
                            .await);
                        }
                        say(format!(
                            "sqlite import: restarting from the beginning: {}",
                            Change::Pinned.reason()
                        ));
                        let later = begin_again(
                            path,
                            &journal_path,
                            &mut journal,
                            true,
                            &mut *target,
                            url,
                            faults,
                        )
                        .await?;
                        progress.later.extend(later);
                        restarted = true;
                        progress.stage = "decision";
                        progress.advice.clear();
                        prepare(
                            path,
                            &journal_path,
                            &mut journal,
                            &mut *target,
                            faults,
                            true,
                        )
                        .await?;
                        continue;
                    }
                    result?;
                }
            }
            Stage::Verify => {
                // The advice follows what PostgreSQL really holds: a
                // completed import is never reset, and its ways out are not
                // those of an unfinished one.
                let complete = super::import_complete(&mut *target).await?;
                progress.advice = if complete || journal.cli_resume {
                    stuck(path, &journal, complete)
                } else {
                    String::new()
                };
                ensure!(
                    complete,
                    "PostgreSQL does not hold the completed import that this stage verifies"
                );
                // An older release may have migrated this database; the final
                // verification is against the latest schema.
                super::importer::bring_up_to_date(&mut *target).await?;
                let report = super::verifier::run(&work, url).await?;
                ensure!(report.complete, "verifier did not pass");
                verified_here = true;
                say(format!(
                    "sqlite import: verifier PASS: {} tables",
                    report.tables.len()
                ));
                // Stamp is durable before any rename; retries reuse it.
                journal.archive_stamp =
                    Some(chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string());
            }
            Stage::Rename => {
                // A crash may span another process lifetime. Verify again before
                // continuing a partially completed archival on this start.
                let result = async {
                    if !verified_here {
                        super::importer::bring_up_to_date(&mut *target).await?;
                        super::verifier::run(&work, url).await?;
                    }
                    archive(path, &journal, faults.stop)
                }
                .await;
                if result.is_err() && faults.stop != Some(Stage::Rename) {
                    if let Err(undo) = rollback_archive(path, &journal) {
                        say(format!(
                            "sqlite import: warning: archived SQLite components were not moved back: {}",
                            super::cli::safe_error(&undo, url)
                        ));
                    }
                }
                result?;
            }
            Stage::Done => unreachable!(),
        }
        say(format!(
            "sqlite import: {} complete: {:.3}s",
            current.name(),
            clock.elapsed().as_secs_f64()
        ));
        // Die after the effect but before its checkpoint, as a real kill can.
        boundary(current, faults.stop)?;
        journal.stage = match current {
            Stage::Copy if journal.cli_resume => Stage::Import,
            Stage::Copy => Stage::Migrate,
            Stage::Migrate => Stage::Repair,
            Stage::Repair => Stage::Import,
            Stage::Import => Stage::Verify,
            Stage::Verify => Stage::Rename,
            Stage::Rename => Stage::Done,
            Stage::Done => unreachable!(),
        };
        if let Err(error) = save(&journal_path, &journal) {
            if current == Stage::Rename {
                // Both cases retain work. A published Done is reconciled on
                // restart; a pre-publication failure retains Rename intent.
                rollback_archive(path, &journal)?;
                if error.published {
                    say("sqlite import: cleanup stage: journal published; archival reconciliation required".into());
                }
            }
            return Err(error.into());
        }
    }
    progress.stage = Stage::Done.name();
    progress.advice.clear();
    // The cutover is complete and verified: now, and only now, the staged
    // copy of exactly that content goes with the other work files.
    if discard_done(path, &journal).is_err() {
        say(
            "sqlite import: warning: cutover succeeded; work cleanup will retry on next start"
                .into(),
        );
    }
    // The cutover is durable. A connection that does not close cleanly must
    // not turn a finished import into a failed start.
    let finished = connection.take().context("import connection is missing")?;
    if let Err(error) = release(finished).await {
        say(format!(
            "sqlite import: warning: cutover succeeded; closing the import connection failed: {}",
            super::cli::safe_error(&error, url)
        ));
    }
    say(format!(
        "sqlite import: complete: {:.3}s total",
        start.elapsed().as_secs_f64()
    ));
    Ok(())
}

fn fingerprint(path: &Path) -> Result<Fingerprint> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = 0;
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        hash.update(&buffer[..n]);
    }
    Ok(Fingerprint {
        bytes,
        sha256: hex::encode(hash.finalize()),
    })
}

fn inventory(path: &Path) -> Result<BTreeMap<String, Fingerprint>> {
    let bytes = COMPONENTS
        .iter()
        .try_fold(0u64, |sum, suffix| -> Result<u64> {
            let file = sibling(path, suffix);
            Ok(sum
                + if file.try_exists()? {
                    fs::metadata(file)?.len()
                } else {
                    0
                })
        })?;
    say(format!(
        "sqlite import: source inventory start: {bytes} bytes"
    ));
    let mut result = BTreeMap::new();
    for suffix in COMPONENTS {
        let file = sibling(path, suffix);
        if file.try_exists()? {
            ensure!(
                fs::symlink_metadata(&file)?.file_type().is_file(),
                "SQLite source component must be a regular file"
            );
            result.insert((*suffix).into(), fingerprint(&file)?);
        }
    }
    ensure!(result.contains_key(""), "original SQLite file is missing");
    Ok(result)
}

#[derive(Debug)]
struct SaveError {
    published: bool,
    error: anyhow::Error,
}
impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for SaveError {}

fn save(path: &Path, journal: &Journal) -> std::result::Result<(), SaveError> {
    save_with_sync(path, journal, sync_directory)
}

fn save_with_sync(
    path: &Path,
    journal: &Journal,
    sync: impl FnOnce(&Path) -> Result<()>,
) -> std::result::Result<(), SaveError> {
    let mut published = false;
    let result = (|| -> Result<()> {
        validate_record(journal)?;
        let next = sibling(
            path,
            journal
                .temporary
                .strip_prefix(".import-work-state")
                .unwrap(),
        );
        // The random name is recorded in this Journal before creation. Initial
        // publication is atomic too: an interrupted, unpublished temporary is
        // unrecorded on disk and must be left alone. Updates use the durable name.
        let initial = !path.try_exists()?;
        if !initial {
            let previous: Journal = serde_json::from_slice(&fs::read(path)?)?;
            ensure!(
                previous.temporary == journal.temporary,
                "journal temporary ownership changed"
            );
            remove_owned(&next)?;
        }
        let mut file = create_private(&next)?;
        file.write_all(&serde_json::to_vec(journal)?)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&next, path)?;
        published = true;
        sync(path)
    })();
    result.map_err(|error| SaveError { published, error })
}

fn create_private(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

fn flush_work(path: &Path) -> Result<()> {
    OpenOptions::new().write(true).open(path)?.sync_all()?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn remove_owned(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                !metadata.is_dir(),
                "import work component must not be a directory"
            );
            fs::remove_file(path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn work_suffixes() -> Vec<String> {
    let mut names = vec![".import-work.repair-lock".into()];
    for work in [".import-work", ".import-work-source"] {
        names.extend(COMPONENTS.iter().map(|suffix| format!("{work}{suffix}")));
    }
    names
}

fn validate_record(journal: &Journal) -> Result<()> {
    let allowed = work_suffixes();
    ensure!(
        journal.created.iter().all(|name| allowed.contains(name)),
        "invalid work ownership record"
    );
    let random = journal.temporary.strip_prefix(".import-work-state.next-");
    ensure!(
        random.is_some_and(|id| uuid::Uuid::parse_str(id).is_ok()),
        "journal lacks a valid random temporary ownership name"
    );
    ensure!(
        journal.probes.len() == 2
            && journal.probes.iter().all(|name| {
                name.strip_prefix(".import-probe-")
                    .and_then(|rest| rest.rsplit_once('-'))
                    .is_some_and(|(id, end)| {
                        uuid::Uuid::parse_str(id).is_ok() && matches!(end, "a" | "b")
                    })
            }),
        "invalid probe ownership record"
    );
    ensure!(
        journal
            .kept
            .as_ref()
            .is_none_or(|kept| valid_stamp(&kept.stamp)),
        "invalid kept-copy record"
    );
    Ok(())
}

fn ensure_work_absent(path: &Path) -> Result<()> {
    report_unrecorded(path, None)?;
    for name in work_suffixes() {
        ensure!(
            fs::symlink_metadata(sibling(path, &name))
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "unrecorded import work file {}; leaving it alone. No startup record lists it: check that it is a leftover of an earlier import attempt, delete it and start again",
            sibling(path, &name).display()
        );
    }
    Ok(())
}

fn remove_recorded(path: &Path, name: &str, journal: &Journal) -> Result<()> {
    let file = sibling(path, name);
    if !journal.created.iter().any(|owned| owned == name) {
        ensure!(
            fs::symlink_metadata(&file).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "unrecorded import work file {}; leaving it alone",
            file.display()
        );
        return Ok(());
    }
    remove_owned(&file)
}

fn remove_database(path: &Path, name: &str, journal: &Journal) -> Result<()> {
    for suffix in COMPONENTS {
        remove_recorded(path, &format!("{name}{suffix}"), journal)?;
    }
    Ok(())
}

/// Remove the staged copy before a new one is staged. Needs the proof.
fn remove_staged(path: &Path, journal: &Journal, _proof: Disposable) -> Result<()> {
    remove_database(path, staged_base(journal), journal)
}

/// The only function that removes the work files of an import as a set. The
/// data components of the staged copy are passed over unless the caller holds
/// the proof that they are not the last copy. The derived work copy, the
/// repair lease and the empty scratch files are always removed.
fn discard_work(path: &Path, journal: &Journal, staged: Option<Disposable>) -> Result<()> {
    remove_work(path, journal, staged)?;
    report_unrecorded(path, Some(journal))
}

/// `discard_work` without its warnings about unrecorded files; the caller
/// says those (`unrecorded`) when it chooses.
fn remove_work(path: &Path, journal: &Journal, staged: Option<Disposable>) -> Result<()> {
    validate_record(journal)?;
    let base = staged_base(journal);
    for name in work_suffixes() {
        let copy = DATA_COMPONENTS
            .iter()
            .any(|suffix| name == format!("{base}{suffix}"));
        if copy && staged.is_none() {
            continue;
        }
        remove_recorded(path, &name, journal)?;
    }
    for name in &journal.probes {
        remove_owned(&sibling(path, name))?;
    }
    remove_owned(&sibling(path, &journal.temporary))?;
    sync_directory(path)
}

fn report_unrecorded(path: &Path, journal: Option<&Journal>) -> Result<()> {
    for line in unrecorded(path, journal)? {
        say(line);
    }
    Ok(())
}

/// One warning line for every file with a work-file name that no record lists.
fn unrecorded(path: &Path, journal: Option<&Journal>) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    let base = path
        .file_name()
        .context("source has no filename")?
        .to_string_lossy();
    for entry in fs::read_dir(path.parent().unwrap_or(Path::new(".")))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(suffix) = name.strip_prefix(base.as_ref()) else {
            continue;
        };
        if suffix == ".import-work-state" {
            continue;
        }
        let recorded = journal.is_some_and(|j| {
            j.created
                .iter()
                .chain(&j.probes)
                .any(|owned| owned == suffix)
                || j.temporary == suffix
        });
        if !recorded && (suffix.starts_with(".import-work") || suffix.starts_with(".import-probe-"))
        {
            lines.push(format!(
                "sqlite import: warning: unrecorded file {}; leaving it alone",
                entry.path().display()
            ));
        }
    }
    lines.sort();
    Ok(lines)
}

/// Resolve the source directory once. Every later path of the run derives
/// from this canonical directory.
fn resolve_source(path: &Path) -> Result<PathBuf> {
    let directory = path.parent().unwrap_or(Path::new(".")).canonicalize()?;
    Ok(directory.join(path.file_name().context("source path has no filename")?))
}

/// Run only on a branch that will create files beside the SQLite file (fresh
/// import, resume, restart, archive completion), before its first file
/// operation. Starts that only warn or do nothing never come here.
fn require_private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        // geteuid takes no arguments and returns the process effective uid.
        validate_directory(path.parent().unwrap_or(Path::new(".")), unsafe {
            geteuid()
        })?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(unix)]
fn validate_directory(directory: &Path, uid: u32) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::metadata(directory)?;
    ensure!(
        metadata.uid() == uid && metadata.mode() & 0o022 == 0,
        "SQLite source directory {0} must be owned by the user that runs the server and not writable by group or others. Run `chmod 700 {0}` (and `chown` it to that user if another user owns it), or copy the SQLite file into a private directory and use the import-sqlite-to-postgres command",
        directory.display()
    );
    Ok(())
}

fn archive_path(path: &Path, journal: &Journal) -> Result<PathBuf> {
    let stamp = journal
        .archive_stamp
        .as_deref()
        .context("missing archive timestamp")?;
    ensure!(
        stamp.len() == 16 && chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M%SZ").is_ok(),
        "invalid archive timestamp"
    );
    Ok(sibling(path, &format!(".imported-{stamp}")))
}

fn disk_budget(original: &BTreeMap<String, Fingerprint>) -> Result<u64> {
    let bytes = original.values().try_fold(0u64, |sum, file| {
        sum.checked_add(file.bytes)
            .ok_or_else(|| anyhow::anyhow!("SQLite size overflows disk budget"))
    })?;
    bytes
        .checked_mul(8)
        .and_then(|n| n.checked_add(256 * 1024 * 1024))
        .ok_or_else(|| anyhow::anyhow!("SQLite size overflows disk budget"))
}

/// `df -Pk` prints `Filesystem 1024-blocks Used Available Capacity Mounted on`.
/// The filesystem name and the mount point can both contain spaces, so the
/// columns are found by their shape, never by their position: three integers
/// followed by an integer percentage, with a name before and a mount point
/// after. More than one such place is refused instead of guessed.
fn parse_df_available(output: &str) -> Result<u64> {
    let line = output
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("empty disk space result"))?;
    let words: Vec<&str> = line.split_whitespace().collect();
    let number = |word: &str| !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit());
    let mut places = (1..words.len().saturating_sub(4)).filter(|&i| {
        number(words[i])
            && number(words[i + 1])
            && number(words[i + 2])
            && words[i + 3].strip_suffix('%').is_some_and(number)
    });
    let place = places
        .next()
        .ok_or_else(|| anyhow::anyhow!("invalid disk space result"))?;
    ensure!(places.next().is_none(), "ambiguous disk space result");
    words[place + 2]
        .parse::<u64>()?
        .checked_mul(1024)
        .ok_or_else(|| anyhow::anyhow!("free space overflow"))
}

// No system-call crate is a direct dependency of this crate (libc and rustix
// are in Cargo.lock only through other crates), and a hand-written statvfs
// layout differs per platform. POSIX `df -Pk` is the portable source.
#[cfg(unix)]
fn free_space(path: &Path) -> Result<u64> {
    let output = std::process::Command::new("df")
        .arg("-Pk")
        .arg(path)
        .env("LC_ALL", "C")
        .output()
        .context("check free disk space with df")?;
    ensure!(output.status.success(), "free disk space check failed");
    parse_df_available(&String::from_utf8(output.stdout)?)
}

#[cfg(windows)]
fn free_space(path: &Path) -> Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            path: *const u16,
            available: *mut u64,
            total: *mut u64,
            free: *mut u64,
        ) -> i32;
    }
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut available = 0;
    // Pointers have the API's required widths and remain valid for the call.
    ensure!(
        unsafe {
            GetDiskFreeSpaceExW(
                name.as_ptr(),
                &mut available,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } != 0,
        "free disk space check failed"
    );
    Ok(available)
}

/// Every private SQLite file of the import is opened by path. A path is never
/// turned into a URL and parsed back: that loses a Windows drive colon, breaks
/// on a `\\?\` prefix and decodes a literal `%`.
fn sqlite_options(path: &Path) -> sqlx::sqlite::SqliteConnectOptions {
    sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .pragma("temp_store", "FILE")
        .disable_statement_logging()
}

static TEMPORARY_DIRECTORY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Point SQLite's temporary files (sorters, TEMP tables, statement journals) of
/// THIS process at `directory`, or give the default back with `None`.
///
/// The bundled SQLite keeps one such directory per process
/// (`sqlite3_temp_directory`). Its Unix and Windows file systems both look at
/// it before any environment variable, and the pragma sets it under SQLite's
/// own mutex. The import runs before the server opens any other SQLite
/// database, and the default is put back when it ends. No process environment
/// variable is changed.
async fn sqlite_temporary_directory(directory: Option<&Path>) -> Result<()> {
    let wanted = match directory {
        Some(directory) => Some(
            directory
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("work directory must be UTF8"))?,
        ),
        None => None,
    };
    // Set and read back as one step, also when tests import in parallel.
    let _serial = TEMPORARY_DIRECTORY.lock().await;
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .in_memory(true)
        .disable_statement_logging();
    let mut conn = SqliteConnection::connect_with(&options).await?;
    sqlx::query(&format!(
        "PRAGMA temp_store_directory = '{}'",
        wanted.unwrap_or_default().replace('\'', "''")
    ))
    .execute(&mut conn)
    .await
    .map_err(|_| anyhow::anyhow!("SQLite cannot write temporary files in the work directory"))?;
    let now: Option<String> = sqlx::query_scalar("PRAGMA temp_store_directory")
        .fetch_optional(&mut conn)
        .await?;
    conn.close().await?;
    ensure!(
        now.as_deref() == wanted,
        "bundled SQLite did not accept the temporary directory"
    );
    Ok(())
}

/// One filesystem holds everything the SQLite side needs: the byte staging
/// set, the work copy, its journals, and SQLite's temporary files of this
/// process and of the repair child. So free space is measured once, there.
fn require_free_space(path: &Path, original: &BTreeMap<String, Fingerprint>) -> Result<()> {
    let directory = path.parent().unwrap_or(Path::new("."));
    let required = disk_budget(original)?;
    let available = free_space(directory)?;
    ensure!(
        available >= required,
        "not enough free disk space in {}: need {required} bytes, have {available} bytes (SQLite copy, migrations, repair and SQLite temporary files all use this directory)",
        directory.display()
    );
    say(format!(
        "sqlite import: disk space: {available} bytes free, {required} bytes required"
    ));
    Ok(())
}

/// Stage the original bytes privately and make the work copy from them.
/// Returns the row counts of a few user-facing tables of that snapshot.
async fn snapshot(
    path: &Path,
    original: &BTreeMap<String, Fingerprint>,
    journal: &Journal,
) -> Result<BTreeMap<String, u64>> {
    let work = sibling(path, ".import-work");
    let staging = sibling(path, ".import-work-source");
    // A staged copy of an earlier attempt is replaced only when it is an
    // interrupted copy or has a byte-identical twin at the original name.
    let proof = disposable_staged(path, journal)?;
    remove_database(path, ".import-work", journal)?;
    remove_staged(path, journal, proof)?;
    require_free_space(path, original)?;
    // VACUUM INTO accepts an existing empty destination. Reserve it privately
    // before SQLite opens it, so passwords/tokens are never world-readable.
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(&work)?.sync_all()?;
    // Never open original with SQLite: mode=ro can create/write its SHM, and
    // immutable=1 would ignore pending WAL. Byte staging is NOT the snapshot.
    // Hash both the staged bytes and originals to reject a torn/changing source.
    for (suffix, expected) in original {
        if suffix == "-shm" {
            continue;
        } // SQLite rebuilds a private WAL index.
        let destination = sibling(&staging, suffix);
        let mut input = File::open(sibling(path, suffix))?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options.open(&destination)?;
        std::io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        ensure!(
            fingerprint(&destination)? == *expected,
            "SQLite source changed during staging"
        );
    }
    ensure!(
        inventory(path)? == *original,
        "SQLite source changed during staging"
    );
    // The staged copy stays byte for byte what was copied: it is the copy
    // that is kept when the original is lost or replaced. SQLite therefore
    // opens it read-only (a read-only connection reads a committed WAL and
    // never checkpoints), or not at all when a rollback journal must be
    // played back, which writes.
    let rows = if original.contains_key("-journal") {
        recover_into_work(
            &staging,
            &work,
            &original.keys().cloned().collect::<Vec<_>>(),
        )
        .await?
    } else {
        let options = sqlite_options(&staging)
            .read_only(true)
            .foreign_keys(false)
            .pragma("cache_size", "-8192");
        let mut conn = SqliteConnection::connect_with(&options).await?;
        // SQLite reads the committed WAL privately.
        sqlx::query("VACUUM INTO $1")
            .bind(
                work.to_str()
                    .ok_or_else(|| anyhow::anyhow!("work path must be UTF8"))?,
            )
            .execute(&mut conn)
            .await?;
        let rows = key_rows(&mut conn).await?;
        conn.close().await?;
        rows
    };
    flush_work(&work)?;
    sync_directory(path)?;
    for (suffix, expected) in original {
        if suffix != "-shm" {
            ensure!(
                fingerprint(&sibling(&staging, suffix))? == *expected,
                "the staged copy of the SQLite source changed while it was read"
            );
        }
    }
    ensure!(
        inventory(path)? == *original,
        "SQLite source changed during snapshot"
    );
    // Retain staging until successful cutover, like the work copy itself.
    Ok(rows)
}

/// A source with a rollback journal: the journal is played back in a second
/// private byte copy (the work copy), never in the staged copy.
async fn recover_into_work(
    staging: &Path,
    work: &Path,
    components: &[String],
) -> Result<BTreeMap<String, u64>> {
    for suffix in components.iter().filter(|suffix| *suffix != "-shm") {
        let destination = sibling(work, suffix);
        let mut input = File::open(sibling(staging, suffix))?;
        let mut output = if suffix.is_empty() {
            // Reserved empty and private by the caller.
            OpenOptions::new().write(true).open(&destination)?
        } else {
            create_private(&destination)?
        };
        std::io::copy(&mut input, &mut output)?;
        output.sync_all()?;
    }
    let mut conn =
        SqliteConnection::connect_with(&sqlite_options(work).foreign_keys(false)).await?;
    // The first read plays the journal back, or replays a committed WAL.
    let rows = key_rows(&mut conn).await?;
    let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&mut conn)
        .await?;
    if journal_mode.eq_ignore_ascii_case("wal") {
        let (busy, _, _): (i64, i64, i64) = sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_one(&mut conn)
            .await?;
        ensure!(busy == 0, "work copy checkpoint is busy");
    }
    conn.close().await?;
    Ok(rows)
}

/// The tables whose row counts tell an owner at a glance that a database is
/// not the one they expect. Tables that an old source does not have yet are
/// left out.
const KEY_TABLES: &[&str] = &["users", "user_library", "comments", "work"];

async fn key_rows(conn: &mut SqliteConnection) -> Result<BTreeMap<String, u64>> {
    let mut rows = BTreeMap::new();
    for table in KEY_TABLES {
        let exists: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sqlite_schema WHERE type='table' AND name=$1")
                .bind(table)
                .fetch_one(&mut *conn)
                .await?;
        if exists == 1 {
            let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM \"{table}\""))
                .fetch_one(&mut *conn)
                .await?;
            rows.insert((*table).to_owned(), u64::try_from(count)?);
        }
    }
    Ok(rows)
}

async fn snapshot_cli(
    path: &Path,
    original: &BTreeMap<String, Fingerprint>,
    journal: &Journal,
) -> Result<()> {
    let work = sibling(path, ".import-work");
    // For a command-line resume the work copy is the byte copy of the
    // original, so it is replaced only by the same rule as a staged copy.
    let proof = disposable_staged(path, journal)?;
    if journal.cli_resume {
        remove_staged(path, journal, proof)?;
    } else {
        remove_database(path, ".import-work", journal)?;
    }
    require_free_space(path, original)?;
    for (suffix, expected) in original {
        if suffix == "-shm" {
            continue;
        }
        let destination = sibling(&work, suffix);
        let mut input = File::open(sibling(path, suffix))?;
        let mut output = create_private(&destination)?;
        std::io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        ensure!(
            fingerprint(&destination)? == *expected,
            "CLI source changed during staging"
        );
    }
    ensure!(
        inventory(path)? == *original,
        "CLI source changed during staging"
    );
    sync_directory(path)?;
    // Opening only the private bytes pins SQLite's consistent read snapshot,
    // includes its WAL, and validates the frozen migration/schema contract.
    let source = super::source::open(&work).await?;
    source.close().await?;
    Ok(())
}

async fn migrate(path: &Path) -> Result<()> {
    let options = sqlite_options(path).foreign_keys(true);
    let mut conn = SqliteConnection::connect_with(&options).await?;
    let result = super::source::SQLITE.run(&mut conn).await;
    conn.close().await?;
    result.map_err(|error| {
        anyhow::anyhow!(
            "SQLite work-copy migration failed: {}",
            super::cli::safe_error(&error.into(), "")
        )
    })?;
    Ok(())
}

/// How the retained date repair is started: its arguments and the variables
/// added to the child's own environment. Built by the same code in production
/// and in tests.
struct RepairChild {
    arguments: Vec<String>,
    environment: Vec<(&'static str, OsString)>,
}

/// The repair command is not part of this module. It accepts one database
/// argument and reads it with SQLx's own parser, which cuts at `?` and decodes
/// `%XX`. The argument is therefore the plain work path with only those two
/// characters escaped, and before a child is started the same parser must give
/// back exactly the same path. No URL is built and nothing is guessed.
fn repair_child(work: &Path) -> Result<RepairChild> {
    let plain = work
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("work path must be UTF8"))?;
    let argument = plain.replace('%', "%25").replace('?', "%3F");
    let read_back = sqlx::sqlite::SqliteConnectOptions::from_str(&argument)
        .map_err(|_| anyhow::anyhow!("work path cannot be given to the date repair"))?;
    ensure!(
        read_back.get_filename() == work,
        "work path cannot be given to the date repair unchanged"
    );
    // SQLite of the child reads SQLITE_TMPDIR first on Unix and the TMP/TEMP
    // directory on Windows: its TEMP tables go beside the work copy.
    let directory = work.parent().unwrap_or(Path::new(".")).as_os_str();
    Ok(RepairChild {
        arguments: vec!["repair-upload-dates".into(), argument, "--apply".into()],
        environment: vec![
            ("RUST_LOG", "off".into()),
            ("SQLITE_TMPDIR", directory.to_owned()),
            ("TMPDIR", directory.to_owned()),
            ("TMP", directory.to_owned()),
            ("TEMP", directory.to_owned()),
        ],
    })
}

async fn repair(path: &Path) -> Result<()> {
    let child = repair_child(path)?;
    let lease_path = sibling(path, ".repair-lock");
    let lease = match create_private(&lease_path) {
        Ok(file) => file,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) =>
        {
            let metadata = fs::symlink_metadata(&lease_path)?;
            ensure!(
                metadata.file_type().is_file() && metadata.len() == 0,
                "repair lease must be an empty regular file"
            );
            // Retain the inode inherited by a surviving child. No bytes are
            // written through this existing handle; it is used only for locking.
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lease_path)?
        }
        Err(error) => return Err(error),
    };
    // A hard-killed parent can leave its CLI child alive. The child inherits
    // this lease through stdin (the repair CLI never reads stdin), so the next
    // start waits for that child before repairing the same work snapshot again.
    let lease = tokio::task::spawn_blocking(move || -> std::io::Result<File> {
        lease.lock()?;
        Ok(lease)
    })
    .await??;
    // Retained CLI validates migration checksums and applies its resumable CAS
    // repair. Production captures its count-only stdout and prefixes each line.
    #[cfg(not(test))]
    {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut process = tokio::process::Command::new(std::env::current_exe()?)
            .args(&child.arguments)
            .envs(child.environment.iter().map(|(name, value)| (name, value)))
            .stdin(std::process::Stdio::from(lease.try_clone()?))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("start retained SQLite date repair")?;
        let mut lines = BufReader::new(process.stdout.take().unwrap()).lines();
        while let Some(line) = lines.next_line().await? {
            say(format!("sqlite import: repair: {line}"));
        }
        ensure!(
            process.wait().await?.success(),
            "retained SQLite date repair failed (details redacted)"
        );
    }
    // Test builds give the same arguments to the same command in-process.
    #[cfg(test)]
    crate::catalog::date_repair::run_cli(&child.arguments[1..]).await?;
    // Close/checkpoint all repair WAL before pinning the importer's file identity.
    let mut conn = SqliteConnection::connect_with(&sqlite_options(path)).await?;
    let (busy, _, _): (i64, i64, i64) = sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
        .fetch_one(&mut conn)
        .await?;
    ensure!(busy == 0, "date repair checkpoint is busy");
    conn.close().await?;
    drop(lease);
    flush_work(path)?;
    sync_directory(path)?;
    Ok(())
}

fn rollback_archive(path: &Path, journal: &Journal) -> Result<()> {
    if let Some(stamp) = &journal.archive_stamp {
        let archive = sibling(path, &format!(".imported-{stamp}"));
        // An archived WAL or journal is never put back beside a file that is
        // no longer the recorded original: SQLite would replay it over that
        // file. In that case every archived component stays in the archive.
        for suffix in COMPONENTS {
            let to = sibling(path, suffix);
            if to.try_exists()? {
                ensure!(
                    journal.original.get(*suffix) == Some(&fingerprint(&to)?),
                    "a file at the original SQLite name changed; archived components stay at {}",
                    archive.display()
                );
            }
        }
        for suffix in COMPONENTS {
            let from = sibling(&archive, suffix);
            let to = sibling(path, suffix);
            if from.try_exists()? && !to.try_exists()? {
                ensure!(
                    journal.original.get(*suffix) == Some(&fingerprint(&from)?),
                    "archive changed; cannot restore original component"
                );
                move_no_replace(&from, &to)?;
            }
        }
        sync_directory(path)?;
    }
    Ok(())
}

// No-replace namespace operations are the preferred capability. Raw OS APIs
// avoid changing Cargo.toml: libc/rustix/windows-sys are only transitive here.
fn atomic_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let from = CString::new(from.as_os_str().as_bytes())?;
        let to = CString::new(to.as_os_str().as_bytes())?;
        #[cfg(target_os = "linux")]
        unsafe extern "C" {
            fn renameat2(
                oldfd: i32,
                old: *const std::ffi::c_char,
                newfd: i32,
                new: *const std::ffi::c_char,
                flags: u32,
            ) -> i32;
        }
        #[cfg(target_os = "macos")]
        unsafe extern "C" {
            fn renamex_np(
                old: *const std::ffi::c_char,
                new: *const std::ffi::c_char,
                flags: u32,
            ) -> i32;
        }
        // NUL-terminated paths stay live for the call. AT_FDCWD=-100 and
        // RENAME_NOREPLACE=1 on Linux; RENAME_EXCL=4 on macOS.
        #[cfg(target_os = "linux")]
        let result = unsafe { renameat2(-100, from.as_ptr(), -100, to.as_ptr(), 1) };
        #[cfg(target_os = "macos")]
        let result = unsafe { renamex_np(from.as_ptr(), to.as_ptr(), 4) };
        if result == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
        }
        let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        // Flags=0 never permits replacement; wide path buffers remain live.
        if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) } != 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = (from, to);
        Err(std::io::ErrorKind::Unsupported.into())
    }
}

fn unsupported_rename(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(target_os = "linux")]
    return matches!(error.raw_os_error(), Some(22 | 38 | 95));
    #[cfg(target_os = "macos")]
    return matches!(error.raw_os_error(), Some(22 | 45 | 78 | 102));
    #[cfg(windows)]
    return matches!(error.raw_os_error(), Some(1 | 50 | 120));
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    false
}

fn move_no_replace(from: &Path, to: &Path) -> Result<()> {
    move_with(from, to, atomic_no_replace, |a, b| fs::hard_link(a, b))
}

fn move_with(
    from: &Path,
    to: &Path,
    rename: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
    link: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    ensure!(
        fs::symlink_metadata(from)?.file_type().is_file(),
        "archive source must be regular"
    );
    match rename(from, to) {
        Ok(()) => return sync_directory(from),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(
                same_file(from, to)?,
                "archive destination already exists; refusing overwrite"
            );
        }
        Err(error) if unsupported_rename(&error) => match link(from, to) {
            Ok(()) => sync_directory(from)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                ensure!(
                    same_file(from, to)?,
                    "archive destination already exists; refusing overwrite"
                );
            }
            Err(error) => {
                return Err(error).context(
                    "filesystem supports neither no-replace rename nor hard-link archival",
                )
            }
        },
        Err(error) => return Err(error).context("atomic no-replace archival failed"),
    }
    fs::remove_file(from)?;
    sync_directory(from)
}

fn capability_probe(path: &Path, journal: &Journal) -> Result<()> {
    probe_with(path, journal, move_no_replace)
}

fn probe_with(
    path: &Path,
    journal: &Journal,
    move_file: impl Fn(&Path, &Path) -> Result<()>,
) -> Result<()> {
    validate_record(journal)?;
    let from = sibling(path, &journal.probes[0]);
    let to = sibling(path, &journal.probes[1]);
    // These exact random names were recorded before their first creation.
    remove_owned(&from)?;
    remove_owned(&to)?;
    let result = (|| {
        create_private(&from)?.sync_all()?;
        create_private(&to)?.sync_all()?;
        ensure!(
            move_file(&from, &to).is_err() && from.try_exists()? && to.try_exists()?,
            "filesystem probe did not refuse existing destination"
        );
        remove_owned(&to)?;
        move_file(&from, &to)?;
        ensure!(
            !from.try_exists()? && to.try_exists()?,
            "filesystem probe did not move its private file"
        );
        Ok(())
    })();
    remove_owned(&from)?;
    remove_owned(&to)?;
    sync_directory(path)?;
    result
}

#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let a = fs::symlink_metadata(a)?;
    let b = fs::symlink_metadata(b)?;
    Ok(a.is_file() && b.is_file() && a.dev() == b.dev() && a.ino() == b.ino())
}

#[cfg(windows)]
fn same_file(a: &Path, b: &Path) -> Result<bool> {
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    #[derive(Default, PartialEq)]
    struct FileId {
        volume: u64,
        id: [u8; 16],
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandleEx(
            handle: *mut std::ffi::c_void,
            class: i32,
            info: *mut std::ffi::c_void,
            size: u32,
        ) -> i32;
    }
    fn id(path: &Path) -> Result<FileId> {
        ensure!(
            fs::symlink_metadata(path)?.file_type().is_file(),
            "archive destination must be regular"
        );
        let file = File::open(path)?;
        let mut result = FileId::default();
        // FileIdInfo=18, structure and writable buffer match the Win32 API.
        ensure!(
            unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    18,
                    std::ptr::from_mut(&mut result).cast(),
                    std::mem::size_of::<FileId>() as u32,
                )
            } != 0,
            "cannot inspect archive file identity"
        );
        Ok(result)
    }
    Ok(id(a)? == id(b)?)
}

fn archive(path: &Path, journal: &Journal, stop: Option<Stage>) -> Result<()> {
    let stamp = journal
        .archive_stamp
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing verified archive timestamp"))?;
    ensure!(
        stamp.len() == 16 && chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M%SZ").is_ok(),
        "invalid archive timestamp"
    );
    let destination = sibling(path, &format!(".imported-{stamp}"));
    // Main last. Durable Rename intent locates every component after a kill.
    for suffix in ["-wal", "-shm", "-journal", ""] {
        let from = sibling(path, suffix);
        let to = sibling(&destination, suffix);
        let Some(expected) = journal.original.get(suffix) else {
            ensure!(
                !from.try_exists()? && !to.try_exists()?,
                "unexpected SQLite component at rename"
            );
            continue;
        };
        if from.try_exists()? {
            ensure!(
                fingerprint(&from)? == *expected,
                "SQLite component changed before rename"
            );
            move_no_replace(&from, &to)?;
            sync_directory(path)?;
        } else {
            ensure!(
                to.try_exists()? && fingerprint(&to)? == *expected,
                "SQLite archive component is missing or changed"
            );
        }
        // Also simulate death in the middle of the multi-file rename.
        if !suffix.is_empty() {
            boundary(Stage::Rename, stop)?;
        }
    }
    say(format!(
        "sqlite import: renamed original to {}",
        destination.display()
    ));
    Ok(())
}

#[cfg(test)]
#[path = "startup_tests.rs"]
mod tests;
