//! The dedup matcher (CATALOGUE.md §4).
//!
//! Resolves a source-series to a canonical `work` via a precision ladder:
//!   1. external-ID exact          -> auto-merge (highest precision, stop on hit)
//!   2. normalized-title exact      -> candidate
//!   3. fuzzy title (token block)   -> candidate shortlist
//!   4. corroborate (description / cover pHash / author / year) -> confidence score
//!   5. exact title + >=90% author characters -> auto-merge; fuzzy high
//!      (cover-corroborated) -> auto-merge; mid -> manual review queue.
//!
//! Titles are compared article-insensitively ("The X" = "X"). Author names ignore
//! case, punctuation and word order, and match through any listed person or
//! parenthesised transliteration. Missing authors remain in review. A novel never
//! matches a non-novel, and an edition (colored, long strip, promo) never matches a
//! title that does not name it. External identities and admin split decisions retain precedence.

use std::collections::HashSet;

use crate::db::Database;
use anyhow::Result;

use crate::catalog::{
    self,
    normalize::{edition_mismatch, match_key, normalize_title, novel_mismatch},
    pair::{is_discriminative_phash, PHASH_CORROBORATION},
    similarity::{
        description_shingles, description_similarity_pre, phash_similarity, title_shingles,
        title_similarity_pre, DescShingles, TitleShingles,
    },
    WorkMatchData,
};

/// A source-series to resolve against the canonical spine.
#[derive(Debug, Clone, Default)]
pub struct Candidate {
    pub title: String,
    pub alt_titles: Vec<String>,
    pub description: Option<String>,
    pub author: Option<String>,
    pub year: Option<i64>,
    pub cover_phash: Option<String>,
    /// `(provider, external_id)` the source exposes (AniList/MAL/…), if any.
    pub external_ids: Vec<(String, String)>,
}

/// The matcher's verdict for a candidate.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    AutoMerge {
        work_id: String,
        score: f64,
        method: String,
    },
    Review {
        work_id: String,
        score: f64,
        method: String,
    },
    New,
}

/// Fuzzy-match bands: a cover-corroborated fuzzy match >= HIGH auto-merges; >= MID
/// goes to review; below is a new work. Exact titles with matching authors bypass
/// these bands — see `resolve`.
pub const HIGH: f64 = 0.85;
pub const MID: f64 = 0.6;
const FUZZY_BLOCK_LIMIT: i64 = 50;
/// How many of the longest title tokens the fuzzy block keys on (DD4). Keying on
/// only the single longest token missed a merge whenever the discriminating token
/// wasn't the longest; unioning the top-N candidate sets closes that recall gap.
const FUZZY_BLOCK_TOKENS: usize = 3;
/// Resolve `cand` against the canonical works in `pool`.
pub async fn resolve(pool: &Database, cand: &Candidate) -> Result<Decision> {
    resolve_ex(pool, cand, None).await
}

/// As [`resolve`], but excludes `exclude_work_id` from the candidate set. The
/// catalogue reconcile re-matches an existing (provisional) work against the spine,
/// where the work's own title is one of its aliases — without excluding it, the work
/// would always "match itself". `None` is the normal add-time path.
/// Returns advisory matching evidence. The caller must revalidate inside the
/// transaction that links/mints/merges a work; returning a decision releases no
/// transaction-owned guard and cannot reserve an absent title or external ID.
pub async fn resolve_ex(
    pool: &Database,
    cand: &Candidate,
    exclude_work_id: Option<&str>,
) -> Result<Decision> {
    let mut conn = pool.acquire().await?;
    resolve_on(&mut conn, cand, exclude_work_id).await
}

/// The advisory keys that reserve a candidate's match domain: one `ExternalIdentity`
/// key for each external id it supplies and one `MatchDomain` key for each of its
/// article-insensitive title keys. Two transactions that could mint or link a work for
/// the same external id or the same exact title therefore wait for each other.
///
/// Fuzzy (token-block) matches are deliberately NOT covered: two differently named
/// candidates that only resemble each other can still be minted side by side, and the
/// consolidation sweep folds them later. The keys are returned unsorted;
/// `locks::advisory` sorts and removes duplicates, so pass them in ONE call together
/// with every other advisory key of the unit.
pub fn match_domain_keys(cand: &Candidate) -> Vec<crate::locks::AdvisoryKey> {
    use crate::locks::{AdvisoryKey, KeyClass};
    let mut keys: Vec<AdvisoryKey> = cand
        .external_ids
        .iter()
        .filter(|(provider, external)| !provider.is_empty() && !external.is_empty())
        .map(|(provider, external)| AdvisoryKey::external_identity(provider, external))
        .collect();
    let mut titles: Vec<String> = std::iter::once(cand.title.as_str())
        .chain(cand.alt_titles.iter().map(String::as_str))
        .map(normalize_title)
        .filter(|title| !title.is_empty())
        .map(|title| match_key(&title))
        .filter(|key| !key.is_empty())
        .collect();
    titles.sort();
    titles.dedup();
    keys.extend(
        titles
            .iter()
            .map(|key| AdvisoryKey::new(KeyClass::MatchDomain, &[key])),
    );
    keys
}

