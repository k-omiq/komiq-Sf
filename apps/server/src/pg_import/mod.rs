//! Offline migration only. No PostgreSQL connections in the request path.
pub(crate) mod cli;
pub(crate) mod conversion;
mod copy;
pub(crate) mod importer;
pub(crate) mod plan;
mod preflight;
mod schema;
pub(crate) mod source;
pub(crate) mod startup;
pub(crate) mod verifier;

pub use cli::run_cli;
// Phase 3 may call this before starting the server on an imported database.
pub use importer::import_complete;

#[cfg(test)]
mod tests;

use anyhow::Result;
use sqlx::{Connection, PgConnection};

/// The runtime uses the same audited timestamp grammar and microsecond cut as
/// the offline importer, without opening or touching a SQLite source.
pub(crate) fn parse_runtime_date(value: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    let (micros, _) = conversion::date(value)?;
    chrono::DateTime::from_timestamp_micros(micros)
        .ok_or_else(|| anyhow::anyhow!("timestamp outside supported range"))
}
pub(crate) fn parse_runtime_epoch_millis(value: i64) -> Result<chrono::DateTime<chrono::Utc>> {
    let micros = conversion::milliseconds(value)?;
    chrono::DateTime::from_timestamp_micros(micros)
        .ok_or_else(|| anyhow::anyhow!("epoch milliseconds outside supported range"))
}

pub(crate) async fn connect(url: &str) -> Result<PgConnection> {
    use sqlx::ConnectOptions;
    // Disable SQLx statement logging, even when RUST_LOG enables SQLx tracing.
    let options: sqlx::postgres::PgConnectOptions = url
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid PostgreSQL connection argument [redacted]"))?;
    let mut conn = PgConnection::connect_with(&options.disable_statement_logging())
        .await
        .map_err(|_| anyhow::anyhow!("PostgreSQL connection failed [redacted]"))?;
    sqlx::raw_sql("SET search_path = public, pg_catalog; SET timezone = 'UTC'; SET datestyle = 'ISO, YMD'; SET extra_float_digits = 3; SET statement_timeout = 0; SET idle_in_transaction_session_timeout = 0; SET lock_timeout = '60s'")
        .execute(&mut conn).await?;
    Ok(conn)
}

/// Close a PostgreSQL connection whose work is finished and durable. The
/// client sends Terminate and then shuts the socket down; when the server
/// hangs up in between, the shutdown fails with "not connected" (macOS, seen
/// under load). No caller can do anything with that error.
pub(crate) async fn hang_up(conn: PgConnection) {
    let _ = conn.close().await;
}

pub(super) async fn target_definition(conn: &mut PgConnection) -> Result<()> {
    let ok: bool = sqlx::query_scalar("SELECT current_setting('server_version_num')::integer / 10000 = 18 AND encoding = pg_char_to_encoding('UTF8') AND datlocprovider = 'b' AND datlocale = 'C.UTF-8' AND datcollate = 'C' AND datctype = 'C' FROM pg_database WHERE datname = current_database()")
        .fetch_one(conn).await?;
    anyhow::ensure!(
        ok,
        "target requires PostgreSQL 18, UTF8, builtin C.UTF-8, LC_COLLATE C, LC_CTYPE C"
    );
    Ok(())
}
