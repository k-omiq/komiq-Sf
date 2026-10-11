use anyhow::{ensure, Result};
use futures::TryStreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{Connection, PgConnection, Row, SqliteConnection};
use std::{collections::BTreeMap, path::Path};

use super::{
    conversion::{frame, pg_cell, time_text, Cell},
    plan::{quote, tables, Table},
    source::{key, row_cells},
};

/// Addition modulo 2^256 preserves multiplicity (XOR loses duplicate pairs).
/// Hashes are domain-separated by table and framed by column order/type/length.
#[derive(Default, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Checksum {
    pub rows: u64,
    pub sum: [u8; 32],
}
impl Checksum {
    pub fn add(&mut self, table: &str, bytes: &[u8]) {
        let mut hash = Sha256::new();
        hash.update(b"komika-pg-import-v1\0");
        hash.update((table.len() as u64).to_be_bytes());
        hash.update(table.as_bytes());
        hash.update(bytes);
        let digest = hash.finalize();
        let mut carry = 0u16;
        for i in (0..32).rev() {
            let sum = u16::from(self.sum[i]) + u16::from(digest[i]) + carry;
            self.sum[i] = sum as u8;
            carry = sum >> 8;
        }
        self.rows += 1;
    }
    fn cells(&mut self, table: &str, cells: &[Cell]) {
        let mut bytes = Vec::new();
        for cell in cells {
            frame(cell, &mut bytes);
        }
        self.add(table, &bytes);
    }
}

#[derive(Serialize)]
pub(super) struct TableCheck {
    pub table: String,
    pub sqlite: Checksum,
    pub postgres: Checksum,
}
#[derive(Serialize)]
pub(super) struct Report {
    pub tables: Vec<TableCheck>,
    pub truncated_fractions: BTreeMap<String, u64>,
    pub postgres_matching_truncated_values: BTreeMap<String, u64>,
    pub complete: bool,
}

