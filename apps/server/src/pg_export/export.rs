//! One export run: a PostgreSQL snapshot, a copy of the template, the copied
//! tables replaced in that copy inside one SQLite transaction, the checks, and
//! the rename to the output name.
use anyhow::{bail, ensure, Context, Result};
use futures::TryStreamExt;
use serde::Serialize;
use sqlx::{Connection, PgConnection, Row, SqliteConnection};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Instant,
};

use super::{
    convert::{value, Value},
    tables::{
        copied, is_copied, OPTIONAL_POSTGRES_INTERNAL, REBUILDABLE_POSTGRES_ONLY,
        REQUIRED_POSTGRES_INTERNAL, TABLES, UNSUPPORTED_POSTGRES_ONLY, WORK_REFERENCES,
    },
    template::{self, sibling, Template},
};
use crate::pg_import::{
    conversion::{frame, pg_cell, Cell},
    plan::{self, quote, Table},
    source::{quick_check, row_cells},
    verifier::{self, Checksum},
};

#[derive(Serialize)]
pub(super) struct LeftOut {
    pub key: BTreeMap<String, String>,
    pub reason: String,
    /// The works of the kept file that PostgreSQL merged into the missing
    /// work. The row can be entered again on one of them.
    pub kept_work_candidates: Vec<String>,
}

#[derive(Serialize)]
pub(super) struct TableReport {
    pub table: String,
    /// Rows the template had in this table before it was replaced.
    pub template_rows: i64,
    pub rows_read: u64,
    pub rows_written: u64,
    pub left_out: Vec<LeftOut>,
    /// Canonical checksum of the rows read and not left out, as they were
    /// written: a row of `chapter_override` that was led back to a kept work
    /// counts with the kept work's id.
    pub postgres: Checksum,
    /// Canonical checksum of the rows read back from the output file.
    pub sqlite: Checksum,
}

#[derive(Serialize)]
pub(super) struct Tombstone {
    pub comment_id: String,
    /// The marker the 0121 file cannot hold: `author` or `moderator`.
    pub deleted_by: String,
}

#[derive(Serialize)]
pub(super) struct Tombstones {
    pub count: usize,
    pub comments: Vec<Tombstone>,
}

#[derive(Serialize)]
pub(super) struct UnresolvedReference {
    pub table: String,
    pub column: String,
    pub rows: i64,
    pub missing_work_ids: Vec<String>,
    /// For a missing id that merges on PostgreSQL lead back to the kept file:
    /// the works of the kept file that were merged into it. Two or more, or
    /// one whose id already has a row with the same key, so nothing was chosen.
    pub kept_work_candidates: BTreeMap<String, Vec<String>>,
}

/// Rows that pointed at a work the kept file does not have and that were
/// written with the id of the one kept work PostgreSQL had merged into it.
#[derive(Serialize)]
pub(super) struct Rewrite {
    pub table: String,
    pub column: String,
    /// The id the rows have on PostgreSQL.
    pub postgres_work_id: String,
    /// The id they have in the output.
    pub kept_work_id: String,
    pub rows: u64,
}

/// `work_alias` is catalogue and is not copied, yet an admin can add and
/// remove an alternative title by hand, and no column tells such a row from
/// one the scanner made. These counts are all the rows that differ, by
/// (work, normalized title, language): the hand-made ones are among them.
#[derive(Serialize)]
pub(super) struct AliasChanges {
    /// Rows PostgreSQL has and the kept file lacks, for works the kept file
    /// has. They are not in the output.
    pub added_on_kept_works: u64,
    /// Rows the kept file has and PostgreSQL lacks. They are in the output.
    pub removed: u64,
}

/// `markSourceNsfw` writes two things: the pin row, which is copied, and
/// `work.is_nsfw_override` on the source's works, which is catalogue and is
/// not. A source in one of these lists has the pin state of PostgreSQL and the
/// works of the kept file: the admin applies the pin again on the old build.
#[derive(Serialize)]
pub(super) struct SourcePins {
    /// Pinned on PostgreSQL, not pinned in the template.
    pub pinned: Vec<String>,
    /// Pinned in the template, no longer pinned on PostgreSQL.
    pub unpinned: Vec<String>,
}

