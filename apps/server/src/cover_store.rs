//! Cover bytes as content-addressed files, indexed in PostgreSQL.
//!
//! Every cover master, card derivative and extension icon is one file
//! `<dir>/objects/<first 2 hex>/<sha256 hex>`, written once and never changed. The
//! `cover_object` table (migration 0014) says which object each asset currently uses:
//! `(asset_key, kind) → sha256`, plus the work cover version for work masters and, for a
//! derivative, the master it was made from.
//!
//! # Writing
//!
//! [`CoverStore::stage`] hashes the bytes and writes them to `tmp/` with an fsync, outside
//! any lock. The caller then opens ONE PostgreSQL transaction, takes the publish locks
//! ([`crate::cover::lock_assets_and_files`]: the `Cover[asset]` keys and a
//! `CoverFile[sha256]` key per object, in one sorted advisory call), checks its guards,
//! calls [`CoverStore::place`] (rename into place, or touch an existing object, then fsync
//! the directory) and writes the index rows in the same transaction. A crash before commit
//! leaves at most an unreferenced file, which GC removes after its grace period; the index
//! never names a file that is not durable.
//!
//! # Reading
//!
//! One index read, then the file by hash: the bytes are pinned by their hash, so a later
//! publish can never mix into a response. A missing file is a missing object (the cover is
//! filled again, or restored from `cover_upload`); any other error is a store error.
//!
//! # The SQLite cache
//!
//! Until the copy job ([`crate::cover_copy`]) has moved `covers.sqlite3` into files, the
//! store keeps that database open as `legacy`: reads fall back to it for assets the copy
//! has not passed yet, and deletes reach it too. Once the copy is done the server stops
//! opening the file.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::db::{Database, SqliteDatabase};

pub type Sha = [u8; 32];

pub fn sha256(bytes: &[u8]) -> Sha {
    Sha256::digest(bytes).into()
}

/// `instance_setting` key holding the copy job's [`CopyCursor`] as JSON.
pub const COPY_SETTING: &str = "cover_copy";

/// What one index row describes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Master,
    /// A card derivative at one of [`crate::cover::VARIANT_WIDTHS`].
    Width(u32),
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Master => "master",
            Kind::Width(192) => "w192",
            Kind::Width(320) => "w320",
            Kind::Width(w) => panic!("cover derivative width {w} is not allowlisted"),
        }
    }
}

/// The index key for a lock/asset string as the cover code spells it: a bare work id,
/// `s:<manga id>` or `icon:<package>`.
pub fn index_key(asset: &str) -> String {
    if asset.starts_with("s:") || asset.starts_with("icon:") {
        asset.to_owned()
    } else {
        format!("w:{asset}")
    }
}

/// The lock/asset string for an index key: the inverse of [`index_key`].
pub fn asset_of(index_key: &str) -> &str {
    index_key.strip_prefix("w:").unwrap_or(index_key)
}

/// The asset string for an extension icon.
pub fn icon_asset(pkg: &str) -> String {
    format!("icon:{pkg}")
}

/// One `cover_object` row.
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectRow {
    pub sha: Sha,
    pub byte_length: i32,
    pub version: Option<i64>,
    pub master: Option<Sha>,
    pub updated_at: DateTime<Utc>,
}

type RawRow = (Vec<u8>, i32, Option<i64>, Option<Vec<u8>>, DateTime<Utc>);

fn to_sha(bytes: Vec<u8>) -> Result<Sha> {
    Sha::try_from(bytes.as_slice()).map_err(|_| anyhow::anyhow!("cover index hash is not 32 bytes"))
}

fn object_row(raw: RawRow) -> Result<ObjectRow> {
    Ok(ObjectRow {
        sha: to_sha(raw.0)?,
        byte_length: raw.1,
        version: raw.2,
        master: raw.3.map(to_sha).transpose()?,
        updated_at: raw.4,
    })
}

/// The index row of `asset` / `kind`, read on `conn` (a publish transaction, or a pool).
pub async fn row<'e, E>(conn: E, asset: &str, kind: Kind) -> Result<Option<ObjectRow>>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let raw: Option<RawRow> = sqlx::query_as(
        "SELECT sha256, byte_length, version, master_sha256, updated_at FROM cover_object \
         WHERE asset_key = $1 AND kind = $2",
    )
    .bind(index_key(asset))
    .bind(kind.as_str())
    .fetch_optional(conn)
    .await?;
    raw.map(object_row).transpose()
}

