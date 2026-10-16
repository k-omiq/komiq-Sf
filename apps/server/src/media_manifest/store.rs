use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::db::Database;
use crate::media_verify::{CopyIdentity, ManifestBody, ManifestPage};

#[derive(Clone)]
pub(crate) struct CollectedPage {
    pub sha256: Vec<u8>,
    pub length: i64,
    pub mime: String,
    pub width: Option<i32>,
    pub height: Option<i32>,
}

/// The source-qualified identity of a chapter in the active generation, and the
/// engine manga it is bound to. Read-only: the copy row is made by `publish`.
pub(crate) async fn identity_for_chapter(
    pool: &Database,
    chapter_id: i64,
) -> Result<Option<(CopyIdentity, i64)>> {
    let identity: Option<(String, String, String, i64)> = sqlx::query_as(
        "SELECT m.source_id, m.manga_key, c.chapter_key, c.manga_id \
         FROM engine_chapter_key c JOIN engine_manga_key m \
           ON m.generation=c.generation AND m.manga_id=c.manga_id \
         JOIN engine_generation g ON g.id=c.generation \
         WHERE g.state='active' AND c.chapter_id=$1",
    )
    .bind(chapter_id)
    .fetch_optional(pool)
    .await?;
    Ok(
        identity.map(|(source_id, manga_key, chapter_key, manga_id)| {
            (
                CopyIdentity {
                    source_id,
                    manga_key,
                    chapter_key,
                },
                manga_id,
            )
        }),
    )
}

/// The copy a reader's chapter is filed under, read-only: `pages` never writes, so a
/// copy row is made only by the ingest pass that publishes its first revision.
pub(crate) async fn existing_copy_for_chapter(
    pool: &Database,
    chapter_id: i64,
) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT cp.id FROM engine_chapter_key c JOIN engine_manga_key m \
           ON m.generation=c.generation AND m.manga_id=c.manga_id \
         JOIN engine_generation g ON g.id=c.generation \
         JOIN media_copy cp ON cp.source_id=m.source_id AND cp.manga_key=m.manga_key \
           AND cp.chapter_key=c.chapter_key \
         WHERE g.state='active' AND c.chapter_id=$1",
    )
    .bind(chapter_id)
    .fetch_optional(pool)
    .await?)
}

/// The active generation's engine chapter of a copy, and the manga it is bound to.
pub(crate) async fn chapter_for_copy(pool: &Database, copy_id: &str) -> Result<Option<(i64, i64)>> {
    Ok(sqlx::query_as(
        "SELECT c.chapter_id, c.manga_id FROM media_copy cp \
         JOIN engine_manga_key m ON m.source_id=cp.source_id AND m.manga_key=cp.manga_key \
         JOIN engine_chapter_key c ON c.generation=m.generation AND c.manga_id=m.manga_id \
           AND c.chapter_key=cp.chapter_key \
         JOIN engine_generation g ON g.id=c.generation \
         WHERE cp.id=$1 AND g.state='active' ORDER BY c.chapter_id LIMIT 1",
    )
    .bind(copy_id)
    .fetch_optional(pool)
    .await?)
}

pub(crate) struct StoredManifest {
    pub body: ManifestBody,
    pub stale: bool,
}

pub(crate) async fn latest(pool: &Database, copy_id: &str) -> Result<Option<StoredManifest>> {
    load(pool, copy_id, None).await
}

pub(crate) async fn revision(
    pool: &Database,
    copy_id: &str,
    rev: i32,
) -> Result<Option<StoredManifest>> {
    load(pool, copy_id, Some(rev)).await
}