#[derive(Serialize)]
pub(super) struct FileReport {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Serialize)]
pub(super) struct Report {
    pub complete: bool,
    /// SQLite migration level of the template and of the output: 121 or 122.
    pub target_sqlite_migration: i64,
    pub postgres_migrations: usize,
    /// UTC clock of PostgreSQL when the repeatable-read snapshot was taken.
    pub snapshot_time: String,
    pub seconds: f64,
    pub peak_memory_bytes: Option<u64>,
    /// The kept file. `sha256` was the same before and after the run.
    pub template: FileReport,
    pub output: FileReport,
    pub tables: Vec<TableReport>,
    pub rows_read: u64,
    pub rows_written: u64,
    pub rows_left_out: u64,
    /// Deleted comments that kept replies. Only a 0122 file can mark them; in
    /// a 0121 file they are comments with an empty body under their author's
    /// name. Each is listed with its marker, so the marker can be put back.
    pub comment_tombstones_written_without_marker: Option<Tombstones>,
    /// Rows that pointed at a work PostgreSQL made by merging exactly one work
    /// of the kept file into a newer one: written with the kept work's id.
    pub work_references_rewritten: Vec<Rewrite>,
    /// Rows that were written and point at a work the output does not have.
    pub unresolved_work_references: Vec<UnresolvedReference>,
    /// Source pins whose effect on the works is not in the output.
    pub source_nsfw_pins_to_apply_again: SourcePins,
    /// Alternative titles that changed on PostgreSQL and are not copied.
    pub work_alias_changes_not_copied: AliasChanges,
    /// `sqlite_sequence` after the run, for the copied AUTOINCREMENT tables.
    pub sequences: BTreeMap<String, i64>,
    /// Tables the export does not copy, with the row count that the template
    /// and the output both have.
    pub not_copied: BTreeMap<String, i64>,
    /// Omitted PostgreSQL-only tables, including nonempty rebuildable tables.
    pub postgres_only_not_copied: BTreeMap<String, i64>,
}

pub(super) struct Arguments {
    pub template: PathBuf,
    pub output: PathBuf,
    pub postgres_url: String,
}

/// The name the output has until every check has passed.
pub(super) fn partial_path(output: &Path) -> PathBuf {
    sibling(output, ".partial")
}

/// The paths with every symbolic link resolved. A link as the template would
/// hide the real file's `-wal`, `-journal` and directory lock from every check
/// that builds a name from the path. A link in the output's directory would
/// hide an output that has the name of one of the template's side files.
pub(super) fn resolve(args: &Arguments) -> Result<Arguments> {
    ensure!(
        args.template.is_absolute() && args.output.is_absolute(),
        "use absolute paths for --template and --output"
    );
    Ok(Arguments {
        template: args
            .template
            .canonicalize()
            .context("the template must be an existing file")?,
        output: real_parent(&args.output)?,
        postgres_url: args.postgres_url.clone(),
    })
}

/// The path with its directory resolved. The file itself need not exist.
pub(super) fn real_parent(path: &Path) -> Result<PathBuf> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        bail!("{} names no file", path.display());
    };
    let parent = parent
        .canonicalize()
        .with_context(|| format!("the directory of {} must exist", path.display()))?;
    Ok(parent.join(name))
}

