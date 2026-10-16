//! Engine identity, the post-connection-error gate, and explicit recovery.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{bail, ensure, Result};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::db::{Database, SqliteDatabase};
use crate::suwayomi::SuwayomiClient;

const MARKER: &str = "komiq.engine.generation";
// One namespace, shared by all server processes and the maintenance CLI.
const LOCK: i64 = 0x4b6f6d697147656e;
const CHECK_TIMEOUT: Duration = Duration::from_secs(3);
const WAIT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceError {
    Unavailable,
    BindingMismatch {
        manga_id: i64,
        chapter_id: Option<i64>,
    },
    StaleEpoch,
}
impl std::fmt::Display for FenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "Source engine identity is being checked or is quarantined",
            Self::BindingMismatch { .. } => "Source engine returned a different source identity",
            Self::StaleEpoch => "Numeric reading progress belongs to an earlier engine generation",
        })
    }
}
impl std::error::Error for FenceError {}

#[derive(Debug, Clone, sqlx::FromRow, async_graphql::SimpleObject)]
pub struct GenerationState {
    pub id: String,
    pub state: String,
    pub reason: Option<String>,
    pub since: String,
    pub unfenced: bool,
}

pub async fn states(pool: &Database) -> Result<Vec<GenerationState>> {
    Ok(sqlx::query_as("SELECT id::text AS id, state, reason, since::text AS since, unfenced FROM engine_generation ORDER BY first_seen, id")
        .fetch_all(pool).await?)
}

async fn current_on(conn: &mut sqlx::PgConnection) -> Result<Option<(Uuid, String, bool, bool)>> {
    Ok(sqlx::query_as(
        "SELECT id, state, unfenced, marker_written FROM engine_generation WHERE state <> 'detached'",
    )
    .fetch_optional(conn)
    .await?)
}

#[derive(Debug, Default)]
struct Gate {
    generation: Option<Uuid>,
    blocked: bool,
    revision: u64,
    unfenced: bool,
    suspect_manga: BTreeSet<i64>,
    suspect_chapter: BTreeSet<i64>,
}

pub struct Fence {
    pool: Database,
    covers: SqliteDatabase,
    // This clone deliberately has no fence: identity checks must bypass the gate.
    engine: SuwayomiClient,
    gate: RwLock<Gate>,
    checking: Mutex<()>,
    warned: std::sync::atomic::AtomicBool,
    gone_manga: Mutex<BTreeSet<i64>>,
    gone_chapter: Mutex<BTreeSet<i64>>,
    last_check: Mutex<Option<tokio::time::Instant>>,
}
impl std::fmt::Debug for Fence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineFence").finish_non_exhaustive()
    }
}

impl Fence {
    pub async fn attach(
        pool: &Database,
        covers: &SqliteDatabase,
        engine: &mut SuwayomiClient,
    ) -> Result<Arc<Self>> {
        if current_on(&mut *pool.acquire().await?).await?.is_none() {
            crate::engine_keys::adopt(pool).await?;
        }
        let current = current_on(&mut *pool.acquire().await?).await?;
        let fence = Arc::new(Self {
            pool: pool.clone(),
            covers: covers.clone(),
            engine: engine.clone(),
            gate: RwLock::new(Gate {
                generation: current.as_ref().map(|r| r.0),
                blocked: current.as_ref().is_some_and(|r| r.1 == "quarantined"),
                unfenced: current.as_ref().is_some_and(|r| r.2),
                revision: 0,
                suspect_manga: Default::default(),
                suspect_chapter: Default::default(),
            }),
            checking: Mutex::new(()),
            warned: Default::default(),
            gone_manga: Default::default(),
            gone_chapter: Default::default(),
            last_check: Default::default(),
        });
        pool.engine_fence
            .set(Arc::downgrade(&fence))
            .map_err(|_| anyhow::anyhow!("engine fence already configured"))?;
        engine.fence = Some(fence.clone());
        fence.check().await;
        Ok(fence)
    }

    pub fn generation(&self) -> Option<Uuid> {
        self.gate
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .generation
    }

    pub fn revision(&self) -> u64 {
        self.gate.read().unwrap_or_else(|e| e.into_inner()).revision
    }

    pub fn invalidate(self: &Arc<Self>) {
        {
            let mut gate = self.gate.write().unwrap_or_else(|e| e.into_inner());
            gate.blocked = true;
            gate.revision += 1;
        }
        self.engine.clear_numeric_memos();
        let fence = self.clone();
        tokio::spawn(async move {
            fence.check().await;
        });
    }

    /// A timer check never closes a healthy gate merely because the read is slow.
    pub async fn check(&self) {
        let _checking = self.checking.lock().await;
        self.check_locked().await;
    }

