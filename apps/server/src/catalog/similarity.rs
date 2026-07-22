//! Cheap similarity signals for the dedup matcher (CATALOGUE.md §4, step 4).
//!
//! Two dependency-free comparators:
//!   * `description_similarity` — exact Jaccard over word/char shingle sets (NOT
//!     MinHash signatures — the full sets are intersected directly). Aggregator sites
//!     copy AniList/MAL descriptions near-verbatim, so plain shingle overlap is the
//!     cheap-no-ML starting point the design locked in (§10). MinHash would only be
//!     needed to approximate this Jaccard at scale; we compute it exactly.
//!   * `phash_similarity` — normalized Hamming similarity over two hex perceptual
//!     hashes. The cover→hash step lives in `crate::phash` (dHash via the `image`
//!     crate) and is populated during catalogue sync when `COVER_PHASH` is on.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};

/// Minimum character similarity for author-corroborated exact-title merges.
pub const AUTHOR_MATCH_THRESHOLD: f64 = 0.90;

/// One name's comparison key: case, punctuation and word order folded away.
fn name_key(name: &str) -> Vec<char> {
    let lower = name.to_lowercase();
    let mut tokens: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    tokens.sort_unstable();
    tokens.concat().chars().collect()
}

/// Words that mark a parenthetical as an organisation or role rather than a second
/// spelling of the person's name.
const AUTHOR_LABEL_WORDS: &[&str] = &[
    "circle",
    "studio",
    "studios",
    "comics",
    "team",
    "group",
    "works",
    "committee",
    "project",
    "production",
    "productions",
    "entertainment",
    "company",
    "artist",
    "author",
    "story",
    "art",
    "original",
];

/// Every name a credit line can be recognised by. Sources credit the same person in
/// different shapes, and comparing the whole line alone missed all of these:
///
///   * a transliteration in parentheses — "Dedart (디다트)" vs "Dedart";
///   * a list of people — "Masashi Kishimoto, Mikio Ikemoto" vs "Kishimoto Masashi".
///
/// So besides the whole line, each listed person and each parenthesised transliteration
/// is a variant. Two guards keep that from inventing matches:
///
///   * a parenthetical only counts when it is in a different script from the name it
///     follows — that is what a transliteration looks like — and when it is not an
///     organisation label (`AUTHOR_LABEL_WORDS`). "(Circle)" and "(REDICE STUDIO)" say
///     what someone belongs to, not who they are: every doujin circle would otherwise
///     match every other one, and so would two artists of one studio.
///   * a single Latin word out of a list is skipped: "Kishimoto, Masashi" is one
///     person written family-name-first, not two people called Kishimoto and Masashi.
///
/// Sites also disagree on where a romanised name breaks ("Hong Junghoon" / "Hong Jung
/// Hoon", "Seo Taerang" / "Seo Tae rang"). Sorting the words — which is what folds name
/// order away — scrambles those differently, so each name is also compared by its
/// letters alone, in the order written.
fn author_variants(line: &str) -> Vec<Vec<char>> {
    let mut outside = String::new();
    let mut inside: Vec<String> = Vec::new();
    let mut depth = 0usize;
    for c in line.chars() {
        match c {
            '(' | '（' => {
                depth += 1;
                if depth == 1 {
                    inside.push(String::new());
                }
                outside.push(' ');
            }
            ')' | '）' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {
                if let Some(current) = inside.last_mut() {
                    current.push(c);
                }
            }
            _ => outside.push(c),
        }
    }
    let foreign = |s: &str| s.chars().any(|c| c.is_alphabetic() && !c.is_ascii());
    let words = |s: &str| {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .count()
    };
    let letters = |s: &str| -> Vec<char> {
        s.to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect()
    };
    let mut variants = vec![name_key(line)];
    let people: Vec<&str> = outside
        .split([',', ';', '/', '&', '×', '、', '，'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    for person in &people {
        if people.len() == 1 || words(person) >= 2 || foreign(person) {
            variants.push(name_key(person));
            variants.push(letters(person));
        }
    }
    for alias in &inside {
        let label = alias
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .any(|word| AUTHOR_LABEL_WORDS.contains(&word));
        if !label && foreign(alias) != foreign(&outside) {
            variants.push(name_key(alias));
        }
    }
    // The whole line keeps whatever it is; a derived variant needs two characters to
    // carry any identity.
    let mut seen: Vec<Vec<char>> = Vec::new();
    for (i, v) in variants.into_iter().enumerate() {
        if ((i == 0 && !v.is_empty()) || v.len() >= 2) && !seen.contains(&v) {
            seen.push(v);
        }
    }
    seen
}

/// Unicode character edit similarity of two name keys (0.0..=1.0).
fn edit_similarity(a: &[char], b: &[char]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let length = a.len().max(b.len());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ac) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, bc) in b.iter().enumerate() {
            let previous = row[j + 1];
            row[j + 1] = (diagonal + usize::from(ac != bc))
                .min(row[j] + 1)
                .min(previous + 1);
            diagonal = previous;
        }
    }
    1.0 - row[b.len()] as f64 / length as f64
}

