use anyhow::{ensure, Context, Result};
use futures::TryStreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{Connection, Row, SqliteConnection};
use std::{collections::BTreeMap, io::Read, path::Path};

use super::{
    conversion::{convert, frame, Cell},
    plan::{quote, tables, Table},
    verifier::Checksum,
};

pub(crate) static SQLITE: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

pub(super) async fn open(path: &Path) -> Result<SqliteConnection> {
    ensure!(path.is_file(), "SQLite source must be an existing file");
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .read_only(true)
        .foreign_keys(true)
        .pragma("query_only", "ON")
        .pragma("temp_store", "FILE")
        .pragma("cache_size", "-8192");
    let mut conn = SqliteConnection::connect_with(&options).await?;
    sqlx::query("BEGIN").execute(&mut conn).await?;
    // First read pins the transaction snapshot, also used by every later check.
    let applied: Vec<(i64, bool, Vec<u8>)> =
        sqlx::query_as("SELECT version,success,checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&mut conn)
            .await
            .context("source has no valid SQLx migration history")?;
    let expected = SQLITE
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
        .collect::<Vec<_>>();
    ensure!(
        applied.len() == expected.len(),
        "source is not at latest SQLite migration; migrate a copy with the SQLite server first"
    );
    for ((v, ok, sum), migration) in applied.iter().zip(expected) {
        ensure!(
            *ok && *v == migration.version && sum.as_slice() == migration.checksum.as_ref(),
            "SQLite migration history/checksum mismatch at {v}"
        );
    }
    let names:Vec<String>=sqlx::query_scalar("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*' AND name NOT IN ('work_fts','work_fts_data','work_fts_idx','work_fts_content','work_fts_docsize','work_fts_config') AND name NOT IN ('_sqlx_migrations','_litestream_lock','_litestream_seq') ORDER BY name").fetch_all(&mut conn).await?;
    let mut planned = tables().iter().map(|t| t.name.clone()).collect::<Vec<_>>();
    planned.sort();
    // Table names are schema facts, not stored values, so the report may name them.
    let unplanned = names
        .iter()
        .filter(|n| !planned.contains(n))
        .collect::<Vec<_>>();
    let missing = planned
        .iter()
        .filter(|n| !names.contains(n))
        .collect::<Vec<_>>();
    ensure!(
        unplanned.is_empty() && missing.is_empty(),
        "SQLite tables differ from the frozen import plan (not in plan: {unplanned:?}; missing from source: {missing:?})"
    );
    for table in tables() {
        let actual = sqlx::query(&format!("PRAGMA table_xinfo({})", quote(&table.name)))
            .fetch_all(&mut conn)
            .await?;
        ensure!(
            actual.len() == table.columns.len(),
            "{} SQLite column count differs from import plan",
            table.name
        );
        for (row, c) in actual.iter().zip(&table.columns) {
            ensure!(
                row.try_get::<String, _>("name")? == c.name
                    && row.try_get::<String, _>("type")? == c.source
                    && (row.try_get::<i64, _>("hidden")? != 0) == c.generated,
                "{}.{} SQLite schema differs from import plan",
                table.name,
                c.name
            );
        }
    }
    Ok(conn)
}

// Keep the expensive integrity walk after the cheap resume-identity comparison.
pub(crate) async fn quick_check(conn: &mut SqliteConnection) -> Result<()> {
    let mut stream = sqlx::query_scalar::<_, String>("PRAGMA quick_check").fetch(&mut *conn);
    while let Some(result) = stream.try_next().await? {
        ensure!(
            result == "ok",
            "SQLite quick_check failed (details redacted)"
        );
    }
    drop(stream);
    Ok(())
}

pub(super) fn key(table: &Table, cells: &[Cell]) -> String {
    let mut bytes = Vec::new();
    let cols = table.copied().collect::<Vec<_>>();
    for k in &table.keys {
        if let Some(i) = cols.iter().position(|c| &c.name == k) {
            frame(&cells[i], &mut bytes);
        }
    }
    format!(
        "pk({}) sha256:{}",
        table.keys.join(","),
        hex::encode(Sha256::digest(bytes))
    )
}

pub(crate) fn row_cells(
    table: &Table,
    row: &sqlx::sqlite::SqliteRow,
) -> Result<(Vec<Cell>, Vec<String>)> {
    let mut cells = Vec::new();
    let mut cuts = Vec::new();
    for (i, c) in table.copied().enumerate() {
        let storage: &str = row.try_get(i * 2)?;
        let bytes = source_bytes(c, row, i)?;
        let (cell, cut) = convert(c, storage, bytes.as_deref())
            .with_context(|| format!("{}.{} {}", table.name, c.name, raw_key(table, row)))?;
        if cut {
            cuts.push(format!("{}.{}", table.name, c.name));
        }
        cells.push(cell);
    }
    Ok((cells, cuts))
}

fn source_bytes(
    c: &super::plan::Column,
    row: &sqlx::sqlite::SqliteRow,
    i: usize,
) -> Result<Option<Vec<u8>>> {
    if c.target == "double precision" {
        let storage: &str = row.try_get(i * 2)?;
        if storage == "null" {
            return Ok(None);
        }
        if matches!(storage, "integer" | "real") {
            return Ok(Some(
                row.try_get::<f64, _>(i * 2 + 1)?.to_string().into_bytes(),
            ));
        }
        // Preserve unexpected storage for the conversion's explicit hard stop.
    }
    Ok(row.try_get(i * 2 + 1)?)
}

fn raw_key(table: &Table, row: &sqlx::sqlite::SqliteRow) -> String {
    let mut hash = Sha256::new();
    for (i, c) in table.copied().enumerate() {
        if table.keys.contains(&c.name) {
            let bytes: Option<Vec<u8>> = row.try_get(i * 2 + 1).unwrap_or(None);
            hash.update((bytes.as_ref().map_or(0, Vec::len) as u64).to_be_bytes());
            hash.update(bytes.unwrap_or_default());
        }
    }
    format!(
        "pk({}) sha256:{}",
        table.keys.join(","),
        hex::encode(hash.finalize())
    )
}

#[derive(Default, Debug, Serialize)]
pub(super) struct Precheck {
    pub rows: BTreeMap<String, u64>,
    pub values_checked: BTreeMap<String, u64>,
    pub text_key_maxima: BTreeMap<String, TextMaximum>,
    pub truncated_fractions: BTreeMap<String, u64>,
    pub rules: BTreeMap<String, Rule>,
    pub source_data_hash: String,
}
#[derive(Default, Debug, Serialize)]
pub(super) struct TextMaximum {
    pub bytes: usize,
    pub row_key: String,
}
#[derive(Debug)]
pub(super) struct PrecheckFailure(pub Precheck);
impl std::fmt::Display for PrecheckFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "source pre-check failed; PostgreSQL has not been written"
        )
    }
}
impl std::error::Error for PrecheckFailure {}
#[derive(Default, Debug, Serialize)]
pub(super) struct Rule {
    pub failures: u64,
    pub examples: Vec<String>,
    pub max_bytes: usize,
}
impl Precheck {
    fn failure(&mut self, rule: String, key: String, size: usize) {
        let r = self.rules.entry(rule).or_default();
        r.failures += 1;
        r.max_bytes = r.max_bytes.max(size);
        if r.examples.len() < 5 {
            r.examples.push(key);
        }
    }
    pub fn passed(&self) -> bool {
        self.rules.values().all(|r| r.failures == 0)
    }
}

