//! `all.mangadex` retirement.
//!
//! The `all.mangadex` Suwayomi extension mirrors MangaDex through Suwayomi. We also
//! mirror MangaDex *directly* (`source_type = 'mangadex'`), which is the richer path:
//! only it has `createdAt` windowing, `links`, full `altTitles`, tags, `year`,
//! `content_rating`, and pages off `*.mangadex.network` instead of our origin. So 10,479
//! `source_series` rows on one source id (`2499283573021220255`) are duplication that
//! consumes the shared Suwayomi scan budget and mints duplicate `w_` works.
//!
//! # Why this module exists rather than a five-line DELETE
//!
//! §7's step 3 gate — "verify every work has a `source_type = 'mangadex'` anchor" — does
//! **not** protect step 4's delete, and §8g proved it on the 2026-07-31 snapshot:
//!
//! | class | rows |
//! |---|---|
//! | UUID of the row == a direct anchor's `source_key` on the SAME work | 9,929 |
//! | work HAS an anchor, but this row points at a **different** MangaDex entry | 54 |
//! | work has NO anchor at all | 496 |
//!
//! Those 54 pass §7's gate cleanly (their work does have *an* anchor) and carry 4,178
//! `chapter` rows. `chapter.source_series_id` is `REFERENCES source_series(id) ON DELETE
//! CASCADE`, so §7-as-written would have cascaded them away silently. They are
//! colored/version/fan editions mis-merged onto the base work (*Kaguya-sama (Official
//! Colored)* 371 ch on the work whose own anchor has 47).
//!
//! **The safety property this module implements is therefore UUID-level, not
//! work-level:** a row is deletable only if its MangaDex UUID equals the `source_key` of
//! a `source_type = 'mangadex'` row on the same work *after `work_redirect`
//! resolution* ([`gate`]).
//!
//! # The five steps
//!
//! 1. [`record_uuids`] — persist every row's MangaDex UUID, read from Suwayomi's
//!    `MangaType.url` (`/manga/<uuid>`), into `all_mangadex_uuid` (migration 0096, whose
//!    header justifies why neither `source_series.source_url` nor `work_external_id` can
//!    be the durable home) plus `source_series.source_url` for as long as the row lives.
//! 2. [`merge_anchorless`] — the 486 anchorless works are duplicate `w_` rows, not
//!    uncatalogued content: §8g measured that all 496 of their UUIDs already exist as a
//!    direct anchor on a *different* work (495 distinct twins, 0 on the same work). So
//!    this is a MERGE onto the existing canonical work, via `catalog::merge_works_ex`,
//!    which is what re-points the referential hazards (see below).
//! 3. [`gate`] — the rewritten redundancy proof. Reports every failing row; the caller
//!    does not proceed past one.
//! 4. [`split_mismatched`] — the 54 mismatch rows are **not deleted**. See that
//!    function's docs for why re-pointing ("split") is the only available action and a
//!    merge would be wrong.
//! 5. [`enrolment_excluded_source_ids`] — all **61** registered `all.mangadex` source
//!    ids are excluded from source-sync enrolment (`sync::sync_extension_inner`), not
//!    just the 1 that carries rows, so a language source that is empty today cannot
//!    re-seed the extension tomorrow.
//!
//! # The delete is OFF BY DEFAULT
//!
//! [`delete_redundant`] reports what it *would* delete unless it is given
//! [`Apply::Yes`] **and** the confirmation token [`DELETE_CONFIRM_TOKEN`]. Nothing in
//! this module deletes a `source_series` row on any other path.

use std::collections::{HashMap, HashSet};

use crate::database::SqliteDatabase;
use crate::db::Database;
use anyhow::{Context, Result};
use chrono::Utc;

/// The Suwayomi extension package that backs every `all.mangadex` source id.
pub const ALL_MANGADEX_PKG: &str = "eu.kanade.tachiyomi.extension.all.mangadex";

/// The one source id that actually carries `source_series` rows (§8g: the other 60 hold
/// zero). Used only for logging/assertions — every query derives the id set from
/// `source_extension` so a 62nd id appearing tomorrow is covered automatically.
pub const PRIMARY_SOURCE_ID: &str = "2499283573021220255";

/// Extra confirmation required by [`delete_redundant`] on top of [`Apply::Yes`]. Two
/// independent tokens because a `--apply` typed for the (harmless, reversible) merge or
/// split steps must not be able to delete 9,929 rows if it lands on the wrong subcommand.
pub const DELETE_CONFIRM_TOKEN: &str = "yes-delete-all-mangadex-rows";

/// Whether a step writes. `No` is the default everywhere: every step reports exactly
/// what it would do and changes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apply {
    No,
    Yes,
}

impl Apply {
    fn is_yes(self) -> bool {
        self == Apply::Yes
    }
    fn label(self) -> &'static str {
        match self {
            Apply::No => "DRY-RUN",
            Apply::Yes => "APPLY",
        }
    }
}

/// SQL that resolves a work id through `work_redirect` to its survivor.
///
/// `merge_works` collapses chains on write, so one hop is enough (see
/// `catalog::redirect_work_id`). This is a no-op on today's data — 0 `source_series`
/// rows sit on a redirected id — and it is here so the invariant holds regardless of
/// the order the steps run in: step 2 merges works away, and a row re-read after that
/// merge must still compare against the survivor.
fn resolved(col: &str) -> String {
    format!("COALESCE((SELECT r.new_id FROM work_redirect r WHERE r.old_id = {col}), {col})")
}

/// Parse the MangaDex UUID out of Suwayomi's `MangaType.url`.
///
/// The MangaDex extension stores `/manga/<uuid>`; §8g verified all 10,479 rows carry
/// one, and this run re-verified 10,628/10,628 of the source's Suwayomi-side mangas.
/// Anything that is not exactly a 8-4-4-4-12 hex UUID is rejected rather than guessed
/// at — a wrong UUID here would make a row look redundant against someone else's anchor.
pub fn uuid_from_manga_url(url: &str) -> Option<String> {
    let rest = url.trim().strip_prefix("/manga/")?;
    let uuid = rest
        .split(['/', '?', '#'])
        .next()?
        .trim()
        .to_ascii_lowercase();
    is_uuid(&uuid).then_some(uuid)
}

fn is_uuid(s: &str) -> bool {
    let groups = [8usize, 4, 4, 4, 12];
    let mut parts = s.split('-');
    for want in groups {
        match parts.next() {
            Some(p) if p.len() == want && p.bytes().all(|b| b.is_ascii_hexdigit()) => {}
            _ => return false,
        }
    }
    parts.next().is_none()
}

/// Every source id the `all.mangadex` extension registers — **all 61**, not just the one
/// holding rows (§8g step-5 note).
///
/// Read from `source_extension` (refreshed daily by `scanner::record_source_extensions`)
/// rather than hardcoded, so a language source added upstream is excluded the day it is
/// recorded.
pub async fn all_mangadex_source_ids(pool: &Database) -> Result<Vec<String>> {
    let ids = crate::db_types::query_scalar::<String>(
        "SELECT source_id FROM source_extension WHERE pkg_name = $1 ORDER BY source_id",
    )
    .bind(ALL_MANGADEX_PKG)
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// Step 5's predicate, in the form `sync` needs it: the set of Suwayomi source ids that
/// discovery must not enrol from.
///
/// Returned as a set (not a filter closure) so the caller can log the size — a silent
/// exclusion that quietly matched nothing is exactly the failure mode §4.12 documents
/// for the suryascans uninstall.
pub async fn enrolment_excluded_source_ids(pool: &Database) -> Result<HashSet<String>> {
    Ok(all_mangadex_source_ids(pool).await?.into_iter().collect())
}

/// Whether an extension package is retired from enrolment altogether (step 5).
///
/// Belt to [`enrolment_excluded_source_ids`]'s braces: the source-id set comes from a
/// table we refresh, so a source that Suwayomi reports but we have not recorded yet
/// would slip through an id-only filter. The pkg name comes from the live
/// `list_sources` response, so this catches it in the same pass.
pub fn is_retired_pkg(pkg: &str) -> bool {
    pkg == ALL_MANGADEX_PKG
}

// ---------------------------------------------------------------------------
// Step 1 — resolve + persist every row's MangaDex UUID
// ---------------------------------------------------------------------------

/// Outcome of [`record_uuids`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UuidReport {
    /// all.mangadex `source_series` rows seen.
    pub rows: u64,
    /// Rows for which a valid UUID was obtained.
    pub resolved: u64,
    /// Rows whose Suwayomi manga id was absent from the map, or whose url did not parse.
    /// **A row here can never pass [`gate`]** — it is unproven, not redundant.
    pub unresolved: Vec<String>,
    /// Rows whose newly-read UUID DISAGREES with one already recorded. Never silently
    /// overwritten: a UUID that changed under us invalidates any earlier proof built on it.
    pub conflicts: Vec<(String, String, String)>,
}