/// Best character edit similarity between any name of one credit line and any name of
/// the other (see [`author_variants`]), after folding case, punctuation and name order.
/// Missing or blank names supply no evidence, even when both are missing.
pub fn author_similarity(a: Option<&str>, b: Option<&str>) -> f64 {
    let (Some(a), Some(b)) = (a, b) else {
        return 0.0;
    };
    let (a, b) = (author_variants(a), author_variants(b));
    a.iter()
        .flat_map(|x| b.iter().map(move |y| edit_similarity(x, y)))
        .fold(0.0_f64, f64::max)
}

pub fn authors_match(a: Option<&str>, b: Option<&str>) -> bool {
    author_similarity(a, b) >= AUTHOR_MATCH_THRESHOLD
}

/// True when any credit of one work names a person the other work credits. `a` and `b`
/// are each work's credit lines (author, artist). One source lists the writer where
/// another lists the illustrator, so the two fields are compared crosswise.
pub fn credits_match(a: [Option<&str>; 2], b: [Option<&str>; 2]) -> bool {
    a.iter().any(|x| b.iter().any(|y| authors_match(*x, *y)))
}

/// Precomputed shingle set for one description — so a candidate whose description is
/// compared against many works (the dedup scoring loop) is shingled ONCE instead of once
/// per comparison. [`description_similarity`] is the string-in convenience wrapper.
#[derive(Clone)]
pub struct DescShingles(HashSet<u64>);

/// Shingle a description once for repeated comparison (see [`DescShingles`]).
pub fn description_shingles(text: &str) -> DescShingles {
    DescShingles(shingles(text))
}

/// Jaccard similarity (0.0..=1.0) of two precomputed description shingle sets.
pub fn description_similarity_pre(a: &DescShingles, b: &DescShingles) -> f64 {
    jaccard(&a.0, &b.0)
}

/// Jaccard similarity (0.0..=1.0) of two descriptions. Uses word 3-shingles when
/// there are enough words, else falls back to character 3-grams so short blurbs
/// still compare. Empty/near-empty inputs score 0.0. The dedup loop uses the
/// precompute path ([`description_similarity_pre`]); this string-in form is retained as
/// the readable public API and the reference the equivalence test scores against.
#[allow(dead_code)]
pub fn description_similarity(a: &str, b: &str) -> f64 {
    description_similarity_pre(&description_shingles(a), &description_shingles(b))
}

fn shingles(text: &str) -> HashSet<u64> {
    let norm = text.to_lowercase();
    let words: Vec<&str> = norm.split_whitespace().collect();
    let mut set = HashSet::new();
    if words.len() >= 3 {
        for w in words.windows(3) {
            set.insert(hash_one(w.join(" ")));
        }
    } else {
        // Char 3-grams over the compacted string for very short text.
        let chars: Vec<char> = norm.chars().filter(|c| !c.is_whitespace()).collect();
        if chars.len() >= 3 {
            for w in chars.windows(3) {
                set.insert(hash_one(w.iter().collect::<String>()));
            }
        } else if !chars.is_empty() {
            set.insert(hash_one(chars.iter().collect::<String>()));
        }
    }
    set
}

