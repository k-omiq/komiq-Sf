//! Rollback only: copy the user tables from PostgreSQL back into a copy of the
//! kept SQLite file, in the text formats the SQLite build of commit 6326033
//! reads. The kept file is never written. Record:
//! `docs/audits/postgres-reverse-export.md`.
mod cli;
mod convert;
mod export;
mod tables;
mod template;

pub use cli::run_cli;

#[cfg(test)]
mod tests;