/// Upsert the master row of `asset` and drop its derivative rows: they were made from the
/// previous master. Caller holds the publish locks and has placed `staged`.
pub async fn put_master(
    conn: &mut sqlx::PgConnection,
    asset: &str,
    staged: &Staged,
    version: Option<i64>,
) -> Result<()> {
    debug_assert!(staged.placed, "index row before its file was placed");
    let key = index_key(asset);
    sqlx::query(
        "INSERT INTO cover_object (asset_key, kind, sha256, byte_length, version, master_sha256, updated_at) \
         VALUES ($1, 'master', $2, $3, $4, NULL, now()) \
         ON CONFLICT (asset_key, kind) DO UPDATE SET sha256 = excluded.sha256, \
           byte_length = excluded.byte_length, version = excluded.version, updated_at = excluded.updated_at",
    )
    .bind(&key)
    .bind(staged.sha.as_slice())
    .bind(staged.byte_length()?)
    .bind(version)
    .execute(&mut *conn)
    .await?;
    sqlx::query("DELETE FROM cover_object WHERE asset_key = $1 AND kind <> 'master'")
        .bind(&key)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Upsert a derivative row, but only while `master` is still the indexed master of
/// `asset`. Returns whether it was written. Caller holds the publish locks and has placed
/// `staged`.
pub async fn put_derivative(
    conn: &mut sqlx::PgConnection,
    asset: &str,
    width: u32,
    staged: &Staged,
    master: &Sha,
) -> Result<bool> {
    debug_assert!(staged.placed, "index row before its file was placed");
    let written = sqlx::query(
        "INSERT INTO cover_object (asset_key, kind, sha256, byte_length, version, master_sha256, updated_at) \
         SELECT $1, $2, $3, $4, NULL, $5, now() FROM cover_object m \
          WHERE m.asset_key = $1 AND m.kind = 'master' AND m.sha256 = $5 \
         ON CONFLICT (asset_key, kind) DO UPDATE SET sha256 = excluded.sha256, \
           byte_length = excluded.byte_length, master_sha256 = excluded.master_sha256, \
           updated_at = excluded.updated_at",
    )
    .bind(index_key(asset))
    .bind(Kind::Width(width).as_str())
    .bind(staged.sha.as_slice())
    .bind(staged.byte_length()?)
    .bind(master.as_slice())
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(written > 0)
}

/// Drop every index row of `asset`. The files go with GC.
pub async fn delete_asset(conn: &mut sqlx::PgConnection, asset: &str) -> Result<u64> {
    Ok(sqlx::query("DELETE FROM cover_object WHERE asset_key = $1")
        .bind(index_key(asset))
        .execute(&mut *conn)
        .await?
        .rows_affected())
}

/// A file written to `tmp/` and fsynced, not yet in the object store. Dropping it without
/// [`CoverStore::place`] removes the temp file.
pub struct Staged {
    pub sha: Sha,
    pub len: usize,
    temp: Option<PathBuf>,
    placed: bool,
}

impl Staged {
    fn byte_length(&self) -> Result<i32> {
        i32::try_from(self.len).context("cover object is larger than 2 GiB")
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if let Some(temp) = self.temp.take() {
            let _ = std::fs::remove_file(temp);
        }
    }
}

/// Copy job position: phases run in this order, each by ascending key.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyPhase {
    #[default]
    Work,
    Source,
    Icon,
    Done,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CopyCursor {
    pub phase: CopyPhase,
    /// The last key handled in `phase`: a work id, a manga id (decimal) or a package.
    /// Empty: nothing handled yet.
    #[serde(default)]
    pub after: String,
}

impl CopyCursor {
    pub fn done() -> Self {
        Self {
            phase: CopyPhase::Done,
            after: String::new(),
        }
    }

    /// Whether the copy has already handled `asset` (a lock/asset string).
    pub fn passed(&self, asset: &str) -> bool {
        let (phase, key) = if let Some(id) = asset.strip_prefix("s:") {
            (CopyPhase::Source, id)
        } else if let Some(pkg) = asset.strip_prefix("icon:") {
            (CopyPhase::Icon, pkg)
        } else {
            (CopyPhase::Work, asset)
        };
        match self.phase.cmp(&phase) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal if phase == CopyPhase::Source => {
                // Numeric order, as the copy walks manga ids.
                match (key.parse::<i64>(), self.after.parse::<i64>()) {
                    (Ok(id), Ok(after)) => id <= after,
                    _ => false,
                }
            }
            std::cmp::Ordering::Equal => !self.after.is_empty() && key <= self.after.as_str(),
        }
    }
}

