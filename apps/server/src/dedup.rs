//! The dedup matcher (CATALOGUE.md §4).
//!
//! Resolves a source-series to a canonical `work` via a precision ladder:
//!   1. external-ID exact          -> auto-merge (highest precision, stop on hit)
//!   2. normalized-title exact      -> candidate
//!   3. fuzzy title (token block)   -> candidate shortlist
//!   4. corroborate (description / cover pHash / author / year) -> confidence score
//!   5. decide: external-ID OR unambiguous exact-title -> auto-merge; fuzzy high
//!      (cover-corroborated) -> auto-merge; mid -> manual review queue; low -> new work
//!
//! Runs at add-time for Tier-2 series, where a human reviews the mid band. Operator
//! policy: an EXACT normalized-title match auto-merges (identical titles = same work)
//! PROVIDED the title identifies exactly one work and is not a short common phrase
//! (`EXACT_MERGE_MIN_TITLE_CHARS`); otherwise it drops to `Review`. A FUZZY
//! (token-block) match reaches `AutoMerge` only when cover-corroborated at HIGH, else it
//! lands in `Review` — so a shared common token can't silently merge distinct works
//! (prequels/sequels/seasons carry distinct titles and stay separate).

use std::collections::HashSet;

use anyhow::Result;
use sqlx::SqlitePool;