pub(super) async fn run(args: &Arguments, stop_after: Option<usize>) -> Result<Report> {
    let start = Instant::now();
    let args = &resolve(args)?;
    let partial = partial_path(&args.output);
    protect_paths(args, &partial)?;
    // PostgreSQL first: a wrong database is refused before 3 GB are copied.
    let mut pg = crate::pg_import::connect(&args.postgres_url).await?;
    // Read only, one snapshot: the export sees one state and changes nothing.
    sqlx::query("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut pg)
        .await?;
    let snapshot_time: String = sqlx::query_scalar(
        "SELECT to_char(clock_timestamp() AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')",
    )
    .fetch_one(&mut pg)
    .await?;
    let postgres_migrations = postgres_level(&mut pg).await?;
    let postgres_only_not_copied = postgres_only_tables(&mut pg).await?;
    let mut template = template::open(&args.template).await?;
    refuse_without_accounts(&mut pg, &mut template.conn).await?;
    let (template_sha256, template_bytes) = template::copy(&args.template, &partial)?;
    // From here on a failure removes what this run made.
    let exported = fill(
        args,
        &partial,
        &mut pg,
        template,
        &template_sha256,
        stop_after,
    )
    .await;
    // The read-only transaction has nothing to commit.
    crate::pg_import::hang_up(pg).await;
    let filled = match exported {
        Ok(filled) => filled,
        Err(error) => {
            remove_partial(&partial);
            return Err(error);
        }
    };
    let output_sha256 = template::sha256(&args.output)?;
    Ok(Report {
        complete: true,
        target_sqlite_migration: filled.target,
        postgres_migrations,
        snapshot_time,
        seconds: start.elapsed().as_secs_f64(),
        peak_memory_bytes: crate::pg_import::importer::peak_memory(),
        template: FileReport {
            path: args.template.clone(),
            bytes: template_bytes,
            sha256: template_sha256,
        },
        output: FileReport {
            path: args.output.clone(),
            bytes: std::fs::metadata(&args.output)?.len(),
            sha256: output_sha256,
        },
        rows_read: filled.tables.iter().map(|t| t.rows_read).sum(),
        rows_written: filled.tables.iter().map(|t| t.rows_written).sum(),
        rows_left_out: filled.tables.iter().map(|t| t.left_out.len() as u64).sum(),
        tables: filled.tables,
        comment_tombstones_written_without_marker: filled.tombstones,
        work_references_rewritten: filled.rewritten,
        unresolved_work_references: filled.unresolved,
        source_nsfw_pins_to_apply_again: filled.pins,
        work_alias_changes_not_copied: filled.aliases,
        sequences: filled.sequences,
        not_copied: filled.not_copied,
        postgres_only_not_copied,
    })
}

/// `args` are resolved paths. A name that exists is never written, and a
/// symbolic link that points nowhere is a name that exists.
fn protect_paths(args: &Arguments, partial: &Path) -> Result<()> {
    let exists = |path: &Path| std::fs::symlink_metadata(path).is_ok();
    ensure!(
        !exists(&args.output),
        "the output path exists; the export writes a new file and never replaces one"
    );
    ensure!(
        !exists(partial),
        "{} exists: an earlier run was killed; look at it, remove it, then run again",
        partial.display()
    );
    for suffix in ["-wal", "-shm", "-journal"] {
        for file in [&args.output, partial] {
            let sidecar = sibling(file, suffix);
            ensure!(
                !exists(&sidecar),
                "{} exists; remove this leftover first",
                sidecar.display()
            );
        }
        // SQLite would take such a file for the kept file's own side file.
        for file in [&args.output, partial] {
            ensure!(
                *file != sibling(&args.template, suffix),
                "the output overlaps a side file of the template"
            );
        }
    }
    Ok(())
}

/// Every embedded PostgreSQL migration is applied, the columns are the import
/// plan's, and the importer left its mark: the export reads exactly the schema
/// it was written for, in a database that a SQLite file was moved into.
async fn postgres_level(pg: &mut PgConnection) -> Result<usize> {
    let embedded = crate::pg_import::importer::POSTGRES
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
        .count();
    let applied = verifier::applied_migrations(pg).await?;
    ensure!(
        applied == embedded,
        "PostgreSQL has {applied} migrations applied and this server has {embedded}: start this server's PostgreSQL build on the database once, or use the release that matches it"
    );
    verifier::columns(pg).await?;
    ensure!(
        crate::pg_import::import_complete(pg).await?,
        "PostgreSQL holds no completed import from SQLite: it is not the database of a cutover, and its rows would replace the accounts of the kept file"
    );
    Ok(applied)
}

/// Classify the whole PostgreSQL relation inventory, not only the frozen
/// SQLite import plan. A migration checksum proves which migrations ran, but
/// does not prove that the exporter preserves every table they created.
async fn postgres_only_tables(pg: &mut PgConnection) -> Result<BTreeMap<String, i64>> {
    let registered: BTreeSet<_> = plan::POSTGRES_ONLY.iter().map(|table| table.name).collect();
    let classified: BTreeSet<_> = UNSUPPORTED_POSTGRES_ONLY
        .iter()
        .chain(REBUILDABLE_POSTGRES_ONLY)
        .map(|(name, _)| *name)
        .collect();
    ensure!(
        registered == classified
            && classified.len() == UNSUPPORTED_POSTGRES_ONLY.len() + REBUILDABLE_POSTGRES_ONLY.len(),
        "PostgreSQL-only rollback classification differs from the import inventory; this release cannot safely export to SQLite"
    );
    let actual: Vec<(String, String)> = sqlx::query_as(
        "SELECT n.nspname,c.relname FROM pg_class c \
         JOIN pg_namespace n ON n.oid=c.relnamespace \
         WHERE c.relkind IN ('r','p','f','m','v') \
           AND n.nspname <> 'information_schema' AND n.nspname !~ '^pg_' \
         ORDER BY n.nspname,c.relname",
    )
    .fetch_all(&mut *pg)
    .await?;
    let actual: BTreeSet<_> = actual.into_iter().collect();
    let required: BTreeSet<_> = TABLES
        .iter()
        .map(|(name, _, _)| *name)
        .chain(REQUIRED_POSTGRES_INTERNAL.iter().copied())
        .chain(classified)
        .map(|name| ("public".to_owned(), name.to_owned()))
        .collect();
    let mut allowed = required.clone();
    allowed.extend(
        OPTIONAL_POSTGRES_INTERNAL
            .iter()
            .map(|name| ("public".to_owned(), (*name).to_owned())),
    );
    let unclassified: Vec<_> = actual.difference(&allowed).collect();
    let missing: Vec<_> = required.difference(&actual).collect();
    ensure!(
        unclassified.is_empty() && missing.is_empty(),
        "PostgreSQL rollback table inventory differs (unclassified relations: {unclassified:?}; missing required tables: {missing:?}); nothing was exported; keep PostgreSQL and use a release that classifies and preserves this data"
    );
    let mut omitted = BTreeMap::new();
    for (name, reason) in UNSUPPORTED_POSTGRES_ONLY {
        let rows: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM public.{}", quote(name)))
            .fetch_one(&mut *pg)
            .await?;
        ensure!(
            rows == 0,
            "cannot export PostgreSQL-only table {name}: {rows} rows would be lost because {reason}; nothing was exported; keep PostgreSQL and its backups until a verified preservation path exists"
        );
        omitted.insert((*name).to_owned(), rows);
    }
    for (name, _) in REBUILDABLE_POSTGRES_ONLY {
        let rows: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM public.{}", quote(name)))
            .fetch_one(&mut *pg)
            .await?;
        omitted.insert((*name).to_owned(), rows);
    }
    Ok(omitted)
}

/// The importer's mark says that some SQLite file was imported, not that the
/// accounts are still there. A database without accounts would empty every
/// user table of a kept file that has them.
async fn refuse_without_accounts(
    pg: &mut PgConnection,
    template: &mut SqliteConnection,
) -> Result<()> {
    let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(template)
        .await?;
    let present: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM public.users)")
        .fetch_one(pg)
        .await?;
    ensure!(
        kept == 0 || present,
        "PostgreSQL has no account and the template has {kept}: this is not the database the template was moved to"
    );
    Ok(())
}

