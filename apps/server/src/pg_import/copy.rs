use anyhow::Result;
use futures::TryStreamExt;
use serde::Serialize;
use sqlx::{PgConnection, SqliteConnection};
use std::time::Instant;

use super::{
    conversion::{time_text, Cell},
    plan::{quote, Table},
    source::row_cells,
};

/// PostgreSQL COPY text: escape the field BEFORE appending its separator.
pub(super) fn encode(cell: &Cell, out: &mut Vec<u8>) -> Result<()> {
    let value = match cell {
        Cell::Null => {
            out.extend_from_slice(b"\\N");
            return Ok(());
        }
        Cell::Bytes(b) => format!("\\x{}", hex::encode(b)),
        Cell::Text(s) | Cell::Uuid(s) => s.clone(),
        Cell::Json(s) => s.input.clone(),
        Cell::Integer(n) => n.to_string(),
        Cell::Real(n) => n.to_string(),
        Cell::Time(us) => time_text(*us)?,
        Cell::Flag(b) => b.to_string(),
    };
    for b in value.bytes() {
        match b {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            _ => out.push(b),
        }
    }
    Ok(())
}

#[derive(Debug, Serialize, serde::Deserialize)]
pub(super) struct TableReport {
    pub table: String,
    pub rows: u64,
    pub seconds: f64,
    pub bytes_sent: u64,
    pub resumed: bool,
}

pub(super) async fn load(
    source: &mut SqliteConnection,
    target: &mut PgConnection,
    table: &Table,
) -> Result<TableReport> {
    let start = Instant::now();
    let mut tx = target.begin().await?;
    // USER excludes PostgreSQL's internal FK triggers. Triggers stay disabled
    // through completion; rollback of this table restores its previous state.
    sqlx::query(&format!(
        "ALTER TABLE public.{} DISABLE TRIGGER USER",
        quote(&table.name)
    ))
    .execute(&mut *tx)
    .await?;
    let statement = format!(
        "COPY public.{} ({}) FROM STDIN WITH (FORMAT text)",
        quote(&table.name),
        table
            .copied()
            .map(|c| quote(&c.name))
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut writer = tx.copy_in_raw(&statement).await?;
    let select = table.sqlite_select();
    let mut stream = sqlx::query(&select).fetch(&mut *source);
    let mut rows = 0;
    let mut bytes = 0;
    let mut buffer = Vec::with_capacity(65536);
    let mut progress = Instant::now();
    while let Some(row) = stream.try_next().await? {
        let cells = row_cells(table, &row)?.0;
        for (i, cell) in cells.iter().enumerate() {
            if i != 0 {
                buffer.push(b'\t');
            }
            encode(cell, &mut buffer)?;
        }
        buffer.push(b'\n');
        rows += 1;
        if buffer.len() >= 65536 {
            bytes += buffer.len() as u64;
            writer.send(&buffer[..]).await?;
            buffer.clear();
            if progress.elapsed().as_secs() >= 5 {
                eprintln!(
                    "sqlite import: table {}: {rows} rows sent, {:.3}s",
                    table.name,
                    start.elapsed().as_secs_f64()
                );
                progress = Instant::now();
            }
        }
    }
    if !buffer.is_empty() {
        bytes += buffer.len() as u64;
        writer.send(&buffer[..]).await?;
    }
    let loaded = writer.finish().await?;
    anyhow::ensure!(loaded == rows, "{} COPY count differs", table.name);
    let report = TableReport {
        table: table.name.clone(),
        rows,
        seconds: start.elapsed().as_secs_f64(),
        bytes_sent: bytes,
        resumed: false,
    };
    sqlx::query(
        "INSERT INTO public.komika_import_table (table_name, report) VALUES ($1,$2::jsonb)",
    )
    .bind(&table.name)
    .bind(serde_json::to_string(&report)?)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(report)
}

use sqlx::Connection;

pub(super) async fn counts(source: &mut SqliteConnection, target: &mut PgConnection) -> Result<()> {
    for t in super::plan::tables() {
        let a: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {}", quote(&t.name)))
            .fetch_one(&mut *source)
            .await?;
        let b: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM public.{}", quote(&t.name)))
            .fetch_one(&mut *target)
            .await?;
        anyhow::ensure!(
            a == b,
            "{} row counts differ: SQLite={a}, PostgreSQL={b}",
            t.name
        );
    }
    Ok(())
}