async fn load(pool: &Database, copy_id: &str, rev: Option<i32>) -> Result<Option<StoredManifest>> {
    type Row = (i32, i64, DateTime<Utc>, bool, String, String, String, i32);
    let row: Option<Row> = sqlx::query_as(
        "SELECT r.revision,r.policy_epoch,r.created_at,r.stale,cp.source_id,cp.manga_key,cp.chapter_key,r.page_count \
         FROM media_manifest r JOIN media_copy cp ON cp.id=r.copy_id \
         WHERE r.copy_id=$1 AND ($2::integer IS NULL OR r.revision=$2) \
           AND (r.superseded_at IS NULL OR r.superseded_at > now()-interval '7 days') \
         ORDER BY r.revision DESC LIMIT 1",
    ).bind(copy_id).bind(rev).fetch_optional(pool).await?;
    let Some((
        revision,
        policy_epoch,
        created_at,
        stale,
        source_id,
        manga_key,
        chapter_key,
        page_count,
    )) = row
    else {
        return Ok(None);
    };
    type PageRow = (Vec<u8>, i64, String, Option<i32>, Option<i32>);
    let pages: Vec<PageRow> = sqlx::query_as(
        "SELECT p.sha256,o.byte_length,o.mime,o.width,o.height FROM media_manifest_page p \
         JOIN media_object o ON o.sha256=p.sha256 WHERE p.copy_id=$1 AND p.revision=$2 ORDER BY p.idx",
    ).bind(copy_id).bind(revision).fetch_all(pool).await?;
    if pages.len() != page_count as usize {
        return Ok(None);
    }
    Ok(Some(StoredManifest {
        body: ManifestBody {
            v: 1,
            copy_id: copy_id.into(),
            copy: CopyIdentity {
                source_id,
                manga_key,
                chapter_key,
            },
            revision: revision.into(),
            policy_epoch,
            issued_at: created_at.timestamp(),
            pages: pages
                .into_iter()
                .map(|(sha256, length, mime, width, height)| ManifestPage {
                    sha256: hex::encode(sha256),
                    length: length as u64,
                    mime,
                    width: width.map(|v| v as u32),
                    height: height.map(|v| v as u32),
                })
                .collect(),
            kid: String::new(),
        },
        stale,
    }))
}

/// Files a finished pass: the copy (made here on its first revision), then revision
/// n+1 when the ordered list differs from the latest. Under the shared generation lock,
/// so a pass collected under one engine generation never commits under another.
/// Returns the copy id and the new revision (`None`: unchanged).
pub(crate) async fn publish(
    pool: &Database,
    generation: Option<uuid::Uuid>,
    identity: &CopyIdentity,
    pages: &[CollectedPage],
) -> Result<(String, Option<i32>)> {
    let page_count = i32::try_from(pages.len()).context("too many manifest pages")?;
    let mut tx = pool.begin().await?;
    crate::engine_generation::shared(&mut tx, generation).await?;
    sqlx::query(
        "INSERT INTO media_copy (id, source_id, manga_key, chapter_key, created_at) \
         VALUES ($1,$2,$3,$4,now()) ON CONFLICT (source_id,manga_key,chapter_key) DO NOTHING",
    )
    .bind(format!("mc_{}", uuid::Uuid::new_v4().simple()))
    .bind(&identity.source_id)
    .bind(&identity.manga_key)
    .bind(&identity.chapter_key)
    .execute(&mut *tx)
    .await?;
    let copy: String = sqlx::query_scalar(
        "SELECT id FROM media_copy WHERE source_id=$1 AND manga_key=$2 AND chapter_key=$3 FOR UPDATE",
    )
    .bind(&identity.source_id)
    .bind(&identity.manga_key)
    .bind(&identity.chapter_key)
    .fetch_one(&mut *tx)
    .await?;
    let copy_id = copy.as_str();
    let mut policy_epoch = crate::media_policy::hold_epoch(&mut tx).await?;
    if let Some(epoch) = crate::media_policy::inherit_for_copy(&mut tx, copy_id).await? {
        policy_epoch = epoch;
    }
    let previous: Option<(i32, bool)> = sqlx::query_as(
        "SELECT revision,stale FROM media_manifest WHERE copy_id=$1 ORDER BY revision DESC LIMIT 1",
    )
    .bind(copy_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((rev, false)) = previous {
        let hashes: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT sha256 FROM media_manifest_page WHERE copy_id=$1 AND revision=$2 ORDER BY idx",
        )
        .bind(copy_id)
        .bind(rev)
        .fetch_all(&mut *tx)
        .await?;
        if hashes.iter().eq(pages.iter().map(|p| &p.sha256)) {
            tx.commit().await?;
            return Ok((copy, None));
        }
    }
    let rev = previous
        .map_or(0, |(rev, _)| rev)
        .checked_add(1)
        .context("manifest revision overflow")?;
    // Stable lock order for objects shared by simultaneous chapter ingests.
    let mut objects: Vec<_> = pages.iter().collect();
    objects.sort_by(|a, b| a.sha256.cmp(&b.sha256));
    objects.dedup_by(|a, b| a.sha256 == b.sha256);
    for page in objects {
        sqlx::query(
            "INSERT INTO media_object (sha256,byte_length,mime,width,height,first_seen) \
             VALUES ($1,$2,$3,$4,$5,now()) ON CONFLICT DO NOTHING",
        )
        .bind(&page.sha256)
        .bind(page.length)
        .bind(&page.mime)
        .bind(page.width)
        .bind(page.height)
        .execute(&mut *tx)
        .await?;
    }
    if let Some((previous, _)) = previous {
        sqlx::query(
            "UPDATE media_manifest SET superseded_at=now() WHERE copy_id=$1 AND revision=$2",
        )
        .bind(copy_id)
        .bind(previous)
        .execute(&mut *tx)
        .await?;
    }
    // The epoch the revision is signed at: a lease older than it cannot vouch for it.
    sqlx::query(
        "INSERT INTO media_manifest (copy_id,revision,page_count,policy_epoch,created_at) \
         VALUES ($1,$2,$3,$4,now())",
    )
    .bind(copy_id)
    .bind(rev)
    .bind(page_count)
    .bind(policy_epoch)
    .execute(&mut *tx)
    .await?;
    for (idx, page) in pages.iter().enumerate() {
        sqlx::query(
            "INSERT INTO media_manifest_page (copy_id,revision,idx,sha256) VALUES ($1,$2,$3,$4)",
        )
        .bind(copy_id)
        .bind(rev)
        .bind(idx as i32)
        .bind(&page.sha256)
        .execute(&mut *tx)
        .await?;
    }
    crate::engine_generation::commit_ready(pool)?;
    tx.commit().await?;
    Ok((copy, Some(rev)))
}