fn remove_partial(partial: &Path) {
    // Only names this run created (`create_new` in `template::copy`).
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(sibling(partial, suffix));
    }
}

struct Filled {
    target: i64,
    tables: Vec<TableReport>,
    tombstones: Option<Tombstones>,
    rewritten: Vec<Rewrite>,
    unresolved: Vec<UnresolvedReference>,
    pins: SourcePins,
    aliases: AliasChanges,
    sequences: BTreeMap<String, i64>,
    not_copied: BTreeMap<String, i64>,
}

async fn fill(
    args: &Arguments,
    partial: &Path,
    pg: &mut PgConnection,
    mut template: Template,
    template_sha256: &str,
    stop_after: Option<usize>,
) -> Result<Filled> {
    let target = template.target;
    let tables = copied(target);
    // Foreign keys are off while the tables are replaced: with them on, the
    // DELETE of `users` would cascade, and SET NULL actions would rewrite
    // copied rows. `foreign_key_check` below proves the result instead.
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(partial)
        .create_if_missing(false)
        .foreign_keys(false)
        .pragma("temp_store", "FILE")
        .pragma("cache_size", "-16000");
    let mut out = SqliteConnection::connect_with(&options).await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut out).await?;

    // A trigger must not fire on a copied row: the row already passed it once.
    // SQLite cannot switch a trigger off, so the triggers of the copied tables
    // are dropped here and created again from their stored text, in the same
    // transaction. The schema comparison below proves they are back unchanged.
    let names = tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>();
    let triggers = template
        .schema
        .iter()
        .filter(|(kind, _, table, _)| kind == "trigger" && names.contains(&table.as_str()))
        .map(|(_, name, _, sql)| (name.clone(), sql.clone().unwrap_or_default()))
        .collect::<Vec<_>>();
    for (name, _) in &triggers {
        sqlx::query(&format!("DROP TRIGGER {}", quote(name)))
            .execute(&mut out)
            .await?;
    }

    let mut reports = Vec::new();
    let mut rewritten = Vec::new();
    for table in &tables {
        let report = load(table, pg, &mut out, &mut rewritten)
            .await
            .with_context(|| format!("table {}", table.name))?;
        eprintln!(
            "sqlite export: table {}: {} read, {} written, {} left out",
            report.table,
            report.rows_read,
            report.rows_written,
            report.left_out.len()
        );
        reports.push(report);
        // Internal test hook only; never sourced from an operator environment.
        if stop_after == Some(reports.len()) {
            // Abrupt process exit without Rust destructors, only in the test
            // executable. The command line always passes None for stop_after.
            #[cfg(test)]
            if std::env::var_os("KOMIKA_PG_EXPORT_TEST_CHILD").is_some() {
                std::process::exit(86);
            }
            bail!("test interruption after {} tables", reports.len());
        }
    }
    let sequences = sequences(&tables, pg, &mut out).await?;
    for (_, sql) in &triggers {
        sqlx::raw_sql(sql).execute(&mut out).await?;
    }

    // The checks, all before the commit.
    let tombstones = if target < 122 {
        let comments: Vec<(String, String)> = sqlx::query_as(
            "SELECT id::text, deleted_by FROM public.comments WHERE deleted_by IS NOT NULL ORDER BY id",
        )
        .fetch_all(&mut *pg)
        .await?;
        Some(Tombstones {
            count: comments.len(),
            comments: comments
                .into_iter()
                .map(|(comment_id, deleted_by)| Tombstone {
                    comment_id,
                    deleted_by,
                })
                .collect(),
        })
    } else {
        None
    };
    for (table, report) in tables.iter().zip(&mut reports) {
        compare(table, report, pg, &mut out).await?;
    }
    // After the comparison: the copy is proven equal to PostgreSQL first, and
    // only then do the listed rows get the id of a work the kept file has.
    let unresolved = work_references(pg, &mut out, &mut rewritten).await?;
    let schema: Vec<(String, String, String, Option<String>)> =
        sqlx::query_as("SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name")
            .fetch_all(&mut out)
            .await?;
    let mut schema = schema;
    schema.sort();
    let mut expected = template.schema.clone();
    expected.sort();
    ensure!(
        schema == expected,
        "the output's schema (tables, indexes, triggers) differs from the template's"
    );
    const MIGRATIONS: &str = "SELECT version,description,success,checksum,execution_time,CAST(installed_on AS TEXT) FROM _sqlx_migrations ORDER BY version";
    type Migration = (i64, String, bool, Vec<u8>, i64, Option<String>);
    let kept: Vec<Migration> = sqlx::query_as(MIGRATIONS)
        .fetch_all(&mut template.conn)
        .await?;
    let written: Vec<Migration> = sqlx::query_as(MIGRATIONS).fetch_all(&mut out).await?;
    ensure!(kept == written, "the output's migration history changed");
    let mut not_copied = BTreeMap::new();
    for table in plan::tables().iter().filter(|t| !is_copied(&t.name)) {
        let count = format!("SELECT count(*) FROM {}", quote(&table.name));
        let kept: i64 = sqlx::query_scalar(&count)
            .fetch_one(&mut template.conn)
            .await?;
        let written: i64 = sqlx::query_scalar(&count).fetch_one(&mut out).await?;
        ensure!(
            kept == written,
            "{} is not copied, yet its row count changed",
            table.name
        );
        not_copied.insert(table.name.clone(), written);
    }
    let mut broken: BTreeMap<String, u64> = BTreeMap::new();
    let mut check = sqlx::query("PRAGMA foreign_key_check").fetch(&mut out);
    while let Some(row) = check.try_next().await? {
        *broken.entry(row.try_get(0)?).or_default() += 1;
    }
    drop(check);
    ensure!(
        broken.is_empty(),
        "foreign_key_check found broken references in the output (table: rows): {broken:?}"
    );
    let pins = source_pins(&mut template.conn, &mut out).await?;
    let aliases = alias_changes(pg, &mut template.conn, &mut out).await?;
    quick_check(&mut out).await?;
    sqlx::query("COMMIT").execute(&mut out).await?;
    out.close().await?;

    // The output is one closed file: SQLite removes its own -wal on the last
    // close. Content there would mean the commit is not in the main file.
    for suffix in ["-wal", "-journal"] {
        let sidecar = sibling(partial, suffix);
        ensure!(
            std::fs::metadata(&sidecar).map_or(0, |m| m.len()) == 0,
            "{} still has content after the last close",
            sidecar.display()
        );
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(sibling(partial, suffix));
    }
    template::close(template).await?;
    template::refuse_live_sidecars(&args.template)?;
    ensure!(
        template::sha256(&args.template)? == template_sha256,
        "the template changed during the run; the output is discarded"
    );
    std::fs::File::open(partial)?.sync_all()?;
    ensure!(
        !args.output.exists(),
        "the output path appeared during the run; the export never replaces a file"
    );
    std::fs::rename(partial, &args.output)?;
    if let Some(directory) = args.output.parent() {
        // Make the new name durable. Not every platform can sync a directory.
        let _ = std::fs::File::open(directory).and_then(|d| d.sync_all());
    }
    Ok(Filled {
        target,
        tables: reports,
        tombstones,
        rewritten,
        unresolved,
        pins,
        aliases,
        sequences,
        not_copied,
    })
}