/// [`resolve_ex`] on the caller's connection, so the decision is read inside the same
/// transaction that then links, mints or merges. Read only: it takes no lock and does
/// not commit.
///
/// Guards the CALLER must already hold for the decision to stay true until its commit:
/// every key of [`match_domain_keys`] for `cand` (taken in the unit's single
/// `locks::advisory` call). With those held, `Decision::New` cannot be invalidated by
/// another writer that holds the same keys, and an `external_id` or `title_exact`
/// decision names a work that existed when this ran. Before acting on a returned
/// `work_id` the caller must lock that work (`locks::lock_works`, with every other work
/// of the unit, sorted) and call this again; if the decision changed, return
/// `locks::RestartTransaction`. A `fuzzy` decision is evidence only (see
/// [`match_domain_keys`]).
pub async fn resolve_on(
    conn: &mut sqlx::PgConnection,
    cand: &Candidate,
    exclude_work_id: Option<&str>,
) -> Result<Decision> {
    // 1. External-ID exact — highest precision, stop on hit.
    for (provider, ext) in &cand.external_ids {
        if provider.is_empty() || ext.is_empty() {
            continue;
        }
        if let Some(work_id) = catalog::find_work_by_external_on(&mut *conn, provider, ext).await? {
            if Some(work_id.as_str()) == exclude_work_id {
                continue;
            }
            return Ok(Decision::AutoMerge {
                work_id,
                score: 1.0,
                method: "external_id".into(),
            });
        }
    }

    // Normalized title set (primary + alts).
    let mut norm_titles: Vec<String> = std::iter::once(cand.title.clone())
        .chain(cand.alt_titles.iter().cloned())
        .map(|t| normalize_title(&t))
        .filter(|s| !s.is_empty())
        .collect();
    norm_titles.sort();
    norm_titles.dedup();
    if norm_titles.is_empty() {
        return Ok(Decision::New);
    }

    // 2. Exact normalized aliases shortlist candidates; author names disambiguate them.
    let mut candidate_ids: HashSet<String> = HashSet::new();
    for nt in &norm_titles {
        candidate_ids.extend(catalog::find_works_by_alias_on(&mut *conn, nt).await?);
    }
    if let Some(ex) = exclude_work_id {
        candidate_ids.remove(ex);
    }
    // 2b. No exact title: the same titles with articles ignored ("The X" = "X"), which
    //     is how one series is named across sites. Only a fallback, so an exactly-named
    //     work is never passed over for an article variant of it.
    let cand_keys: Vec<String> = norm_titles.iter().map(|t| match_key(t)).collect();
    let mut article_hit = false;
    if candidate_ids.is_empty() {
        for key in &cand_keys {
            candidate_ids.extend(catalog::find_works_by_match_key_on(&mut *conn, key).await?);
        }
        if let Some(ex) = exclude_work_id {
            candidate_ids.remove(ex);
        }
        article_hit = !candidate_ids.is_empty();
    }
    let exact_hit = !candidate_ids.is_empty();
    let exact_ids = candidate_ids.clone();

    // 3. Fuzzy blocking when no exact hit: block on the top-N longest title tokens
    //    (union), so the discriminating token being shorter than a generic one
    //    doesn't drop the real match (DD4).
    if candidate_ids.is_empty() {
        for token in top_tokens(&norm_titles, FUZZY_BLOCK_TOKENS) {
            let ids =
                catalog::candidate_work_ids_by_token_on(&mut *conn, &token, FUZZY_BLOCK_LIMIT)
                    .await?;
            candidate_ids.extend(ids);
        }
        if let Some(ex) = exclude_work_id {
            candidate_ids.remove(ex);
        }
    }
    if candidate_ids.is_empty() {
        return Ok(Decision::New);
    }

    // 4. Score every candidate; keep the best (carrying whether the cover pHash
    //    corroborated it, needed for the DD3 auto-merge guard). `candidate_ids` is a
    //    HashSet with nondeterministic iteration, so ties are broken by the lowest
    //    work_id — otherwise the surfaced Review candidate would be arbitrary across
    //    runs when several equal-scoring works share the exact title (DD7).
    // Batch-load all blocked candidates' match data in two queries (OPT-7) rather than
    // 2 round-trips per candidate — up to ~300 candidates on a reconcile.
    let candidate_id_list: Vec<String> = candidate_ids.iter().cloned().collect();
    let match_data = catalog::load_match_data_batch_on(&mut *conn, &candidate_id_list).await?;
    // Precompute the CANDIDATE's shingle sets once — they're compared against every
    // candidate work's aliases (works carry 20+ localized aliases) and description, so
    // shingling them per comparison inside the loop was the reconcile CPU hot spot (OPT-D).
    let cand_title_shingles: Vec<TitleShingles> =
        norm_titles.iter().map(|t| title_shingles(t)).collect();
    let cand_desc_shingles: Option<DescShingles> =
        cand.description.as_deref().map(description_shingles);
    let mut best: Option<(Scored, String)> = None;
    let mut author_hits = Vec::new();
    for wid in &candidate_ids {
        let Some(md) = match_data.get(wid) else {
            continue;
        };
        // A novel and its comic adaptation share a title, alt titles and an author, so
        // they score as a perfect duplicate. They are not one: never offer the pair.
        if novel_mismatch(Some(&cand.title), md.primary_title.as_deref()) {
            continue;
        }
        // A colored, long-strip or other edition is never the series itself, though the
        // title key ignores "Official Colored": never merge or offer the pair.
        if edition_mismatch(Some(&cand.title), md.primary_title.as_deref()) {
            continue;
        }
        if exact_ids.contains(wid)
            && crate::catalog::similarity::authors_match(
                cand.author.as_deref(),
                md.author.as_deref(),
            )
        {
            author_hits.push(wid.clone());
        }
        let scored = score_candidate(
            cand,
            &cand_title_shingles,
            &cand_keys,
            cand_desc_shingles.as_ref(),
            md,
        );
        let replace = match best.as_ref() {
            None => true,
            Some((s, w)) => scored.score > s.score || (scored.score == s.score && wid < w),
        };
        if replace {
            best = Some((scored, wid.clone()));
        }
    }
    // Author corroboration disambiguates unrelated namesakes. When several copies
    // qualify, attach to the richest one; the consolidation sweep folds the others.
    if !author_hits.is_empty() {
        let wid = catalog::pick_survivor_on(&mut *conn, &author_hits).await?;
        let md = &match_data[&wid];
        best = Some((
            score_candidate(
                cand,
                &cand_title_shingles,
                &cand_keys,
                cand_desc_shingles.as_ref(),
                md,
            ),
            wid,
        ));
    }
    let Some((scored, work_id)) = best else {
        return Ok(Decision::New);
    };

    // A rejected review or an admin split takes precedence over automatic matching.
    if let Some(self_id) = exclude_work_id {
        let rejected: bool = crate::db_types::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM merge_candidate mc JOIN source_series ss \
             ON ss.id = mc.source_series_id WHERE mc.status = 'rejected' AND \
             ((ss.work_id = $1 AND mc.candidate_work_id = $2) OR \
              (ss.work_id = $3 AND mc.candidate_work_id = $4)))",
        )
        .bind(self_id)
        .bind(&work_id)
        .bind(&work_id)
        .bind(self_id)
        .fetch_one(&mut *conn)
        .await?;
        if rejected || catalog::is_split_pair_on(&mut *conn, self_id, &work_id).await? {
            return Ok(Decision::New);
        }
    }
    let score = scored.score;

    // Exact titles require author corroboration. Fuzzy matches retain the cover gate.
    let method = if exact_hit { "title_exact" } else { "fuzzy" };
    // Require both a tight pHash match AND that the candidate's own cover hash is
    // discriminative — a near-uniform hash must not corroborate an auto-merge.
    let cover_corroborated = scored
        .phash_sim
        .map(|p| p >= PHASH_CORROBORATION)
        .unwrap_or(false)
        && cand
            .cover_phash
            .as_deref()
            .map(is_discriminative_phash)
            .unwrap_or(false);
    // An article-only match that fits several same-author works has no single right
    // target; that is a question for review, not for `pick_survivor`'s sort order.
    let exact_auto_merge =
        exact_hit && !author_hits.is_empty() && !(article_hit && author_hits.len() > 1);
    let decision = if exact_auto_merge || (score >= HIGH && cover_corroborated) {
        Decision::AutoMerge {
            work_id,
            score,
            method: method.into(),
        }
    } else if score >= MID {
        Decision::Review {
            work_id,
            score,
            method: method.into(),
        }
    } else {
        Decision::New
    };
    Ok(decision)
}

