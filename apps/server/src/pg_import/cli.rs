use anyhow::{ensure, Result};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

const USAGE:&str="usage: komika-server {import-sqlite-to-postgres|verify-sqlite-postgres} --sqlite /absolute/copy.sqlite3 [--postgres <url> | KOMIKA_IMPORT_POSTGRES_URL] [--json-out /absolute/report.json]";

pub async fn run_cli(args: &[String]) -> Result<()> {
    let (command, rest) = args.split_first().ok_or_else(|| anyhow::anyhow!(USAGE))?;
    let mut source = None;
    let mut postgres = None;
    let mut output = None;
    ensure!(rest.len() % 2 == 0, "{USAGE}");
    for pair in rest.chunks_exact(2) {
        let slot = match pair[0].as_str() {
            "--sqlite" => &mut source,
            "--postgres" => &mut postgres,
            "--json-out" => &mut output,
            _ => anyhow::bail!(USAGE),
        };
        ensure!(slot.is_none(), "duplicate CLI option; {USAGE}");
        *slot = Some(pair[1].clone());
    }
    let path = PathBuf::from(source.ok_or_else(|| anyhow::anyhow!(USAGE))?);
    ensure!(
        path.is_absolute(),
        "use an absolute SQLite database-copy path"
    );
    let url = postgres
        .or_else(|| std::env::var("KOMIKA_IMPORT_POSTGRES_URL").ok())
        .ok_or_else(|| {
            anyhow::anyhow!("PostgreSQL URL required via --postgres or KOMIKA_IMPORT_POSTGRES_URL")
        })?;
    if let Some(out) = &output {
        protect_output(&path, Path::new(out))?;
    }
    // Do not expose DB error detail/context: PG errors can quote a complete row,
    // passwords, tokens, or URL components. Our own structured row errors contain
    // only names, counts and hashed primary keys. Driver errors get SQLSTATE only.
    let result = execute(command, &path, &url).await;
    let value = match result {
        Ok(v) => v,
        Err(error) => {
            let message = safe_error(&error, &url);
            if let Some(out) = output {
                let precheck = error
                    .downcast_ref::<super::source::PrecheckFailure>()
                    .map(|e| &e.0);
                write_output(
                    Path::new(&out),
                    &serde_json::json!({"complete":false,"error":message,"precheck":precheck}),
                )?;
            }
            anyhow::bail!("{message}")
        }
    };
    if let Some(out) = output {
        write_output(Path::new(&out), &value)?;
    }
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn write_output(path: &Path, value: &serde_json::Value) -> Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?
        .write_all(&serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

async fn execute(command: &str, path: &Path, url: &str) -> Result<serde_json::Value> {
    match command {
        "import-sqlite-to-postgres" => Ok(serde_json::to_value(
            super::importer::run(path, url, None).await?,
        )?),
        "verify-sqlite-postgres" => Ok(serde_json::to_value(
            super::verifier::run(path, url).await?,
        )?),
        _ => anyhow::bail!(USAGE),
    }
}

pub(crate) fn safe_error(error: &anyhow::Error, url: &str) -> String {
    for cause in error.chain() {
        if let Some(driver) = cause.downcast_ref::<sqlx::Error>() {
            return match driver {
                sqlx::Error::Database(db) if db.code().as_deref() == Some("55P03") =>
                    "PostgreSQL lock wait exceeded importer lock_timeout (60s); another writer conflicts with the import".into(),
                sqlx::Error::Database(db) => format!(
                    "database operation failed (SQLSTATE/code {}, detail redacted)",
                    db.code().as_deref().unwrap_or("unknown")
                ),
                _ => "database operation failed (connection/driver detail redacted)".into(),
            };
        }
    }
    sanitize(&format!("{error:#}"), url)
}

fn protect_output(source: &Path, output: &Path) -> Result<()> {
    ensure!(output.is_absolute(), "JSON output path must be absolute");
    // Refuse existing outputs as well: avoids symlinks/hard links to source,
    // WAL/SHM, protected config, or any unrelated file. create_new used below.
    ensure!(!output.exists(), "JSON output must be a new file");
    let parent = output
        .parent()
        .ok_or_else(|| anyhow::anyhow!("JSON output has no parent"))?
        .canonicalize()?;
    let source = source.canonicalize()?;
    let destination = parent.join(
        output
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("invalid JSON filename"))?,
    );
    let source_str = source.to_string_lossy();
    ensure!(
        destination != source
            && destination != Path::new(&format!("{source_str}-wal"))
            && destination != Path::new(&format!("{source_str}-shm"))
            && destination != Path::new(&format!("{source_str}-journal")),
        "JSON output overlaps SQLite source/sidecar"
    );
    Ok(())
}

fn sanitize(message: &str, url: &str) -> String {
    let message = if url.is_empty() {
        message.to_owned()
    } else {
        message.replace(url, "[redacted PostgreSQL URL]")
    };
    // Mask complete URL tokens (including credentials/query parameters) and
    // password assignments. Structured DB-driver errors stay code-only above.
    let mut output = Vec::new();
    for word in message.split_whitespace() {
        let lower = word.to_ascii_lowercase();
        if lower.contains("://") {
            output.push("[redacted URL]".to_owned());
        } else if lower.contains("password") {
            // Password values may be quoted and span several words. Preserve
            // the preceding cause, then mask the remainder conservatively.
            output.push("password=[redacted]".to_owned());
            break;
        } else {
            output.push(word.to_owned());
        }
    }
    output.join(" ")
}
