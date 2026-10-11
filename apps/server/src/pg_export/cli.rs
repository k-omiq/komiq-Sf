use anyhow::{ensure, Context, Result};
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};

use super::export::{self, Arguments};

const USAGE: &str = "usage: komika-server export-postgres-to-sqlite --template /absolute/kept.sqlite3 --output /absolute/new.sqlite3 [--postgres <url> | KOMIKA_IMPORT_POSTGRES_URL] [--json-out /absolute/report.json]";

pub async fn run_cli(args: &[String]) -> Result<()> {
    let (command, rest) = args.split_first().ok_or_else(|| anyhow::anyhow!(USAGE))?;
    ensure!(command == "export-postgres-to-sqlite", "{USAGE}");
    let mut template = None;
    let mut output = None;
    let mut postgres = None;
    let mut report = None;
    ensure!(rest.len() % 2 == 0, "{USAGE}");
    for pair in rest.chunks_exact(2) {
        let slot = match pair[0].as_str() {
            "--template" => &mut template,
            "--output" => &mut output,
            "--postgres" => &mut postgres,
            "--json-out" => &mut report,
            _ => anyhow::bail!(USAGE),
        };
        ensure!(slot.is_none(), "duplicate CLI option; {USAGE}");
        *slot = Some(pair[1].clone());
    }
    let arguments = export::resolve(&Arguments {
        template: PathBuf::from(template.ok_or_else(|| anyhow::anyhow!(USAGE))?),
        output: PathBuf::from(output.ok_or_else(|| anyhow::anyhow!(USAGE))?),
        // The same setting as the importer's.
        postgres_url: postgres
            .or_else(|| std::env::var("KOMIKA_IMPORT_POSTGRES_URL").ok())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "PostgreSQL URL required via --postgres or KOMIKA_IMPORT_POSTGRES_URL"
                )
            })?,
    })?;
    // The report file is made before the export and held to the end: a report
    // path that cannot be written stops the run before it writes anything.
    let mut report = match report {
        Some(path) => Some(create_report(&arguments, Path::new(&path))?),
        None => None,
    };
    let value = match export::run(&arguments, None).await {
        Ok(report) => serde_json::to_value(report)?,
        Err(error) => {
            let message = safe_error(&error, &arguments.postgres_url);
            if let Some(file) = &mut report {
                let failure = serde_json::json!({"complete": false, "error": message});
                if let Err(error) = write_report(file, &failure) {
                    anyhow::bail!("{message}; the report could not be written either: {error}")
                }
            }
            anyhow::bail!("{message}")
        }
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    if let Some(file) = &mut report {
        // The one failure after the output has its name. The report is on
        // standard output above.
        if let Err(error) = write_report(file, &value) {
            anyhow::bail!(
                "the export is complete and {} exists, but the report file could not be written: {error}",
                arguments.output.display()
            )
        }
    }
    Ok(())
}

fn write_report(file: &mut File, value: &serde_json::Value) -> Result<()> {
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    Ok(())
}

/// `arguments` are resolved paths. The report is a new file in an existing
/// directory, and never a database file or one of its side files.
fn create_report(arguments: &Arguments, report: &Path) -> Result<File> {
    ensure!(report.is_absolute(), "JSON output path must be absolute");
    let report = export::real_parent(report)?;
    ensure!(
        std::fs::symlink_metadata(&report).is_err(),
        "JSON output must be a new file"
    );
    let partial = export::partial_path(&arguments.output);
    for database in [&arguments.template, &arguments.output, &partial] {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            ensure!(
                report != super::template::sibling(database, suffix),
                "JSON output overlaps a database file"
            );
        }
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&report)
        .with_context(|| format!("create {}", report.display()))
}

/// A driver error can quote a whole row, so only its code is printed. The
/// lines this module adds around it name a table, a column or a file and
/// never hold a stored value: they stay, so the operator sees where it failed.
pub(super) fn safe_error(error: &anyhow::Error, url: &str) -> String {
    let mut place = Vec::new();
    for cause in error.chain() {
        if cause.downcast_ref::<sqlx::Error>().is_some() {
            place.push(crate::pg_import::cli::safe_error(error, url));
            return place.join(": ");
        }
        place.push(cause.to_string());
    }
    crate::pg_import::cli::safe_error(error, url)
}