/// Read the table back from the output and compare it with what PostgreSQL
/// gave, in the importer's canonical form: the same count, and for every
/// column the same value after the importer's own SQLite reading rule.
pub(super) async fn compare(
    table: &Table,
    report: &mut TableReport,
    pg: &mut PgConnection,
    out: &mut SqliteConnection,
) -> Result<()> {
    let sql = table.sqlite_select();
    let mut rows = sqlx::query(&sql).fetch(&mut *out);
    while let Some(row) = rows.try_next().await? {
        add(&mut report.sqlite, table, &row_cells(table, &row)?.0);
    }
    drop(rows);
    if report.sqlite != report.postgres {
        // Rows left out on purpose show here as "extra in PostgreSQL".
        let differences = verifier::differences(table, out, pg).await?;
        bail!(
            "{} differs between PostgreSQL ({} rows) and the output ({} rows); primary keys/columns: {}",
            table.name,
            report.postgres.rows,
            report.sqlite.rows,
            differences.join("; ")
        );
    }
    Ok(())
}

fn add(checksum: &mut Checksum, table: &Table, cells: &[Cell]) {
    let mut bytes = Vec::new();
    for cell in cells {
        frame(cell, &mut bytes);
    }
    checksum.add(&table.name, &bytes);
}

fn key_text(cell: &Cell) -> String {
    match cell {
        Cell::Text(s) | Cell::Uuid(s) => s.clone(),
        Cell::Integer(n) => n.to_string(),
        // No planned primary key has another type.
        _ => String::new(),
    }
}