struct Inner {
    dir: PathBuf,
    legacy: Option<SqliteDatabase>,
    done: AtomicBool,
    cursor: RwLock<CopyCursor>,
}

/// The cover store: the object directory, plus the SQLite cache while the copy runs.
/// Cheap to clone.
#[derive(Clone)]
pub struct CoverStore(Arc<Inner>);

impl std::fmt::Debug for CoverStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoverStore")
            .field("dir", &self.0.dir)
            .field("legacy", &self.0.legacy.is_some())
            .finish()
    }
}

/// Where the store lives when `KOMIKA_COVERS_DIR` is not set: `covers` beside the covers
/// SQLite file (`/data/covers` in Docker), or `covers` in the working directory.
pub fn default_dir(covers_database_url: &str) -> PathBuf {
    match crate::db::covers_database_file(covers_database_url) {
        Ok(Some(file)) => file
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .join("covers"),
        _ => PathBuf::from("covers"),
    }
}

/// Read the copy cursor from `instance_setting`. No row: the copy has not started.
pub async fn load_cursor(main: &Database) -> Result<Option<CopyCursor>> {
    let value: Option<String> =
        sqlx::query_scalar("SELECT value FROM instance_setting WHERE key = $1")
            .bind(COPY_SETTING)
            .fetch_optional(main)
            .await?;
    value
        .map(|v| serde_json::from_str(&v).context("cover copy cursor is not valid JSON"))
        .transpose()
}

/// Persist the copy cursor.
pub async fn save_cursor<'e, E>(conn: E, cursor: &CopyCursor) -> Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query(
        "INSERT INTO instance_setting (key, value, updated_at) VALUES ($1, $2, now()) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(COPY_SETTING)
    .bind(serde_json::to_string(cursor)?)
    .execute(conn)
    .await?;
    Ok(())
}

impl CoverStore {
    /// Open the store for the server or a maintenance command.
    ///
    /// The SQLite cache is opened only while the copy is not done. A server with no cache
    /// file (a fresh install) has nothing to copy: the copy is marked done and the file is
    /// never created. `existing_only` (maintenance commands) never creates or upgrades it.
    pub async fn open(
        main: &Database,
        dir: PathBuf,
        covers_database_url: &str,
        existing_only: bool,
    ) -> Result<Self> {
        let mut cursor = load_cursor(main).await?;
        let legacy = if cursor.as_ref().is_some_and(|c| c.phase == CopyPhase::Done) {
            None
        } else {
            let file = crate::db::covers_database_file(covers_database_url)?;
            match file {
                Some(file) if file.try_exists()? => Some(if existing_only {
                    crate::db::connect_existing_covers(covers_database_url).await?
                } else {
                    crate::db::init_covers(covers_database_url).await?
                }),
                _ => {
                    let done = CopyCursor::done();
                    if !existing_only {
                        save_cursor(main, &done).await?;
                    }
                    cursor = Some(done);
                    None
                }
            }
        };
        Self::with_parts(dir, legacy, cursor.unwrap_or_default())
    }