pub(crate) async fn mark_stale(pool: &Database, copy_id: &str, revision: i32) -> Result<()> {
    sqlx::query(
        "UPDATE media_manifest SET stale=true WHERE copy_id=$1 AND revision=$2 AND NOT stale",
    )
    .bind(copy_id)
    .bind(revision)
    .execute(pool)
    .await?;
    Ok(())
}

pub(crate) async fn object_lookup(
    pool: &Database,
    copy_id: &str,
    sha256: &[u8],
) -> Result<Option<(i32, i32, i64, String)>> {
    Ok(sqlx::query_as(
        "SELECT r.revision,p.idx,o.byte_length,o.mime FROM media_manifest r \
         JOIN media_manifest_page p ON p.copy_id=r.copy_id AND p.revision=r.revision \
         JOIN media_object o ON o.sha256=p.sha256 \
         WHERE r.copy_id=$1 AND p.sha256=$2 AND NOT r.stale \
           AND (r.superseded_at IS NULL OR r.superseded_at > now()-interval '7 days') \
         ORDER BY r.revision DESC,p.idx LIMIT 1",
    )
    .bind(copy_id)
    .bind(sha256)
    .fetch_optional(pool)
    .await?)
}

pub(crate) async fn page_media(
    pool: &Database,
    copy_id: &str,
    expected_count: usize,
) -> Result<Option<Vec<(String, i64, String)>>> {
    let latest: Option<(i32,i32,bool)> = sqlx::query_as(
        "SELECT revision,page_count,stale FROM media_manifest WHERE copy_id=$1 ORDER BY revision DESC LIMIT 1",
    ).bind(copy_id).fetch_optional(pool).await?;
    let Some((rev, count, false)) = latest else {
        return Ok(None);
    };
    if count as usize != expected_count {
        return Ok(None);
    }
    let pages: Vec<(Vec<u8>,i64,String)> = sqlx::query_as(
        "SELECT p.sha256,o.byte_length,o.mime FROM media_manifest_page p \
         JOIN media_object o ON o.sha256=p.sha256 WHERE p.copy_id=$1 AND p.revision=$2 ORDER BY p.idx",
    ).bind(copy_id).bind(rev).fetch_all(pool).await?;
    if pages.len() != expected_count {
        return Ok(None);
    }
    Ok(Some(
        pages
            .into_iter()
            .map(|(sha, len, mime)| (hex::encode(sha), len, mime))
            .collect(),
    ))
}

pub(crate) async fn prune(pool: &Database) -> Result<()> {
    // Pages have no foreign key to their revision (one-column keys only), so the
    // revision and its pages go in one statement.
    sqlx::query(
        "WITH gone AS (DELETE FROM media_manifest WHERE (copy_id,revision) IN ( \
         SELECT r.copy_id,r.revision FROM media_manifest r \
         WHERE r.superseded_at < now()-interval '7 days' \
           AND r.revision < (SELECT max(n.revision) FROM media_manifest n WHERE n.copy_id=r.copy_id) \
         ORDER BY r.superseded_at LIMIT 128 FOR UPDATE SKIP LOCKED) RETURNING copy_id,revision) \
         DELETE FROM media_manifest_page p USING gone g \
         WHERE p.copy_id=g.copy_id AND p.revision=g.revision",
    ).execute(pool).await?;
    sqlx::query(
        "DELETE FROM media_object WHERE sha256 IN (SELECT o.sha256 FROM media_object o \
         WHERE NOT EXISTS (SELECT 1 FROM media_manifest_page p WHERE p.sha256=o.sha256) \
         ORDER BY o.sha256 LIMIT 512 FOR UPDATE SKIP LOCKED)",
    )
    .execute(pool)
    .await?;
    Ok(())
}