pub(super) async fn run(path: &Path, url: &str) -> Result<Report> {
    let mut source = super::source::open(path).await?;
    super::source::quick_check(&mut source).await?;
    let mut target = super::connect(url).await?;
    // A repeatable, read-only PostgreSQL snapshot: no rebuild, ALTER, temp table,
    // sequence advancement, or SQLx migration writes in the verifier.
    sqlx::query("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut target)
        .await?;
    super::target_definition(&mut target).await?;
    let report = compare_tables(&mut source, &mut target).await?;
    constraints(&mut target).await?;
    sequences(&mut target).await?;
    search(&mut target).await?;
    let disabled:i64=sqlx::query_scalar("SELECT count(*) FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND NOT t.tgisinternal AND t.tgenabled <> 'O'").fetch_one(&mut target).await?;
    ensure!(
        disabled == 0,
        "user triggers are disabled or not in normal enabled mode"
    );
    ensure!(
        super::import_complete(&mut target).await?,
        "import completion marker is absent"
    );
    source.close().await?;
    // The read-only transaction has nothing to commit, and the result stands.
    // See `hang_up` for why a failing close is not an error.
    super::hang_up(target).await;
    Ok(Report {
        complete: true,
        ..report
    })
}

fn pg_cells(table: &Table, row: &sqlx::postgres::PgRow) -> Result<Vec<Cell>> {
    table
        .copied()
        .enumerate()
        .map(|(i, c)| pg_cell(c, row.try_get::<Option<&str>, _>(i)?))
        .collect()
}

pub(super) async fn compare_tables(
    source: &mut SqliteConnection,
    target: &mut PgConnection,
) -> Result<Report> {
    super::schema::verify(target).await?;
    schema(target).await?;
    compare_rows(source, target).await
}

/// The importer's own check before it marks completion. The importer's
/// migrator has run by then, so the target is at the latest level and this is
/// the same check as `compare_tables`; it is written against the level the
/// target has only so that a test can run the importer of an older release
/// (`importer::run_with_migrator`) to its end.
pub(super) async fn compare_tables_at_applied_level(
    source: &mut SqliteConnection,
    target: &mut PgConnection,
) -> Result<Report> {
    super::schema::verify(target).await?;
    schema_at_applied_level(target).await?;
    compare_rows(source, target).await
}

async fn compare_rows(source: &mut SqliteConnection, target: &mut PgConnection) -> Result<Report> {
    let mut report = Report {
        tables: Vec::new(),
        truncated_fractions: BTreeMap::new(),
        postgres_matching_truncated_values: BTreeMap::new(),
        complete: false,
    };
    for table in tables() {
        let mut a = Checksum::default();
        let mut b = Checksum::default();
        let mut cut_batch = Vec::new();
        let sql = table.sqlite_select();
        let mut stream = sqlx::query(&sql).fetch(&mut *source);
        while let Some(row) = stream.try_next().await? {
            let (cells, cuts) = row_cells(&table, &row)?;
            a.cells(&table.name, &cells);
            if !cuts.is_empty() {
                let mut object = serde_json::Map::new();
                for (c, v) in table.copied().zip(&cells) {
                    if table.keys.contains(&c.name) {
                        object.insert(c.name.clone(), serde_json::Value::String(key_value(v)?));
                    }
                    if cuts.contains(&format!("{}.{}", table.name, c.name)) {
                        let Cell::Time(us) = v else { unreachable!() };
                        object.insert(c.name.clone(), serde_json::Value::String(time_text(*us)?));
                        *report
                            .truncated_fractions
                            .entry(format!("{}.{}", table.name, c.name))
                            .or_default() += 1;
                    }
                }
                cut_batch.push(serde_json::Value::Object(object));
                if cut_batch.len() == 128 {
                    matching_cuts(
                        &table,
                        target,
                        &cut_batch,
                        &mut report.postgres_matching_truncated_values,
                    )
                    .await?;
                    cut_batch.clear();
                }
            }
        }
        drop(stream);
        if !cut_batch.is_empty() {
            matching_cuts(
                &table,
                target,
                &cut_batch,
                &mut report.postgres_matching_truncated_values,
            )
            .await?;
        }
        let sql = table.pg_select();
        let mut stream = sqlx::query(&sql).fetch(&mut *target);
        while let Some(row) = stream.try_next().await? {
            b.cells(&table.name, &pg_cells(&table, &row)?);
        }
        drop(stream);
        eprintln!(
            "sqlite import: verify {}: SQLite={} PG={} checksum {}",
            table.name,
            a.rows,
            b.rows,
            if a == b { "OK" } else { "DIFF" }
        );
        if a != b {
            let differences = differences(&table, source, target).await?;
            anyhow::bail!(
                "{} mismatch (SQLite {}, PG {} rows); differing primary keys/columns: {}",
                table.name,
                a.rows,
                b.rows,
                differences.join("; ")
            );
        }
        report.tables.push(TableCheck {
            table: table.name,
            sqlite: a,
            postgres: b,
        });
    }
    ensure!(
        report.truncated_fractions == report.postgres_matching_truncated_values,
        "truncated fraction obligations differ on PostgreSQL"
    );
    Ok(report)
}

/// The schema of the LATEST migration level: every column of the import plan
/// and every index of every embedded migration. This is the final
/// verification; it runs after the importer's migrator (or
/// `importer::bring_up_to_date`) brought the target to that level.
pub(super) async fn schema(target: &mut PgConnection) -> Result<()> {
    columns(target).await?;
    indexes(target, &expected_indexes()).await
}

/// The schema of the migration level that the target REALLY has, for the
/// ownership and emptiness proof. A target that an older release migrated has
/// fewer indexes than this server's migrations create in the end, and that is
/// no reason to refuse it: the migrator adds them later. Its indexes must be
/// exactly those of the migrations it has applied. The columns of the import
/// plan describe the latest level only, so they are compared only there (the
/// final verification compares them in every case).
pub(super) async fn schema_at_applied_level(target: &mut PgConnection) -> Result<()> {
    let applied = applied_migrations(target).await?;
    if applied == embedded_migrations().len() {
        columns(target).await?;
    }
    indexes(target, &expected_indexes_through(applied)).await
}

fn embedded_migrations() -> Vec<&'static sqlx::migrate::Migration> {
    let mut migrations = super::importer::POSTGRES
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
        .collect::<Vec<_>>();
    migrations.sort_by_key(|m| m.version);
    migrations
}

