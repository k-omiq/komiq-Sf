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