    async fn check_locked(&self) {
        let revision = self.revision();
        match tokio::time::timeout(CHECK_TIMEOUT, self.check_inner(revision)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                if crate::suwayomi::is_connection_error(&error) {
                    let mut gate = self.gate.write().unwrap_or_else(|e| e.into_inner());
                    gate.blocked = true;
                    gate.revision += 1;
                    self.engine.clear_numeric_memos();
                }
                tracing::warn!(%error, "engine generation check did not complete");
            }
            Err(_) => {
                tracing::debug!("engine generation check timed out; keeping the current gate")
            }
        }
        *self.last_check.lock().await = Some(tokio::time::Instant::now());
    }

    pub async fn scanner_tick(&self) {
        // Source loops tick together. Share the identity read across those ticks.
        let _checking = self.checking.lock().await;
        let recent = self
            .last_check
            .lock()
            .await
            .is_some_and(|t| t.elapsed() < Duration::from_secs(1));
        let unfenced = self.gate.read().unwrap_or_else(|e| e.into_inner()).unfenced;
        if !recent && (!unfenced || self.is_blocked()) {
            self.check_locked().await;
        }
    }

    fn is_blocked(&self) -> bool {
        self.gate.read().unwrap_or_else(|e| e.into_inner()).blocked
    }

    pub async fn wait(&self) -> Result<()> {
        // Observe maintenance CLI transitions as well as this process's checks.
        let current = current_on(&mut *self.pool.acquire().await?).await?;
        if current.as_ref().map(|r| r.0) != self.generation()
            || current.as_ref().is_some_and(|r| r.1 == "quarantined")
        {
            self.gate.write().unwrap_or_else(|e| e.into_inner()).blocked = true;
        }
        if self.is_blocked() || self.generation().is_none() {
            let _ = tokio::time::timeout(WAIT_TIMEOUT, self.check()).await;
        }
        if self.is_blocked() || self.generation().is_none() {
            return Err(FenceError::Unavailable.into());
        }
        Ok(())
    }

    async fn check_inner(&self, revision: u64) -> Result<()> {
        let current = current_on(&mut *self.pool.acquire().await?).await?;
        let marker = read_marker(&self.engine).await?;
        let mut unfenced =
            matches!(marker, Marker::Unsupported) || current.as_ref().is_some_and(|row| row.2);
        if unfenced && !self.warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!("engine has no global meta support; running unfenced with fingerprint and write guards");
        }
        if let Some((id, _, _, true)) = &current {
            if !unfenced && !matches!(&marker, Marker::Value(value) if value == &id.to_string()) {
                // A known marker mismatch closes the gate now, not after a fingerprint
                // that may outlast the check timeout. Only the outcome below reopens it.
                self.gate.write().unwrap_or_else(|e| e.into_inner()).blocked = true;
                self.engine.clear_numeric_memos();
            }
        }
        let Some((id, _, was_unfenced, marker_written)) = current else {
            let id = Uuid::new_v4();
            if !unfenced {
                unfenced = !write_marker(&self.engine, id).await?;
            }
            let mut tx = self.pool.begin().await?;
            exclusive(&mut tx).await?;
            ensure!(
                current_on(&mut tx).await?.is_none(),
                "engine generation changed during adoption"
            );
            sqlx::query("INSERT INTO engine_generation(id,first_seen,last_seen,state,unfenced,marker_written) VALUES ($1,now(),now(),'active',$2,NOT $2)")
                .bind(id).bind(unfenced).execute(&mut *tx).await?;
            tx.commit().await?;
            self.allow(id, unfenced, revision);
            return Ok(());
        };
        if !marker_written && !was_unfenced {
            // First adoption, including a crash/slow read before the marker was
            // retained. Numeric data already belongs to this generation.
            if !unfenced {
                unfenced = !write_marker(&self.engine, id).await?;
            }
            let mut tx = self.pool.begin().await?;
            exclusive(&mut tx).await?;
            ensure!(
                current_on(&mut tx).await?.is_some_and(|r| r.0 == id),
                "engine generation changed during adoption"
            );
            sqlx::query(
                "UPDATE engine_generation SET marker_written=NOT $2,unfenced=$2 WHERE id=$1",
            )
            .bind(id)
            .bind(unfenced)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            self.allow(id, unfenced, revision);
            return Ok(());
        }
        let fingerprint = self.fingerprint(id, false).await?;
        if !fingerprint.mismatch {
            let gate = self.gate.read().unwrap_or_else(|e| e.into_inner());
            if !gate.suspect_manga.is_empty() || !gate.suspect_chapter.is_empty() {
                return Ok(());
            }
        }
        let references = has_references(&self.pool).await?;
        let matches =
            unfenced || matches!(&marker, Marker::Value(value) if value == &id.to_string());
        if !fingerprint.mismatch && matches && (!fingerprint.empty || !references) {
            // Cover purge must finish before a previously quarantined generation resumes.
            let mut tx = self.pool.begin().await?;
            exclusive(&mut tx).await?;
            ensure!(
                current_on(&mut tx).await?.is_some_and(|r| r.0 == id),
                "engine generation changed during check"
            );
            sqlx::query("UPDATE engine_generation SET state='active',reason=NULL,since=now(),last_seen=now(),unfenced=$2 WHERE id=$1 AND (state <> 'active' OR unfenced <> $2)")
                .bind(id).bind(unfenced).execute(&mut *tx).await?;
            tx.commit().await?;
            self.allow(id, unfenced, revision);
        } else if fingerprint.empty && !references {
            if !unfenced {
                unfenced = !write_marker(&self.engine, id).await?;
            }
            let mut tx = self.pool.begin().await?;
            exclusive(&mut tx).await?;
            ensure!(
                current_on(&mut tx).await?.is_some_and(|r| r.0 == id),
                "engine generation changed during adoption"
            );
            sqlx::query("UPDATE engine_generation SET state='active',reason=NULL,since=now(),unfenced=$2 WHERE id=$1")
                .bind(id).bind(unfenced).execute(&mut *tx).await?;
            tx.commit().await?;
            self.allow(id, unfenced, revision);
        } else {
            let reason = if fingerprint.mismatch {
                "Stored numeric ids answer different source identities"
            } else if fingerprint.empty {
                "The engine is empty but PostgreSQL still has Suwayomi references"
            } else {
                "The engine generation marker is missing or different"
            };
            self.quarantine(id, reason).await?;
        }
        Ok(())
    }

    fn allow(&self, id: Uuid, unfenced: bool, revision: u64) {
        if unfenced && !self.warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!("engine has no global meta support; running unfenced with fingerprint and write guards");
        }
        let mut gate = self.gate.write().unwrap_or_else(|e| e.into_inner());
        if gate.generation != Some(id) {
            self.engine.clear_numeric_memos();
        }
        gate.generation = Some(id);
        gate.unfenced = unfenced;
        // A connection failed while the check was in flight: its check must still run.
        if gate.revision == revision
            && gate.suspect_manga.is_empty()
            && gate.suspect_chapter.is_empty()
        {
            gate.blocked = false;
        }
    }

    async fn quarantine(&self, id: Uuid, reason: &str) -> Result<()> {
        self.gate.write().unwrap_or_else(|e| e.into_inner()).blocked = true;
        self.engine.clear_numeric_memos();
        let mut tx = self.pool.begin().await?;
        exclusive(&mut tx).await?;
        ensure!(
            current_on(&mut tx).await?.is_some_and(|r| r.0 == id),
            "engine generation changed during quarantine"
        );
        let changed = sqlx::query("UPDATE engine_generation SET state='quarantined',reason=$2,since=now() WHERE id=$1 AND (state <> 'quarantined' OR reason IS DISTINCT FROM $2)")
            .bind(id).bind(reason).execute(&mut *tx).await?.rows_affected() > 0;
        if changed {
            purge_covers(&self.covers).await?;
        }
        tx.commit().await?;
        crate::graphql::clear_feed_caches();
        if changed {
            tracing::warn!(generation=%id, reason, "engine quarantined; restore the engine volume or explicitly start a new generation");
        }
        Ok(())
    }

    async fn fingerprint(&self, generation: Uuid, full: bool) -> Result<Fingerprint> {
        let mut gone_manga = self.gone_manga.lock().await;
        let mut gone_chapter = self.gone_chapter.lock().await;
        let (suspect_manga, suspect_chapter) = {
            let gate = self.gate.read().unwrap_or_else(|e| e.into_inner());
            (
                gate.suspect_manga.iter().copied().collect::<Vec<_>>(),
                gate.suspect_chapter.iter().copied().collect::<Vec<_>>(),
            )
        };
        let limit = if full { i64::MAX } else { 16 };
        let skip_manga: Vec<_> = if full {
            vec![]
        } else {
            gone_manga
                .iter()
                .copied()
                .filter(|id| !suspect_manga.contains(id))
                .collect()
        };
        let skip_chapter: Vec<_> = if full {
            vec![]
        } else {
            gone_chapter
                .iter()
                .copied()
                .filter(|id| !suspect_chapter.contains(id))
                .collect()
        };
        let mangas: Vec<(i64, String, String)> = if suspect_manga.is_empty() {
            sqlx::query_as("SELECT manga_id,source_id,manga_key FROM engine_manga_key WHERE generation=$1 AND NOT(manga_id=ANY($2)) ORDER BY manga_id LIMIT $3")
                .bind(generation).bind(&skip_manga).bind(limit).fetch_all(&self.pool).await?
        } else {
            sqlx::query_as("SELECT manga_id,source_id,manga_key FROM engine_manga_key WHERE generation=$1 AND NOT(manga_id=ANY($2)) ORDER BY (manga_id=ANY($4)) DESC,manga_id LIMIT $3")
                .bind(generation).bind(&skip_manga).bind(limit).bind(&suspect_manga).fetch_all(&self.pool).await?
        };
        let chapters: Vec<(i64, i64, String, String, String)> = if suspect_chapter.is_empty() {
            sqlx::query_as("SELECT c.chapter_id,c.manga_id,m.source_id,m.manga_key,c.chapter_key FROM engine_chapter_key c JOIN engine_manga_key m ON m.generation=c.generation AND m.manga_id=c.manga_id WHERE c.generation=$1 AND NOT(c.chapter_id=ANY($2)) ORDER BY c.chapter_id LIMIT $3")
                .bind(generation).bind(&skip_chapter).bind(limit).fetch_all(&self.pool).await?
        } else {
            sqlx::query_as("SELECT c.chapter_id,c.manga_id,m.source_id,m.manga_key,c.chapter_key FROM engine_chapter_key c JOIN engine_manga_key m ON m.generation=c.generation AND m.manga_id=c.manga_id WHERE c.generation=$1 AND NOT(c.chapter_id=ANY($2)) ORDER BY (c.chapter_id=ANY($4)) DESC,c.chapter_id LIMIT $3")
                .bind(generation).bind(&skip_chapter).bind(limit).bind(&suspect_chapter).fetch_all(&self.pool).await?
        };
        let batches = if full {
            mangas.len().max(chapters.len()).div_ceil(16).max(1)
        } else {
            1
        };
        let mut result = Fingerprint {
            mismatch: false,
            empty: false,
            compared: 0,
        };
        for batch in 0..batches {
            let ms = &mangas[(batch * 16).min(mangas.len())..((batch + 1) * 16).min(mangas.len())];
            let cs =
                &chapters[(batch * 16).min(chapters.len())..((batch + 1) * 16).min(chapters.len())];
            let mids = ms
                .iter()
                .map(|r| r.0.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let cids = cs
                .iter()
                .map(|r| r.0.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let doc = format!("query EngineFingerprint {{ probe: mangas(first:1) {{ nodes {{ id }} }} mangas(filter:{{id:{{in:[{mids}]}}}}) {{ nodes {{ id sourceId url }} }} chapters(filter:{{id:{{in:[{cids}]}}}}) {{ nodes {{ id mangaId url manga {{ sourceId url }} }} }} }}");
            let data = self.engine.identity_query(&doc, json!({})).await?;
            let mn = nodes(&data, "mangas")?;
            let cn = nodes(&data, "chapters")?;
            result.empty = nodes(&data, "probe")?.is_empty();
            for (id, source, key) in ms {
                if let Some(live) = mn.iter().find(|r| r["id"].as_i64() == Some(*id)) {
                    result.compared += 1;
                    result.mismatch |= live["sourceId"].as_str() != Some(source)
                        || live["url"].as_str() != Some(key);
                } else {
                    gone_manga.insert(*id);
                }
            }
            for (id, manga, source, manga_key, chapter_key) in cs {
                if let Some(live) = cn.iter().find(|r| r["id"].as_i64() == Some(*id)) {
                    result.compared += 1;
                    result.mismatch |= live["mangaId"].as_i64() != Some(*manga)
                        || live["manga"]["sourceId"].as_str() != Some(source)
                        || live["manga"]["url"].as_str() != Some(manga_key)
                        || live["url"].as_str() != Some(chapter_key);
                } else {
                    gone_chapter.insert(*id);
                }
            }
            if !result.mismatch {
                let mut gate = self.gate.write().unwrap_or_else(|e| e.into_inner());
                for row in ms {
                    gate.suspect_manga.remove(&row.0);
                }
                for row in cs {
                    gate.suspect_chapter.remove(&row.0);
                }
            }
            if result.mismatch {
                return Ok(result);
            }
        }
        Ok(result)
    }

    pub async fn reattach(&self) -> Result<()> {
        let _checking = self.checking.lock().await;
        let (old, _, _, _) = current_on(&mut *self.pool.acquire().await?)
            .await?
            .ok_or_else(|| anyhow::anyhow!("No engine generation to reattach"))?;
        ensure!(matches!(read_marker(&self.engine).await?, Marker::Missing), "Reattach requires a lost marker; restore the original engine or start a new generation");
        let fingerprint = self.fingerprint(old, true).await?;
        ensure!(
            !fingerprint.mismatch && fingerprint.compared > 0 && !fingerprint.empty,
            "Cannot reattach: source identities differ or no fingerprint can be verified"
        );
        let id = Uuid::new_v4();
        self.gate.write().unwrap_or_else(|e| e.into_inner()).blocked = true;
        let mut tx = self.pool.begin().await?;
        exclusive(&mut tx).await?;
        ensure!(
            current_on(&mut tx).await?.is_some_and(|r| r.0 == old),
            "Engine generation changed during reattach"
        );
        sqlx::query("UPDATE engine_generation SET state='detached',reason='Reattached after marker loss',since=now() WHERE id=$1").bind(old).execute(&mut *tx).await?;
        // marker_written=false: the check below writes the marker after commit, and
        // retries it if the process stops in between (the engine marker stays missing).
        sqlx::query("INSERT INTO engine_generation(id,first_seen,last_seen,state,marker_written) VALUES ($1,now(),now(),'active',false)").bind(id).execute(&mut *tx).await?;
        for table in ["engine_manga_key", "engine_chapter_key"] {
            sqlx::query(&format!(
                "UPDATE {table} SET generation=$1 WHERE generation=$2"
            ))
            .bind(id)
            .bind(old)
            .execute(&mut *tx)
            .await?;
        }
        purge_covers(&self.covers).await?;
        tx.commit().await?;
        self.engine.clear_numeric_memos();
        self.check_locked().await;
        Ok(())
    }

    pub async fn start_new_generation(&self, replace: Replace) -> Result<()> {
        let _checking = self.checking.lock().await;
        let old = replaceable(current_on(&mut *self.pool.acquire().await?).await?, replace)?;
        let unfenced = matches!(read_marker(&self.engine).await?, Marker::Unsupported);
        let id = Uuid::new_v4();
        self.gate.write().unwrap_or_else(|e| e.into_inner()).blocked = true;
        let mut tx = self.pool.begin().await?;
        exclusive(&mut tx).await?;
        ensure!(
            replaceable(current_on(&mut tx).await?, replace)? == old,
            "Engine generation changed during recovery"
        );
        sqlx::query("INSERT INTO suwayomi_progress_detached SELECT $1,user_id,chapter_id,series_id,last_page_read,read,updated_at FROM suwayomi_progress").bind(old).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM suwayomi_progress")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM progress_untick WHERE chapter_id ~ '^[0-9]+$'")
            .execute(&mut *tx)
            .await?;
        // Keep rows and every FK to them. Prefixing the target with the old
        // generation also keeps two detached generations' threads separate.
        for table in ["comments", "notifications", "user_activity"] {
            sqlx::query(&format!("UPDATE {table} SET target_id=$1 || ':' || target_id, target_type='detached_' || target_type WHERE target_type IN ('series','chapter') AND target_id ~ '^[0-9]+$'"))
                .bind(old.to_string()).execute(&mut *tx).await?;
        }
        // Before the mappings go: they are what resolves a numeric id to its work.
        detach_numeric_rows(&mut tx, old).await?;
        sqlx::query("UPDATE engine_generation SET state='detached',reason='Operator started a new engine generation',since=now() WHERE id=$1").bind(old).execute(&mut *tx).await?;
        // Delete the old numeric projections; works and work-level user data stay.
        // Source-series deletion cascades only to that source's spine chapters.
        sqlx::query("DELETE FROM source_series WHERE source_type='suwayomi'")
            .execute(&mut *tx)
            .await?;
        for table in [
            "suwayomi_chapter",
            "suwayomi_series",
            "suwayomi_cover_issue",
            "series_scan_state",
            "source_latest_snapshot",
        ] {
            sqlx::query(&format!("DELETE FROM {table}"))
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM feed_series_updates WHERE reader_id ~ '^[0-9]+$'")
            .execute(&mut *tx)
            .await?;
        // marker_written=false: the check below writes the marker only after commit.
        // A marker written first would quarantine the old generation if this
        // transaction failed, and a crash in between is retried by that check.
        sqlx::query("INSERT INTO engine_generation(id,first_seen,last_seen,state,unfenced,marker_written) VALUES ($1,now(),now(),'active',$2,false)").bind(id).bind(unfenced).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO engine_numeric_state(singleton,epoch,changed_at) VALUES (true,1,now()) ON CONFLICT(singleton) DO UPDATE SET epoch=engine_numeric_state.epoch+1,changed_at=now()")
            .execute(&mut *tx).await?;
        self.gate.write().unwrap_or_else(|e| e.into_inner()).blocked = true;
        self.engine.clear_numeric_memos();
        purge_covers(&self.covers).await?;
        tx.commit().await?;
        {
            let mut gate = self.gate.write().unwrap_or_else(|e| e.into_inner());
            gate.suspect_manga.clear();
            gate.suspect_chapter.clear();
        }
        self.gone_manga.lock().await.clear();
        self.gone_chapter.lock().await.clear();
        crate::graphql::clear_feed_caches();
        self.check_locked().await;
        Ok(())
    }
}

/// Which generation the operator means to give up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replace {
    /// The quarantined generation the operator was shown.
    Quarantined(Uuid),
    /// The current generation, even an active one (CLI `--force` only).
    Force,
}