    /// A store over `dir` with an optional SQLite cache, for tests and [`Self::open`].
    pub fn with_parts(
        dir: PathBuf,
        legacy: Option<SqliteDatabase>,
        cursor: CopyCursor,
    ) -> Result<Self> {
        // Every prefix directory exists, durably, before the first publish. A publisher
        // that creates one itself would race another publisher of the same prefix, which
        // could commit an index row before the directory entry is on disk.
        let objects = dir.join("objects");
        let mut made = Vec::new();
        for path in std::iter::once(dir.join("tmp"))
            .chain(std::iter::once(objects.clone()))
            .chain((0..=255u8).map(|b| objects.join(hex::encode([b]))))
        {
            if !path.try_exists()? {
                std::fs::create_dir_all(&path)
                    .with_context(|| format!("create cover directory {}", path.display()))?;
                made.push(path);
            }
        }
        if !made.is_empty() {
            sync_dir(&objects)?;
            sync_dir(&dir)?;
            if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
                sync_dir(parent)?;
            }
        }
        let done = cursor.phase == CopyPhase::Done || legacy.is_none();
        Ok(Self(Arc::new(Inner {
            dir,
            legacy,
            done: AtomicBool::new(done),
            cursor: RwLock::new(cursor),
        })))
    }

    pub fn dir(&self) -> &Path {
        &self.0.dir
    }

    pub fn object_path(&self, sha: &Sha) -> PathBuf {
        let hex = hex::encode(sha);
        self.0.dir.join("objects").join(&hex[..2]).join(hex)
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.0.dir.join("tmp")
    }

    pub fn objects_dir(&self) -> PathBuf {
        self.0.dir.join("objects")
    }

    /// Whether the SQLite copy has finished.
    pub fn copy_done(&self) -> bool {
        self.0.done.load(Ordering::Acquire)
    }

    pub fn copy_cursor(&self) -> CopyCursor {
        self.0
            .cursor
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Record copy progress in memory (the caller has persisted it).
    pub fn set_copy_cursor(&self, cursor: CopyCursor) {
        let done = cursor.phase == CopyPhase::Done;
        *self.0.cursor.write().unwrap_or_else(|p| p.into_inner()) = cursor;
        if done {
            self.0.done.store(true, Ordering::Release);
        }
    }

    /// The SQLite cache while the copy is not done: the copy's source, and a store every
    /// delete still has to reach so the copy cannot bring a deleted cover back.
    pub fn legacy(&self) -> Option<&SqliteDatabase> {
        if self.copy_done() {
            return None;
        }
        self.0.legacy.as_ref()
    }

    /// The SQLite cache for a read of `asset`, only while the copy has not passed it.
    pub fn legacy_for_read(&self, asset: &str) -> Option<&SqliteDatabase> {
        let legacy = self.legacy()?;
        (!self.copy_cursor().passed(asset)).then_some(legacy)
    }

    /// Close the SQLite cache, if open.
    pub async fn close(&self) {
        if let Some(legacy) = &self.0.legacy {
            legacy.close().await;
        }
    }

    /// Hash `bytes` and write them to a synced temp file. Call outside every lock.
    pub async fn stage(&self, bytes: &[u8]) -> Result<Staged> {
        let tmp = self.tmp_dir();
        let bytes = bytes.to_vec();
        tokio::task::spawn_blocking(move || -> Result<Staged> {
            use std::io::Write;
            let sha = sha256(&bytes);
            let temp = tmp.join(format!("{}.tmp", uuid::Uuid::new_v4()));
            let staged = Staged {
                sha,
                len: bytes.len(),
                temp: Some(temp.clone()),
                placed: false,
            };
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .with_context(|| format!("create cover temp file {}", temp.display()))?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            Ok(staged)
        })
        .await?
    }

    /// Move `staged` into the object store, or touch the existing object with the same
    /// hash (so GC's grace period starts again), then fsync the directory. Call inside the
    /// publish transaction, holding `CoverFile[staged.sha]`.
    pub async fn place(&self, staged: &mut Staged) -> Result<()> {
        let target = self.object_path(&staged.sha);
        let objects = self.objects_dir();
        let temp = staged.temp.take();
        let result = tokio::task::spawn_blocking(move || -> Result<()> {
            let parent = target.parent().expect("object path has a parent");
            if !parent.try_exists()? {
                std::fs::create_dir_all(parent)?;
                sync_dir(&objects)?;
            }
            match std::fs::OpenOptions::new().write(true).open(&target) {
                Ok(existing) => {
                    existing.set_modified(std::time::SystemTime::now())?;
                    if let Some(temp) = temp {
                        let _ = std::fs::remove_file(temp);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let temp = temp.context("cover object staged twice")?;
                    std::fs::rename(&temp, &target).with_context(|| {
                        format!("move cover object into place at {}", target.display())
                    })?;
                }
                Err(e) => return Err(e.into()),
            }
            sync_dir(parent)?;
            Ok(())
        })
        .await?;
        result?;
        staged.placed = true;
        Ok(())
    }

    /// The bytes of object `sha`; `None` when the file is missing. Any other IO error is a
    /// store error.
    pub async fn read_object(&self, sha: &Sha) -> std::io::Result<Option<Vec<u8>>> {
        match tokio::fs::read(self.object_path(sha)).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn object_exists(&self, sha: &Sha) -> std::io::Result<bool> {
        tokio::fs::try_exists(self.object_path(sha)).await
    }

    /// Stage `bytes` and place them under a `CoverFile` lock of their own, without touching
    /// the index: rewrites a lost file whose row still names it (an upload restore).
    pub async fn restore_object(&self, main: &Database, bytes: &[u8]) -> Result<Sha> {
        let mut staged = self.stage(bytes).await?;
        let mut tx = main.begin().await?;
        crate::locks::advisory(&mut tx, &[file_key(&staged.sha)]).await?;
        self.place(&mut staged).await?;
        tx.commit().await?;
        Ok(staged.sha)
    }
}

/// The advisory key that orders a file's placement against its GC.
pub fn file_key(sha: &Sha) -> crate::locks::AdvisoryKey {
    crate::locks::AdvisoryKey::new(crate::locks::KeyClass::CoverFile, &[&hex::encode(sha)])
}

#[cfg(windows)]
fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    // Windows cannot open a directory as a file, and NTFS journals the rename itself.
    Ok(())
}

#[cfg(not(windows))]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    // A directory fsync makes the rename durable. Some file systems refuse to open a
    // directory for syncing; there is nothing more to do on those.
    match std::fs::File::open(dir) {
        Ok(handle) => match handle.sync_all() {
            Ok(()) => Ok(()),
            // EINVAL
            Err(e) if e.raw_os_error() == Some(22) => Ok(()),
            Err(e) => Err(e),
        },
        Err(e) => Err(e),
    }
}