/// Persist the UUID of every all.mangadex row (step 1).
///
/// `uuid_by_suwayomi_key` maps a Suwayomi manga id (== `source_series.source_key`) to
/// that manga's `MangaType.url`. Fetch it with [`fetch_suwayomi_urls`]; it is a
/// parameter rather than an internal call so the whole step is testable against a
/// snapshot without an engine.
///
/// Writes two places, for the reasons migration 0096's header sets out: the durable
/// `all_mangadex_uuid` ledger (survives the delete) and `source_series.source_url` (dies
/// with the row, but makes the mapping legible in place while it lives).
pub async fn record_uuids(
    pool: &Database,
    uuid_by_suwayomi_key: &HashMap<String, String>,
    apply: Apply,
) -> Result<UuidReport> {
    let ids = all_mangadex_source_ids(pool).await?;
    if ids.is_empty() {
        anyhow::bail!("no all.mangadex source ids recorded — refusing to guess; run a source-extension refresh first");
    }
    let sql = format!(
        "SELECT id, work_id, source_key FROM source_series \
         WHERE source_type = 'suwayomi' AND source_id IN ({}) ORDER BY id",
        crate::sql_params::placeholders(1, ids.len())
    );
    let mut q = crate::db_types::query_tuple::<(String, String, String)>(&sql);
    for id in &ids {
        q = q.bind(id);
    }
    let rows = q.fetch_all(pool).await?;

    let now = Utc::now().to_rfc3339();
    let mut report = UuidReport {
        rows: rows.len() as u64,
        ..Default::default()
    };
    for (ss_id, _work_id, key) in rows {
        let Some(uuid) = uuid_by_suwayomi_key
            .get(&key)
            .and_then(|u| uuid_from_manga_url(u))
        else {
            report.unresolved.push(ss_id);
            continue;
        };
        if !apply.is_yes() {
            let previous: Option<String> = crate::db_types::query_scalar(
                "SELECT mangadex_uuid FROM all_mangadex_uuid WHERE source_series_id = $1",
            )
            .bind(&ss_id)
            .fetch_optional(pool)
            .await?;
            if let Some(previous) = previous.filter(|previous| previous != &uuid) {
                report.conflicts.push((ss_id, previous, uuid));
            } else {
                report.resolved += 1;
            }
            continue;
        }
        let conflict = crate::db::retry_transaction(|| async {
            let mut tx = pool.begin().await?;
            lock_proof_inputs(&mut tx, &ss_id, false).await?;
            let owner: Option<(String, String)> = sqlx::query_as("SELECT work_id, source_key FROM source_series WHERE id = $1")
                .bind(&ss_id).fetch_optional(&mut *tx).await?;
            let Some((work_id, current_key)) = owner else { return Err(crate::locks::RestartTransaction.into()); };
            if current_key != key { return Err(crate::locks::RestartTransaction.into()); }
            let previous: Option<String> = crate::db_types::query_scalar("SELECT mangadex_uuid FROM all_mangadex_uuid WHERE source_series_id = $1")
                .bind(&ss_id).fetch_optional(&mut *tx).await?;
            if let Some(previous) = previous.filter(|previous| previous != &uuid) {
                tx.commit().await?;
                return Ok(Some(previous));
            }
            crate::catalog::guards::held().await;
            sqlx::query("INSERT INTO all_mangadex_uuid (source_series_id, work_id, suwayomi_key, mangadex_uuid, resolved_at) VALUES ($1,$2,$3,$4,$5) ON CONFLICT(source_series_id) DO UPDATE SET work_id = excluded.work_id, resolved_at = excluded.resolved_at")
                .bind(&ss_id).bind(&work_id).bind(&key).bind(crate::db_types::uuid(&uuid)?).bind(crate::db_types::date(&now)?)
                .execute(&mut *tx).await?;
            sqlx::query("UPDATE source_series SET source_url = $1 WHERE id = $2")
                .bind(format!("/manga/{uuid}")).bind(&ss_id).execute(&mut *tx).await?;
            tx.commit().await?;
            Ok(None)
        }).await?;
        if let Some(previous) = conflict {
            report.conflicts.push((ss_id, previous, uuid));
        } else {
            report.resolved += 1;
        }
    }
    Ok(report)
}

/// Read `MangaType.url` for every manga Suwayomi holds on the given source ids.
///
/// This is a **local** Suwayomi GraphQL query (`mangas(condition: { sourceId })`), not a
/// `fetchMangaAndChapters` mutation: it reads Suwayomi's own database and performs no
/// upstream fetch, so it is safe to run over 10k series in one pass and cannot trip a
/// MangaDex rate limit. Paged, because the source holds ~10.6k mangas.
///
/// Implemented here with a plain HTTP POST rather than through `SuwayomiClient` because
/// that client's `gql` is private and its `MANGA_FIELDS` selection carries a per-manga
/// `chapters { totalCount }` N+1 that makes a full-library fetch take ~50 s (its own
/// docs); this needs two scalar fields.
pub async fn fetch_suwayomi_urls(
    base_url: &str,
    source_ids: &[String],
) -> Result<HashMap<String, String>> {
    const PAGE: i64 = 500;
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let endpoint = format!("{}/api/graphql", base_url.trim_end_matches('/'));
    let mut out = HashMap::new();
    for source_id in source_ids {
        let mut offset = 0i64;
        loop {
            let body = serde_json::json!({
                "query": "query F($src: LongString!, $first: Int!, $offset: Int!) { \
                    mangas(condition: { sourceId: $src }, first: $first, offset: $offset) { \
                      pageInfo { hasNextPage } nodes { id url } } }",
                "variables": { "src": source_id, "first": PAGE, "offset": offset },
            });
            let res: serde_json::Value = http
                .post(&endpoint)
                .json(&body)
                .send()
                .await
                .context("suwayomi graphql request failed")?
                .json()
                .await
                .context("suwayomi graphql response was not json")?;
            if let Some(errors) = res.get("errors") {
                anyhow::bail!("suwayomi graphql errors: {errors}");
            }
            let mangas = &res["data"]["mangas"];
            let nodes = mangas["nodes"].as_array().cloned().unwrap_or_default();
            let n = nodes.len();
            for node in nodes {
                let (Some(id), Some(url)) = (node["id"].as_i64(), node["url"].as_str()) else {
                    continue;
                };
                out.insert(id.to_string(), url.to_string());
            }
            if n == 0 || !mangas["pageInfo"]["hasNextPage"].as_bool().unwrap_or(false) {
                break;
            }
            offset += PAGE;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Classification — the one query every step reads from
// ---------------------------------------------------------------------------

/// What Phase F may do with one all.mangadex row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The row's UUID IS a direct anchor on the same work. Deletable (step 4's delete).
    Redundant,
    /// The work has a direct anchor, but for a DIFFERENT MangaDex entry. **Never
    /// deletable** — deleting cascades this row's chapters away. Handled by
    /// [`split_mismatched`].
    Mismatch,
    /// The work has no direct anchor at all. Handled by [`merge_anchorless`].
    Anchorless,
    /// No UUID could be resolved for the row, so nothing about it can be proven.
    Unresolved,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Redundant => "redundant",
            Verdict::Mismatch => "mismatch",
            Verdict::Anchorless => "anchorless",
            Verdict::Unresolved => "unresolved",
        }
    }
}

/// The classification rule, isolated from SQL so it can be reasoned about directly.
///
/// Order matters: `uuid_matches_anchor` is checked FIRST, because that — not "the work
/// has an anchor" — is the redundancy proof. A work can have five anchors and still be
/// unable to prove anything about this row.
pub fn verdict_of(has_uuid: bool, anchor_count: i64, uuid_matches_anchor: bool) -> Verdict {
    if !has_uuid {
        Verdict::Unresolved
    } else if uuid_matches_anchor {
        Verdict::Redundant
    } else if anchor_count == 0 {
        Verdict::Anchorless
    } else {
        Verdict::Mismatch
    }
}

/// One classified all.mangadex row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedRow {
    pub source_series_id: String,
    /// The row's work, resolved through `work_redirect`.
    pub work_id: String,
    pub suwayomi_key: String,
    pub mangadex_uuid: Option<String>,
    /// Direct (`source_type = 'mangadex'`) anchors on this work.
    pub anchor_count: i64,
    /// True when one of those anchors IS this row's UUID.
    pub uuid_matches_anchor: bool,
    /// The work that owns this UUID's direct anchor elsewhere in the catalogue,
    /// resolved. `None` when nothing in the catalogue mirrors this UUID directly.
    pub uuid_owner_work: Option<String>,
    /// `chapter` rows that would cascade if this row were deleted.
    pub chapters: i64,
    /// What Phase F recorded for this row, if it has acted on it.
    pub disposition: Option<String>,
}

impl ClassifiedRow {
    pub fn verdict(&self) -> Verdict {
        verdict_of(
            self.mangadex_uuid.is_some(),
            self.anchor_count,
            self.uuid_matches_anchor,
        )
    }
}

/// Classify every all.mangadex `source_series` row.
///
/// LEFT JOIN on `all_mangadex_uuid`, deliberately: a row whose UUID was never resolved
/// must appear as [`Verdict::Unresolved`] rather than vanish from the report. A row that
/// is missing from the classification is a row nobody proved anything about.
pub async fn classify(pool: &Database) -> Result<Vec<ClassifiedRow>> {
    let mut conn = pool.acquire().await?;
    classify_on(&mut conn, None).await
}

async fn classify_on(
    conn: &mut sqlx::PgConnection,
    source: Option<&str>,
) -> Result<Vec<ClassifiedRow>> {
    classify_scope_on(conn, source, None).await
}

async fn classify_scope_on(
    conn: &mut sqlx::PgConnection,
    source: Option<&str>,
    work: Option<&str>,
) -> Result<Vec<ClassifiedRow>> {
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT source_id FROM source_extension WHERE pkg_name = $1 ORDER BY source_id",
    )
    .bind(ALL_MANGADEX_PKG)
    .fetch_all(&mut *conn)
    .await?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rw = resolved("s.work_id");
    let ro = resolved("o.work_id");
    // "the anchor's work, resolved, is this row's work, resolved" — written as an
    // index-usable disjunction rather than the literal `resolve(a.work_id) = resolve(
    // s.work_id)`. Both forms mean the same thing (`merge_works` collapses chains, so
    // resolution is one hop), but wrapping the anchor side in COALESCE makes it an
    // expression, which the planner cannot seek on: it degenerated into a full scan of
    // all 113,889 `mangadex` rows PER all.mangadex row — 1.2 billion comparisons, and a
    // classification pass that never finished. This form seeks `idx_source_series_work`
    // on both branches and the whole pass runs in ~4.5 s over production's data.
    let same_work = format!(
        "(a.work_id = {rw} OR a.work_id IN \
           (SELECT r2.old_id FROM work_redirect r2 WHERE r2.new_id = {rw}))"
    );
    let sql = format!(
        "SELECT s.id, {rw} AS work_id, s.source_key, u.mangadex_uuid, \
                (SELECT COUNT(*) FROM source_series a \
                  WHERE a.source_type = 'mangadex' AND {same_work}) AS anchors, \
                EXISTS (SELECT 1 FROM source_series a \
                         WHERE a.source_type = 'mangadex' \
                           AND a.source_key = u.mangadex_uuid::text AND {same_work}) AS matches, \
                (SELECT {ro} FROM source_series o \
                  WHERE o.source_type = 'mangadex' AND o.source_key = u.mangadex_uuid::text \
                  LIMIT 1) AS uuid_owner, \
                (SELECT COUNT(*) FROM chapter c WHERE c.source_series_id = s.id) AS chapters, \
                u.disposition \
           FROM source_series s \
           LEFT JOIN all_mangadex_uuid u ON u.source_series_id = s.id \
          WHERE s.source_type = 'suwayomi' AND s.source_id IN ({}) \
          {} ORDER BY s.id",
        crate::sql_params::placeholders(1, ids.len()),
        if source.is_some() {
            format!("AND s.id = ${}", ids.len() + 1)
        } else if work.is_some() {
            format!("AND s.work_id = ${}", ids.len() + 1)
        } else {
            String::new()
        }
    );
    type Row = (
        String,
        String,
        String,
        Option<uuid::Uuid>,
        i64,
        bool,
        Option<String>,
        i64,
        Option<String>,
    );
    let mut q = sqlx::query_as::<_, Row>(&sql);
    for id in &ids {
        q = q.bind(id);
    }
    if let Some(scope) = source.or(work) {
        q = q.bind(scope);
    }
    let rows = q.fetch_all(&mut *conn).await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, work_id, key, uuid, anchors, matches, owner, chapters, disposition)| {
                ClassifiedRow {
                    source_series_id: id,
                    work_id,
                    suwayomi_key: key,
                    mangadex_uuid: uuid.map(|id| id.to_string()),
                    anchor_count: anchors,
                    uuid_matches_anchor: matches,
                    uuid_owner_work: owner,
                    chapters,
                    disposition,
                }
            },
        )
        .collect())
}