use crate::catalog::{
    self,
    normalize::normalize_title,
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
/// goes to the manual review queue; below is a new work. (An EXACT normalized-title
/// match bypasses these bands and auto-merges outright — see `resolve`.)
pub const HIGH: f64 = 0.85;
pub const MID: f64 = 0.6;
const FUZZY_BLOCK_LIMIT: i64 = 50;
/// Minimum normalized-title length for an EXACT-title auto-merge. Mirrors
/// `graphql::FEDERATED_MIN_TITLE_CHARS`, which guards the federated consolidation
/// path for exactly this reason: below it a title is a common phrase rather than an
/// identity. Live catalogue evidence — "first love" resolves to 18 distinct works,
/// "love letter" 13, "change" 12, "confession" 12, and 1,624 works carry a colliding
/// normalized title under 5 characters. Auto-merging on one of those physically
/// DELETES an unrelated work, so they go to the review queue instead.
const EXACT_MERGE_MIN_TITLE_CHARS: usize = 5;
/// How many of the longest title tokens the fuzzy block keys on (DD4). Keying on
/// only the single longest token missed a merge whenever the discriminating token
/// wasn't the longest; unioning the top-N candidate sets closes that recall gap.
const FUZZY_BLOCK_TOKENS: usize = 3;
/// Minimum cover-pHash similarity that counts as corroboration for a title-driven
/// auto-merge (DD3). A shared normalized title plus mere description overlap must
/// not auto-merge on its own — without a corroborating cover it lands in Review.
/// Set to 0.90 on the 64-bit dHash → at most ~6 differing bits, tight enough that
/// two genuinely different covers don't corroborate a merge (was 0.8 = up to 12
/// bits, which let flat/letterboxed covers false-merge distinct series).
const PHASH_CORROBORATION: f64 = 0.90;
/// A near-uniform dHash (almost-all-0 or almost-all-1 bits) carries little signal —
/// flat/letterboxed/dark covers collapse to such hashes, so two DIFFERENT series
/// can share one. Only accept a cover as corroborating when its own hash's popcount
/// sits away from both extremes (12..=52 of 64 bits).
fn is_discriminative_phash(hex: &str) -> bool {
    let Ok(bytes) = hex::decode(hex) else {
        return false;
    };
    if bytes.len() != 8 {
        return false;
    }
    let ones: u32 = bytes.iter().map(|b| b.count_ones()).sum();
    (12..=52).contains(&ones)
}

/// Resolve `cand` against the canonical works in `pool`.
pub async fn resolve(pool: &SqlitePool, cand: &Candidate) -> Result<Decision> {
    resolve_ex(pool, cand, None).await
}

/// As [`resolve`], but excludes `exclude_work_id` from the candidate set. The
/// catalogue reconcile re-matches an existing (provisional) work against the spine,
/// where the work's own title is one of its aliases — without excluding it, the work
/// would always "match itself". `None` is the normal add-time path.
pub async fn resolve_ex(
    pool: &SqlitePool,
    cand: &Candidate,
    exclude_work_id: Option<&str>,
) -> Result<Decision> {
    // 1. External-ID exact — highest precision, stop on hit.
    for (provider, ext) in &cand.external_ids {
        if provider.is_empty() || ext.is_empty() {
            continue;
        }
        if let Some(work_id) = catalog::find_work_by_external(pool, provider, ext).await? {
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

    // 2. Normalized-title exact -> candidate ids. `exact_norm_hits` records WHICH
    //    normalized titles hit, so the auto-merge guard below can reject a hit that
    //    rests only on a too-short (common-phrase) title.
    let mut candidate_ids: HashSet<String> = HashSet::new();
    let mut exact_hit = false;
    let mut exact_norm_hits: Vec<&str> = Vec::new();
    for nt in &norm_titles {
        let ids = catalog::find_works_by_alias(pool, nt).await?;
        // Only a title that matched some work OTHER than the one being reconciled is
        // evidence of a duplicate. Counting a self-match here let a distinctive title
        // that hit ONLY the work itself satisfy the length guard below, while the actual
        // link to the other work rested entirely on a short common phrase — precisely the
        // auto-merge the guard exists to refuse. (`exact_hit` is corrected for the same
        // reason a few lines down, once the excluded id has been removed from the set.)
        if ids.iter().any(|id| Some(id.as_str()) != exclude_work_id) {
            exact_hit = true;
            exact_norm_hits.push(nt.as_str());
        }
        candidate_ids.extend(ids);
    }
    // Drop the work being reconciled (it matches its own aliases). If that leaves no
    // exact candidate, there was no OTHER exact match → clear exact_hit so the fuzzy
    // fallback still runs and a title-only self-match can't force an auto-merge.
    if let Some(ex) = exclude_work_id {
        candidate_ids.remove(ex);
        if candidate_ids.is_empty() {
            exact_hit = false;
        }
    }
    // How many DISTINCT works the exact-title lookup returned. Captured here because
    // `candidate_ids` is refilled from the fuzzy block below when it is empty; more
    // than one means the title does not identify a single work.
    let exact_candidates = candidate_ids.len();

    // 3. Fuzzy blocking when no exact hit: block on the top-N longest title tokens
    //    (union), so the discriminating token being shorter than a generic one
    //    doesn't drop the real match (DD4).
    if candidate_ids.is_empty() {
        for token in top_tokens(&norm_titles, FUZZY_BLOCK_TOKENS) {
            let ids = catalog::candidate_work_ids_by_token(pool, &token, FUZZY_BLOCK_LIMIT).await?;
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
    let match_data = catalog::load_match_data_batch(pool, &candidate_id_list).await?;
    // Precompute the CANDIDATE's shingle sets once — they're compared against every
    // candidate work's aliases (works carry 20+ localized aliases) and description, so
    // shingling them per comparison inside the loop was the reconcile CPU hot spot (OPT-D).
    let cand_title_shingles: Vec<TitleShingles> =
        norm_titles.iter().map(|t| title_shingles(t)).collect();
    let cand_desc_shingles: Option<DescShingles> =
        cand.description.as_deref().map(description_shingles);
    let mut best: Option<(Scored, String)> = None;
    for wid in &candidate_ids {
        let Some(md) = match_data.get(wid) else {
            continue;
        };
        let scored = score_candidate(cand, &cand_title_shingles, cand_desc_shingles.as_ref(), md);
        let replace = match best.as_ref() {
            None => true,
            Some((s, w)) => scored.score > s.score || (scored.score == s.score && wid < w),
        };
        if replace {
            best = Some((scored, wid.clone()));
        }
    }
    let Some((scored, work_id)) = best else {
        return Ok(Decision::New);
    };
    let score = scored.score;

    // 5. Decide. Operator policy (explicit): an UNAMBIGUOUS exact normalized-title
    //    match auto-merges outright — identical titles are treated as the same work
    //    (this is what consolidates the existing Suwayomi catalogue into the MangaDex
    //    spine on reconcile). A FUZZY (token-block) match is riskier — a shared common
    //    token can span distinct works — so it still auto-merges ONLY with cover
    //    corroboration at HIGH, otherwise it lands in Review. (Prequels/sequels/seasons
    //    carry distinct titles, so they don't exact-match and stay separate / queued.)
    // `method` labels align to the documented `merge_candidate.method` enum
    // (migration 0005): external_id / title_exact / fuzzy / description / cover. The
    // scored path emits title_exact (exact alias hit) or fuzzy (token-block hit); the
    // finer description/cover distinction isn't surfaced separately (DD6).
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
    // The exact-title rung auto-merges only when the title actually IDENTIFIES one
    // work. Two guards, both of which the federated path already applies
    // (`graphql::federated_consolidate_ok`) and this rung previously skipped entirely:
    //   * exactly one candidate — `find_works_by_alias` routinely returns many ids for
    //     a common phrase, and the tie-break above then picks the lowest work_id, i.e.
    //     an arbitrary one of 18 unrelated "first love" works;
    //   * a normalized title of at least `EXACT_MERGE_MIN_TITLE_CHARS`.
    // Neither can be rescued by cover corroboration in production, where
    // `COVER_PHASH=off` leaves `phash_sim` permanently `None`. A hit that fails either
    // guard is DOWNGRADED to `Review` (an exact hit always scores >= MID) rather than
    // irreversibly merged — a human resolves it from the queue.
    let exact_title_long_enough = exact_norm_hits
        .iter()
        .any(|t| t.chars().count() >= EXACT_MERGE_MIN_TITLE_CHARS);
    let exact_auto_merge = exact_hit && exact_candidates == 1 && exact_title_long_enough;
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

/// Order-insensitive author-name key: lowercased word tokens, sorted (D1). Folds
/// "Masashi Kishimoto" and "Kishimoto Masashi" to the same key so a cross-source
/// name-order variance doesn't defeat author corroboration.
fn author_key(name: &str) -> Vec<String> {
    let mut toks: Vec<String> = name
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    toks.sort();
    toks
}

fn author_matches(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            let ka = author_key(a);
            // Non-empty and equal as a sorted token multiset (order-insensitive).
            !ka.is_empty() && ka == author_key(b)
        }
        _ => false,
    }
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

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    async fn seed_slime(pool: &SqlitePool) -> String {
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
    async fn exact_title_only_auto_merges() {
        let pool = pool().await;
        let w = seed_slime(&pool).await;
        // Same alt-title, no description/cover — an exact normalized-title hit, which
        // now auto-merges outright under the exact-title policy.
        let cand = Candidate {
            title: "Tensei Shitara Slime Datta Ken".into(),
            ..Default::default()
        };
        match resolve(&pool, &cand).await.unwrap() {
            Decision::AutoMerge { work_id, .. } => assert_eq!(work_id, w),
            other => panic!("expected auto-merge (exact title), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exact_title_auto_merges_without_cover() {
        // Operator policy: an EXACT normalized-title match auto-merges outright, even
        // without a corroborating cover — identical titles are treated as the same work
        // (this is what folds the Suwayomi catalogue into the MangaDex spine on
        // reconcile). Distinct works with a genuinely identical title are the accepted
        // trade-off of this policy; near-matches (sequels/seasons) carry distinct titles
        // and take the fuzzy path instead.
        let pool = pool().await;
        let w = seed_slime(&pool).await;
        let cand = Candidate {
            title: "Tensei Shitara Slime Datta Ken".into(),
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
        // exact-title policy this now auto-merges outright (an exact alt-title alias
        // hit), consolidating the same series across extensions automatically.
        let pool = pool().await;
        let w = seed_slime(&pool).await; // primary "That Time..."; alt "Tensei Shitara Slime Datta Ken"
        let localized = Candidate {
            // A different extension only knows the romaji title.
            title: "Tensei Shitara Slime Datta Ken".into(),
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

        // A bare title-only hit with NO author still SCORES exactly MID (the C2 guard's
        // floor) — corroboration genuinely absent, no false lift — though the exact
        // title still auto-merges it.
        let bare = Candidate {
            title: "Naruto".into(),
            ..Default::default()
        };
        match resolve(&pool, &bare).await.unwrap() {
            Decision::AutoMerge { score, .. } => {
                assert!(
                    (score - MID).abs() < 1e-9,
                    "bare title-only must stay at MID; got {score}"
                );
            }
            other => panic!("expected bare-title auto-merge at MID, got {other:?}"),
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
}