/// Replace one table: delete the template's rows, then write PostgreSQL's
/// rows one at a time as they arrive. Nothing is held but the current row.
///
/// A foreign key to `work`, which is not copied: the parent must be in the
/// kept catalogue, or the row cannot be written. When PostgreSQL merged
/// exactly one work of the kept file into the missing parent, the row is
/// written with that work's id and listed in `rewritten`. Otherwise it is left
/// out and listed with the candidates.
///
/// Such rows are written in a second pass over the table, after every row
/// that is written as it is. A row of the kept work itself then always wins
/// the key over a row that was led back to it, whichever PostgreSQL returns
/// first, and the row that was led back is left out and listed.
pub(super) async fn load(
    table: &Table,
    pg: &mut PgConnection,
    out: &mut SqliteConnection,
    rewritten: &mut Vec<Rewrite>,
) -> Result<TableReport> {
    let columns = table.copied().collect::<Vec<_>>();
    let name = quote(&table.name);
    let template_rows: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {name}"))
        .fetch_one(&mut *out)
        .await?;
    sqlx::query(&format!("DELETE FROM {name}"))
        .execute(&mut *out)
        .await?;
    let insert = format!(
        "INSERT INTO {name} ({}) VALUES ({})",
        columns
            .iter()
            .map(|c| quote(&c.name))
            .collect::<Vec<_>>()
            .join(","),
        vec!["?"; columns.len()].join(",")
    );
    // For each such foreign key: the parents the kept file lacks, each with
    // the kept works that were merged into it. Read before the rows, in the
    // same snapshot; the table is one an admin fills by hand.
    let mut parents = Vec::new();
    for fk in table.fks.iter().filter(|fk| !is_copied(&fk.parent)) {
        ensure!(
            fk.parent == "work" && fk.parent_column == "id",
            "{}.{} points at {}, which is not copied: no rule for it",
            table.name,
            fk.column,
            fk.parent
        );
        let Some(position) = columns.iter().position(|c| c.name == fk.column) else {
            bail!("foreign-key column is not copied");
        };
        let used: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT DISTINCT {column}::text FROM public.{name} WHERE {column} IS NOT NULL",
            column = quote(&fk.column)
        ))
        .fetch_all(&mut *pg)
        .await?;
        let mut missing = BTreeMap::new();
        for id in used {
            let present: bool =
                sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM work WHERE id = ?)")
                    .bind(&id)
                    .fetch_one(&mut *out)
                    .await?;
            if !present {
                let kept = merged_from(pg, out, &id).await?;
                missing.insert(id, kept);
            }
        }
        parents.push((position, fk.column.clone(), missing));
    }
    let mut report = TableReport {
        table: table.name.clone(),
        template_rows,
        rows_read: 0,
        rows_written: 0,
        left_out: Vec::new(),
        postgres: Checksum::default(),
        sqlite: Checksum::default(),
    };
    let mut rewrites: BTreeMap<(String, String, String), u64> = BTreeMap::new();
    let lead_back = parents
        .iter()
        .any(|(_, _, missing)| missing.values().any(|kept| kept.len() == 1));
    for led_back in [false, true] {
        if led_back && !lead_back {
            break;
        }
        let mut select = table.pg_select();
        if led_back {
            // Two rows led back to one key: the same one is written each time.
            let order = table
                .keys
                .iter()
                .map(|key| format!("{}::text COLLATE \"C\"", quote(key)))
                .collect::<Vec<_>>();
            select = format!("{select} ORDER BY {}", order.join(","));
        }
        let mut rows = sqlx::query(&select).fetch(&mut *pg);
        while let Some(row) = rows.try_next().await? {
            let mut cells = columns
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    pg_cell(c, row.try_get::<Option<&str>, _>(i)?)
                        .with_context(|| format!("column {}", c.name))
                })
                .collect::<Result<Vec<_>>>()?;
            let key = |cells: &[Cell]| {
                table
                    .keys
                    .iter()
                    .map(|k| {
                        let i = columns.iter().position(|c| &c.name == k);
                        (k.clone(), i.map_or(String::new(), |i| key_text(&cells[i])))
                    })
                    .collect::<BTreeMap<_, _>>()
            };
            // The key as PostgreSQL has it, for the report.
            let postgres_key = key(&cells);
            let mut left_out = None;
            let mut rewrite = None;
            for (position, column, missing) in &parents {
                let id = key_text(&cells[*position]);
                let Some(kept) = missing.get(&id) else {
                    continue;
                };
                if let [kept_id] = kept.as_slice() {
                    cells[*position] = Cell::Text(kept_id.clone());
                    rewrite = Some((column.clone(), id, kept_id.clone()));
                } else {
                    left_out = Some(LeftOut {
                        key: postgres_key.clone(),
                        reason: format!("{column} is not in the kept file's work table"),
                        kept_work_candidates: kept.clone(),
                    });
                }
            }
            // Each row belongs to one pass.
            if (left_out.is_none() && rewrite.is_some()) != led_back {
                continue;
            }
            report.rows_read += 1;
            if let Some(left_out) = left_out {
                report.left_out.push(left_out);
                continue;
            }
            let mut query = sqlx::query(&insert);
            for (c, cell) in columns.iter().zip(&cells) {
                query = match value(&table.name, c, cell)
                    .with_context(|| format!("column {}", c.name))?
                {
                    Value::Null => query.bind(None::<String>),
                    Value::Text(s) => query.bind(s),
                    Value::Integer(n) => query.bind(n),
                    Value::Real(n) => query.bind(n),
                    Value::Blob(b) => query.bind(b),
                };
            }
            match (query.execute(&mut *out).await, rewrite) {
                (Ok(_), rewrite) => {
                    if let Some(rewrite) = rewrite {
                        *rewrites.entry(rewrite).or_default() += 1;
                    }
                }
                // The kept work already has a row with this key: nothing is
                // written over it. SQLite undid the statement.
                (Err(sqlx::Error::Database(error)), Some((column, _, kept_id)))
                    if error.is_unique_violation() =>
                {
                    report.left_out.push(LeftOut {
                    key: postgres_key,
                    reason: format!(
                        "{column} is not in the kept file's work table, and the kept work it was merged from already has this row"
                    ),
                    kept_work_candidates: vec![kept_id],
                });
                    continue;
                }
                (Err(error), _) => return Err(error.into()),
            }
            // The row as it was written: for a rewritten row, with the kept id.
            add(&mut report.postgres, table, &cells);
            report.rows_written += 1;
        }
    }
    for ((column, postgres_work_id, kept_work_id), rows) in rewrites {
        rewritten.push(Rewrite {
            table: table.name.clone(),
            column,
            postgres_work_id,
            kept_work_id,
            rows,
        });
    }
    Ok(report)
}