/// Counts by verdict, for reporting.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Census {
    pub total: u64,
    pub redundant: u64,
    pub mismatch: u64,
    pub anchorless: u64,
    pub unresolved: u64,
    pub works: u64,
    pub mismatch_chapters: i64,
    pub anchorless_chapters: i64,
}

pub fn census(rows: &[ClassifiedRow]) -> Census {
    let mut c = Census {
        total: rows.len() as u64,
        works: rows
            .iter()
            .map(|r| r.work_id.as_str())
            .collect::<HashSet<_>>()
            .len() as u64,
        ..Default::default()
    };
    for r in rows {
        match r.verdict() {
            Verdict::Redundant => c.redundant += 1,
            Verdict::Mismatch => {
                c.mismatch += 1;
                c.mismatch_chapters += r.chapters;
            }
            Verdict::Anchorless => {
                c.anchorless += 1;
                c.anchorless_chapters += r.chapters;
            }
            Verdict::Unresolved => c.unresolved += 1,
        }
    }
    c
}

// ---------------------------------------------------------------------------
// Step 3 — the rewritten gate
// ---------------------------------------------------------------------------

/// One row that failed the redundancy proof, with the reason a human needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateFailure {
    pub source_series_id: String,
    pub work_id: String,
    pub mangadex_uuid: Option<String>,
    pub verdict: Verdict,
    pub chapters: i64,
    /// Where this UUID *is* anchored, when it is anchored somewhere.
    pub uuid_owner_work: Option<String>,
}

/// The gate's outcome. `failures` is exhaustive — every failing row, not a sample.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GateReport {
    pub passed: u64,
    pub failures: Vec<GateFailure>,
    /// Chapters that would have cascaded had the failures been deleted anyway.
    pub chapters_at_risk: i64,
}

impl GateReport {
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty()
    }
}

/// **The safety property.** A row passes only when its MangaDex UUID equals the
/// `source_key` of a `source_type = 'mangadex'` row on the *same work, after
/// `work_redirect` resolution*.
///
/// This is §8g's rewrite of §7 step 3, and it is deliberately NOT §7's wording
/// ("verify every work has a mangadex anchor"), which is both unachievable — step 2
/// merges the anchorless works out of existence, so they cannot be verified as having
/// anything — and unsafe: 54 rows satisfy it while pointing at a different MangaDex
/// entry, and deleting them cascades 4,178 chapters.
pub async fn gate(pool: &Database) -> Result<GateReport> {
    let rows = classify(pool).await?;
    Ok(gate_rows(rows))
}

fn gate_rows(rows: Vec<ClassifiedRow>) -> GateReport {
    let mut report = GateReport::default();
    for r in rows {
        let verdict = r.verdict();
        if verdict == Verdict::Redundant {
            report.passed += 1;
        } else {
            report.chapters_at_risk += r.chapters;
            report.failures.push(GateFailure {
                source_series_id: r.source_series_id,
                work_id: r.work_id,
                mangadex_uuid: r.mangadex_uuid,
                verdict,
                chapters: r.chapters,
                uuid_owner_work: r.uuid_owner_work,
            });
        }
    }
    report
}

// ---------------------------------------------------------------------------
// Step 2 — merge the anchorless works onto the work that already owns their UUID
// ---------------------------------------------------------------------------

/// What one step did (or would have done).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StepReport {
    pub considered: u64,
    pub acted: u64,
    /// Rows the step could not act on, with a reason. Never silently skipped.
    pub blocked: Vec<(String, String)>,
}