/// A read failure that is not a plain miss. Serving must not turn it into a source fetch.
#[derive(Debug)]
pub struct StoreError(pub anyhow::Error);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl<E: Into<anyhow::Error>> From<E> for StoreError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

/// A master as read for serving.
#[derive(Clone, Debug)]
pub struct Master {
    pub bytes: Vec<u8>,
    pub sha: Sha,
    /// The work cover version (work masters; also from the SQLite cache).
    pub version: Option<i64>,
    /// Read from the SQLite cache: derivatives of it are served but not stored.
    pub legacy: bool,
}

impl CoverStore {
    /// The master of `asset` and, when `width` is set, its derivative at that width made
    /// from that same master, read as one index snapshot.
    ///
    /// A file that disappears under its row is re-checked once against the index (a newer
    /// publish plus GC can overtake the read). Still missing: a work master is rewritten from
    /// `cover_upload` when that row names the same bytes; otherwise the master is missing.
    /// A missing derivative file is no derivative.
    pub async fn read_master(
        &self,
        main: &Database,
        asset: &str,
        width: Option<u32>,
    ) -> Result<Option<(Master, Option<Vec<u8>>)>, StoreError> {
        for attempt in 0..2 {
            let found = self.read_indexed(main, asset, width).await?;
            let Some((row, derivative)) = found else {
                return self.read_legacy(main, asset, width).await;
            };
            match self.read_object(&row.sha).await? {
                Some(bytes) => {
                    let derivative = match derivative {
                        Some(sha) => self.read_object(&sha).await?,
                        None => None,
                    };
                    return Ok(Some((
                        Master {
                            bytes,
                            sha: row.sha,
                            version: row.version,
                            legacy: false,
                        },
                        derivative,
                    )));
                }
                None if attempt == 0 => continue,
                None => {
                    if !asset.starts_with("s:") && !asset.starts_with("icon:") {
                        if let Some(bytes) = self.upload_for(main, asset, &row.sha).await? {
                            self.restore_object(main, &bytes).await?;
                            return Ok(Some((
                                Master {
                                    bytes,
                                    sha: row.sha,
                                    version: row.version,
                                    legacy: false,
                                },
                                None,
                            )));
                        }
                    }
                    return Ok(None);
                }
            }
        }
        unreachable!("loop returns on its second attempt")
    }

