//! Is this pair of works one series? The single rule set behind the duplicate sweep
//! and the review queue.
//!
//! [`assess`] sorts a pair three ways:
//!
//!   * `Merge`    — one series; fold the pair (`merge_works` deletes the loser);
//!   * `Distinct` — two series; never offered to a human, never merged;
//!   * `Review`   — a real question; kept in the review queue, ranked by `score`.
//!
//! A shared title is never enough on its own. What else is required depends on who the
//! two works are:
//!
//!   * Two MangaDex entries are separate works by construction and are always
//!     `Distinct`, even under one title and one author (a mature and an erotic
//!     version of one series are two entries).
//!   * Two titles where one names an edition the other does not (colored, long strip,
//!     promo, side story) are `Distinct`.
//!   * A site's copy of a series is usually a duplicate of the entry that carries its
//!     name. It merges on a shared credit, description, cover or native-script title —
//!     or on the title alone when that title is long enough to be specific.
//!
//! The caller owns one more rule this module cannot see: a merge needs exactly one
//! qualifying target.

use super::normalize::{
    edition_mismatch, match_key, normalize_title, novel_mismatch, number_mismatch,
};
use super::similarity::{
    credits_match, description_similarity, phash_similarity, title_shingles, title_similarity_pre,
};
use super::WorkMatchData;

/// Minimum cover-pHash similarity that corroborates a merge: at most ~6 differing bits
/// of the 64-bit dHash.
pub const PHASH_CORROBORATION: f64 = 0.90;
/// Description overlap (shingle Jaccard) that corroborates a title match. Sites copy a
/// series' blurb near-verbatim, so two different series do not reach it.
const DESCRIPTION_MATCH: f64 = 0.5;
/// Description overlap that carries a merge without any exact title.
const DESCRIPTION_STRONG: f64 = 0.8;
/// A title shorter than this names too many series to match on without a credit.
const MIN_TITLE_CHARS: usize = 5;
/// A native-script title shorter than this is too common to be evidence.
const MIN_NATIVE_CHARS: usize = 4;

/// A near-uniform dHash (almost-all-0 or almost-all-1 bits) carries no signal: flat,
/// letterboxed and placeholder covers all collapse to one, so two unrelated works score
/// 1.00 against each other. Only a hash whose popcount sits away from both extremes
/// (12..=52 of 64 bits) may corroborate anything.
pub fn is_discriminative_phash(hex: &str) -> bool {
    let Ok(bytes) = hex::decode(hex) else {
        return false;
    };
    if bytes.len() != 8 {
        return false;
    }
    let ones: u32 = bytes.iter().map(|b| b.count_ones()).sum();
    (12..=52).contains(&ones)
}

/// How the two works' titles meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// Both works carry the same primary title.
    Exact,
    /// One work's primary title is an alternative title of the other.
    Alias,
    /// They only share an alternative title: a franchise, not a name.
    Shared,
    /// No shared title; the primary titles are merely similar.
    Fuzzy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Merge,
    Distinct,
    Review,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Assessment {
    pub verdict: Verdict,
    pub link: Link,
    /// Short machine-friendly label for logs.
    pub reason: &'static str,
    /// Confidence in [0,1] that the pair is one series; orders the review queue.
    pub score: f64,
    /// The duplicate key (`work_alias.match_key`) an `Exact` or `Alias` link rests on.
    pub key: Option<String>,
}

/// True when the article-insensitive title `key` is long enough to name one series:
/// "king of violence" does, "first love" and "ours" do not.
fn is_specific(key: &str) -> bool {
    let tokens = key.split_whitespace().count();
    tokens >= 4 || (tokens >= 3 && key.chars().count() >= 16)
}

/// True for a title written in kana, hangul or han — the script a series is first
/// published in. Such a title is far more specific than its English rendering.
fn is_native(title: &str) -> bool {
    let native = |c: char| {
        matches!(c as u32,
            0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7A3)
    };
    title.chars().any(native)
        && title.chars().filter(|c| !c.is_whitespace()).count() >= MIN_NATIVE_CHARS
}