/// Step 2 — fold each anchorless work into the work that already owns its UUID's direct
/// anchor.
///
/// §7 words this as "upsert from the direct API by UUID", but §8g measured what that
/// upsert would actually find: **all 496 anchorless UUIDs are already in our catalogue**
/// as a `source_type = 'mangadex'` row on a *different* work (495 distinct twins, 0 on
/// the same work), and 0 of the 496 carry more chapters than their twin. So the work to
/// do is not an ingest — it is a MERGE that kills a duplicate `w_` row, which is exactly
/// what §7 step 2's own second sentence says ("This also merges them onto an existing
/// canonical work when the UUID already maps elsewhere").
///
/// The merge goes through `catalog::merge_works_ex`, which is what makes the referential
/// hazards §8g enumerated survive:
///
/// * **18 `work_redirect` rows point AT 11 of these works** — `merge_works` rewrites
///   `new_id` to the survivor before deleting (`UPDATE work_redirect SET new_id = ?
///   WHERE new_id = ?`), so §4.14's "0 stale rows" baseline holds.
/// * **428 `merge_candidate`** rows — `candidate_work_id` is re-pointed, and pending
///   self-references created by the fold are purged.
/// * **22,991 `release_event`** rows — moved by `ledger::merge_release_events` BEFORE
///   the `work` delete, taking the earliest `first_seen_at` per chapter key, which is
///   what stops the merged-in back catalogue re-announcing itself on /updates.
/// * **486 `browse_catalogue` + 37 `feed_series_updates`** rows — these cascade off the
///   losing work by design (both are DERIVED caches, rebuilt from `work`/`chapter`/
///   `release_event`), and the survivor's own row is re-projected here so the cards do
///   not wait for the next 6-hourly rebuild.
/// * User data (`user_library`, `reviews`, `canonical_progress`, comments,
///   notifications, activity, view counters) is re-pointed or summed by the same
///   function.
///
/// Deliberately per-WORK, not per-row: 10 anchorless works carry two all.mangadex rows
/// whose UUIDs are owned by two DIFFERENT twins. A merge can only satisfy one of them,
/// so the winner is the row with the most chapters (ties by `source_series.id`, so the
/// choice is deterministic and re-runnable) and the loser falls through to
/// [`split_mismatched`], which re-points it onto its own UUID's owner. Composing the two
/// steps resolves all 496 without either one having to guess.
pub async fn merge_anchorless(
    pool: &Database,
    covers: Option<&SqliteDatabase>,
    apply: Apply,
) -> Result<StepReport> {
    let rows = classify(pool).await?;
    let mut by_work: HashMap<String, Vec<ClassifiedRow>> = HashMap::new();
    for r in rows
        .into_iter()
        .filter(|r| r.verdict() == Verdict::Anchorless)
    {
        by_work.entry(r.work_id.clone()).or_default().push(r);
    }
    let mut report = StepReport {
        considered: by_work.len() as u64,
        ..Default::default()
    };
    let mut works: Vec<String> = by_work.keys().cloned().collect();
    works.sort();
    for work in works {
        let mut candidates = by_work.remove(&work).unwrap_or_default();
        candidates.sort_by(|a, b| {
            b.chapters
                .cmp(&a.chapters)
                .then_with(|| a.source_series_id.cmp(&b.source_series_id))
        });
        let winner = &candidates[0];
        let Some(target) = winner.uuid_owner_work.clone() else {
            report.blocked.push((
                work.clone(),
                format!(
                    "uuid {} is not mirrored directly anywhere — a fresh MangaDex ingest is needed, not a merge",
                    winner.mangadex_uuid.clone().unwrap_or_default()
                ),
            ));
            continue;
        };
        if target == work {
            report
                .blocked
                .push((work.clone(), "uuid already anchors this work".into()));
            continue;
        }
        report.acted += 1;
        if !apply.is_yes() {
            continue;
        }
        crate::db::retry_transaction(|| async {
            let mut tx = pool.begin().await?;
            crate::catalog::lock_merge_inputs(&mut tx, &work, &target).await?;
            let current = classify_scope_on(&mut tx, None, Some(&work)).await?;
            let mut eligible: Vec<_> = current
                .into_iter()
                .filter(|row| row.work_id == work)
                .collect();
            eligible.sort_by(|a, b| {
                b.chapters
                    .cmp(&a.chapters)
                    .then_with(|| a.source_series_id.cmp(&b.source_series_id))
            });
            if eligible.first().is_none_or(|row| {
                row.verdict() != Verdict::Anchorless
                    || row.uuid_owner_work.as_deref() != Some(target.as_str())
                    || row.source_series_id != winner.source_series_id
            }) {
                anyhow::bail!(
                    "source changed under anchorless merge proof — reload classification"
                );
            }
            crate::catalog::merge_works_on(&mut tx, &work, &target, None, None).await?;
            let now = Utc::now().to_rfc3339();
            for c in &eligible {
                sqlx::query(
                    "UPDATE all_mangadex_uuid \
                    SET disposition = 'merged', disposed_at = $1, prev_work_id = $2, work_id = $3 \
                  WHERE source_series_id = $4",
                )
                .bind(crate::db_types::date(&now)?)
                .bind(&work)
                .bind(&target)
                .bind(&c.source_series_id)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            Ok(())
        })
        .await
        .with_context(|| format!("merge {work} -> {target}"))?;
        if let Some(covers) = covers {
            if let Err(error) = crate::catalog::reclaim_cover_blob(covers, &work).await {
                crate::db::observe_database_error(&error);
                tracing::warn!(%error, work, "phase-f: cover reclaim failed");
            }
        }
    }
    // NOT re-projected per merge, deliberately. `feed_series_updates` and
    // `browse_catalogue` are DERIVED caches: the loser's rows cascade off the deleted
    // work and the survivor's are rebuilt by the periodic refresh — which is exactly
    // what the admin `mergeWorks` mutation relies on today (it calls `merge_works_ex`
    // and re-projects nothing). Projecting the feed from the ledger for each work
    // instead costs a full `ledger::is_complete` sweep per call, whose production memo
    // is `#[cfg(not(test))]` (§8h caveat (c)). Measured on a 2.3 GB snapshot copy: with
    // the per-work projection this step made no measurable progress in 90 s; without it
    // all 486 merges commit in about 5 s. That is a lot of work to pre-warm a cache the
    // periodic refresh rebuilds anyway.
    Ok(report)
}

// ---------------------------------------------------------------------------
// Step 4 — the 54 mismatch rows: split, never delete
// ---------------------------------------------------------------------------

/// Step 4 — re-point each UUID-mismatch row onto the work its UUID actually identifies.
///
/// §8g offers two options, "create the missing direct anchor, or split the mis-merged
/// work". **Creating the anchor is not available**, and that is measured, not assumed:
/// all 54 mismatch UUIDs are ALREADY a `source_type = 'mangadex'` row on another work,
/// and all 54 are already in `work_external_id`. `source_series` is
/// `UNIQUE (source_type, source_id, source_key)` and `work_external_id` is
/// `PRIMARY KEY (provider, external_id)` — globally unique, not per-work — so creating
/// the anchor on this work would either fail or require stealing the id from its owner.
///
/// That leaves the split, and the split is also the better of the two on its merits:
///
/// * It is **one `UPDATE`** and it deletes nothing. The alternative reading of "split"
///   (fold the twin work INTO this work) would destroy 51 works to paper over a mapping
///   error, and destroy the very distinction MangaDex draws between a base series and
///   its colored/version edition.
/// * It is **reversible**: `all_mangadex_uuid.prev_work_id` records the work the row came
///   from, so the move is undone with one `UPDATE` per row.
/// * It **loses no chapters**: nothing is deleted, and the row keeps every `chapter` it
///   had. (Measured on the snapshot: for 0 of the 54 does the all.mangadex row hold more
///   chapters than the direct mirror of the *same* UUID. §8g's "~40 hold MORE chapters"
///   compares against the work's OWN anchor, i.e. against a different MangaDex entry —
///   which is exactly the mis-merge, not evidence of unique content.)
/// * It makes the mapping TRUE rather than making the catalogue coarser: after it, the
///   row sits on the work whose direct anchor is its own UUID.
///
/// `release_event` is moved with the row, but only for chapter keys the row was the sole
/// source of on the old work (`first_source_series_id` is this row AND no remaining
/// source of the old work still has that key). Anything else stays: the old work still
/// has that chapter from another source, and the ledger's rule is earliest-anywhere.
///
/// **These rows are still not deleted**, here or by [`delete_redundant`] (which carves
/// out `disposition = 'split'` unless separately opted in) — §8g's step 4 says do not
/// delete them, and a re-point does not by itself prove per-chapter-key parity with the
/// direct anchor.
pub async fn split_mismatched(pool: &Database, apply: Apply) -> Result<StepReport> {
    let rows = classify(pool).await?;
    let mismatched: Vec<ClassifiedRow> = rows
        .into_iter()
        .filter(|r| r.verdict() == Verdict::Mismatch)
        .collect();
    let mut report = StepReport {
        considered: mismatched.len() as u64,
        ..Default::default()
    };
    for r in mismatched {
        let Some(target) = r.uuid_owner_work.clone() else {
            report.blocked.push((
                r.source_series_id.clone(),
                format!(
                    "uuid {} is mirrored nowhere — needs a direct MangaDex ingest before this row can be placed",
                    r.mangadex_uuid.clone().unwrap_or_default()
                ),
            ));
            continue;
        };
        if target == r.work_id {
            report.blocked.push((
                r.source_series_id.clone(),
                "already on the work that owns its uuid".into(),
            ));
            continue;
        }
        report.acted += 1;
        if !apply.is_yes() {
            continue;
        }
        crate::db::retry_transaction(|| async {
        let mut tx = pool.begin_with("BEGIN").await?;
        lock_proof_inputs(&mut tx, &r.source_series_id, false).await?;
        let current = classify_on(&mut tx, Some(&r.source_series_id)).await?;
        let current = current.first().ok_or_else(|| anyhow::anyhow!("source disappeared under split proof"))?;
        if current.verdict() != Verdict::Mismatch || current.work_id != r.work_id || current.uuid_owner_work.as_deref() != Some(target.as_str()) {
            anyhow::bail!("source changed under split proof — reload classification");
        }
        crate::catalog::guards::held().await;
        sqlx::query("UPDATE source_series SET work_id = $1 WHERE id = $2")
            .bind(&target)
            .bind(&r.source_series_id)
            .execute(&mut *tx)
            .await?;
        // The row's chapter-list entries follow it, as in an admin split (W3a).
        crate::catalog::work_chapter::split_entries_on(
            &mut tx,
            &r.work_id,
            &target,
            std::slice::from_ref(&r.source_series_id),
            false,
        )
        .await?;
        // Move the release events this row was the ONLY source of. `INSERT OR IGNORE`
        // then delete: if the target already knows the chapter key, its own (possibly
        // earlier) first_seen_at wins, which is the ledger's rule everywhere else.
        sqlx::query(
            "INSERT INTO release_event \
                 (work_id, chapter_key, first_seen_at, first_source_series_id, label, announce_at) \
             SELECT $1, e.chapter_key, e.first_seen_at, e.first_source_series_id, e.label, \
                    e.announce_at \
               FROM release_event e \
              WHERE e.work_id = $2 AND e.first_source_series_id = $3 \
                AND NOT EXISTS (SELECT 1 FROM chapter c \
                                  JOIN source_series s ON s.id = c.source_series_id \
                                 WHERE s.work_id = $4 AND c.chapter_key = e.chapter_key) ON CONFLICT DO NOTHING",
        )
        .bind(&target)
        .bind(&r.work_id)
        .bind(&r.source_series_id)
        .bind(&r.work_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM release_event \
              WHERE work_id = $1 AND first_source_series_id = $2 \
                AND NOT EXISTS (SELECT 1 FROM chapter c \
                                  JOIN source_series s ON s.id = c.source_series_id \
                                 WHERE s.work_id = $3 AND c.chapter_key = release_event.chapter_key)",
        )
        .bind(&r.work_id)
        .bind(&r.source_series_id)
        .bind(&r.work_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE all_mangadex_uuid \
                SET disposition = 'split', disposed_at = $1, prev_work_id = $2, work_id = $3 \
              WHERE source_series_id = $4",
        )
        .bind(Utc::now())
        .bind(&r.work_id)
        .bind(&target)
        .bind(&r.source_series_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
        }).await?;
    }
    // Same as the merge step: the two derived caches are left to the periodic rebuild
    // rather than re-projected per row. See `merge_anchorless`.
    Ok(report)
}

// ---------------------------------------------------------------------------
// The destructive step — OFF unless explicitly opted into, twice
// ---------------------------------------------------------------------------

/// What [`delete_redundant`] did, or would do.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DeleteReport {
    /// Rows proven redundant at the UUID level.
    pub deletable: u64,
    /// Rows actually deleted (0 unless both opt-ins were given).
    pub deleted: u64,
    /// `chapter` rows that will cascade with them. These are duplicates of the direct
    /// mirror's by construction — that IS what the gate proves.
    pub cascading_chapters: i64,
    /// Rows held back because [`split_mismatched`] moved them (§8g: do not delete the 54).
    pub withheld_split: u64,
    /// `series_scan_state` rows retired alongside them — see the note on
    /// [`delete_redundant`]. This is the number that actually frees scan budget.
    pub scan_states: u64,
    /// The gate's verdict. The delete refuses to run while this is non-empty.
    pub gate: GateReport,
}

/// Delete the proven-redundant all.mangadex `source_series` rows.
///
/// **Inert by default.** It requires BOTH [`Apply::Yes`] and `confirm ==
/// `[`DELETE_CONFIRM_TOKEN`], and it aborts before writing anything if [`gate`] reports
/// a single failure (§8g: "Report every row that fails; do not proceed past one").
///
/// Rows disposed as `'split'` are withheld even though the gate now passes them, because
/// §8g's step 4 says the mismatch cohort is not to be deleted; `include_split` exists so
/// a later, separately-justified pass can include them without editing this code.
///
/// # It also retires the row's `series_scan_state`, and it has to
///
/// §7 step 4 is "delete only proven-redundant `source_series` rows", and F8's whole
/// stated benefit is that all.mangadex stops "consuming the shared Suwayomi scan
/// budget". **Deleting the `source_series` row alone achieves none of that**, and this
/// is measured, not argued: the scheduler picks work with
/// `SELECT series_id FROM series_scan_state WHERE next_scan_at <= ?`
/// (`scanner::due_series_ids_for_source`) and joins NOTHING. **10,479 of production's
/// 14,169 `series_scan_state` rows (74%) are all.mangadex series**, one per row deleted
/// here, and every one of them would keep being scanned forever against a mapping that
/// no longer exists.
///
/// So the scan-state row goes in the SAME transaction, guarded by "no other
/// `source_series` still claims this Suwayomi key" so a series that is also carried by a
/// second source keeps its schedule. Nothing else is touched:
/// `suwayomi_series` and its cover blobs are deliberately LEFT (§7's cover note — reader
/// home/discovery serves `/api/v1/manga/{id}/thumbnail` out of `suwayomi_cover_blob`,
/// not `/covers/`), and removing the series from Suwayomi's own library is the owner's
/// separate, engine-side step.
type ProofInput = (String, String, Option<String>, String, Option<uuid::Uuid>);

async fn proof_input(conn: &mut sqlx::PgConnection, source: &str) -> Result<Option<ProofInput>> {
    Ok(sqlx::query_as(
        "SELECT s.work_id, s.source_type, s.source_id, s.source_key, u.mangadex_uuid \
         FROM source_series s LEFT JOIN all_mangadex_uuid u ON u.source_series_id = s.id WHERE s.id = $1",
    ).bind(source).fetch_optional(conn).await?)
}

async fn lock_proof_inputs(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    source: &str,
    deleting: bool,
) -> Result<()> {
    let before = proof_input(tx, source).await?;
    let Some((owner, kind, sid, key, uuid)) = &before else {
        anyhow::bail!("source disappeared under proof");
    };
    let mut keys = crate::catalog::guards::source_keys(kind, sid.as_deref().unwrap_or(""), key);
    if let Some(uuid) = uuid {
        let uuid = uuid.to_string();
        keys.extend(crate::catalog::guards::source_keys(
            "mangadex", "mangadex", &uuid,
        ));
        keys.push(crate::locks::AdvisoryKey::external_identity(
            "mangadex", &uuid,
        ));
    }
    crate::locks::advisory(tx, &keys).await?;
    let anchors: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, work_id FROM source_series WHERE source_type = 'mangadex' AND source_key = $1 ORDER BY id",
    ).bind(uuid.map(|id| id.to_string())).fetch_all(&mut **tx).await?;
    let mut works = vec![owner.clone()];
    works.extend(anchors.iter().map(|(_, work)| work.clone()));
    crate::locks::lock_works(tx, &works).await?;
    let mut sources = vec![source.to_owned()];
    sources.extend(anchors.iter().map(|(id, _)| id.clone()));
    sources.sort();
    sources.dedup();
    for id in sources {
        let sql = if deleting && id == source {
            "SELECT id FROM source_series WHERE id = $1 FOR UPDATE"
        } else {
            "SELECT id FROM source_series WHERE id = $1 FOR NO KEY UPDATE"
        };
        sqlx::query(sql).bind(id).fetch_optional(&mut **tx).await?;
    }
    if proof_input(tx, source).await? != before {
        return Err(crate::locks::RestartTransaction.into());
    }
    let current: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, work_id FROM source_series WHERE source_type = 'mangadex' AND source_key = $1 ORDER BY id",
    ).bind(uuid.map(|id| id.to_string())).fetch_all(&mut **tx).await?;
    if current != anchors {
        return Err(crate::locks::RestartTransaction.into());
    }
    Ok(())
}