/// A candidate's confidence plus the cover-pHash signal that fed it (kept separate so
/// the decision can require cover corroboration for a title-driven auto-merge — DD3).
struct Scored {
    score: f64,
    /// Cover-pHash similarity vs the candidate work, if both had a hash.
    phash_sim: Option<f64>,
}

/// Confidence in [0,1]: `0.6*title + 0.4*corroboration`, plus small author/year
/// boosters. Title alone (no description/cover overlap) tops out at ~0.6 → Review,
/// so auto-merge needs corroboration on top of the title.
fn score_candidate(
    cand: &Candidate,
    cand_titles: &[TitleShingles],
    cand_keys: &[String],
    cand_desc: Option<&DescShingles>,
    md: &WorkMatchData,
) -> Scored {
    // Shingle this candidate work's aliases once, then do cheap set comparisons against
    // the pre-shingled candidate titles (T+K shinglings instead of the former T×K).
    let alias_shingles: Vec<TitleShingles> = md
        .aliases_norm
        .iter()
        .map(|al| title_shingles(al))
        .collect();
    let title_sim = cand_titles
        .iter()
        .flat_map(|nt| {
            alias_shingles
                .iter()
                .map(move |al| title_similarity_pre(nt, al))
        })
        .fold(0.0_f64, f64::max);
    // Titles that differ only by an article are the same title. Shingle overlap scores
    // "The Box" against "Box" at about 0.5, which would drop an uncorroborated article
    // match below MID and mint a silent duplicate instead of asking for review.
    let title_sim = if md
        .aliases_norm
        .iter()
        .any(|al| cand_keys.contains(&match_key(al)))
    {
        1.0
    } else {
        title_sim
    };

    let mut corrob = 0.0_f64;
    if let (Some(a), Some(b)) = (cand_desc, &md.description) {
        corrob = corrob.max(description_similarity_pre(a, &description_shingles(b)));
    }
    let phash_sim = phash_similarity(cand.cover_phash.as_deref(), md.cover_phash.as_deref());
    if let Some(p) = phash_sim {
        corrob = corrob.max(p);
    }

    let mut score = 0.6 * title_sim + 0.4 * corrob;
    if author_matches(&cand.author, &md.author) {
        score += 0.05;
    }
    if year_close(cand.year, md.year) {
        score += 0.03;
    }
    Scored {
        score: score.min(1.0),
        phash_sim,
    }
}