    async fn read_indexed(
        &self,
        main: &Database,
        asset: &str,
        width: Option<u32>,
    ) -> Result<Option<(ObjectRow, Option<Sha>)>, StoreError> {
        let kind = width.map(|w| Kind::Width(w).as_str());
        let raw: Option<(
            Vec<u8>,
            i32,
            Option<i64>,
            Option<Vec<u8>>,
            DateTime<Utc>,
            Option<Vec<u8>>,
        )> = sqlx::query_as(
            "SELECT m.sha256, m.byte_length, m.version, m.master_sha256, m.updated_at, d.sha256 \
                 FROM cover_object m \
                 LEFT JOIN cover_object d ON d.asset_key = m.asset_key AND d.kind = $2 \
                   AND d.master_sha256 = m.sha256 \
                 WHERE m.asset_key = $1 AND m.kind = 'master'",
        )
        .bind(index_key(asset))
        .bind(kind)
        .fetch_optional(main)
        .await?;
        let Some((sha, len, version, master, updated_at, derivative)) = raw else {
            return Ok(None);
        };
        let row = object_row((sha, len, version, master, updated_at))?;
        Ok(Some((row, derivative.map(to_sha).transpose()?)))
    }

    /// `cover_upload` bytes of `work_id` when they are the master `sha`.
    pub async fn upload_for(
        &self,
        main: &Database,
        work_id: &str,
        sha: &Sha,
    ) -> Result<Option<Vec<u8>>> {
        let bytes: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT bytes FROM cover_upload WHERE work_id = $1 AND sha256 = $2")
                .bind(work_id)
                .bind(sha.as_slice())
                .fetch_optional(main)
                .await?;
        Ok(bytes.filter(|b| &sha256(b) == sha))
    }

    /// The SQLite fallback for an asset the copy has not reached. Numeric covers only
    /// while the engine generation is active: the cache never holds another generation's.
    async fn read_legacy(
        &self,
        main: &Database,
        asset: &str,
        width: Option<u32>,
    ) -> Result<Option<(Master, Option<Vec<u8>>)>, StoreError> {
        let Some(legacy) = self.legacy_for_read(asset) else {
            return Ok(None);
        };
        let mut tx = legacy.read().begin().await?;
        let stored: Option<(Vec<u8>, Option<i64>)> = if let Some(id) = asset.strip_prefix("s:") {
            let Ok(id) = id.parse::<i64>() else {
                return Ok(None);
            };
            if crate::engine_generation::commit_ready(main).is_err() {
                return Ok(None);
            }
            sqlx::query_as("SELECT webp, NULL FROM suwayomi_cover_blob WHERE manga_id = $1")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
        } else if asset.starts_with("icon:") {
            return Ok(None);
        } else {
            sqlx::query_as("SELECT webp, version FROM work_cover_blob WHERE work_id = $1")
                .bind(asset)
                .fetch_optional(&mut *tx)
                .await?
        };
        let Some((bytes, version)) = stored else {
            tx.commit().await?;
            return Ok(None);
        };
        let derivative = match width {
            Some(w) => {
                sqlx::query_scalar(
                    "SELECT webp FROM cover_variant WHERE asset_key = $1 AND width = $2",
                )
                .bind(asset)
                .bind(w)
                .fetch_optional(&mut *tx)
                .await?
            }
            None => None,
        };
        tx.commit().await?;
        let sha = sha256(&bytes);
        Ok(Some((
            Master {
                bytes,
                sha,
                version,
                legacy: true,
            },
            derivative,
        )))
    }

    /// A stored derivative of `asset` at `width`, only while `master` is still its master.
    pub async fn derivative_of(
        &self,
        main: &Database,
        asset: &str,
        width: u32,
        master: &Master,
    ) -> Option<Vec<u8>> {
        if master.legacy {
            let legacy = self.legacy()?;
            return crate::cover::legacy_variant_of_master(legacy, asset, width, &master.bytes)
                .await;
        }
        let sha: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT sha256 FROM cover_object WHERE asset_key = $1 AND kind = $2 AND master_sha256 = $3 \
             AND EXISTS (SELECT 1 FROM cover_object m WHERE m.asset_key = $1 AND m.kind = 'master' \
                         AND m.sha256 = $3)",
        )
        .bind(index_key(asset))
        .bind(Kind::Width(width).as_str())
        .bind(master.sha.as_slice())
        .fetch_optional(main)
        .await
        .inspect_err(crate::db::record_read_database_error)
        .ok()
        .flatten();
        let sha = to_sha(sha?).ok()?;
        self.read_object(&sha).await.ok().flatten()
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A fresh store directory under the system temp dir, removed on drop.
    pub struct TestDir(pub PathBuf);