/// AUTOINCREMENT never gives an id a second time. SQLite raised
/// `sqlite_sequence` to the highest id written; PostgreSQL's sequence can be
/// further on (rows made and deleted since), and the template's value is kept
/// by the DELETE. The highest of the three stays.
async fn sequences(
    tables: &[Table],
    pg: &mut PgConnection,
    out: &mut SqliteConnection,
) -> Result<BTreeMap<String, i64>> {
    let mut result = BTreeMap::new();
    for table in tables {
        let Some(c) = table.columns.iter().find(|c| c.identity) else {
            continue;
        };
        let autoincrement: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE type='table' AND name=? AND sql LIKE '%AUTOINCREMENT%')",
        )
        .bind(&table.name)
        .fetch_one(&mut *out)
        .await?;
        if !autoincrement {
            continue;
        }
        let sequence: String = sqlx::query_scalar("SELECT pg_get_serial_sequence($1,$2)")
            .bind(format!("public.{}", table.name))
            .bind(&c.name)
            .fetch_one(&mut *pg)
            .await?;
        // A qualified, quoted identifier from PostgreSQL, never operator input.
        ensure!(
            sequence
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.\"".contains(&b)),
            "unexpected sequence identifier"
        );
        let (last, called): (i64, bool) =
            sqlx::query_as(&format!("SELECT last_value,is_called FROM {sequence}"))
                .fetch_one(&mut *pg)
                .await?;
        let issued = if called { last } else { 0 };
        let current: Option<i64> =
            sqlx::query_scalar("SELECT max(seq) FROM sqlite_sequence WHERE name=?")
                .bind(&table.name)
                .fetch_one(&mut *out)
                .await?;
        let seq = current.unwrap_or(0).max(issued);
        result.insert(table.name.clone(), seq);
        if current == Some(seq) || (current.is_none() && seq == 0) {
            continue;
        }
        sqlx::query("DELETE FROM sqlite_sequence WHERE name=?")
            .bind(&table.name)
            .execute(&mut *out)
            .await?;
        sqlx::query("INSERT INTO sqlite_sequence (name,seq) VALUES (?,?)")
            .bind(&table.name)
            .bind(seq)
            .execute(&mut *out)
            .await?;
    }
    Ok(result)
}

/// Written rows whose `w_` work id is neither a work of the output nor a
/// redirect to one. The work was made, merged or deleted on PostgreSQL after
/// the cutover.
///
/// A merge on PostgreSQL moves the user rows of a work the kept file has to
/// the surviving work. When the survivor was made after the cutover, the kept
/// file has only the merged work. If exactly one work of the kept file was
/// merged into the missing one, the rows get that work's id back and the
/// report lists the rewrite. With two or more there is no way to tell which
/// one a row belonged to, and a row with the kept id may already hold the same
/// key: then the row keeps its id and the report names the candidates. No row
/// is dropped either way.
async fn work_references(
    pg: &mut PgConnection,
    out: &mut SqliteConnection,
    rewritten: &mut Vec<Rewrite>,
) -> Result<Vec<UnresolvedReference>> {
    let mut unresolved = Vec::new();
    let mut merged: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (table, column, filter) in WORK_REFERENCES {
        let sql = format!(
            "SELECT t.{column}, count(*) FROM {table} t WHERE ({filter}) AND t.{column} GLOB 'w_*' \
             AND NOT EXISTS (SELECT 1 FROM work w WHERE w.id = t.{column}) \
             AND NOT EXISTS (SELECT 1 FROM work_redirect r JOIN work w ON w.id = r.new_id WHERE r.old_id = t.{column}) \
             GROUP BY t.{column} ORDER BY t.{column}"
        );
        let missing: Vec<(String, i64)> = sqlx::query_as(&sql).fetch_all(&mut *out).await?;
        let mut left = UnresolvedReference {
            table: (*table).into(),
            column: (*column).into(),
            rows: 0,
            missing_work_ids: Vec::new(),
            kept_work_candidates: BTreeMap::new(),
        };
        for (id, rows) in missing {
            if !merged.contains_key(&id) {
                merged.insert(id.clone(), merged_from(pg, out, &id).await?);
            }
            let kept = &merged[&id];
            if let [kept_id] = kept.as_slice() {
                // One statement: SQLite undoes all of it when one row would
                // repeat a unique key, and the transaction goes on.
                let update = sqlx::query(&format!(
                    "UPDATE {table} SET {column} = ? WHERE ({filter}) AND {column} = ?"
                ))
                .bind(kept_id)
                .bind(&id)
                .execute(&mut *out)
                .await;
                match update {
                    Ok(done) => {
                        ensure!(
                            done.rows_affected() == rows as u64,
                            "{table}.{column}: {} rows were rewritten, {rows} pointed at the missing work",
                            done.rows_affected()
                        );
                        rewritten.push(Rewrite {
                            table: (*table).into(),
                            column: (*column).into(),
                            postgres_work_id: id,
                            kept_work_id: kept_id.clone(),
                            rows: rows as u64,
                        });
                        continue;
                    }
                    Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {}
                    Err(error) => return Err(error.into()),
                }
            }
            if !kept.is_empty() {
                left.kept_work_candidates.insert(id.clone(), kept.clone());
            }
            left.rows += rows;
            left.missing_work_ids.push(id);
        }
        if !left.missing_work_ids.is_empty() {
            unresolved.push(left);
        }
    }
    Ok(unresolved)
}