fn replaceable(current: Option<(Uuid, String, bool, bool)>, replace: Replace) -> Result<Uuid> {
    let (id, state, _, _) =
        current.ok_or_else(|| anyhow::anyhow!("No engine generation to replace"))?;
    if let Replace::Quarantined(expected) = replace {
        ensure!(
            id == expected,
            "The engine generation changed since it was shown; refresh and check again"
        );
        ensure!(
            state == "quarantined",
            "The engine is not quarantined; refresh and check again"
        );
    }
    Ok(id)
}

/// Resolve to: the work a numeric Suwayomi id is mapped to, while the mapping exists.
const NUMERIC_WORK: &str = "SELECT DISTINCT ON (source_key) source_key AS sid, work_id FROM source_series WHERE source_type='suwayomi' AND source_key ~ '^[0-9]+$' ORDER BY source_key, work_id";

/// Numeric library, review, view and admin rows would name whatever series the new
/// engine gives the same number. Move the ones with a work to it, the way a work merge
/// folds them; set the rest aside with the old generation. `has_references` sees none.
async fn detach_numeric_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    old: Uuid,
) -> Result<()> {
    let mapped_works = format!("SELECT DISTINCT work_id FROM ({NUMERIC_WORK}) m ORDER BY work_id");
    let works: Vec<String> = sqlx::query_scalar(&mapped_works)
        .fetch_all(&mut **tx)
        .await?;
    crate::locks::lock_works(tx, &works).await?;
    // A merge may have moved the mappings while we waited. Locking the new set now
    // would take work locks out of order, so the operator runs it again instead.
    let current: Vec<String> = sqlx::query_scalar(&mapped_works)
        .fetch_all(&mut **tx)
        .await?;
    ensure!(
        current == works,
        "A catalogue merge ran at the same time; start the new generation again"
    );
    sqlx::query(&format!(
        "WITH m AS ({NUMERIC_WORK}), moved AS (DELETE FROM user_library l USING m WHERE l.series_id=m.sid RETURNING l.user_id,m.work_id,l.created_at,l.status,l.is_favorite) \
         INSERT INTO user_library(user_id,series_id,created_at,status,is_favorite) \
         SELECT user_id,work_id,min(created_at),(array_agg(status ORDER BY created_at DESC) FILTER (WHERE status IS NOT NULL))[1],bool_or(is_favorite) FROM moved GROUP BY user_id,work_id \
         ON CONFLICT(user_id,series_id) DO UPDATE SET created_at=LEAST(user_library.created_at,excluded.created_at),status=COALESCE(user_library.status,excluded.status),is_favorite=(user_library.is_favorite OR excluded.is_favorite)"
    ))
    .execute(&mut **tx)
    .await?;
    // A work keeps one review per author: an existing work review wins, else the newest.
    sqlx::query(&format!(
        "WITH m AS ({NUMERIC_WORK}), pick AS (SELECT DISTINCT ON (r.user_id,m.work_id) r.id,m.work_id FROM reviews r JOIN m ON m.sid=r.series_id \
         WHERE NOT EXISTS(SELECT 1 FROM reviews s WHERE s.series_id=m.work_id AND s.user_id=r.user_id) ORDER BY r.user_id,m.work_id,r.updated_at DESC,r.id) \
         UPDATE reviews r SET series_id=pick.work_id FROM pick WHERE r.id=pick.id"
    ))
    .execute(&mut **tx)
    .await?;
    sqlx::query(&format!(
        "WITH m AS ({NUMERIC_WORK}), moved AS (DELETE FROM series_views v USING m WHERE v.series_key=m.sid RETURNING m.work_id,v.total,v.updated_at) \
         INSERT INTO series_views(series_key,total,updated_at) SELECT work_id,sum(total)::bigint,max(updated_at) FROM moved GROUP BY work_id \
         ON CONFLICT(series_key) DO UPDATE SET total=series_views.total+excluded.total,updated_at=GREATEST(series_views.updated_at,excluded.updated_at)"
    ))
    .execute(&mut **tx)
    .await?;
    sqlx::query(&format!(
        "WITH m AS ({NUMERIC_WORK}), moved AS (DELETE FROM series_view_bucket b USING m WHERE b.series_key=m.sid RETURNING m.work_id,b.hour_ts,b.views) \
         INSERT INTO series_view_bucket(series_key,hour_ts,views) SELECT work_id,hour_ts,sum(views)::bigint FROM moved GROUP BY work_id,hour_ts \
         ON CONFLICT(series_key,hour_ts) DO UPDATE SET views=series_view_bucket.views+excluded.views"
    ))
    .execute(&mut **tx)
    .await?;
    // series_admin is the scanner's per-engine-series row: it never moves to a work.
    for (table, column, key, user) in [
        ("user_library", "series_id", "jsonb_build_array(user_id,series_id)", "user_id"),
        ("reviews", "series_id", "jsonb_build_array(id)", "user_id"),
        ("series_admin", "series_id", "jsonb_build_array(series_id)", "NULL::uuid"),
        ("series_views", "series_key", "jsonb_build_array(series_key)", "NULL::uuid"),
        ("series_view_bucket", "series_key", "jsonb_build_array(series_key,hour_ts)", "NULL::uuid"),
    ] {
        sqlx::query(&format!("INSERT INTO engine_detached_row(generation,source_table,row_key,user_id,data) SELECT $1,'{table}',{key}::text,{user},to_jsonb(t) FROM {table} t WHERE {column} ~ '^[0-9]+$'"))
            .bind(old)
            .execute(&mut **tx)
            .await?;
        sqlx::query(&format!("DELETE FROM {table} WHERE {column} ~ '^[0-9]+$'"))
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

struct Fingerprint {
    mismatch: bool,
    empty: bool,
    compared: usize,
}
fn nodes<'a>(data: &'a Value, field: &str) -> Result<&'a Vec<Value>> {
    data[field]["nodes"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Engine fingerprint omitted {field}"))
}
enum Marker {
    Value(String),
    Missing,
    Unsupported,
}
fn meta_unsupported(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_lowercase();
    message.contains("meta")
        && (message.contains("cannot query field")
            || message.contains("fieldundefined")
            || message.contains("unknown field"))
}
fn missing_meta_key(message: &str) -> bool {
    let message = message.to_lowercase();
    [
        "not found",
        "does not exist",
        "no value present",
        "nosuchelement",
        "no meta",
        "could not find",
        "couldn't find",
    ]
    .iter()
    .any(|missing| message.contains(missing))
}
async fn read_marker(engine: &SuwayomiClient) -> Result<Marker> {
    match engine
        .identity_query(
            "query EngineMarker($key:String!) { meta(key:$key) { key value } }",
            json!({"key":MARKER}),
        )
        .await
    {
        Ok(data) => match &data["meta"]["value"] {
            Value::String(value) => Ok(Marker::Value(value.clone())),
            _ if data.get("meta").is_some_and(Value::is_null) => Ok(Marker::Missing),
            _ => bail!("Engine marker response omitted meta"),
        },
        Err(error) => {
            if meta_unsupported(&error) {
                Ok(Marker::Unsupported)
            } else if error
                .downcast_ref::<crate::suwayomi::EngineGraphqlError>()
                .is_some()
                && missing_meta_key(&error.to_string())
            {
                Ok(Marker::Missing)
            } else {
                Err(error)
            }
        }
    }
}
async fn write_marker(engine: &SuwayomiClient, id: Uuid) -> Result<bool> {
    let result = engine.identity_query("mutation EngineMarker($key:String!,$value:String!) { setGlobalMeta(input:{meta:{key:$key,value:$value}}) { meta { key value } } }", json!({"key":MARKER,"value":id.to_string()})).await;
    if let Err(error) = result {
        if meta_unsupported(&error) {
            return Ok(false);
        }
        return Err(error);
    }
    ensure!(
        matches!(read_marker(engine).await?, Marker::Value(value) if value == id.to_string()),
        "Engine did not retain its generation marker"
    );
    Ok(true)
}
async fn has_references(pool: &Database) -> Result<bool> {
    let mut predicates = vec![
        "EXISTS(SELECT 1 FROM suwayomi_series)".to_string(),
        "EXISTS(SELECT 1 FROM suwayomi_chapter)".to_string(),
        "EXISTS(SELECT 1 FROM suwayomi_progress)".to_string(),
        "EXISTS(SELECT 1 FROM suwayomi_cover_issue)".to_string(),
        "EXISTS(SELECT 1 FROM source_latest_snapshot WHERE ordered_ids <> '[]'::jsonb)".to_string(),
        "EXISTS(SELECT 1 FROM feed_series_updates WHERE reader_id ~ '^[0-9]+$')".to_string(),
        "EXISTS(SELECT 1 FROM source_series WHERE source_type='suwayomi')".to_string(),
        "EXISTS(SELECT 1 FROM engine_manga_key k JOIN engine_generation g ON g.id=k.generation WHERE g.state <> 'detached')".to_string(),
        "EXISTS(SELECT 1 FROM engine_chapter_key k JOIN engine_generation g ON g.id=k.generation WHERE g.state <> 'detached')".to_string(),
    ];
    for (table, column) in [
        ("user_library", "series_id"),
        ("reviews", "series_id"),
        ("series_admin", "series_id"),
        ("series_scan_state", "series_id"),
        ("series_views", "series_key"),
        ("series_view_bucket", "series_key"),
    ] {
        predicates.push(format!(
            "EXISTS(SELECT 1 FROM {table} WHERE {column} ~ '^[0-9]+$')"
        ));
    }
    for table in ["comments", "notifications", "user_activity"] {
        predicates.push(format!("EXISTS(SELECT 1 FROM {table} WHERE target_type IN ('series','chapter') AND target_id ~ '^[0-9]+$')"));
    }
    Ok(
        sqlx::query_scalar(&format!("SELECT {}", predicates.join(" OR ")))
            .fetch_one(pool)
            .await?,
    )
}

async fn exclusive(conn: &mut sqlx::PgConnection) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(LOCK)
        .execute(conn)
        .await?;
    Ok(())
}
pub async fn shared(conn: &mut sqlx::PgConnection, expected: Option<Uuid>) -> Result<Option<Uuid>> {
    sqlx::query("SELECT pg_advisory_xact_lock_shared($1)")
        .bind(LOCK)
        .execute(&mut *conn)
        .await?;
    let current = current_on(conn).await?;
    if current.as_ref().is_some_and(|r| r.1 != "active")
        || expected.is_some_and(|id| current.as_ref().map(|r| r.0) != Some(id))
    {
        return Err(FenceError::Unavailable.into());
    }
    Ok(current.map(|r| r.0))
}
/// The current generation id. Read it before loading numeric rows that a later write
/// checks with [`shared`].
pub async fn current(pool: &Database) -> Result<Option<Uuid>> {
    Ok(current_on(&mut *pool.acquire().await?)
        .await?
        .map(|r| r.0))
}
pub async fn wait(pool: &Database) -> Result<()> {
    if let Some(fence) = pool.engine_fence.get().and_then(std::sync::Weak::upgrade) {
        fence.wait().await?;
    }
    Ok(())
}
/// Called without waiting while a writer holds the shared generation lock.
/// A connection loss during the transaction must not let it commit through the
/// pending post-error check (waiting here would deadlock that exclusive check).
pub fn commit_ready(pool: &Database) -> Result<()> {
    if pool
        .engine_fence
        .get()
        .and_then(std::sync::Weak::upgrade)
        .is_some_and(|f| f.is_blocked())
    {
        return Err(FenceError::Unavailable.into());
    }
    Ok(())
}