/// How many of the embedded PostgreSQL migrations the target has applied,
/// proven against `_sqlx_migrations`: every applied migration completed, is
/// the embedded migration of that version with the same checksum, and the
/// applied ones are the first N in version order. A migration that this
/// server does not know, or that differs from the embedded file, is refused.
/// A target without the table has applied none.
pub(crate) async fn applied_migrations(target: &mut PgConnection) -> Result<usize> {
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut *target)
            .await?;
    if !exists {
        return Ok(0);
    }
    let applied: Vec<(i64, bool, Vec<u8>)> = sqlx::query_as(
        "SELECT version,success,checksum FROM public._sqlx_migrations ORDER BY version",
    )
    .fetch_all(&mut *target)
    .await?;
    let embedded = embedded_migrations();
    ensure!(
        applied.len() <= embedded.len(),
        "the PostgreSQL database has {} migrations applied and this server knows {}: it belongs to a newer release",
        applied.len(),
        embedded.len()
    );
    for ((version, success, checksum), migration) in applied.iter().zip(&embedded) {
        ensure!(
            *version == migration.version,
            "PostgreSQL migration {version} is not the next migration of this server ({})",
            migration.version
        );
        ensure!(*success, "PostgreSQL migration {version} did not complete");
        ensure!(
            checksum.as_slice() == &*migration.checksum,
            "PostgreSQL migration {version} differs from the migration this server embeds"
        );
    }
    Ok(applied.len())
}

pub(crate) async fn columns(target: &mut PgConnection) -> Result<()> {
    for table in tables() {
        let columns:Vec<(String,String,bool,bool,bool)>=sqlx::query_as("SELECT a.attname,format_type(a.atttypid,a.atttypmod),a.attnotnull,a.attgenerated <> '',a.attidentity <> '' FROM pg_attribute a WHERE a.attrelid=to_regclass($1) AND a.attnum>0 AND NOT a.attisdropped ORDER BY a.attnum")
            .bind(format!("public.{}",table.name)).fetch_all(&mut *target).await?;
        ensure!(
            columns.len() == table.columns.len(),
            "{} PostgreSQL schema column count differs",
            table.name
        );
        for ((name, kind, required, generated, identity), c) in columns.iter().zip(&table.columns) {
            let target_kind = if c.target == "timestamptz" {
                "timestamp with time zone"
            } else {
                &c.target
            };
            ensure!(
                name == &c.name
                    && kind == target_kind
                    && *required == c.required
                    && *generated == c.generated
                    && *identity == c.identity,
                "{}.{} PostgreSQL schema differs",
                table.name,
                c.name
            );
        }
    }
    Ok(())
}

// Missing unique indexes must fail just as an invalid existing index does.
// These names are the migration contract, independent of import state.
async fn indexes(target: &mut PgConnection, expected: &BTreeMap<String, bool>) -> Result<()> {
    let actual:Vec<(String,bool)>=sqlx::query_as("SELECT x.relname,i.indisunique FROM pg_index i JOIN pg_class c ON c.oid=i.indrelid JOIN pg_class x ON x.oid=i.indexrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relname NOT IN ('_sqlx_migrations','komika_import_state','komika_import_table','komika_startup_import')")
        .fetch_all(&mut *target).await?;
    let actual = actual
        .into_iter()
        .collect::<std::collections::BTreeMap<_, _>>();
    if actual != *expected {
        let names = |a: &BTreeMap<String, bool>, b: &BTreeMap<String, bool>| {
            a.iter()
                .filter(|(name, unique)| b.get(*name) != Some(unique))
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>()
        };
        anyhow::bail!(
            "PostgreSQL indexes differ from the migrations_pg files (missing or changed in the database: {:?}; not created by a migration this verifier can read: {:?})",
            names(expected, &actual),
            names(&actual, expected)
        );
    }
    Ok(())
}