fn author_matches(a: &Option<String>, b: &Option<String>) -> bool {
    crate::catalog::similarity::authors_match(a.as_deref(), b.as_deref())
}

fn year_close(a: Option<i64>, b: Option<i64>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if (a - b).abs() <= 1)
}

/// The `n` longest distinct title tokens (≥3 chars), longest first — the fuzzy block
/// keys on these (DD4). Ties broken lexically so the selection is deterministic.
fn top_tokens(norm_titles: &[String], n: usize) -> Vec<String> {
    let mut toks: Vec<String> = norm_titles
        .iter()
        .flat_map(|t| t.split_whitespace())
        .filter(|w| w.chars().count() >= 3)
        .map(|w| w.to_string())
        .collect();
    // Longest first, then lexical — so equal-length duplicates sit adjacent for dedup.
    toks.sort_by(|a, b| b.chars().count().cmp(&a.chars().count()).then(a.cmp(b)));
    toks.dedup();
    toks.truncate(n);
    toks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Alias, WorkInput};

    async fn pool() -> Database {
        let pool = Database::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations_pg")
            .run(pool.write())
            .await
            .unwrap();
        pool
    }

    async fn seed_slime(pool: &Database) -> String {
        let input = WorkInput {
            primary_title: Some("That Time I Got Reincarnated as a Slime".into()),
            description: Some(
                "The ordinary Mikami Satoru found himself dying after being stabbed by a slasher."
                    .into(),
            ),
            year: Some(2015),
            author: Some("Fuse".into()),
            cover_phash: Some("ffff0000ffff0000".into()),
            aliases: vec![
                Alias {
                    raw: "That Time I Got Reincarnated as a Slime".into(),
                    lang: Some("en".into()),
                },
                Alias {
                    raw: "Tensei Shitara Slime Datta Ken".into(),
                    lang: Some("ja-ro".into()),
                },
            ],
            external_ids: vec![("al".into(), "101517".into())],
            ..Default::default()
        };
        catalog::upsert_work_from_mangadex(pool, "md-slime", &input)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn exact_title_and_fuzzy_author_auto_merge_even_for_short_names() {
        let pool = pool().await;
        let mut matching = String::new();
        for author in ["Masashi Kishimoto", "Different Person"] {
            let id = catalog::create_work(
                &pool,
                &WorkInput {
                    primary_title: Some("Ao".into()),
                    author: Some(author.into()),
                    aliases: vec![Alias {
                        raw: "Ao".into(),
                        lang: None,
                    }],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            if author == "Masashi Kishimoto" {
                matching = id;
            }
        }
        let cand = Candidate {
            title: "AO!".into(),
            author: Some("Kishimot, Masashi".into()),
            ..Default::default()
        };
        match resolve(&pool, &cand).await.unwrap() {
            Decision::AutoMerge {
                work_id, method, ..
            } => {
                assert_eq!(work_id, matching);
                assert_eq!(method, "title_exact");
            }
            other => panic!("expected author-corroborated merge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn external_id_auto_merges() {
        let pool = pool().await;
        let w = seed_slime(&pool).await;
        let cand = Candidate {
            title: "Totally Different Title".into(),
            external_ids: vec![("al".into(), "101517".into())],
            ..Default::default()
        };
        match resolve(&pool, &cand).await.unwrap() {
            Decision::AutoMerge {
                work_id,
                method,
                score,
            } => {
                assert_eq!(work_id, w);
                assert_eq!(method, "external_id");
                assert_eq!(score, 1.0);
            }
            other => panic!("expected external-id auto-merge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exact_title_without_author_requires_review() {
        let pool = pool().await;
        let w = seed_slime(&pool).await;
        // Title alone supplies no author corroboration.
        let cand = Candidate {
            title: "Tensei Shitara Slime Datta Ken".into(),
            ..Default::default()
        };
        match resolve(&pool, &cand).await.unwrap() {
            Decision::Review { work_id, .. } => assert_eq!(work_id, w),
            other => panic!("expected review without an author, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exact_title_auto_merges_without_cover() {
        // Matching authors authorize an exact-title merge without a cover.
        let pool = pool().await;
        let w = seed_slime(&pool).await;
        let cand = Candidate {
            title: "Tensei Shitara Slime Datta Ken".into(),
            author: Some("Fuse".into()),
            description: Some(
                "The ordinary Mikami Satoru found himself dying after being stabbed by a slasher."
                    .into(),
            ),
            ..Default::default()
        };
        match resolve(&pool, &cand).await.unwrap() {
            Decision::AutoMerge {
                work_id, method, ..
            } => {
                assert_eq!(work_id, w);
                assert_eq!(method, "title_exact");
            }
            other => panic!("expected auto-merge (exact title policy), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exact_title_with_cover_phash_auto_merges() {
        // DD3: the same exact-title match auto-merges once a corroborating cover backs
        // it — here an identical pHash to the seeded work's `ffff0000ffff0000`.
        let pool = pool().await;
        let w = seed_slime(&pool).await;
        let cand = Candidate {
            title: "Tensei Shitara Slime Datta Ken".into(),
            cover_phash: Some("ffff0000ffff0000".into()),
            ..Default::default()
        };
        match resolve(&pool, &cand).await.unwrap() {
            Decision::AutoMerge { work_id, .. } => assert_eq!(work_id, w),
            other => panic!("expected auto-merge (cover corroborated), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fuzzy_block_finds_work_when_discriminating_token_is_not_longest() {
        // DD4: the fuzzy block used to key on only the single longest token. The
        // existing work's alias is "phantom dungeon"; the candidate adds a strictly
        // longer, non-shared word ("extraordinary", 13 > 7) so the single-longest block
        // keys on a token absent from every alias → returned nothing → Decision::New (a
        // silent duplicate). The top-N union still keys on the shared "dungeon"/"phantom"
        // tokens and finds it. A copied description lifts it into the Review band.
        let pool = pool().await;
        let blurb = "A lone delver descends the phantom dungeon beneath the ruined capital.";
        let existing = catalog::create_work(
            &pool,
            &WorkInput {
                primary_title: Some("Phantom Dungeon".into()),
                description: Some(blurb.into()),
                aliases: vec![Alias {
                    raw: "Phantom Dungeon".into(),
                    lang: None,
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let cand = Candidate {
            title: "Extraordinary Phantom Dungeon".into(),
            description: Some(blurb.into()),
            ..Default::default()
        };
        match resolve(&pool, &cand).await.unwrap() {
            Decision::New => panic!("DD4: multi-token block should have found the work"),
            Decision::Review { work_id, .. } | Decision::AutoMerge { work_id, .. } => {
                assert_eq!(work_id, existing)
            }
        }
    }

    #[tokio::test]
    async fn tied_exact_title_candidate_is_deterministic() {
        // DD7 + the ambiguity guard: two distinct works share the exact normalized
        // title with no corroboration. The exact rung must NOT auto-merge (which
        // physically deletes one of them, chosen arbitrarily) — it drops to Review. The
        // candidate set is a HashSet (nondeterministic order), so the surfaced work_id
        // must still be pinned by the lowest-work_id tiebreak and stable across resolves.
        let pool = pool().await;
        let mut ids = Vec::new();
        for _ in 0..2 {
            ids.push(
                catalog::create_work(
                    &pool,
                    &WorkInput {
                        primary_title: Some("Ambiguous Title".into()),
                        aliases: vec![Alias {
                            raw: "Ambiguous Title".into(),
                            lang: None,
                        }],
                        ..Default::default()
                    },
                )
                .await
                .unwrap(),
            );
        }
        let expected = ids.iter().min().unwrap().clone();
        let cand = Candidate {
            title: "Ambiguous Title".into(),
            ..Default::default()
        };
        for _ in 0..3 {
            match resolve(&pool, &cand).await.unwrap() {
                Decision::Review { work_id, .. } => assert_eq!(work_id, expected),
                other => panic!("expected review (ambiguous exact title), got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn short_common_exact_title_does_not_auto_merge() {
        // A normalized title under `EXACT_MERGE_MIN_TITLE_CHARS` is a common phrase, not
        // an identity ("love", "hero", "change"). Even a SINGLE exact candidate must
        // land in Review rather than irreversibly folding two unrelated works.
        let pool = pool().await;
        let existing = catalog::create_work(
            &pool,
            &WorkInput {
                primary_title: Some("Love".into()),
                aliases: vec![Alias {
                    raw: "Love".into(),
                    lang: None,
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let cand = Candidate {
            title: "Love".into(),
            ..Default::default()
        };
        match resolve(&pool, &cand).await.unwrap() {
            Decision::Review { work_id, .. } => assert_eq!(work_id, existing),
            other => panic!("expected review (short exact title), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_self_matching_long_title_cannot_unlock_a_short_title_auto_merge() {
        // The reconcile path excludes the work being re-matched, because its own title is
        // one of its aliases. The length guard was measured over EVERY normalized title
        // that hit anything — including titles that hit only the excluded work itself. So
        // a provisional work with a distinctive primary title plus one generic alt title
        // passed the guard on the distinctive title (which linked it to nothing but
        // itself) while the actual cross-work link rested entirely on the 4-character
        // "love" — exactly the irreversible merge of two unrelated works the guard exists
        // to refuse.
        let pool = pool().await;
        let mine = catalog::create_work(
            &pool,
            &WorkInput {
                primary_title: Some("Distinctive Longtitle".into()),
                aliases: vec![
                    Alias {
                        raw: "Distinctive Longtitle".into(),
                        lang: None,
                    },
                    Alias {
                        raw: "Love".into(),
                        lang: None,
                    },
                ],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let other = catalog::create_work(
            &pool,
            &WorkInput {
                primary_title: Some("Love".into()),
                aliases: vec![Alias {
                    raw: "Love".into(),
                    lang: None,
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let cand = Candidate {
            title: "Distinctive Longtitle".into(),
            alt_titles: vec!["Love".into()],
            ..Default::default()
        };
        match resolve_ex(&pool, &cand, Some(&mine)).await.unwrap() {
            Decision::Review { work_id, .. } => assert_eq!(work_id, other),
            other => panic!("expected review (only a short title links the works), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn external_id_still_auto_merges_on_a_short_title() {
        // The length/ambiguity guards are specific to the TITLE rung — an external-ID
        // hit is an exact identity claim and must keep auto-merging regardless.
        let pool = pool().await;
        let existing = catalog::create_work(
            &pool,
            &WorkInput {
                primary_title: Some("Love".into()),
                aliases: vec![Alias {
                    raw: "Love".into(),
                    lang: None,
                }],
                external_ids: vec![("al".into(), "999".into())],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let cand = Candidate {
            title: "Love".into(),
            external_ids: vec![("al".into(), "999".into())],
            ..Default::default()
        };
        match resolve(&pool, &cand).await.unwrap() {
            Decision::AutoMerge {
                work_id, method, ..
            } => {
                assert_eq!(work_id, existing);
                assert_eq!(method, "external_id");
            }
            other => panic!("expected external-id auto-merge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn matches_via_localized_alt_title_across_extensions() {
        // S2: the cross-extension dedup payoff. A MangaDex spine entry stores ALL
        // its localized alt titles (here the romaji `ja-ro`). A series arriving
        // from another extension under only that localized title — with no shared
        // description or cover — must still resolve to the same work. Under the
        // exact-title and author policy this auto-merges (an exact alt-title alias
        // hit), consolidating the same series across extensions automatically.
        let pool = pool().await;
        let w = seed_slime(&pool).await; // primary "That Time..."; alt "Tensei Shitara Slime Datta Ken"
        let localized = Candidate {
            // A different extension only knows the romaji title.
            title: "Tensei Shitara Slime Datta Ken".into(),
            author: Some("Fuse".into()),
            ..Default::default()
        };
        match resolve(&pool, &localized).await.unwrap() {
            Decision::AutoMerge {
                work_id, method, ..
            } => {
                assert_eq!(work_id, w, "localized title resolves to the spine work");
                assert_eq!(
                    method, "title_exact",
                    "matched via the exact alt-title alias"
                );
            }
            other => panic!("expected auto-merge via alt title, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn author_corroboration_is_order_insensitive() {
        // D1: a federated hit that IS the existing MangaDex-anchored work — same
        // title, same author but with the name order reversed (the real Naruto
        // case: "Kishimoto Masashi" vs "Masashi Kishimoto"). The exact title auto-merges
        // it either way; this asserts the SCORING still corroborates order-insensitively
        // (author lifts the score above the bare-title MID floor), which drives the
        // fuzzy path for non-exact titles.
        let pool = pool().await;
        let existing = catalog::create_work(
            &pool,
            &WorkInput {
                primary_title: Some("Naruto".into()),
                author: Some("Kishimoto Masashi".into()),
                aliases: vec![Alias {
                    raw: "Naruto".into(),
                    lang: None,
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // Reversed author order + a genuinely different (source-specific) blurb.
        let cand = Candidate {
            title: "Naruto".into(),
            author: Some("Masashi Kishimoto".into()),
            description: Some("A ninja's tale from a different source.".into()),
            ..Default::default()
        };
        match resolve(&pool, &cand).await.unwrap() {
            Decision::AutoMerge { work_id, score, .. } => {
                assert_eq!(work_id, existing);
                assert!(
                    score > MID,
                    "author corroboration must lift the score above MID ({MID}); got {score}"
                );
            }
            other => panic!("expected an exact-title auto-merge above MID, got {other:?}"),
        }

        // Missing authors stay in review and score at MID.
        let bare = Candidate {
            title: "Naruto".into(),
            ..Default::default()
        };
        match resolve(&pool, &bare).await.unwrap() {
            Decision::Review { score, .. } => {
                assert!(
                    (score - MID).abs() < 1e-9,
                    "bare title-only must stay at MID; got {score}"
                );
            }
            other => panic!("expected bare-title review at MID, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unrelated_title_is_new() {
        let pool = pool().await;
        seed_slime(&pool).await;
        let cand = Candidate {
            title: "Berserk".into(),
            ..Default::default()
        };
        assert_eq!(resolve(&pool, &cand).await.unwrap(), Decision::New);
    }

    #[tokio::test]
    async fn a_split_pair_is_never_re_matched_to_each_other() {
        // The reconcile sweep's exact shape: a Suwayomi-only work re-matched against the
        // spine, hitting a work it shares a normalized title with. That rung auto-merges
        // with NO corroboration, and `merge_works` deletes the loser — so a work an admin
        // just split off would be destroyed by the next background pass.
        let pool = pool().await;
        let title = "Distinctive Longtitle";
        let mk = |t: &'static str| {
            let pool = pool.clone();
            async move {
                catalog::create_work(
                    &pool,
                    &WorkInput {
                        primary_title: Some(t.into()),
                        author: Some("Known Author".into()),
                        aliases: vec![Alias {
                            raw: t.into(),
                            lang: None,
                        }],
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
            }
        };
        let original = mk(title).await;
        let split_off = mk(title).await;
        let cand = Candidate {
            title: title.into(),
            author: Some("Known Author".into()),
            ..Default::default()
        };

        // Control: with no split on record this is exactly the destructive auto-merge.
        match resolve_ex(&pool, &cand, Some(&split_off)).await.unwrap() {
            Decision::AutoMerge { work_id, .. } => assert_eq!(work_id, original),
            other => {
                panic!("expected the author-corroborated exact-title auto-merge, got {other:?}")
            }
        }

        // What `catalog::split_source_series` leaves behind (migration 0098): the pair,
        // sorted, because the sweep chooses its own survivor.
        let (a, b) = if original < split_off {
            (&original, &split_off)
        } else {
            (&split_off, &original)
        };
        sqlx::query(
            r#"INSERT INTO work_split (work_a, work_b, split_at, split_by) VALUES ($1, $2, '2026-08-01T00:00:00Z', '575494de-4b35-4fe2-8f2d-da78fcf520c8')"#,
        )
        .bind(a)
        .bind(b)
        .execute(&pool)
        .await
        .unwrap();

        // Recorded: left alone entirely — NOT queued for review either, since the queue
        // asks a question an admin has already answered.
        assert_eq!(
            resolve_ex(&pool, &cand, Some(&split_off)).await.unwrap(),
            Decision::New
        );
        // Symmetric: the sweep may reach either half first.
        assert_eq!(
            resolve_ex(&pool, &cand, Some(&original)).await.unwrap(),
            Decision::New
        );
        // A third work with the same title is unaffected — the guard is per-PAIR, not a
        // blanket "this title is untouchable".
        let third = mk(title).await;
        assert!(matches!(
            resolve_ex(&pool, &cand, Some(&third)).await.unwrap(),
            Decision::AutoMerge { .. } | Decision::Review { .. }
        ));
    }
    /// `resolve_on` reads through the caller's transaction: it matches a work that
    /// transaction has minted and not yet committed, which the pool form cannot see,
    /// and it leaves the transaction open for the caller to finish.
    #[tokio::test]
    async fn resolve_on_reads_inside_the_callers_transaction() {
        let pool = pool().await;
        let input = WorkInput {
            primary_title: Some("Quiet Harbour".into()),
            author: Some("Mori".into()),
            aliases: vec![Alias {
                raw: "Quiet Harbour".into(),
                lang: None,
            }],
            external_ids: vec![("al".into(), "4242".into())],
            ..Default::default()
        };
        let by_title = Candidate {
            title: "The Quiet Harbour".into(),
            author: Some("Mori".into()),
            ..Default::default()
        };
        let by_id = Candidate {
            title: "Unrelated".into(),
            external_ids: vec![("al".into(), "4242".into())],
            ..Default::default()
        };
        let mut tx = pool.begin().await.unwrap();
        let mut keys = match_domain_keys(&by_title);
        keys.extend(match_domain_keys(&by_id));
        crate::locks::advisory(&mut tx, &keys).await.unwrap();
        assert_eq!(
            resolve_on(&mut tx, &by_title, None).await.unwrap(),
            Decision::New
        );
        let minted = catalog::create_work_on(&mut tx, &input).await.unwrap();
        match resolve_on(&mut tx, &by_title, None).await.unwrap() {
            Decision::AutoMerge {
                work_id, method, ..
            } => {
                assert_eq!(work_id, minted);
                assert_eq!(method, "title_exact");
            }
            other => panic!("expected an exact-title match, got {other:?}"),
        }
        match resolve_on(&mut tx, &by_id, None).await.unwrap() {
            Decision::AutoMerge {
                work_id, method, ..
            } => {
                assert_eq!(work_id, minted);
                assert_eq!(method, "external_id");
            }
            other => panic!("expected an external-id match, got {other:?}"),
        }
        // Excluding the work itself is honoured on the caller's connection too.
        assert_eq!(
            resolve_on(&mut tx, &by_id, Some(&minted)).await.unwrap(),
            Decision::New
        );
        // The pool form cannot see the uncommitted work.
        assert_eq!(resolve(&pool, &by_title).await.unwrap(), Decision::New);
        tx.rollback().await.unwrap();
        assert_eq!(resolve(&pool, &by_id).await.unwrap(), Decision::New);
    }

    /// The match domain is the article-insensitive title plus every external id.
    #[test]
    fn match_domain_keys_cover_article_variants_and_external_ids() {
        use crate::locks::{AdvisoryKey, KeyClass};
        let with_article = match_domain_keys(&Candidate {
            title: "The Quiet Harbour".into(),
            alt_titles: vec!["Shizuka na Minato".into()],
            external_ids: vec![("al".into(), "4242".into()), (String::new(), "x".into())],
            ..Default::default()
        });
        let bare = match_domain_keys(&Candidate {
            title: "Quiet Harbour".into(),
            ..Default::default()
        });
        let other = match_domain_keys(&Candidate {
            title: "Loud Harbour".into(),
            external_ids: vec![("al".into(), "4243".into())],
            ..Default::default()
        });
        // Two title keys and one external id; the empty provider is skipped.
        assert_eq!(with_article.len(), 3);
        assert_eq!(bare.len(), 1);
        assert!(with_article.contains(&bare[0]));
        assert!(with_article.contains(&AdvisoryKey::external_identity("al", "4242")));
        let title_key = match_key(&normalize_title("Quiet Harbour"));
        assert_eq!(
            bare[0],
            AdvisoryKey::new(KeyClass::MatchDomain, &[&title_key])
        );
        assert!(!other.iter().any(|key| with_article.contains(key)));
        assert!(match_domain_keys(&Candidate::default()).is_empty());
    }
}