pub async fn delete_redundant(
    pool: &Database,
    apply: Apply,
    confirm: Option<&str>,
    include_split: bool,
) -> Result<DeleteReport> {
    let rows = classify(pool).await?;
    let gate_report = gate(pool).await?;
    let mut report = DeleteReport {
        gate: gate_report,
        ..Default::default()
    };
    let mut targets: Vec<&ClassifiedRow> = Vec::new();
    for r in &rows {
        if r.verdict() != Verdict::Redundant {
            continue;
        }
        if r.disposition.as_deref() == Some("split") && !include_split {
            report.withheld_split += 1;
            continue;
        }
        report.deletable += 1;
        report.cascading_chapters += r.chapters;
        targets.push(r);
    }
    // Project the scan-state half of the delete BEFORE deciding whether to write, so a
    // dry run reports the number that actually matters (74% of the scheduler's table).
    // One query over ~14k rows, intersected in memory, rather than 10k bound parameters.
    let keys: HashSet<&str> = targets.iter().map(|r| r.suwayomi_key.as_str()).collect();
    let scheduled: Vec<String> =
        crate::db_types::query_scalar("SELECT series_id FROM series_scan_state")
            .fetch_all(pool)
            .await?;
    report.scan_states = scheduled
        .iter()
        .filter(|s| keys.contains(s.as_str()))
        .count() as u64;
    if !apply.is_yes() || confirm != Some(DELETE_CONFIRM_TOKEN) {
        return Ok(report);
    }
    if !report.gate.is_clean() {
        anyhow::bail!(
            "gate reported {} failing row(s) ({} chapters at risk) — refusing to delete",
            report.gate.failures.len(),
            report.gate.chapters_at_risk
        );
    }
    let now = Utc::now().to_rfc3339();
    report.scan_states = 0; // re-counted from what the writes actually removed
    for r in targets {
        #[cfg(test)]
        crate::catalog::guards::pause("delete-discovered").await;
        let n = crate::db::retry_transaction(|| async {
            let mut tx = pool.begin_with("BEGIN").await?;
            lock_proof_inputs(&mut tx, &r.source_series_id, true).await?;
            let current = classify_on(&mut tx, Some(&r.source_series_id)).await?;
            let current = current
                .first()
                .ok_or_else(|| anyhow::anyhow!("source disappeared under deletion proof"))?;
            if current.verdict() != Verdict::Redundant
                || (current.disposition.as_deref() == Some("split") && !include_split)
            {
                anyhow::bail!("source changed under deletion proof — refusing to delete");
            }
            // UUID agreement alone cannot justify cascading a uniquely held chapter.
            // Parity is by chapter-list entry (W3a): a linked copy needs a mirror copy
            // linked to the SAME entry; only a copy not linked yet falls back to the
            // spine key. A linked copy never passes on key equality alone.
            let unique: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM chapter c \
                   LEFT JOIN work_chapter_link l ON l.chapter_id = c.id \
                  WHERE c.source_series_id = $1 AND CASE \
                    WHEN l.chapter_id IS NOT NULL THEN NOT EXISTS(SELECT 1 FROM chapter mirror \
                      JOIN work_chapter_link ml ON ml.chapter_id = mirror.id \
                      JOIN source_series anchor ON anchor.id = mirror.source_series_id \
                      WHERE anchor.source_type = 'mangadex' AND anchor.work_id = $2 \
                        AND anchor.source_key = $3 AND ml.work_chapter_id = l.work_chapter_id) \
                    ELSE (c.chapter_key IS NULL OR NOT EXISTS(SELECT 1 FROM chapter mirror \
                      JOIN source_series anchor ON anchor.id = mirror.source_series_id \
                      WHERE anchor.source_type = 'mangadex' AND anchor.work_id = $2 \
                        AND anchor.source_key = $3 AND mirror.chapter_key = c.chapter_key)) END)",
            )
            .bind(&current.source_series_id)
            .bind(&current.work_id)
            .bind(&current.mangadex_uuid)
            .fetch_one(&mut *tx)
            .await?;
            if unique {
                anyhow::bail!(
                    "chapter parity failed for {} — refusing to delete",
                    current.source_series_id
                );
            }
            crate::catalog::guards::held().await;
            sqlx::query("DELETE FROM source_series WHERE id = $1")
                .bind(&r.source_series_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "UPDATE all_mangadex_uuid SET disposition = 'deleted', disposed_at = $1 \
              WHERE source_series_id = $2",
            )
            .bind(crate::db_types::date(&now)?)
            .bind(&r.source_series_id)
            .execute(&mut *tx)
            .await?;
            // …and the schedule that would otherwise keep scanning it (see above). The
            // NOT EXISTS is what makes this safe for a Suwayomi key some other row still
            // claims — impossible today (a manga id belongs to one source), cheap insurance.
            let n = sqlx::query(
                "DELETE FROM series_scan_state \
              WHERE series_id = $1 \
                AND NOT EXISTS (SELECT 1 FROM source_series s \
                                 WHERE s.source_type = 'suwayomi' AND s.source_key = $2)",
            )
            .bind(&r.suwayomi_key)
            .bind(&r.suwayomi_key)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            tx.commit().await?;
            Ok(n)
        })
        .await?;
        report.deleted += 1;
        report.scan_states += n;
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// `komika-server phase-f <stage> [--apply] [--confirm <token>] [--include-split]`.
///
/// Every stage defaults to a dry run. Stages, in order:
///   * `resolve` — step 1, read UUIDs from Suwayomi and persist them.
///   * `report`  — read-only census + the gate.
///   * `merge`   — step 2.
///   * `split`   — step 4.
///   * `delete`  — the destructive step; needs `--apply --confirm <token>`.
pub async fn run_cli(
    pool: &Database,
    covers: Option<&SqliteDatabase>,
    suwayomi_url: &str,
    args: &[String],
) -> Result<()> {
    let stage = args.first().map(String::as_str).unwrap_or("report");
    let apply = if args.iter().any(|a| a == "--apply") {
        Apply::Yes
    } else {
        Apply::No
    };
    let confirm = args
        .iter()
        .position(|a| a == "--confirm")
        .and_then(|i| args.get(i + 1))
        .map(String::as_str);
    let include_split = args.iter().any(|a| a == "--include-split");

    let source_ids = all_mangadex_source_ids(pool).await?;
    println!(
        "phase-f {stage} [{}] — {} registered all.mangadex source id(s)",
        apply.label(),
        source_ids.len()
    );
    match stage {
        "resolve" => {
            let map = fetch_suwayomi_urls(suwayomi_url, &source_ids).await?;
            println!("  suwayomi returned {} manga urls", map.len());
            let r = record_uuids(pool, &map, apply).await?;
            println!(
                "  rows={} resolved={} unresolved={} conflicts={}",
                r.rows,
                r.resolved,
                r.unresolved.len(),
                r.conflicts.len()
            );
            for id in r.unresolved.iter().take(20) {
                println!("    UNRESOLVED {id}");
            }
            for (id, was, now) in r.conflicts.iter().take(20) {
                println!("    CONFLICT {id}: {was} -> {now}");
            }
        }
        "report" => {
            // Which of the 61 registered ids actually carry rows. §8g measured exactly
            // one (`PRIMARY_SOURCE_ID`); a second one appearing here means an empty
            // language source has started enrolling again, which is precisely what step
            // 5's exclusion exists to prevent — so it is surfaced, not assumed away.
            let sql = format!(
                "SELECT source_id, COUNT(*) FROM source_series \
                  WHERE source_type = 'suwayomi' AND source_id IN ({}) \
                  GROUP BY source_id ORDER BY 2 DESC",
                crate::sql_params::placeholders(1, source_ids.len())
            );
            let mut q = crate::db_types::query_tuple::<(String, i64)>(&sql);
            for id in &source_ids {
                q = q.bind(id);
            }
            let per_source = q.fetch_all(pool).await?;
            for (sid, n) in &per_source {
                let tag = if sid == PRIMARY_SOURCE_ID {
                    ""
                } else {
                    "  <-- UNEXPECTED: not the source id §8g measured"
                };
                println!("  source {sid}: {n} rows{tag}");
            }
            let rows = classify(pool).await?;
            let c = census(&rows);
            println!(
                "  rows={} works={} redundant={} mismatch={} anchorless={} unresolved={}",
                c.total, c.works, c.redundant, c.mismatch, c.anchorless, c.unresolved
            );
            println!(
                "  chapters on mismatch rows={} on anchorless rows={}",
                c.mismatch_chapters, c.anchorless_chapters
            );
            let g = gate(pool).await?;
            println!(
                "  GATE passed={} failed={} chapters_at_risk={}",
                g.passed,
                g.failures.len(),
                g.chapters_at_risk
            );
            // Every failure is REPORTED (§8g: "report every row that fails"), but a
            // pre-`resolve` run fails all 10,479, and 10k lines of scrollback is how a
            // report stops being read. Full list on `--all`; the counts above are never
            // truncated, and the delete refuses to run while any of them is non-zero.
            let cap = if args.iter().any(|a| a == "--all") {
                usize::MAX
            } else {
                50
            };
            for f in g.failures.iter().take(cap) {
                println!(
                    "    FAIL {} work={} uuid={} verdict={} chapters={} uuid_owner={}",
                    f.source_series_id,
                    f.work_id,
                    f.mangadex_uuid.clone().unwrap_or_else(|| "-".into()),
                    f.verdict.as_str(),
                    f.chapters,
                    f.uuid_owner_work.clone().unwrap_or_else(|| "-".into())
                );
            }
            if g.failures.len() > cap {
                println!(
                    "    … {} more failing row(s) — re-run with `--all` to list every one",
                    g.failures.len() - cap
                );
            }
        }
        "merge" => {
            let r = merge_anchorless(pool, covers, apply).await?;
            println!(
                "  works considered={} merged={} blocked={}",
                r.considered,
                r.acted,
                r.blocked.len()
            );
            for (id, why) in &r.blocked {
                println!("    BLOCKED {id}: {why}");
            }
        }
        "split" => {
            let r = split_mismatched(pool, apply).await?;
            println!(
                "  rows considered={} split={} blocked={}",
                r.considered,
                r.acted,
                r.blocked.len()
            );
            for (id, why) in &r.blocked {
                println!("    BLOCKED {id}: {why}");
            }
        }
        "delete" => {
            let r = delete_redundant(pool, apply, confirm, include_split).await?;
            println!(
                "  deletable={} cascading_chapters={} withheld_split={} scan_states={} deleted={}",
                r.deletable, r.cascading_chapters, r.withheld_split, r.scan_states, r.deleted
            );
            println!(
                "  GATE passed={} failed={}",
                r.gate.passed,
                r.gate.failures.len()
            );
            if r.deleted == 0 {
                println!(
                    "  NOTHING WAS DELETED — pass `--apply --confirm {DELETE_CONFIRM_TOKEN}` to execute"
                );
            }
        }
        other => anyhow::bail!(
            "unknown phase-f stage `{other}` (resolve | report | merge | split | delete)"
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Migration 0096 verbatim, so the fixtures and the snapshot test can never drift
    /// from the DDL production actually applies.
    const MIGRATION_0096: &str = include_str!("../migrations/0096_all_mangadex_uuid.sql");

    async fn pool() -> Database {
        let pool = Database::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations_pg")
            .run(pool.write())
            .await
            .unwrap();
        pool
    }

    #[test]
    fn a_manga_url_yields_its_uuid_and_anything_else_yields_nothing() {
        assert_eq!(
            uuid_from_manga_url("/manga/a77742b1-befd-49a4-bff5-1ad4e6b0ef7b").as_deref(),
            Some("a77742b1-befd-49a4-bff5-1ad4e6b0ef7b")
        );
        // Upstream sometimes appends a slug/query; the uuid is the first segment.
        assert_eq!(
            uuid_from_manga_url("/manga/A77742B1-BEFD-49A4-BFF5-1AD4E6B0EF7B/title?x=1").as_deref(),
            Some("a77742b1-befd-49a4-bff5-1ad4e6b0ef7b")
        );
        // A row we cannot prove anything about must NOT be guessed at: every one of
        // these becomes `Verdict::Unresolved`, which the gate fails.
        assert_eq!(uuid_from_manga_url("/manga/"), None);
        assert_eq!(uuid_from_manga_url("/manga/not-a-uuid"), None);
        assert_eq!(
            uuid_from_manga_url("/series/a77742b1-befd-49a4-bff5-1ad4e6b0ef7b"),
            None
        );
        assert_eq!(
            uuid_from_manga_url("/manga/a77742b1-befd-49a4-bff5-1ad4e6b0ef7bZ"),
            None
        );
        assert_eq!(
            uuid_from_manga_url("/manga/g77742b1-befd-49a4-bff5-1ad4e6b0ef7b"),
            None
        );
    }

    #[test]
    fn the_classification_rule_checks_the_uuid_before_it_checks_for_an_anchor() {
        // THE bug §8g found: "the work has an anchor" is not a redundancy proof.
        assert_eq!(verdict_of(true, 1, false), Verdict::Mismatch);
        assert_eq!(verdict_of(true, 5, false), Verdict::Mismatch);
        assert_eq!(verdict_of(true, 1, true), Verdict::Redundant);
        assert_eq!(verdict_of(true, 0, false), Verdict::Anchorless);
        // No uuid = nothing proven, whatever the anchors say.
        assert_eq!(verdict_of(false, 3, false), Verdict::Unresolved);
    }

    /// A miniature of production: one redundant row, one mismatch row (its work has an
    /// anchor, for a different entry), one anchorless row, one unresolved row.
    async fn fixture() -> Database {
        let pool = pool().await;
        sqlx::query(
            r#"INSERT INTO source_extension (source_id, pkg_name, repo_url, updated_at) VALUES ('2499283573021220255', $1, 'r', '2020-01-01T00:00:00Z'), ('999', $2, 'r', '2020-01-01T00:00:00Z')"#,
        )
        .bind(ALL_MANGADEX_PKG)
        .bind(ALL_MANGADEX_PKG)
        .execute(&pool)
        .await
        .unwrap();
        for (id, title) in [
            ("w_base", "Base"),
            ("w_dupe", "Duplicate"),
            ("w_twin", "Twin"),
            ("w_colored", "Colored edition"),
        ] {
            sqlx::query(
                r#"INSERT INTO work (id, primary_title, created_at, updated_at) VALUES ($1, $2, '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')"#,
            )
            .bind(id)
            .bind(title)
            .execute(&pool)
            .await
            .unwrap();
        }
        // (source_series id, work, type, source_id, key)
        for (id, work, ty, sid, key) in [
            (
                "ss_md_base",
                "w_base",
                "mangadex",
                "",
                "11111111-1111-1111-1111-111111111111",
            ),
            (
                "ss_md_twin",
                "w_twin",
                "mangadex",
                "",
                "22222222-2222-2222-2222-222222222222",
            ),
            (
                "ss_md_colored",
                "w_colored",
                "mangadex",
                "",
                "33333333-3333-3333-3333-333333333333",
            ),
            // redundant: same uuid as w_base's anchor
            ("ss_ok", "w_base", "suwayomi", "2499283573021220255", "100"),
            // mismatch: w_base has an anchor, but this row is the colored edition
            (
                "ss_mismatch",
                "w_base",
                "suwayomi",
                "2499283573021220255",
                "101",
            ),
            // anchorless: w_dupe has no mangadex row at all; its uuid is w_twin's anchor
            (
                "ss_anchorless",
                "w_dupe",
                "suwayomi",
                "2499283573021220255",
                "102",
            ),
            // unresolved: no all_mangadex_uuid row will be written for it
            (
                "ss_nouuid",
                "w_base",
                "suwayomi",
                "2499283573021220255",
                "103",
            ),
        ] {
            sqlx::query(
                r#"INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) VALUES ($1, $2, $3, $4, $5, '2020-01-01T00:00:00Z')"#,
            )
            .bind(id).bind(work).bind(ty).bind(sid).bind(key)
            .execute(&pool).await.unwrap();
        }
        // One scan schedule per Suwayomi series — the table that is 74% all.mangadex in
        // production and that the scheduler reads WITHOUT joining `source_series`.
        for key in ["100", "101", "102", "103"] {
            sqlx::query(r#"INSERT INTO series_scan_state (series_id, updated_at, next_scan_at) VALUES ($1, '2020-01-01T00:00:00Z', '1970-01-01T00:00:00+00:00')"#)
                .bind(key)
                .execute(&pool)
                .await
                .unwrap();
        }
        pool
    }

    async fn uuid_map() -> HashMap<String, String> {
        HashMap::from([
            (
                "100".into(),
                "/manga/11111111-1111-1111-1111-111111111111".into(),
            ),
            (
                "101".into(),
                "/manga/33333333-3333-3333-3333-333333333333".into(),
            ),
            (
                "102".into(),
                "/manga/22222222-2222-2222-2222-222222222222".into(),
            ),
            ("103".into(), "/manga/garbage".into()),
        ])
    }

    #[tokio::test]
    async fn step_one_persists_the_uuid_where_it_outlives_the_row_and_never_guesses() {
        let pool = fixture().await;
        let r = record_uuids(&pool, &uuid_map().await, Apply::No)
            .await
            .unwrap();
        assert_eq!((r.rows, r.resolved, r.unresolved.len()), (4, 3, 1));
        // Dry run wrote nothing.
        let n: i64 = crate::db_types::query_scalar("SELECT COUNT(*) FROM all_mangadex_uuid")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0);

        record_uuids(&pool, &uuid_map().await, Apply::Yes)
            .await
            .unwrap();
        let n: i64 = crate::db_types::query_scalar("SELECT COUNT(*) FROM all_mangadex_uuid")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 3);
        // …and on the row itself, for as long as the row exists.
        let url: Option<String> = crate::db_types::query_scalar(
            "SELECT source_url FROM source_series WHERE id = 'ss_ok'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            url.as_deref(),
            Some("/manga/11111111-1111-1111-1111-111111111111")
        );

        // A UUID that changed under us is reported, never silently overwritten.
        let mut moved = uuid_map().await;
        moved.insert(
            "100".into(),
            "/manga/44444444-4444-4444-4444-444444444444".into(),
        );
        let r = record_uuids(&pool, &moved, Apply::Yes).await.unwrap();
        assert_eq!(r.conflicts.len(), 1);
        let stored: String = crate::db_types::query_scalar(
            "SELECT mangadex_uuid FROM all_mangadex_uuid WHERE source_series_id = 'ss_ok'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(stored, "11111111-1111-1111-1111-111111111111");
    }

    #[tokio::test]
    async fn the_gate_fails_the_row_that_seven_would_have_deleted() {
        let pool = fixture().await;
        record_uuids(&pool, &uuid_map().await, Apply::Yes)
            .await
            .unwrap();
        let c = census(&classify(&pool).await.unwrap());
        assert_eq!(
            (c.total, c.redundant, c.mismatch, c.anchorless, c.unresolved),
            (4, 1, 1, 1, 1)
        );
        let g = gate(&pool).await.unwrap();
        assert_eq!(g.passed, 1);
        let mut failed: Vec<(String, Verdict)> = g
            .failures
            .iter()
            .map(|f| (f.source_series_id.clone(), f.verdict))
            .collect();
        failed.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            failed,
            vec![
                ("ss_anchorless".to_string(), Verdict::Anchorless),
                ("ss_mismatch".to_string(), Verdict::Mismatch),
                ("ss_nouuid".to_string(), Verdict::Unresolved),
            ]
        );
        // §7's gate ("the work has a mangadex anchor") would have passed ss_mismatch —
        // that work DOES have one. This is the whole difference.
        let mismatch = g
            .failures
            .iter()
            .find(|f| f.source_series_id == "ss_mismatch")
            .unwrap();
        assert_eq!(mismatch.uuid_owner_work.as_deref(), Some("w_colored"));
    }

    #[tokio::test]
    async fn the_delete_is_inert_without_both_opt_ins_and_refuses_a_dirty_gate() {
        let pool = fixture().await;
        record_uuids(&pool, &uuid_map().await, Apply::Yes)
            .await
            .unwrap();
        for (apply, confirm) in [
            (Apply::No, None),
            (Apply::No, Some(DELETE_CONFIRM_TOKEN)),
            (Apply::Yes, None),
            (Apply::Yes, Some("yes")),
        ] {
            let r = delete_redundant(&pool, apply, confirm, false)
                .await
                .unwrap();
            assert_eq!(r.deleted, 0, "{apply:?}/{confirm:?} must not delete");
            assert_eq!(r.deletable, 1);
            // The dry run still PROJECTS the scan-state half honestly.
            assert_eq!(r.scan_states, 1);
        }
        let n: i64 = crate::db_types::query_scalar("SELECT COUNT(*) FROM source_series")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 7);
        assert_eq!(
            crate::db_types::query_scalar::<i64>("SELECT COUNT(*) FROM series_scan_state")
                .fetch_one(&pool)
                .await
                .unwrap(),
            4,
            "a dry run must not retire a schedule either"
        );
        // Both opt-ins, but the gate is dirty -> abort, still nothing deleted.
        let e = delete_redundant(&pool, Apply::Yes, Some(DELETE_CONFIRM_TOKEN), false)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("refusing to delete"), "{e}");
        let n: i64 = crate::db_types::query_scalar("SELECT COUNT(*) FROM source_series")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 7);
    }

    #[tokio::test]
    async fn merge_then_split_clears_the_gate_and_only_then_does_the_delete_run() {
        let pool = fixture().await;
        // Give the unresolved row a uuid so the fixture can reach a clean gate.
        let mut map = uuid_map().await;
        map.insert(
            "103".into(),
            "/manga/11111111-1111-1111-1111-111111111111".into(),
        );
        record_uuids(&pool, &map, Apply::Yes).await.unwrap();

        assert_eq!(
            merge_anchorless(&pool, None, Apply::No)
                .await
                .unwrap()
                .acted,
            1
        );
        // Dry run changed nothing.
        assert_eq!(gate(&pool).await.unwrap().failures.len(), 2);

        assert_eq!(
            merge_anchorless(&pool, None, Apply::Yes)
                .await
                .unwrap()
                .acted,
            1
        );
        // The duplicate work is gone, its row moved, and a redirect was left behind.
        let survivor: String = crate::db_types::query_scalar(
            "SELECT work_id FROM source_series WHERE id = 'ss_anchorless'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(survivor, "w_twin");
        let redirect: Option<String> = crate::db_types::query_scalar(
            "SELECT new_id FROM work_redirect WHERE old_id = 'w_dupe'",
        )
        .fetch_optional(&pool)
        .await
        .unwrap();
        assert_eq!(redirect.as_deref(), Some("w_twin"));
        assert_eq!(
            crate::db_types::query_scalar::<i64>("SELECT COUNT(*) FROM work WHERE id = 'w_dupe'")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );

        assert_eq!(split_mismatched(&pool, Apply::Yes).await.unwrap().acted, 1);
        let moved: String = crate::db_types::query_scalar(
            "SELECT work_id FROM source_series WHERE id = 'ss_mismatch'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(moved, "w_colored");
        // …and the move is undoable from the ledger alone.
        let prev: Option<String> = crate::db_types::query_scalar(
            "SELECT prev_work_id FROM all_mangadex_uuid WHERE source_series_id = 'ss_mismatch'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(prev.as_deref(), Some("w_base"));

        let g = gate(&pool).await.unwrap();
        assert!(g.is_clean(), "{:?}", g.failures);
        assert_eq!(g.passed, 4);

        // The 54-cohort analogue is STILL withheld even though the gate now passes it.
        let r = delete_redundant(&pool, Apply::Yes, Some(DELETE_CONFIRM_TOKEN), false)
            .await
            .unwrap();
        assert_eq!((r.deletable, r.deleted, r.withheld_split), (3, 3, 1));
        // …and the schedules go with them, or Phase F frees no scan budget at all.
        assert_eq!(r.scan_states, 3);
        let scheduled: Vec<String> = crate::db_types::query_scalar(
            "SELECT series_id FROM series_scan_state ORDER BY series_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            scheduled,
            vec!["101".to_string()],
            "the split row keeps its schedule"
        );
        let left: Vec<String> = crate::db_types::query_scalar(
            "SELECT id FROM source_series WHERE source_type = 'suwayomi' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(left, vec!["ss_mismatch".to_string()]);
        let disposed: Vec<String> = crate::db_types::query_scalar(
            "SELECT disposition FROM all_mangadex_uuid ORDER BY source_series_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        // ss_anchorless / ss_nouuid / ss_ok were deleted; ss_mismatch was only split.
        // The merged row's disposition ADVANCES 'merged' -> 'deleted' (it became
        // genuinely redundant once its work was folded into the twin), which is why
        // `prev_work_id`, not `disposition`, is what keeps the merge auditable.
        assert_eq!(
            disposed,
            vec!["deleted", "split", "deleted", "deleted"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
        let merged_from: Option<String> = crate::db_types::query_scalar(
            "SELECT prev_work_id FROM all_mangadex_uuid WHERE source_series_id = 'ss_anchorless'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(merged_from.as_deref(), Some("w_dupe"));
    }

    #[tokio::test]
    async fn enrolment_exclusion_covers_every_registered_source_id_not_just_the_one_with_rows() {
        let pool = fixture().await;
        let ids = enrolment_excluded_source_ids(&pool).await.unwrap();
        // '999' holds zero rows — §8g's step-5 note is exactly that the 60 empty ids
        // must be excluded too, or one of them re-seeds the extension later.
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("2499283573021220255") && ids.contains("999"));
        assert!(is_retired_pkg(ALL_MANGADEX_PKG));
        assert!(!is_retired_pkg(
            "eu.kanade.tachiyomi.extension.en.asurascans"
        ));
    }

    // ------------------------------------------------------------------
    // The production-shaped test. Runs against a MUTABLE COPY of a snapshot,
    // never production, and skips when the snapshot is absent.
    //
    //   PHASE_F_SNAPSHOT=/tmp/phase_f_copy.sqlite3 \
    //   PHASE_F_UUIDS=/tmp/phase_f_uuids.json \
    //   cargo test --bin komika-server phase_f::tests::snapshot -- --ignored --nocapture
    // ------------------------------------------------------------------
    #[tokio::test]
    #[ignore = "needs a snapshot copy; see the comment above"]
    async fn snapshot_pipeline_reproduces_8g_and_then_clears_the_gate() {
        let (Ok(db), Ok(uuids)) = (
            std::env::var("PHASE_F_SNAPSHOT"),
            std::env::var("PHASE_F_UUIDS"),
        ) else {
            eprintln!("PHASE_F_SNAPSHOT / PHASE_F_UUIDS unset — skipping");
            return;
        };
        let opts = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&db)
            // FKs ON, because half of what this test proves is what does and does not
            // cascade (`chapter.source_series_id`, `release_event.work_id`).
            .foreign_keys(true)
            // `synchronous = OFF` only because this is a THROWAWAY COPY: the pipeline is
            // ~40k single-statement transactions and an fsync each turns a 2-minute test
            // into an hour. Production's pool keeps NORMAL (see `db::init`).
            .synchronous(sqlx::sqlite::SqliteSynchronous::Off)
            .busy_timeout(std::time::Duration::from_secs(30));
        let pool = crate::database::TestPoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        // Only OUR migration is applied — the snapshot is production's schema, and
        // 0093-0095 belong to other work in flight.
        for stmt in MIGRATION_0096.split(";\n") {
            if stmt
                .trim()
                .lines()
                .any(|l| !l.trim_start().starts_with("--") && !l.trim().is_empty())
            {
                sqlx::query(stmt).execute(&pool).await.unwrap();
            }
        }
        let raw = std::fs::read_to_string(&uuids).unwrap();
        let map: HashMap<String, String> = serde_json::from_str(&raw).unwrap();

        let r = record_uuids(&pool, &map, Apply::Yes).await.unwrap();
        println!("resolve: {r:?}");
        assert_eq!(r.rows, 10_479);
        assert_eq!(r.resolved, 10_479, "every row's uuid is obtainable (§8g)");

        let before = census(&classify(&pool).await.unwrap());
        println!("census before: {before:?}");
        assert_eq!(
            (
                before.total,
                before.redundant,
                before.mismatch,
                before.anchorless,
                before.unresolved
            ),
            (10_479, 9_929, 54, 496, 0),
            "§8g's table, reproduced"
        );
        assert_eq!(before.mismatch_chapters, 4_178);

        let g = gate(&pool).await.unwrap();
        assert_eq!(g.passed, 9_929);
        assert_eq!(g.failures.len(), 550);
        assert_eq!(
            g.failures
                .iter()
                .filter(|f| f.verdict == Verdict::Mismatch)
                .count(),
            54,
            "the 54 that §7's gate would have passed"
        );

        // Referential hazards, measured before the merge.
        let stale_redirects_before: i64 = crate::db_types::query_scalar(
            "SELECT COUNT(*) FROM work_redirect r WHERE NOT EXISTS (SELECT 1 FROM work w WHERE w.id = r.new_id)",
        ).fetch_one(&pool).await.unwrap();
        assert_eq!(stale_redirects_before, 0, "§4.14 baseline");

        let m = merge_anchorless(&pool, None, Apply::Yes).await.unwrap();
        println!(
            "merge: considered={} acted={} blocked={:?}",
            m.considered, m.acted, m.blocked
        );
        assert_eq!(m.considered, 486);
        assert_eq!(m.acted, 486);
        assert!(m.blocked.is_empty());

        let s = split_mismatched(&pool, Apply::Yes).await.unwrap();
        println!(
            "split: considered={} acted={} blocked={:?}",
            s.considered, s.acted, s.blocked
        );
        // 64, not 54: the 10 anchorless works that carried TWO all.mangadex rows with
        // two different twins can only have one of them satisfied by the merge, so the
        // other lands here as a mismatch row and is re-pointed onto its own UUID's
        // owner. That composition is why neither step has to guess (see
        // `merge_anchorless`), and it is the whole difference between 496 and 486.
        assert_eq!((s.considered, s.acted), (64, 64));
        assert!(s.blocked.is_empty());

        // Hazard checks AFTER the merge.
        let stale_redirects: i64 = crate::db_types::query_scalar(
            "SELECT COUNT(*) FROM work_redirect r WHERE NOT EXISTS (SELECT 1 FROM work w WHERE w.id = r.new_id)",
        ).fetch_one(&pool).await.unwrap();
        assert_eq!(
            stale_redirects, 0,
            "18 redirects pointed AT the merged works"
        );
        for (table, col) in [
            ("merge_candidate", "candidate_work_id"),
            ("browse_catalogue", "work_id"),
            ("feed_series_updates", "work_id"),
            ("release_event", "work_id"),
            ("source_series", "work_id"),
            ("chapter_override", "work_id"),
        ] {
            let orphans: i64 = crate::db_types::query_scalar(&format!(
                "SELECT COUNT(*) FROM {table} t \
                  WHERE NOT EXISTS (SELECT 1 FROM work w WHERE w.id = t.{col})"
            ))
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(orphans, 0, "{table}.{col} left dangling by the merge");
        }

        let g = gate(&pool).await.unwrap();
        println!(
            "gate after: passed={} failed={} at_risk={}",
            g.passed,
            g.failures.len(),
            g.chapters_at_risk
        );
        for f in g.failures.iter().take(10) {
            println!("  FAIL {f:?}");
        }
        assert!(g.is_clean(), "{} rows still unproven", g.failures.len());

        // The delete is still inert without both opt-ins…
        let d = delete_redundant(&pool, Apply::Yes, None, false)
            .await
            .unwrap();
        println!("delete dry: {d:?}");
        assert_eq!(d.deleted, 0);
        // The mismatch cohort — 54 of §8g's plus the 10 the merge surfaced — is held
        // back even though the gate now passes every one of them (§8g step 4: do not
        // delete them).
        assert_eq!(d.withheld_split, 64, "held back by disposition = 'split'");
        assert_eq!(d.deletable, 10_479 - 64);
        // 74% of the scheduler's table, which §7 step 4 alone would have left scanning.
        assert_eq!(d.scan_states, 10_479 - 64);
        assert_eq!(d.cascading_chapters, 366_340);
        let still_there: i64 = crate::db_types::query_scalar(
            "SELECT COUNT(*) FROM source_series WHERE source_type = 'suwayomi' AND source_id = $1",
        )
        .bind(PRIMARY_SOURCE_ID)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(still_there, 10_479, "dry run deleted nothing");

        // …and the real thing, ON THE COPY, so the destructive path is proven before
        // anyone points it at production. `PHASE_F_RUN_DELETE=1` gates it so a snapshot
        // someone wants to keep for inspection is not consumed by accident.
        if std::env::var("PHASE_F_RUN_DELETE").as_deref() != Ok("1") {
            eprintln!("PHASE_F_RUN_DELETE != 1 — stopping before the destructive step");
            return;
        }
        let chapters_before: i64 = crate::db_types::query_scalar("SELECT COUNT(*) FROM chapter")
            .fetch_one(&pool)
            .await
            .unwrap();
        let scan_before: i64 =
            crate::db_types::query_scalar("SELECT COUNT(*) FROM series_scan_state")
                .fetch_one(&pool)
                .await
                .unwrap();
        let d = delete_redundant(&pool, Apply::Yes, Some(DELETE_CONFIRM_TOKEN), false)
            .await
            .unwrap();
        println!("delete applied: {d:?}");
        assert_eq!(d.deleted, 10_479 - 64);
        assert_eq!(d.scan_states, 10_479 - 64);
        let left: i64 = crate::db_types::query_scalar(
            "SELECT COUNT(*) FROM source_series WHERE source_type = 'suwayomi' AND source_id = $1",
        )
        .bind(PRIMARY_SOURCE_ID)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(left, 64, "only the split cohort survives");
        let chapters_after: i64 = crate::db_types::query_scalar("SELECT COUNT(*) FROM chapter")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            chapters_before - chapters_after,
            366_340,
            "exactly the cascade the gate accounted for — no chapter went unaccounted"
        );
        let scan_after: i64 =
            crate::db_types::query_scalar("SELECT COUNT(*) FROM series_scan_state")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(scan_before - scan_after, 10_479 - 64);
        // Nothing anywhere points at a `source_series` row that no longer exists.
        for (table, col) in [
            ("chapter", "source_series_id"),
            ("merge_candidate", "source_series_id"),
        ] {
            let orphans: i64 = crate::db_types::query_scalar(&format!(
                "SELECT COUNT(*) FROM {table} t \
                  WHERE NOT EXISTS (SELECT 1 FROM source_series s WHERE s.id = t.{col})"
            ))
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(orphans, 0, "{table}.{col} dangling after the delete");
        }
        // And the ledger survived: `release_event.first_source_series_id` is nullable and
        // NON-ENFORCING by design (§9's mitigation), so a deleted source must not have
        // taken any first-seen history with it.
        let events_orphaned: i64 = crate::db_types::query_scalar(
            "SELECT COUNT(*) FROM release_event e \
              WHERE NOT EXISTS (SELECT 1 FROM work w WHERE w.id = e.work_id)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(events_orphaned, 0);
    }
    #[tokio::test]
    async fn uuid_evidence_and_source_url_choose_one_conflicting_writer() {
        let pool = fixture().await.foreground();
        let first = uuid_map().await;
        let mut second = first.clone();
        second.insert(
            "102".into(),
            "/manga/33333333-3333-3333-3333-333333333333".into(),
        );
        let (a, b) = crate::catalog::phase4_tests::overlap(
            &pool,
            move |pool| async move { record_uuids(&pool, &first, Apply::Yes).await },
            move |pool| async move { record_uuids(&pool, &second, Apply::Yes).await },
        )
        .await;
        assert!(a.unwrap().conflicts.is_empty());
        let conflicts = b.unwrap().conflicts;
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].0, "ss_anchorless");
        let values:(uuid::Uuid,String)=sqlx::query_as("SELECT u.mangadex_uuid,s.source_url FROM all_mangadex_uuid u JOIN source_series s ON s.id=u.source_series_id WHERE s.id='ss_anchorless'").fetch_one(&pool).await.unwrap();
        assert_eq!(values.1, format!("/manga/{}", values.0));
        assert_eq!(values.0.to_string(), "22222222-2222-2222-2222-222222222222");
    }

    #[tokio::test]
    async fn anchorless_merge_and_uuid_recording_preserve_disposition_in_both_orders() {
        for merge_first in [false, true] {
            let pool = fixture().await.foreground();
            let map = uuid_map().await;
            record_uuids(&pool, &map, Apply::Yes).await.unwrap();
            let merge = move |pool: Database| async move {
                merge_anchorless(&pool, None, Apply::Yes).await.map(|_| ())
            };
            let record = move |pool: Database| async move {
                record_uuids(&pool, &map, Apply::Yes).await.map(|_| ())
            };
            let (a, b) = if merge_first {
                crate::catalog::phase4_tests::overlap(&pool, merge, record).await
            } else {
                crate::catalog::phase4_tests::overlap(&pool, record, merge).await
            };
            a.unwrap();
            b.unwrap();
            let values:(String,String,String)=sqlx::query_as("SELECT s.work_id,u.work_id,u.disposition FROM source_series s JOIN all_mangadex_uuid u ON u.source_series_id=s.id WHERE s.id='ss_anchorless'").fetch_one(&pool).await.unwrap();
            assert_eq!(values, ("w_twin".into(), "w_twin".into(), "merged".into()));
            assert_eq!(
                crate::catalog::redirect_work_id(&pool, "w_dupe")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("w_twin")
            );
        }
    }

    #[tokio::test]
    async fn mismatch_split_and_chapter_writer_keep_the_new_owner() {
        let pool = fixture().await.foreground();
        record_uuids(&pool, &uuid_map().await, Apply::Yes)
            .await
            .unwrap();
        let chapter = crate::catalog::ChapterInput {
            external_id: "before".into(),
            number: Some("1".into()),
            published_at: Some("2020-01-01T00:00:00Z".into()),
            ..Default::default()
        };
        crate::catalog::upsert_chapter(&pool, "ss_mismatch", &chapter)
            .await
            .unwrap();
        crate::catalog::ledger::record_source_series(&pool, "ss_mismatch")
            .await
            .unwrap();
        let (a, b) = crate::catalog::phase4_tests::overlap(
            &pool,
            move |pool| async move { split_mismatched(&pool, Apply::Yes).await },
            move |pool| async move {
                crate::catalog::upsert_chapter(
                    &pool,
                    "ss_mismatch",
                    &crate::catalog::ChapterInput {
                        external_id: "after".into(),
                        number: Some("2".into()),
                        ..Default::default()
                    },
                )
                .await
            },
        )
        .await;
        assert_eq!(a.unwrap().acted, 1);
        b.unwrap();
        let values:(String,String,String)=sqlx::query_as("SELECT s.work_id,u.prev_work_id,u.disposition FROM source_series s JOIN all_mangadex_uuid u ON u.source_series_id=s.id WHERE s.id='ss_mismatch'").fetch_one(&pool).await.unwrap();
        assert_eq!(
            values,
            ("w_colored".into(), "w_base".into(), "split".into())
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT work_id FROM release_event WHERE first_source_series_id='ss_mismatch'"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            "w_colored"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM chapter WHERE source_series_id='ss_mismatch'"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            2
        );
    }

    async fn clean_delete_fixture() -> Database {
        let pool = fixture().await.foreground();
        let mut map = uuid_map().await;
        map.insert(
            "103".into(),
            "/manga/11111111-1111-1111-1111-111111111111".into(),
        );
        record_uuids(&pool, &map, Apply::Yes).await.unwrap();
        merge_anchorless(&pool, None, Apply::Yes).await.unwrap();
        split_mismatched(&pool, Apply::Yes).await.unwrap();
        assert!(gate(&pool).await.unwrap().is_clean());
        pool
    }

    #[tokio::test]
    async fn delete_revalidates_chapter_parity_after_discovery() {
        let pool = clean_delete_fixture().await;
        let points = crate::locks::pause_points::PausePoints::default();
        let discovered = points.arm("delete-discovered");
        let delete_pool = pool.clone();
        let deleting = tokio::spawn(
            crate::catalog::guards::TEST_PAUSE.scope(points, async move {
                delete_redundant(&delete_pool, Apply::Yes, Some(DELETE_CONFIRM_TOKEN), false).await
            }),
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), discovered.reached())
            .await
            .unwrap();
        crate::catalog::upsert_chapter(
            &pool,
            "ss_anchorless",
            &crate::catalog::ChapterInput {
                external_id: "unique".into(),
                number: Some("777".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        discovered.release();
        let error = deleting.await.unwrap().unwrap_err();
        assert!(
            error.to_string().contains("chapter parity failed"),
            "{error:#}"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM source_series WHERE id='ss_anchorless'"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM chapter WHERE external_id='unique'")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM series_scan_state WHERE series_id='102'"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn delete_and_split_keep_a_defined_winner_in_both_orders() {
        for delete_first in [false, true] {
            let pool = clean_delete_fixture().await;
            let delete = move |pool: Database| async move {
                delete_redundant(&pool, Apply::Yes, Some(DELETE_CONFIRM_TOKEN), false)
                    .await
                    .map(|_| ())
            };
            let split = move |pool: Database| async move {
                crate::catalog::split_source_series(
                    &pool,
                    "w_twin",
                    &["ss_anchorless".into()],
                    None,
                )
                .await
                .map(|_| ())
            };
            let (a, b) = if delete_first {
                crate::catalog::phase4_tests::overlap(&pool, delete, split).await
            } else {
                crate::catalog::phase4_tests::overlap(&pool, split, delete).await
            };
            a.unwrap();
            let error = b.unwrap_err();
            assert!(
                error.to_string().contains(if delete_first {
                    "not a source"
                } else {
                    "refusing to delete"
                }),
                "{error:#}"
            );
            let left: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM source_series WHERE id='ss_anchorless'")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(left, i64::from(!delete_first));
        }
    }
}