pub(super) async fn precheck(conn: &mut SqliteConnection) -> Result<Precheck> {
    quick_check(conn).await?;
    let mut report = Precheck::default();
    let mut source_hash = Sha256::new();
    let mut orphan_keys: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    // Count every orphan, retaining only five examples, without collecting rows.
    let mut fks = sqlx::query("PRAGMA foreign_key_check").fetch(&mut *conn);
    while let Some(row) = fks.try_next().await? {
        let table: String = row.try_get(0)?;
        let rowid: Option<i64> = row.try_get(1)?;
        report.failure(
            format!("{table}: FK orphan"),
            format!("SQLite rowid {rowid:?}"),
            0,
        );
        let examples = orphan_keys.entry(table).or_default();
        if examples.len() < 5 {
            if let Some(id) = rowid {
                examples.push(id);
            }
        }
    }
    drop(fks);
    for (name, rowids) in orphan_keys {
        if let Some(table) = tables().into_iter().find(|t| t.name == name) {
            let mut examples = Vec::new();
            for rowid in rowids {
                let row = sqlx::query(&format!("{} WHERE rowid=$1", table.sqlite_select()))
                    .bind(rowid)
                    .fetch_one(&mut *conn)
                    .await?;
                examples.push(raw_key(&table, &row));
            }
            report
                .rules
                .get_mut(&format!("{name}: FK orphan"))
                .unwrap()
                .examples = examples;
        }
    }
    for table in tables() {
        eprintln!("sqlite import: precheck {} start", table.name);
        let mut raw = Checksum::default();
        let mut count = 0;
        let sql = table.sqlite_select();
        let mut rows = sqlx::query(&sql).fetch(&mut *conn);
        while let Some(row) = rows.try_next().await? {
            count += 1;
            let mut raw_bytes = Vec::new();
            let mut cells = Vec::new();
            for (i, c) in table.copied().enumerate() {
                *report
                    .values_checked
                    .entry(format!("{}.{}", table.name, c.name))
                    .or_default() += 1;
                let storage: &str = row.try_get(i * 2)?;
                let bytes = source_bytes(c, &row, i)?;
                frame(&Cell::Text(storage.into()), &mut raw_bytes);
                frame(
                    &bytes
                        .as_ref()
                        .map_or(Cell::Null, |b| Cell::Bytes(b.clone())),
                    &mut raw_bytes,
                );
                match convert(c, storage, bytes.as_deref()) {
                    Ok((v, cut)) => {
                        if cut {
                            *report
                                .truncated_fractions
                                .entry(format!("{}.{}", table.name, c.name))
                                .or_default() += 1;
                        }
                        cells.push(v);
                    }
                    Err(error) => {
                        report.failure(
                            format!("{}.{}: {error}", table.name, c.name),
                            raw_key(&table, &row),
                            0,
                        );
                        cells.push(Cell::Null);
                    }
                }
            }
            raw.add(&table.name, &raw_bytes);
            // Check every composite text key, including generated match_key.
            // ASCII folding does not change UTF8 byte length. Reproduce the
            // baseline/SQLite generated match_key expression for its index gate.
            let cols = table.copied().collect::<Vec<_>>();
            for index in &table.indexes {
                let mut size = 0;
                for name in &index.text_columns {
                    let lookup = if name == "match_key" {
                        "normalized_title"
                    } else {
                        name.as_str()
                    };
                    if let Some(i) = cols.iter().position(|c| c.name == lookup) {
                        if let Cell::Text(s) = &cells[i] {
                            let length = if name == "match_key" {
                                match_key(s).len()
                            } else {
                                s.len()
                            };
                            size += length;
                            let maximum = report
                                .text_key_maxima
                                .entry(format!("{}.{}", table.name, name))
                                .or_default();
                            if length > maximum.bytes || maximum.row_key.is_empty() {
                                maximum.bytes = length;
                                maximum.row_key = raw_key(&table, &row);
                            }
                        }
                    }
                }
                // Treat partial indexes conservatively: checking extra rows cannot
                // admit an oversized key. Never weaken/truncate the key.
                if size >= 2000 {
                    report.failure(
                        format!(
                            "{}.{}: text-key budget >=2000 bytes ({})",
                            table.name,
                            index.name,
                            index.text_columns.join(",")
                        ),
                        raw_key(&table, &row),
                        size,
                    );
                }
            }
            // Derived search identities add six bytes ('alias:'/'title:').
            if matches!(table.name.as_str(), "work" | "work_alias") {
                let i = cols.iter().position(|c| c.name == "id").unwrap();
                if let Cell::Text(s) = &cells[i] {
                    if s.len() + 6 >= 2000 {
                        report.failure(
                            format!("{}.id: derived search identity text-key budget", table.name),
                            raw_key(&table, &row),
                            s.len() + 6,
                        );
                    }
                }
            }
        }
        drop(rows);
        eprintln!(
            "sqlite import: precheck {} complete: {count} rows",
            table.name
        );
        report.rows.insert(table.name.clone(), count);
        source_hash.update(table.name.as_bytes());
        source_hash.update(count.to_be_bytes());
        source_hash.update(raw.sum);
        for check in &table.checks {
            let sql = format!("{} WHERE NOT ({check})", table.sqlite_select());
            let mut bad = sqlx::query(&sql).fetch(&mut *conn);
            while let Some(row) = bad.try_next().await? {
                report.failure(
                    format!("{}: CHECK {check}", table.name),
                    raw_key(&table, &row),
                    0,
                );
            }
        }
        // SQLite sorts/spills GROUP BY itself. Canonical UUID text in keys is
        // case-insensitive hex with hyphens removed. No client-sized hash set.
        for index in table.indexes.iter().filter(|i| i.unique) {
            let expressions = index
                .columns
                .iter()
                .map(|name| {
                    let c = table.columns.iter().find(|c| &c.name == name).unwrap();
                    let q = quote(name);
                    if c.target == "uuid" {
                        format!("lower(replace({q},'-',''))")
                    } else if index.ascii {
                        format!("{q} COLLATE NOCASE")
                    } else {
                        q
                    }
                })
                .collect::<Vec<_>>()
                .join(",");
            let nonnull = index
                .columns
                .iter()
                .map(|c| format!("{} IS NOT NULL", quote(c)))
                .collect::<Vec<_>>()
                .join(" AND ");
            let predicate = if index.predicate.is_empty() {
                "1"
            } else {
                &index.predicate
            };
            let projection = table
                .keys
                .iter()
                .map(|k| format!("CAST({} AS BLOB)", quote(k)))
                .collect::<Vec<_>>()
                .join(",");
            let sql=format!("SELECT COUNT(*),{projection} FROM {} WHERE {nonnull} AND ({predicate}) GROUP BY {expressions} HAVING COUNT(*)>1",quote(&table.name));
            let mut groups = sqlx::query(&sql).fetch(&mut *conn);
            while let Some(row) = groups.try_next().await? {
                let n: i64 = row.try_get(0)?;
                report.failure(
                    format!("{}.{}: canonical unique collision", table.name, index.name),
                    format!("{n} rows; {}", group_key(&table, &row)?),
                    0,
                );
            }
        }
        // Empty source IDs must survive byte-identically with user triggers disabled.
        // Still gate keys as they will be normalized on later PostgreSQL writes.
        if table.name == "source_series" {
            let sql="SELECT count(*),CAST(id AS BLOB) FROM source_series GROUP BY source_type, NULLIF(source_id,''), source_key HAVING count(*)>1 AND NULLIF(source_id,'') IS NULL";
            let mut groups = sqlx::query(sql).fetch(&mut *conn);
            while let Some(row) = groups.try_next().await? {
                let n: i64 = row.try_get(0)?;
                report.failure(
                    "source_series: normalized unknown-key collision".into(),
                    format!("{n} rows; {}", group_key(&table, &row)?),
                    0,
                );
            }
        }
    }
    report.source_data_hash = hex::encode(source_hash.finalize());
    Ok(report)
}