pub fn observe_write<T>(pool: &Database, result: Result<T>) -> Result<T> {
    if let Some(FenceError::BindingMismatch {
        manga_id,
        chapter_id,
    }) = result
        .as_ref()
        .err()
        .and_then(|e| e.downcast_ref::<FenceError>())
    {
        if let Some(fence) = pool.engine_fence.get().and_then(std::sync::Weak::upgrade) {
            {
                let mut gate = fence.gate.write().unwrap_or_else(|e| e.into_inner());
                gate.blocked = true;
                gate.revision += 1;
                gate.suspect_manga.insert(*manga_id);
                if let Some(id) = chapter_id {
                    gate.suspect_chapter.insert(*id);
                }
            }
            fence.engine.clear_numeric_memos();
            tokio::spawn(async move {
                let _checking = fence.checking.lock().await;
                if let Some(id) = fence.generation() {
                    if let Err(error) = fence
                        .quarantine(id, "Write guard found a different source identity")
                        .await
                    {
                        crate::db::observe_database_error(&error);
                        tracing::warn!(%error,"could not persist engine quarantine");
                    }
                }
                fence.check_locked().await;
            });
        }
    }
    result
}
pub async fn purge_covers(pool: &SqliteDatabase) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM suwayomi_cover_blob")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM cover_variant WHERE asset_key LIKE 's:%'")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
pub async fn numeric_epoch(pool: &Database) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE((SELECT epoch FROM engine_numeric_state WHERE singleton),0)",
    )
    .fetch_one(pool)
    .await?)
}
fn epoch_allowed(current: i64, recent_change: bool, supplied: Option<i64>) -> bool {
    supplied.map_or(!recent_change, |epoch| epoch == current)
}