/// The language a work was first published in, without its region ("zh-hk" -> "zh").
fn base_language(lang: Option<&str>) -> Option<&str> {
    lang.map(|l| l.split('-').next().unwrap_or(l))
        .filter(|l| !l.is_empty())
}

/// Sort a pair of works. See the module docs for the rules.
pub fn assess(a: &WorkMatchData, b: &WorkMatchData) -> Assessment {
    let (title_a, title_b) = (a.primary_title.as_deref(), b.primary_title.as_deref());
    let norm = |t: Option<&str>| t.map(normalize_title).filter(|n| !n.is_empty());
    let (norm_a, norm_b) = (norm(title_a), norm(title_b));
    let (key_a, key_b) = (
        norm_a.as_deref().map(match_key),
        norm_b.as_deref().map(match_key),
    );
    let keys = |w: &WorkMatchData, primary: &Option<String>| -> Vec<String> {
        let mut keys: Vec<String> = w.aliases_norm.iter().map(|n| match_key(n)).collect();
        keys.extend(primary.clone());
        keys
    };
    let (keys_a, keys_b) = (keys(a, &key_a), keys(b, &key_b));

    // The title link, strongest first.
    let same_title = norm_a.is_some() && norm_a == norm_b;
    let (link, key) = if key_a.is_some() && key_a == key_b {
        (Link::Exact, key_a.clone())
    } else if key_a.as_ref().is_some_and(|k| keys_b.contains(k)) {
        (Link::Alias, key_a.clone())
    } else if key_b.as_ref().is_some_and(|k| keys_a.contains(k)) {
        (Link::Alias, key_b.clone())
    } else if keys_a.iter().any(|k| keys_b.contains(k)) {
        (Link::Shared, None)
    } else {
        (Link::Fuzzy, None)
    };
    // How alike the primary titles are when nothing links them: each primary against
    // every title of the other work. Alt titles are never compared with alt titles —
    // that is the franchise overlap `Link::Shared` already names.
    let title_sim = match link {
        Link::Exact => 1.0,
        Link::Alias => 0.9,
        Link::Shared => 0.6,
        Link::Fuzzy => {
            let against = |primary: &Option<String>, other: &WorkMatchData| {
                let Some(primary) = primary else { return 0.0 };
                let primary = title_shingles(primary);
                other
                    .aliases_norm
                    .iter()
                    .map(|al| title_similarity_pre(&primary, &title_shingles(al)))
                    .fold(0.0_f64, f64::max)
            };
            against(&norm_a, b).max(against(&norm_b, a))
        }
    };

    // Evidence beyond the title.
    let author = credits_match(
        [a.author.as_deref(), a.artist.as_deref()],
        [b.author.as_deref(), b.artist.as_deref()],
    );
    let description = match (a.description.as_deref(), b.description.as_deref()) {
        (Some(x), Some(y)) => description_similarity(x, y),
        _ => 0.0,
    };
    let cover_sim = match (a.cover_phash.as_deref(), b.cover_phash.as_deref()) {
        (Some(x), Some(y)) if is_discriminative_phash(x) && is_discriminative_phash(y) => {
            phash_similarity(Some(x), Some(y)).unwrap_or(0.0)
        }
        _ => 0.0,
    };
    let cover = cover_sim >= PHASH_CORROBORATION;
    let native = a
        .aliases_norm
        .iter()
        .any(|al| is_native(al) && b.aliases_norm.contains(al));
    let year = matches!((a.year, b.year), (Some(x), Some(y)) if (x - y).abs() <= 1);
    let language_conflict = match (
        base_language(a.original_language.as_deref()),
        base_language(b.original_language.as_deref()),
    ) {
        (Some(x), Some(y)) => x != y,
        _ => false,
    };
    let short = key
        .as_deref()
        .is_some_and(|k| k.chars().count() < MIN_TITLE_CHARS);
    let specific = key.as_deref().is_some_and(is_specific);

    let corroboration = [
        description,
        cover_sim,
        if author { 0.8 } else { 0.0 },
        if native { 0.6 } else { 0.0 },
        if year { 0.3 } else { 0.0 },
    ]
    .into_iter()
    .fold(0.0_f64, f64::max);
    let score = (0.6 * title_sim + 0.4 * corroboration).min(0.99);
    let verdict = |verdict, reason| Assessment {
        verdict,
        link,
        reason,
        score,
        key: key.clone(),
    };
    use Verdict::{Distinct, Merge, Review};

    // A novel and its comic adaptation share a name, alt titles and an author.
    if novel_mismatch(title_a, title_b) {
        return verdict(Distinct, "novel_mismatch");
    }

    // Two MangaDex entries are never one work. MangaDex lists a sequel, a colored
    // edition, a promo, and a mature and an erotic version of one series as separate
    // entries, often under one name and by one author; folding any pair of them mixes
    // two works' metadata and covers. `merge_works_on` refuses the merge as well.
    if a.mangadex_anchored && b.mangadex_anchored {
        return verdict(Distinct, "separate_mangadex_entries");
    }
    // One title names an edition the other does not (colored, long strip, promo, side
    // story): related, but never one work, and not a question worth asking either.
    if edition_mismatch(title_a, title_b) {
        return verdict(Distinct, "edition_mismatch");
    }

    // At least one side is a site's copy.
    if language_conflict {
        return if author {
            verdict(Review, "language_mismatch")
        } else {
            verdict(Distinct, "language_mismatch")
        };
    }
    if short && !author {
        return verdict(Review, "short_title");
    }
    match link {
        Link::Exact => {
            // A year only corroborates the very same title, never an article variant.
            if author || cover || description >= DESCRIPTION_MATCH || native {
                verdict(Merge, "same_title_and_evidence")
            } else if same_title && year {
                verdict(Merge, "same_title_and_year")
            } else if specific {
                verdict(Merge, "specific_title")
            } else {
                verdict(Review, "uncorroborated")
            }
        }
        Link::Alias => {
            if author || cover || description >= DESCRIPTION_MATCH || native {
                verdict(Merge, "alt_title_and_evidence")
            } else if specific {
                verdict(Merge, "specific_alt_title")
            } else {
                verdict(Review, "uncorroborated")
            }
        }
        Link::Shared => verdict(Review, "alt_title_only"),
        Link::Fuzzy => {
            let long = |n: &Option<String>| {
                n.as_deref()
                    .is_some_and(|n| n.split_whitespace().count() >= 5)
            };
            if number_mismatch(title_a, title_b) {
                verdict(Review, "number_mismatch")
            } else if (author && description >= DESCRIPTION_MATCH)
                || description >= DESCRIPTION_STRONG
            {
                verdict(Merge, "same_description")
            } else if (author && title_sim >= 0.75) || (cover && title_sim >= 0.6) {
                verdict(Merge, "similar_title_and_evidence")
            } else if title_sim >= 0.85 && long(&norm_a) && long(&norm_b) {
                verdict(Merge, "near_identical_title")
            } else {
                verdict(Review, "similar_title")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A site's copy of a series: one title, optional author.
    fn site(title: &str, author: Option<&str>) -> WorkMatchData {
        WorkMatchData {
            work_id: title.into(),
            primary_title: Some(title.into()),
            author: author.map(str::to_string),
            aliases_norm: vec![normalize_title(title)],
            ..Default::default()
        }
    }

    /// A MangaDex entry: primary title, alt titles, author, year, language.
    fn md(titles: &[&str], author: Option<&str>, year: Option<i64>, lang: &str) -> WorkMatchData {
        WorkMatchData {
            work_id: titles[0].into(),
            primary_title: Some(titles[0].into()),
            author: author.map(str::to_string),
            year,
            original_language: Some(lang.into()),
            mangadex_anchored: true,
            aliases_norm: titles.iter().map(|t| normalize_title(t)).collect(),
            ..Default::default()
        }
    }

    fn verdict(a: &WorkMatchData, b: &WorkMatchData) -> (Verdict, &'static str) {
        let out = assess(a, b);
        // The verdict never depends on which work is listed first.
        assert_eq!(out.verdict, assess(b, a).verdict, "asymmetric: {out:?}");
        (out.verdict, out.reason)
    }

    /// Two MangaDex entries are never one work: not under one title and one credit
    /// (a mature and an erotic version), not across an edition, a sequel or a language.
    #[test]
    fn two_mangadex_entries_are_never_merged() {
        let author = Some("Fujimi Rio");
        let gege = Some("Akutami Gege");
        let pairs = [
            (
                md(&["Pretty Rhythm - Aurora Dream"], author, Some(2011), "ja"),
                md(&["Pretty Rhythm: Aurora Dream"], author, None, "ja"),
            ),
            (
                md(
                    &["Deriheru Yondara Moto Doukyuusei ga Kita"],
                    author,
                    Some(2020),
                    "ja",
                ),
                md(
                    &["Deriheru Yondara Moto Doukyuusei ga Kita"],
                    author,
                    Some(2020),
                    "ja",
                ),
            ),
            (
                md(&["Arslan Senki"], Some("Arakawa Hiromu"), Some(2013), "ja"),
                md(&["Arslan Senki"], Some("Tanaka Yoshiki"), Some(1991), "ja"),
            ),
            (
                md(&["Rosario to Vampire"], author, Some(2004), "ja"),
                md(&["Rosario to Vampire Season II"], author, Some(2007), "ja"),
            ),
            (
                md(&["Jujutsu Kaisen"], gege, Some(2018), "ja"),
                md(
                    &["Jujutsu Kaisen (Official Colored)"],
                    gege,
                    Some(2020),
                    "ja",
                ),
            ),
            (
                md(&["Naruto"], Some("Masashi Kishimoto"), None, "ja"),
                md(&["Naruto"], None, None, "ja"),
            ),
        ];
        for (a, b) in &pairs {
            assert_eq!(
                verdict(a, b),
                (Verdict::Distinct, "separate_mangadex_entries")
            );
        }
        // Not even when the blurb and the cover are the same.
        let (mut a, mut b) = pairs[1].clone();
        a.description = Some("The same blurb on both versions of the series".into());
        b.description = a.description.clone();
        a.cover_phash = Some("ff00ff00ff00ff00".into());
        b.cover_phash = a.cover_phash.clone();
        assert_eq!(verdict(&a, &b).0, Verdict::Distinct);
    }

    /// The pairs that flooded the review queue: two spine entries that only share an
    /// alt title, or differ by an article. They are separate works, not questions.
    #[test]
    fn mangadex_entries_that_share_only_an_alt_title_are_distinct() {
        let araki = Some("Hirohiko Araki");
        let franchise = "JoJo's Bizarre Adventure";
        assert_eq!(
            verdict(
                &md(&["Phantom Blood", franchise], araki, Some(1987), "ja"),
                &md(&["Battle Tendency", franchise], araki, Some(1987), "ja"),
            ),
            (Verdict::Distinct, "separate_mangadex_entries")
        );
        assert_eq!(
            verdict(
                &md(&["Grave"], Some("Tsurita Kuniko"), Some(1969), "ja"),
                &md(&["Haka", "Grave"], Some("Kashimiharu"), Some(2019), "ja"),
            ),
            (Verdict::Distinct, "separate_mangadex_entries")
        );
        assert_eq!(
            verdict(
                &md(&["Beach Stars"], Some("Morio Masahiro"), Some(2007), "ja"),
                &md(
                    &["The!! Beach Stars"],
                    Some("Morio Masahiro"),
                    Some(2009),
                    "ja"
                ),
            ),
            (Verdict::Distinct, "separate_mangadex_entries")
        );
    }

    #[test]
    fn a_site_copy_merges_into_the_entry_that_carries_its_name() {
        let spine = md(
            &["Kusuriya no Hitorigoto", "The Apothecary Diaries"],
            Some("Hyuuga Natsu"),
            Some(2017),
            "ja",
        );
        // Named by the alt title, credited with a longer author list.
        assert_eq!(
            verdict(
                &spine,
                &site("Apothecary Diaries", Some("Hyuuga Natsu, Nanao Itsuki"))
            ),
            (Verdict::Merge, "alt_title_and_evidence")
        );
        // No credit and a two-word name: not enough.
        assert_eq!(
            verdict(&spine, &site("The Apothecary Diaries", None)),
            (Verdict::Review, "uncorroborated")
        );
        // No credit, but a name long enough to mean one series.
        assert_eq!(
            verdict(
                &md(
                    &["Pongnyeok ui Wang", "King of Violence"],
                    Some("LICO"),
                    Some(2024),
                    "ko"
                ),
                &site("King of Violence", None),
            ),
            (Verdict::Merge, "specific_alt_title")
        );
        // Two sites, one drops the article.
        assert_eq!(
            verdict(
                &site("The Invincible Escort of the North", None),
                &site("Invincible Escort of the North", None),
            ),
            (Verdict::Merge, "specific_title")
        );
        assert_eq!(
            verdict(&site("The Mad Dog", None), &site("Mad Dog", None)),
            (Verdict::Review, "uncorroborated")
        );
        // The writer on one side, the illustrator on the other.
        let mut both = md(&["Game Choegang Troller"], Some("Fried-Mandu"), None, "ko");
        both.artist = Some("Park Jong Suk".into());
        both.aliases_norm
            .push(normalize_title("World's Strongest Troll"));
        assert_eq!(
            verdict(
                &both,
                &site("World's Strongest Troll", Some("Park Jong Suk"))
            ),
            (Verdict::Merge, "alt_title_and_evidence")
        );
    }

    #[test]
    fn a_site_copy_is_never_merged_on_a_weak_or_contradicted_title() {
        // Too short to mean anything without a credit.
        assert_eq!(
            verdict(&site("Ao", None), &site("Ao", None)),
            (Verdict::Review, "short_title")
        );
        assert_eq!(
            verdict(&site("Ao", Some("A B")), &site("Ao", Some("B A"))).0,
            Verdict::Merge
        );
        // A novel next to its comic.
        assert_eq!(
            verdict(
                &site("Solo Leveling", Some("Chugong")),
                &site("Solo Leveling (Novel)", Some("Chugong")),
            ),
            (Verdict::Distinct, "novel_mismatch")
        );
        // A promo or pre-serialization is not the series.
        let daul = Some("DAUL");
        assert_eq!(
            verdict(
                &site("Solo Leveling: Ragnarok", daul),
                &md(
                    &[
                        "Solo Leveling: Ragnarok (Pre-serialization)",
                        "Solo Leveling: Ragnarok"
                    ],
                    daul,
                    Some(2024),
                    "ko"
                ),
            ),
            (Verdict::Distinct, "edition_mismatch")
        );
        // A colored or long-strip release is not offered as a duplicate of the series.
        for edition in [
            "Killing Stalking (Fan Colored)",
            "Killing Stalking (Official Colored)",
            "Killing Stalking [Long Strip]",
            "Killing Stalking - Webtoon",
        ] {
            assert_eq!(
                verdict(
                    &site("Killing Stalking", Some("Koogi")),
                    &md(
                        &[edition, "Killing Stalking"],
                        Some("Koogi"),
                        Some(2016),
                        "ko"
                    ),
                ),
                (Verdict::Distinct, "edition_mismatch")
            );
        }
        // Only an alt title in common, even with the franchise's author.
        let araki = Some("Hirohiko Araki");
        let mut part = site("Battle Tendency", araki);
        part.aliases_norm
            .push(normalize_title("JoJo's Bizarre Adventure"));
        assert_eq!(
            verdict(
                &md(
                    &["Phantom Blood", "JoJo's Bizarre Adventure"],
                    araki,
                    Some(1987),
                    "ja"
                ),
                &part,
            ),
            (Verdict::Review, "alt_title_only")
        );
        // Article variants are not corroborated by a year.
        let mut early = site("The Robot Emperor", None);
        let mut late = site("Robot Emperor", None);
        (early.year, late.year) = (Some(1958), Some(1959));
        assert_eq!(verdict(&early, &late), (Verdict::Review, "uncorroborated"));
        // The very same title is.
        let mut twin = site("The Robot Emperor", None);
        twin.year = Some(1959);
        assert_eq!(
            verdict(&early, &twin),
            (Verdict::Merge, "same_title_and_year")
        );
    }

    #[test]
    fn similar_titles_merge_only_on_evidence() {
        let blurb = "A regressor returns to the tutorial tower and climbs it again with \
                     the knowledge of every floor he cleared in his first life.";
        let mut translated = site(
            "The Tutorial Tower of the Advanced Player",
            Some("Bangguseok Gimssi"),
        );
        let mut original = md(
            &["The Advanced Player of the Tutorial Tower"],
            Some("Bangguseok Gimssi (방구석김씨)"),
            Some(2020),
            "ko",
        );
        assert_eq!(
            verdict(&translated, &original),
            (Verdict::Merge, "similar_title_and_evidence")
        );
        // The same blurb carries a pair whose titles are in different languages.
        translated.primary_title = Some("Battle of the Six Realms".into());
        translated.aliases_norm = vec![normalize_title("Battle of the Six Realms")];
        original.primary_title = Some("Rokudou Tousouki".into());
        original.aliases_norm = vec![normalize_title("Rokudou Tousouki")];
        translated.description = Some(blurb.into());
        original.description = Some(blurb.into());
        assert_eq!(
            verdict(&translated, &original),
            (Verdict::Merge, "same_description")
        );
        // A sequel number is not a typo.
        assert_eq!(
            verdict(
                &site("My Stepmom Made Me Her Toy", Some("RK-2")),
                &site("My Stepmom Made Me Her Toy 2", Some("RK-2")),
            ),
            (Verdict::Review, "number_mismatch")
        );
        // Two renderings of one long title.
        assert_eq!(
            verdict(
                &site("I Thought It Was Just an Ordinary Potion", None),
                &site("I Thought It Was an Ordinary Potion", None),
            ),
            (Verdict::Merge, "near_identical_title")
        );
        // Similar, short, and nothing else: a question.
        assert_eq!(
            verdict(
                &site("Rebirth of the Super Battle God", None),
                &site("Rebirth of the Ultimate War God", None),
            ),
            (Verdict::Review, "similar_title")
        );
    }

    /// A flat cover hash scores 1.00 against another flat one. It is not evidence.
    #[test]
    fn a_blank_cover_corroborates_nothing() {
        let with_hash = |hash: &str| {
            let mut w = site("Blank Cover", None);
            w.cover_phash = Some(hash.into());
            w
        };
        for flat in ["0000000000000000", "ffffffffffffffff"] {
            assert_eq!(
                verdict(&with_hash(flat), &with_hash(flat)),
                (Verdict::Review, "uncorroborated")
            );
        }
        assert_eq!(
            verdict(
                &with_hash("ff00ff00ff00ff00"),
                &with_hash("0000000000000000")
            )
            .0,
            Verdict::Review
        );
        assert_eq!(
            verdict(
                &with_hash("ff00ff00ff00ff00"),
                &with_hash("ff00ff00ff00ff01")
            )
            .0,
            Verdict::Merge
        );
        assert!(!is_discriminative_phash("0000000000000001"));
        assert!(!is_discriminative_phash("ff00"));
        assert!(!is_discriminative_phash("zzzzzzzzzzzzzzzz"));
    }

    /// A likelier duplicate sorts above a weaker one in the review queue.
    #[test]
    fn the_score_ranks_stronger_evidence_higher() {
        let exact = assess(&site("The Mad Dog", None), &site("Mad Dog", None));
        let similar = assess(
            &site("Rebirth of the Super Battle God", None),
            &site("Rebirth of the Ultimate War God", None),
        );
        let credited = assess(
            &site("Mad Dog", Some("Some Author")),
            &site("Mad Dog", Some("Some Author")),
        );
        assert!(credited.score > exact.score && exact.score > similar.score);
        assert!(credited.score <= 0.99);
    }
}