/// Index name -> unique, read from EVERY embedded PostgreSQL migration in
/// version order. A later migration that creates or drops an index changes
/// this expectation by itself, as long as it keeps the one-statement-per-line
/// spelling of the baseline. The test
/// `postgres_migrated_schema_matches_every_index_and_foreign_key_the_verifier_expects`
/// fails when a migration uses a spelling this reader does not understand.
pub(super) fn expected_indexes() -> BTreeMap<String, bool> {
    expected_indexes_through(usize::MAX)
}

/// The same for a target that has applied only the first `applied` embedded
/// migrations.
pub(super) fn expected_indexes_through(applied: usize) -> BTreeMap<String, bool> {
    let mut expected = BTreeMap::new();
    for migration in embedded_migrations().into_iter().take(applied) {
        index_statements(&migration.sql, &mut expected);
    }
    expected
}

pub(super) fn index_statements(sql: &str, expected: &mut BTreeMap<String, bool>) {
    let name = |word: Option<&str>| {
        word.unwrap_or_default()
            .trim_end_matches(';')
            .trim_matches('"')
            .to_owned()
    };
    for line in sql.lines() {
        let line = line.trim();
        let mut words = line
            .split_whitespace()
            .filter(|word| !matches!(*word, "CONCURRENTLY" | "IF" | "NOT" | "EXISTS"));
        match (words.next(), words.next()) {
            (Some("CREATE"), Some("INDEX")) => {
                expected.insert(name(words.next()), false);
            }
            (Some("CREATE"), Some("UNIQUE")) if words.next() == Some("INDEX") => {
                expected.insert(name(words.next()), true);
            }
            (Some("DROP"), Some("INDEX")) => {
                expected.remove(&name(words.next()));
            }
            (Some("CONSTRAINT"), constraint)
                if line.contains("PRIMARY KEY") || line.contains(" UNIQUE ") =>
            {
                expected.insert(name(constraint), true);
            }
            _ => {}
        }
    }
}

fn key_value(cell: &Cell) -> Result<String> {
    match cell {
        Cell::Text(s) | Cell::Uuid(s) => Ok(s.clone()),
        Cell::Integer(n) => Ok(n.to_string()),
        _ => anyhow::bail!("invalid primary key type"),
    }
}

