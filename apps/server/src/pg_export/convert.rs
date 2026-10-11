//! The inverse of `pg_import::conversion`: a PostgreSQL value, already parsed
//! into the importer's canonical cell, becomes the value the SQLite build of
//! commit 6326033 stores in that column.
use anyhow::{ensure, Result};
use chrono::{DateTime, Timelike};

use crate::pg_import::{conversion::Cell, plan::Column};

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Value {
    Null,
    Text(String),
    Integer(i64),
    Real(f64),
    Blob(Vec<u8>),
}

/// The text the old build writes for this instant in this column.
///
/// Every date column of a copied table is written with `Utc::now().to_rfc3339()`
/// by the old build, and read back as text, by string comparison with another
/// `to_rfc3339()` value, or by `DateTime::parse_from_rfc3339`. The one
/// exception is `sessions.expires_at`: `auth::format_ts` writes whole seconds
/// with a `Z`, and `session_user_for_token` compares it as a string with a
/// bound of the same spelling. PostgreSQL keeps microseconds, so a fraction is
/// 0, 3 or 6 digits here, the same widths `to_rfc3339()` gives for such an
/// instant; the old Linux build wrote 9.
pub(super) fn date_text(table: &str, column: &str, micros: i64) -> Result<String> {
    let instant = DateTime::from_timestamp_micros(micros)
        .ok_or_else(|| anyhow::anyhow!("timestamp out of range"))?;
    if (table, column) == ("sessions", "expires_at") {
        // Neither build writes a fraction here. One that is there all the same
        // stays, after the date-time digits that decide the comparison.
        return Ok(if instant.nanosecond() == 0 {
            instant.format("%Y-%m-%dT%H:%M:%SZ").to_string()
        } else {
            instant.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
        });
    }
    Ok(instant.to_rfc3339())
}

pub(super) fn value(table: &str, c: &Column, cell: &Cell) -> Result<Value> {
    Ok(match cell {
        Cell::Null => {
            ensure!(!c.required, "NULL in required/primary-key column");
            Value::Null
        }
        Cell::Text(s) => Value::Text(s.clone()),
        Cell::Bytes(b) => Value::Blob(b.clone()),
        // `Uuid::new_v4().to_string()` of the old build: lower case with hyphens.
        Cell::Uuid(s) => Value::Text(s.clone()),
        Cell::Flag(b) => Value::Integer(i64::from(*b)),
        // `suwayomi_series.latest_chapter_at` keeps its integer as text.
        Cell::Integer(n) if c.source == "TEXT" => Value::Text(n.to_string()),
        Cell::Integer(n) => Value::Integer(*n),
        Cell::Real(n) => Value::Real(*n),
        // PostgreSQL's own jsonb text: valid JSON, which is all `serde_json`
        // of the old build asks for. Key order and spacing are PostgreSQL's.
        Cell::Json(json) => Value::Text(json.input.clone()),
        Cell::Time(micros) if c.source == "INTEGER" => {
            ensure!(
                micros % 1000 == 0,
                "epoch column holds a value finer than a millisecond"
            );
            Value::Integer(micros / 1000)
        }
        Cell::Time(micros) => Value::Text(date_text(table, &c.name, *micros)?),
    })
}