/// The works of the kept file that PostgreSQL merged into `id`, directly or
/// through another merged work. The merge keeps one row in `work_redirect`
/// for each merged work and points it at the survivor.
async fn merged_from(
    pg: &mut PgConnection,
    out: &mut SqliteConnection,
    id: &str,
) -> Result<Vec<String>> {
    let sources: Vec<String> = sqlx::query_scalar(
        "WITH RECURSIVE source(id) AS (\
           SELECT old_id FROM public.work_redirect WHERE new_id = $1 \
           UNION SELECT r.old_id FROM public.work_redirect r JOIN source s ON r.new_id = s.id) \
         SELECT id FROM source",
    )
    .bind(id)
    .fetch_all(&mut *pg)
    .await?;
    let mut kept = Vec::new();
    for source in sources {
        let present: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM work WHERE id = ?)")
            .bind(&source)
            .fetch_one(&mut *out)
            .await?;
        if present {
            kept.push(source);
        }
    }
    kept.sort();
    Ok(kept)
}

/// Both sides in the order of the unique key, one row of each in memory.
/// PostgreSQL sorts by bytes (`COLLATE "C"`), as SQLite's default collation
/// and Rust's string order do.
async fn alias_changes(
    pg: &mut PgConnection,
    template: &mut SqliteConnection,
    out: &mut SqliteConnection,
) -> Result<AliasChanges> {
    type Alias = (String, String, Option<String>);
    let mut theirs = sqlx::query_as::<_, Alias>(
        "SELECT work_id, normalized_title, lang FROM public.work_alias \
         ORDER BY work_id COLLATE \"C\", normalized_title COLLATE \"C\", lang COLLATE \"C\" NULLS FIRST",
    )
    .fetch(&mut *pg);
    let mut ours = sqlx::query_as::<_, Alias>(
        "SELECT work_id, normalized_title, lang FROM work_alias ORDER BY work_id, normalized_title, lang",
    )
    .fetch(&mut *template);
    let mut changes = AliasChanges {
        added_on_kept_works: 0,
        removed: 0,
    };
    // The last work looked up: the rows of one work follow each other.
    let mut kept_work: Option<(String, bool)> = None;
    let (mut their, mut our) = (theirs.try_next().await?, ours.try_next().await?);
    loop {
        let order = match (&their, &our) {
            (None, None) => break,
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(their), Some(our)) => their.cmp(our),
        };
        if let (true, Some((work, _, _))) = (order.is_lt(), &their) {
            if kept_work.as_ref().is_none_or(|(id, _)| id != work) {
                let present: bool =
                    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM work WHERE id = ?)")
                        .bind(work)
                        .fetch_one(&mut *out)
                        .await?;
                kept_work = Some((work.clone(), present));
            }
            if kept_work.as_ref().is_some_and(|(_, present)| *present) {
                changes.added_on_kept_works += 1;
            }
        }
        if order.is_gt() {
            changes.removed += 1;
        }
        if order.is_le() {
            their = theirs.try_next().await?;
        }
        if order.is_ge() {
            our = ours.try_next().await?;
        }
    }
    Ok(changes)
}

/// The pins that changed between the template and PostgreSQL. One row for
/// each source an admin pinned, so both lists are short.
async fn source_pins(
    template: &mut SqliteConnection,
    out: &mut SqliteConnection,
) -> Result<SourcePins> {
    const PINS: &str = "SELECT source_id FROM source_nsfw_pin ORDER BY source_id";
    let kept: Vec<String> = sqlx::query_scalar(PINS).fetch_all(&mut *template).await?;
    let written: Vec<String> = sqlx::query_scalar(PINS).fetch_all(&mut *out).await?;
    Ok(SourcePins {
        pinned: written
            .iter()
            .filter(|source| !kept.contains(source))
            .cloned()
            .collect(),
        unpinned: kept
            .iter()
            .filter(|source| !written.contains(source))
            .cloned()
            .collect(),
    })
}