fn jaccard(a: &HashSet<u64>, b: &HashSet<u64>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

fn hash_one<T: Hash>(v: T) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

/// Precomputed token + char-3-gram sets for one already-normalized title. The dedup
/// scoring loop compares the SAME candidate titles against every candidate work's aliases
/// (works can carry 20+ localized aliases), so shingling each title once here — instead of
/// once per (title, alias) pair inside [`title_similarity`] — removes the hot-path CPU.
#[derive(Clone)]
pub struct TitleShingles {
    /// The normalized title verbatim — drives the empty / exact-equal short-circuits.
    raw: String,
    /// Whitespace-split word tokens (token Jaccard).
    tokens: HashSet<String>,
    /// Hashed char 3-grams (char-n-gram Jaccard).
    ngrams: HashSet<u64>,
}

/// Shingle a normalized title once for repeated comparison (see [`TitleShingles`]).
pub fn title_shingles(s: &str) -> TitleShingles {
    TitleShingles {
        raw: s.to_string(),
        tokens: s.split_whitespace().map(str::to_string).collect(),
        ngrams: char_ngrams(s, 3),
    }
}

/// Similarity (0.0..=1.0) of two precomputed titles. Same blend as [`title_similarity`]:
/// word-token Jaccard (robust to appended words) maxed with char 3-gram Jaccard (robust to
/// spacing/typos), with the empty and exact-equal short-circuits preserved.
pub fn title_similarity_pre(a: &TitleShingles, b: &TitleShingles) -> f64 {
    if a.raw.is_empty() || b.raw.is_empty() {
        return 0.0;
    }
    if a.raw == b.raw {
        return 1.0;
    }
    let token = if a.tokens.is_empty() || b.tokens.is_empty() {
        0.0
    } else {
        a.tokens.intersection(&b.tokens).count() as f64 / a.tokens.union(&b.tokens).count() as f64
    };
    token.max(jaccard(&a.ngrams, &b.ngrams))
}

/// Similarity (0.0..=1.0) of two already-normalized titles. Blends word-token
/// Jaccard (robust to appended words like "manhwa"/"official") with character
/// 3-gram Jaccard (robust to spacing/typos) and takes the stronger signal. The dedup
/// loop uses the precompute path ([`title_similarity_pre`]); this string-in form is
/// retained as the readable public API and the reference the equivalence test scores against.
#[allow(dead_code)]
pub fn title_similarity(a: &str, b: &str) -> f64 {
    title_similarity_pre(&title_shingles(a), &title_shingles(b))
}

/// Hashed char `n`-grams over the whitespace-stripped string (falls back to the whole
/// string when shorter than `n`).
fn char_ngrams(s: &str, n: usize) -> HashSet<u64> {
    let chars: Vec<char> = s.chars().filter(|c| !c.is_whitespace()).collect();
    let mut set = HashSet::new();
    if chars.len() >= n {
        for w in chars.windows(n) {
            set.insert(hash_one(w.iter().collect::<String>()));
        }
    } else if !chars.is_empty() {
        set.insert(hash_one(chars.iter().collect::<String>()));
    }
    set
}

/// Normalized Hamming similarity (0.0..=1.0) of two equal-length hex perceptual
/// hashes. Returns `None` if either is missing, non-hex, or lengths differ.
pub fn phash_similarity(a: Option<&str>, b: Option<&str>) -> Option<f64> {
    let (a, b) = (a?, b?);
    let (ba, bb) = (hex::decode(a).ok()?, hex::decode(b).ok()?);
    if ba.is_empty() || ba.len() != bb.len() {
        return None;
    }
    let bits = (ba.len() * 8) as f64;
    let dist: u32 = ba
        .iter()
        .zip(bb.iter())
        .map(|(x, y)| (x ^ y).count_ones())
        .sum();
    Some(1.0 - (dist as f64 / bits))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn author_character_similarity_threshold_and_normalization() {
        assert_eq!(
            author_similarity(Some("Masashi Kishimoto"), Some("KISHIMOTO, Masashi")),
            1.0
        );
        assert!(authors_match(
            Some("Masashi Kishimoto"),
            Some("Masashi Kishimot")
        ));
        assert!(authors_match(Some("abcdefghij"), Some("abcdefghiX"))); // exactly 90%
        assert!(!authors_match(Some("abcdefghij"), Some("abcdefghXY")));
        assert!(authors_match(Some("山田 太郎"), Some("太郎 山田")));
        for missing in [None, Some(""), Some(" , ")] {
            assert!(!authors_match(missing, missing));
            assert!(!authors_match(missing, Some("Known Author")));
        }
        assert!(!authors_match(
            Some("Masashi Kishimoto"),
            Some("Eiichiro Oda")
        ));
    }

    /// The shapes real sources credit one person in, taken from catalogue pairs the
    /// whole-line comparison refused.
    #[test]
    fn author_lines_match_through_transliterations_and_lists() {
        // A native-script transliteration in parentheses.
        assert!(authors_match(Some("Dedart (디다트)"), Some("Dedart")));
        assert!(authors_match(
            Some("常盘勇者 (Chang Pan Yong Zhe)"),
            Some("Chang Pan Yong Zhe")
        ));
        // One person out of a list, in either name order.
        assert!(authors_match(
            Some("Masashi Kishimoto, Mikio Ikemoto"),
            Some("Kishimoto Masashi")
        ));
        assert!(authors_match(
            Some("Team the J/ Han Yurim"),
            Some("Han Yurim (한유림)")
        ));
        // A same-script parenthetical is a label, not a name: two circles do not match.
        assert!(!authors_match(
            Some("Moon Clock (Circle)"),
            Some("Tsubame Gaeshi! (Circle)")
        ));
        assert!(!authors_match(
            Some("TryTry (REDICE STUDIO)"),
            Some("Someone Else (REDICE STUDIO)")
        ));
        // "Family, Given" is one person; a shared given name is not a match.
        assert!(!authors_match(
            Some("Kishimoto, Masashi"),
            Some("Asada, Masashi")
        ));
        // …also when the name before the label is in another script, which makes the
        // label look like a transliteration.
        assert!(!authors_match(
            Some("박성우 (REDICE STUDIO)"),
            Some("장성락 (REDICE STUDIO)")
        ));
        assert!(!authors_match(
            Some("José (Circle)"),
            Some("André (Circle)")
        ));
        // Unbalanced parentheses must not panic.
        assert!(!authors_match(Some(")(("), Some("Known Author")));
        // The same romanised name, broken into words differently.
        assert!(authors_match(
            Some("Hong Junghoon"),
            Some("Hong Jung Hoon (홍정훈)")
        ));
        assert!(authors_match(
            Some("Seo Taerang"),
            Some("Seo Tae rang (서태랑)")
        ));
        assert!(authors_match(Some("BOM Ga'eul"), Some("Bomgaeul (봄가을)")));
        assert!(!authors_match(Some("Kim Hyun-woo"), Some("Kim Yeon Woo")));
        // A writer credited on one side and an illustrator on the other.
        assert!(credits_match(
            [Some("Fried-Mandu"), Some("Park Jong Suk")],
            [Some("Park Jong Suk"), None]
        ));
        assert!(!credits_match([Some("A Writer"), None], [None, None]));
    }

    #[test]
    fn identical_descriptions_score_one() {
        let t = "A boy is reincarnated as a slime in a fantasy world of monsters.";
        assert!((description_similarity(t, t) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn copied_description_scores_high() {
        let a = "The ordinary Mikami Satoru found himself dying after being stabbed.";
        let b = "The ordinary Mikami Satoru found himself dying after being stabbed by a slasher.";
        assert!(
            description_similarity(a, b) > 0.5,
            "copied text should overlap strongly"
        );
    }

    #[test]
    fn unrelated_descriptions_score_low() {
        let a = "A high school romance about a boy and a dress-up doll.";
        let b = "A hunter gains power in a world of dungeons and monsters.";
        assert!(description_similarity(a, b) < 0.2);
    }

    #[test]
    fn empty_scores_zero() {
        assert_eq!(description_similarity("", "anything at all here"), 0.0);
    }

    #[test]
    fn phash_identical_and_distant() {
        assert_eq!(
            phash_similarity(Some("ffffffffffffffff"), Some("ffffffffffffffff")),
            Some(1.0)
        );
        // 64-bit hash, fully inverted -> similarity 0.0
        assert_eq!(
            phash_similarity(Some("0000000000000000"), Some("ffffffffffffffff")),
            Some(0.0)
        );
        // one byte differs by one bit -> 63/64
        let s = phash_similarity(Some("0000000000000000"), Some("0000000000000001")).unwrap();
        assert!((s - 63.0 / 64.0).abs() < 1e-9);
    }

    #[test]
    fn title_similarity_scores() {
        assert_eq!(title_similarity("solo leveling", "solo leveling"), 1.0);
        // appended source word: 2/3 tokens overlap
        assert!(title_similarity("solo leveling", "solo leveling manhwa") > 0.6);
        // unrelated titles
        assert!(title_similarity("one piece", "berserk") < 0.2);
        assert_eq!(title_similarity("", "anything"), 0.0);
    }

    #[test]
    fn precomputed_paths_match_string_paths() {
        // The precomputed (shingle-once) path must score IDENTICALLY to the string path —
        // it's a pure CPU refactor for the dedup hot loop, not a scoring change.
        let titles = [
            "",
            "solo leveling",
            "solo leveling manhwa",
            "one piece",
            "berserk",
            "tensei shitara slime datta ken",
            "a",
        ];
        for a in titles {
            for b in titles {
                let string_path = title_similarity(a, b);
                let pre_path = title_similarity_pre(&title_shingles(a), &title_shingles(b));
                assert_eq!(string_path, pre_path, "title mismatch for ({a:?}, {b:?})");
            }
        }

        let descs = [
            "",
            "A boy is reincarnated as a slime in a fantasy world of monsters.",
            "The ordinary Mikami Satoru found himself dying after being stabbed.",
            "A hunter gains power in a world of dungeons and monsters.",
            "short",
        ];
        for a in descs {
            for b in descs {
                let string_path = description_similarity(a, b);
                let pre_path =
                    description_similarity_pre(&description_shingles(a), &description_shingles(b));
                assert_eq!(string_path, pre_path, "desc mismatch for ({a:?}, {b:?})");
            }
        }
    }

    #[test]
    fn phash_guards_bad_input() {
        assert_eq!(phash_similarity(None, Some("ff")), None);
        assert_eq!(phash_similarity(Some("zz"), Some("ff")), None);
        assert_eq!(phash_similarity(Some("ffff"), Some("ff")), None); // length mismatch
    }
}