async fn matching_cuts(
    table: &Table,
    target: &mut PgConnection,
    batch: &[serde_json::Value],
    counts: &mut BTreeMap<String, u64>,
) -> Result<()> {
    let dates = table
        .copied()
        .filter(|c| c.target == "timestamptz" && c.source == "TEXT")
        .collect::<Vec<_>>();
    let join = table
        .keys
        .iter()
        .map(|k| {
            let c = table.columns.iter().find(|c| &c.name == k).unwrap();
            format!("p.{}=(wanted->>'{}')::{}", quote(k), k, c.target)
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    let sums = dates
        .iter()
        .map(|c| {
            format!(
                "count(*) FILTER (WHERE wanted ? '{}' AND p.{}=(wanted->>'{}')::timestamptz)",
                c.name,
                quote(&c.name),
                c.name
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT {sums} FROM jsonb_array_elements($1::jsonb) wanted JOIN public.{} p ON {join}",
        quote(&table.name)
    );
    let row = sqlx::query(&sql)
        .bind(serde_json::to_string(batch)?)
        .fetch_one(target)
        .await?;
    for (i, c) in dates.iter().enumerate() {
        let n: i64 = row.try_get(i)?;
        if n > 0 {
            *counts
                .entry(format!("{}.{}", table.name, c.name))
                .or_default() += n as u64;
        }
    }
    Ok(())
}

// Diagnostics never affect the table verdict, which is already a checksum mismatch.
const DIFFERENCE_LIMIT: usize = 5;

pub(crate) async fn differences(
    table: &Table,
    source: &mut SqliteConnection,
    target: &mut PgConnection,
) -> Result<Vec<String>> {
    let mut found = Vec::new();
    let cols = table.copied().collect::<Vec<_>>();
    let lookup_pg = format!(
        "{} WHERE {}",
        table.pg_select(),
        table
            .keys
            .iter()
            .enumerate()
            .map(|(i, k)| {
                let c = table.columns.iter().find(|c| &c.name == k).unwrap();
                format!("{}=${}::{}", quote(k), i + 1, c.target)
            })
            .collect::<Vec<_>>()
            .join(" AND ")
    );
    let sql = format!(
        "{} ORDER BY {}",
        table.sqlite_select(),
        table
            .keys
            .iter()
            .map(|k| quote(k))
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut stream = sqlx::query(&sql).fetch(&mut *source);
    while let Some(row) = stream.try_next().await? {
        let cells = row_cells(table, &row)?.0;
        let mut q = sqlx::query(&lookup_pg);
        for k in &table.keys {
            q = q.bind(key_value(
                &cells[cols.iter().position(|c| &c.name == k).unwrap()],
            )?);
        }
        let other = q.fetch_optional(&mut *target).await?;
        if let Some(other) = other {
            let other = pg_cells(table, &other)?;
            let names = cols
                .iter()
                .zip(cells.iter().zip(&other))
                .filter(|(_, (a, b))| a != b)
                .map(|(c, _)| c.name.as_str())
                .collect::<Vec<_>>();
            if !names.is_empty() {
                found.push(format!(
                    "{} columns {}",
                    key(table, &cells),
                    names.join(",")
                ));
            }
        } else {
            found.push(format!("{} missing in PostgreSQL", key(table, &cells)));
        }
        if found.len() >= DIFFERENCE_LIMIT {
            break;
        }
    }
    drop(stream);
    if found.len() < DIFFERENCE_LIMIT {
        let lookup_sqlite = format!(
            "{} WHERE {}",
            table.sqlite_select(),
            table
                .keys
                .iter()
                .enumerate()
                .map(|(i, k)| {
                    let c = table.columns.iter().find(|c| &c.name == k).unwrap();
                    if c.target == "uuid" {
                        format!(
                            "lower(replace({},'-',''))=lower(replace(${},'-',''))",
                            quote(k),
                            i + 1
                        )
                    } else {
                        format!("CAST({} AS TEXT)=${}", quote(k), i + 1)
                    }
                })
                .collect::<Vec<_>>()
                .join(" AND ")
        );
        let sql = format!(
            "{} ORDER BY {}",
            table.pg_select(),
            table
                .keys
                .iter()
                .map(|k| quote(k))
                .collect::<Vec<_>>()
                .join(",")
        );
        let mut stream = sqlx::query(&sql).fetch(&mut *target);
        while let Some(row) = stream.try_next().await? {
            let cells = pg_cells(table, &row)?;
            let mut q = sqlx::query(&lookup_sqlite);
            for k in &table.keys {
                q = q.bind(key_value(
                    &cells[cols.iter().position(|c| &c.name == k).unwrap()],
                )?);
            }
            if q.fetch_optional(&mut *source).await?.is_none() {
                found.push(format!("{} extra in PostgreSQL", key(table, &cells)));
            }
            if found.len() >= DIFFERENCE_LIMIT {
                break;
            }
        }
    }
    Ok(found)
}

pub(super) async fn constraints(target: &mut PgConnection) -> Result<()> {
    let bad:i64=sqlx::query_scalar("SELECT count(*) FROM pg_constraint c JOIN pg_namespace n ON n.oid=c.connamespace WHERE n.nspname='public' AND NOT c.convalidated").fetch_one(&mut *target).await?;
    ensure!(bad == 0, "PostgreSQL has unvalidated constraints");
    let fk_count:i64=sqlx::query_scalar("SELECT count(*) FROM pg_constraint c JOIN pg_namespace n ON n.oid=c.connamespace WHERE n.nspname='public' AND c.contype='f'").fetch_one(&mut *target).await?;
    // Every planned foreign key has one column, so constraints and endpoints
    // count alike. The plan is checked against the real migrations by
    // `plan_matches_real_migrations_baseline_map_and_fk_order`.
    let planned = expected_foreign_keys();
    ensure!(
        fk_count as usize == planned.len(),
        "PostgreSQL has {fk_count} foreign keys, the import plan and derived search expect {}",
        planned.len()
    );
    let endpoints:Vec<(String,String,String,String)>=sqlx::query_as("SELECT child.relname,ca.attname,parent.relname,pa.attname FROM pg_constraint con JOIN pg_class child ON child.oid=con.conrelid JOIN pg_class parent ON parent.oid=con.confrelid JOIN pg_namespace ns ON ns.oid=child.relnamespace CROSS JOIN LATERAL unnest(con.conkey,con.confkey) AS keys(child_key,parent_key) JOIN pg_attribute ca ON ca.attrelid=child.oid AND ca.attnum=keys.child_key JOIN pg_attribute pa ON pa.attrelid=parent.oid AND pa.attnum=keys.parent_key WHERE ns.nspname='public' AND con.contype='f'").fetch_all(&mut *target).await?;
    let endpoints = endpoints
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    ensure!(
        endpoints == planned,
        "PostgreSQL foreign keys differ from the import plan (only in the database: {:?}; only in the plan: {:?})",
        endpoints.difference(&planned).collect::<Vec<_>>(),
        planned.difference(&endpoints).collect::<Vec<_>>()
    );
    // Anti-joins also catch corruption introduced while integrity triggers were
    // disabled by someone else; convalidated alone cannot prove current rows.
    for table in tables() {
        for fk in table.fks {
            let sql=format!("SELECT EXISTS (SELECT 1 FROM public.{} c WHERE c.{} IS NOT NULL AND NOT EXISTS (SELECT 1 FROM public.{} p WHERE p.{}=c.{}))",quote(&table.name),quote(&fk.column),quote(&fk.parent),quote(&fk.parent_column),quote(&fk.column));
            let orphan: bool = sqlx::query_scalar(&sql).fetch_one(&mut *target).await?;
            ensure!(!orphan, "{}.{} FK orphan", table.name, fk.column);
        }
    }
    let orphan:bool=sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM public.work_search_alias s WHERE NOT EXISTS (SELECT 1 FROM public.work w WHERE w.id=s.work_id))").fetch_one(&mut *target).await?;
    ensure!(!orphan, "search FK orphan");
    let bad:i64=sqlx::query_scalar("SELECT count(*) FROM pg_index i JOIN pg_class c ON c.oid=i.indrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND (NOT i.indisvalid OR NOT i.indisready OR NOT i.indislive)").fetch_one(&mut *target).await?;
    ensure!(bad == 0, "PostgreSQL has invalid/unready indexes");
    let indexes:Vec<(String,String,i32,Option<String>,i64,bool)>=sqlx::query_as("SELECT c.relname,x.relname,i.indnkeyatts::integer,pg_get_expr(i.indpred,i.indrelid),i.indexrelid::bigint,i.indnullsnotdistinct FROM pg_index i JOIN pg_class c ON c.oid=i.indrelid JOIN pg_class x ON x.oid=i.indexrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND i.indisunique").fetch_all(&mut *target).await?;
    for (table, index, n, predicate, oid, nulls_equal) in indexes {
        let mut exprs = Vec::new();
        for i in 1..=n {
            let definition =
                sqlx::query_scalar::<_, String>("SELECT pg_get_indexdef($1::bigint::oid,$2,true)")
                    .bind(oid)
                    .bind(i)
                    .fetch_one(&mut *target)
                    .await?;
            exprs.push(index_expression(&definition).to_owned());
        }
        let nonnull = if nulls_equal {
            "true".into()
        } else {
            exprs
                .iter()
                .map(|e| format!("({e}) IS NOT NULL"))
                .collect::<Vec<_>>()
                .join(" AND ")
        };
        let sql=format!("SELECT EXISTS (SELECT 1 FROM public.{} WHERE ({}) AND {nonnull} GROUP BY {} HAVING count(*)>1)",quote(&table),predicate.as_deref().unwrap_or("true"),exprs.join(","));
        let collision: bool = sqlx::query_scalar(&sql).fetch_one(&mut *target).await?;
        ensure!(
            !collision,
            "PostgreSQL unique index {index} is inconsistent"
        );
    }
    Ok(())
}

/// (child table, child column, parent table, parent column) of every foreign
/// key: the import plan's, plus the one of the derived search table.
pub(super) fn expected_foreign_keys() -> std::collections::BTreeSet<(String, String, String, String)>
{
    let mut expected = tables()
        .into_iter()
        .flat_map(|t| {
            t.fks
                .into_iter()
                .map(move |f| (t.name.clone(), f.column, f.parent, f.parent_column))
        })
        .collect::<std::collections::BTreeSet<_>>();
    expected.insert((
        "work_search_alias".into(),
        "work_id".into(),
        "work".into(),
        "id".into(),
    ));
    expected
}

pub(super) fn index_expression(mut definition: &str) -> &str {
    // PostgreSQL's column deparser can append B-tree ordering. Those clauses
    // are not GROUP BY expressions. All unique baseline indexes use default
    // B-tree opclasses; remove only known trailing clauses, not text literals.
    loop {
        let trimmed = [" NULLS FIRST", " NULLS LAST", " ASC", " DESC"]
            .into_iter()
            .find_map(|suffix| definition.strip_suffix(suffix));
        match trimmed {
            Some(value) => definition = value,
            None => return definition,
        }
    }
}

async fn sequences(target: &mut PgConnection) -> Result<()> {
    for table in tables() {
        for c in table.columns.iter().filter(|c| c.identity) {
            let max: Option<i64> = sqlx::query_scalar(&format!(
                "SELECT max({}) FROM public.{}",
                quote(&c.name),
                quote(&table.name)
            ))
            .fetch_one(&mut *target)
            .await?;
            let sequence: String = sqlx::query_scalar("SELECT pg_get_serial_sequence($1,$2)")
                .bind(format!("public.{}", table.name))
                .bind(&c.name)
                .fetch_one(&mut *target)
                .await?;
            // pg_get_serial_sequence returns qualified quoted identifiers from PG,
            // never operator input. Baseline sequence names contain only these bytes.
            ensure!(
                sequence
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.\"".contains(&b)),
                "unexpected sequence identifier"
            );
            let (last, called): (i64, bool) =
                sqlx::query_as(&format!("SELECT last_value,is_called FROM {sequence}"))
                    .fetch_one(&mut *target)
                    .await?;
            let next = if called {
                last.checked_add(1)
            } else {
                Some(last)
            };
            ensure!(
                next.is_some_and(|n| n >= 1 && max.is_none_or(|m| n > m)),
                "{}.{} identity sequence is not ahead of max",
                table.name,
                c.name
            );
        }
    }
    Ok(())
}

const EXPECTED_SEARCH:&str="WITH aliases AS MATERIALIZED (SELECT 'alias:'||id AS id,work_id,raw_title,public.search_fold(raw_title) AS normalized_title,'alias'::text AS kind FROM public.work_alias), titles AS MATERIALIZED (SELECT 'title:'||id AS id,id AS work_id,coalesce(title_override,primary_title) AS raw_title,public.search_fold(coalesce(title_override,primary_title)) AS normalized_title,'title'::text AS kind FROM public.work), base AS (SELECT * FROM aliases WHERE normalized_title <> '' UNION ALL SELECT * FROM titles WHERE normalized_title <> '') SELECT id,work_id,normalized_title,raw_title,kind,public.search_trigram_key(normalized_title) AS trigram_key,to_tsvector('pg_catalog.simple'::regconfig,normalized_title)::text AS doc FROM base";

pub(super) async fn search(target: &mut PgConnection) -> Result<()> {
    super::schema::search_contract(target).await?;
    // Read-only equivalent of the baseline rebuild: same raw sources/fold/nonempty
    // predicate, with generated expressions included. No state metadata needed.
    let mut expected = Checksum::default();
    let mut actual = Checksum::default();
    for (sql,checksum) in [(EXPECTED_SEARCH,&mut expected),("SELECT id,work_id,normalized_title,raw_title,kind,trigram_key,doc::text FROM public.work_search_alias",&mut actual)] {
        let mut rows=sqlx::query(sql).fetch(&mut *target);
        while let Some(row)=rows.try_next().await? {
            let mut cells=Vec::new();for i in 0..7 {cells.push(Cell::Text(row.try_get(i)?));}
            checksum.cells("work_search_alias",&cells);
        }
    }
    ensure!(
        actual == expected,
        "derived search count/checksum differs from baseline rebuild"
    );
    Ok(())
}