pub async fn progress_guard(conn: &mut sqlx::PgConnection, epoch: Option<i64>) -> Result<()> {
    shared(conn, None).await?;
    let state: Option<(i64, bool)> = sqlx::query_as("SELECT epoch, changed_at > now() - interval '30 days' FROM engine_numeric_state WHERE singleton").fetch_optional(conn).await?;
    let (current, recent) = state.unwrap_or((0, false));
    if !epoch_allowed(current, recent, epoch) {
        return Err(FenceError::StaleEpoch.into());
    }
    Ok(())
}

pub async fn run_cli(
    pool: &Database,
    covers: &SqliteDatabase,
    engine: &mut SuwayomiClient,
    args: &[String],
) -> Result<()> {
    const USAGE: &str =
        "Usage: komika-server engine <reattach|start-new-generation <generation-id>|start-new-generation --force>";
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let replace = match args.as_slice() {
        ["reattach"] => None,
        ["start-new-generation", "--force"] => Some(Replace::Force),
        ["start-new-generation", id] => Some(Replace::Quarantined(
            Uuid::parse_str(id).map_err(|_| anyhow::anyhow!("{USAGE}"))?,
        )),
        _ => bail!("{USAGE}"),
    };
    let fence = Fence::attach(pool, covers, engine).await?;
    match replace {
        None => fence.reattach().await,
        Some(replace) => fence.start_new_generation(replace).await,
    }
}

#[cfg(test)]
#[path = "engine_generation_tests.rs"]
pub(crate) mod tests;