fn group_key(table: &Table, row: &sqlx::sqlite::SqliteRow) -> Result<String> {
    let mut hash = Sha256::new();
    for i in 0..table.keys.len() {
        let bytes: Option<Vec<u8>> = row.try_get(i + 1)?;
        hash.update((bytes.as_ref().map_or(0, Vec::len) as u64).to_be_bytes());
        hash.update(bytes.unwrap_or_default());
    }
    Ok(format!(
        "pk({}) sha256:{}",
        table.keys.join(","),
        hex::encode(hash.finalize())
    ))
}

fn match_key(normalized: &str) -> String {
    let mut s = format!(" {normalized}");
    for article in [" the ", " the ", " an ", " an ", " a ", " a "] {
        s = s.replace(article, " ");
    }
    s.trim_matches(' ').to_owned()
}

pub(super) async fn identity(
    path: &Path,
    conn: &mut SqliteConnection,
    pre: &Precheck,
) -> Result<String> {
    let metadata = std::fs::metadata(path)?;
    let mut header = [0u8; 100];
    std::fs::File::open(path)?.read_exact(&mut header)?;
    ensure!(
        &header[..16] == b"SQLite format 3\0",
        "invalid SQLite header"
    );
    let schema: Vec<(String, String, String, Option<String>)> =
        sqlx::query_as("SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name")
            .fetch_all(&mut *conn)
            .await?;
    let version: i64 = sqlx::query_scalar("PRAGMA data_version")
        .fetch_one(&mut *conn)
        .await?;
    let mut hash = Sha256::new();
    hash.update(header);
    hash.update(serde_json::to_vec(&schema)?);
    for m in SQLITE.iter() {
        hash.update(m.version.to_be_bytes());
        hash.update(&m.checksum);
    }
    Ok(serde_json::json!({"file_size":metadata.len(),"header_schema_migrations_sha256":hex::encode(hash.finalize()),"data_version":version,"header_change_counter":u32::from_be_bytes(header[24..28].try_into()?),"snapshot_data_sha256":pre.source_data_hash,"plan_sha256":hex::encode(Sha256::digest(include_bytes!("plan.json")))}).to_string())
}