    impl TestDir {
        pub fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("komiq-covers-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A store with no SQLite cache (copy done).
    pub fn store(dir: &TestDir) -> CoverStore {
        CoverStore::with_parts(dir.0.clone(), None, CopyCursor::done()).unwrap()
    }

    /// An in-memory SQLite cover cache with the schema `db::init_covers` creates.
    pub async fn legacy_pool() -> SqliteDatabase {
        let pool = crate::sqlite_database::SqliteTestPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        for table in [
            "CREATE TABLE work_cover_blob (work_id TEXT PRIMARY KEY, webp BLOB NOT NULL, \
             version INTEGER NOT NULL, updated_at TEXT NOT NULL)",
            "CREATE TABLE suwayomi_cover_blob (manga_id INTEGER PRIMARY KEY, webp BLOB NOT NULL, \
             updated_at TEXT NOT NULL)",
            "CREATE TABLE cover_variant (asset_key TEXT NOT NULL, width INTEGER NOT NULL, \
             webp BLOB NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY (asset_key, width))",
            "CREATE TABLE cover_maintenance (key TEXT PRIMARY KEY, cursor TEXT NOT NULL, \
             done INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL)",
            "CREATE TABLE extension_icon_blob (pkg_name TEXT PRIMARY KEY, png BLOB NOT NULL, \
             updated_at TEXT NOT NULL)",
        ] {
            sqlx::query(table).execute(&pool).await.unwrap();
        }
        pool
    }

    /// A store mid-copy over `legacy` at `cursor`.
    pub fn store_with_legacy(
        dir: &TestDir,
        legacy: SqliteDatabase,
        cursor: CopyCursor,
    ) -> CoverStore {
        CoverStore::with_parts(dir.0.clone(), Some(legacy), cursor).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_cursor_passes_assets_in_phase_and_key_order() {
        let start = CopyCursor::default();
        assert!(!start.passed("w_a"));
        assert!(!start.passed("s:1"));
        let work = CopyCursor {
            phase: CopyPhase::Work,
            after: "w_m".into(),
        };
        assert!(work.passed("w_a"));
        assert!(work.passed("w_m"));
        assert!(!work.passed("w_z"));
        assert!(!work.passed("s:1"));
        let source = CopyCursor {
            phase: CopyPhase::Source,
            after: "100".into(),
        };
        assert!(source.passed("w_z"));
        assert!(source.passed("s:99"));
        assert!(source.passed("s:100"));
        assert!(!source.passed("s:101"));
        // Numeric, not text, order.
        assert!(!source.passed("s:1000"));
        assert!(!source.passed("icon:a"));
        assert!(CopyCursor::done().passed("icon:z"));
    }

    #[test]
    fn index_keys_namespace_works() {
        assert_eq!(index_key("w_1"), "w:w_1");
        assert_eq!(index_key("s:5"), "s:5");
        assert_eq!(index_key("icon:eu.kanade"), "icon:eu.kanade");
    }

    #[tokio::test]
    async fn staging_then_placing_writes_one_content_addressed_file() {
        let dir = test_support::TestDir::new();
        let store = test_support::store(&dir);
        let mut first = store.stage(b"cover bytes").await.unwrap();
        let temp = first.temp.clone().unwrap();
        assert!(temp.exists());
        store.place(&mut first).await.unwrap();
        assert!(!temp.exists(), "the temp file was renamed");
        let path = store.object_path(&first.sha);
        assert_eq!(std::fs::read(&path).unwrap(), b"cover bytes");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(48 * 3600);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        // Same bytes again: the object is reused and its mtime refreshed; the temp goes.
        let mut second = store.stage(b"cover bytes").await.unwrap();
        let temp = second.temp.clone().unwrap();
        store.place(&mut second).await.unwrap();
        assert!(!temp.exists());
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(modified > old + std::time::Duration::from_secs(3600));
        // A staged file that is never placed leaves nothing behind.
        let dropped = store.stage(b"never published").await.unwrap();
        let temp = dropped.temp.clone().unwrap();
        drop(dropped);
        assert!(!temp.exists());
        assert!(!store.object_path(&sha256(b"never published")).exists());
    }
}
