//! Title normalization for the dedup matcher's alias index (CATALOGUE.md §4, step 2).
//!
//! Produces a canonical lookup key: lowercased, non-alphanumerics folded to spaces,
//! whitespace collapsed, and a small set of trailing "noise" tokens (season/part
//! markers, format words) stripped. Romanization of raw scripts is intentionally NOT
//! done here — we rely on MangaDex's own romaji alt-titles (`ja-ro`) for that. CJK
//! characters are `char::is_alphanumeric`, so a Japanese title normalizes to its own
//! characters and still matches an identically-normalized MangaDex `ja` alias.

/// Trailing tokens that add no identity and cause near-duplicates to miss
/// (e.g. "Overlord Season 2" vs "Overlord"). Stripped only from the *end*.
const NOISE_TAIL: &[&str] = &[
    "season",
    "part",
    "cour",
    "arc",
    "the",
    "animation",
    "manga",
    "manhwa",
    "manhua",
    "official",
    "colored",
    "color",
    "remake",
    "remaster",
    "remastered",
    "uncensored",
];

/// Fold a lowercased character to its diacritic-free base so romanization and accent
/// variants of the same title collide in the alias index. Macron vowels are the
/// high-value case for manga — MangaDex romaji uses `ō/ū/ā/ē/ī` ("Tōkyō") where an
/// aggregator writes the bare vowel ("Tokyo") — alongside the common Latin accents.
/// Non-Latin scripts (CJK, Cyrillic, …) pass through unchanged. Returns the char as-is
/// when it carries no diacritic.
fn fold_diacritic(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' | 'ǎ' | 'ả' | 'ạ' => 'a',
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' | 'ẹ' | 'ẻ' => 'e',
        'ì' | 'í' | 'î' | 'ï' | 'ī' | 'ĭ' | 'į' | 'ı' | 'ǐ' | 'ị' => 'i',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' | 'ǒ' | 'ơ' | 'ọ' | 'ỏ' => {
            'o'
        }
        'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' | 'ǔ' | 'ư' | 'ụ' => 'u',
        'ñ' | 'ń' | 'ň' | 'ņ' => 'n',
        'ç' | 'ć' | 'č' | 'ĉ' | 'ċ' => 'c',
        'ý' | 'ÿ' | 'ỳ' | 'ỹ' => 'y',
        'š' | 'ś' | 'ş' | 'ŝ' => 's',
        'ž' | 'ź' | 'ż' => 'z',
        'ğ' | 'ĝ' | 'ġ' | 'ģ' => 'g',
        'ł' => 'l',
        'đ' | 'ð' => 'd',
        _ => c,
    }
}

/// Normalize a raw title to its alias-index key. Returns `""` for input that has no
/// alphanumeric content (which callers should skip rather than index).
pub fn normalize_title(raw: &str) -> String {
    // Spell out `&` (and its full-width form) before folding so "Fist of the North &
    // South" and "…and South" share a key. Done pre-lowercase; the tokens are ASCII.
    let expanded = raw.replace(['&', '＆'], " and ");
    let lowered = expanded.to_lowercase();
    let spaced: String = lowered
        .chars()
        // Fold diacritics/macrons FIRST, then keep alphanumerics, else space. A folded
        // char is always alphanumeric, so folding before the alnum test is safe.
        .map(fold_diacritic)
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    let mut tokens: Vec<&str> = spaced.split_whitespace().collect();
    // The folded-but-not-yet-stripped form. Stripping noise must never reduce a
    // non-empty title to "" (e.g. "The Mix" -> strip "mix" (a valid numeral by form)
    // -> strip "the" -> ""); fall back to this pre-strip form if that happens.
    let pre_strip: Vec<&str> = tokens.clone();

    // Strip trailing noise tokens, plus a trailing bare number that trails a noise
    // word ("season 2" -> both go). Iterate from the end until nothing more strips.
    loop {
        let before = tokens.len();
        if let Some(&last) = tokens.last() {
            let is_number = last.chars().all(|c| c.is_ascii_digit());
            let is_roman = is_roman_numeral(last);
            if NOISE_TAIL.contains(&last) {
                tokens.pop();
            } else if (is_number || is_roman) && tokens.len() >= 2 {
                // Drop a trailing number/roman only when the token before it is noise
                // (e.g. "part iii"); a title ending in a number that carries meaning
                // ("re zero 2" is rare, but "7 seeds" keeps its 7 as a lead token).
                let prev = tokens[tokens.len() - 2];
                if NOISE_TAIL.contains(&prev) {
                    tokens.pop();
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        if tokens.len() == before {
            break;
        }
    }

    // Never let noise-stripping erase a real title.
    if tokens.is_empty() && !pre_strip.is_empty() {
        return pre_strip.join(" ");
    }
    tokens.join(" ")
}

/// The article-insensitive duplicate key for an already-normalized title: the tokens
/// `the` / `a` / `an` dropped, except in last position. Sites disagree on articles
/// ("The Newbie Is Too Strong" vs "Newbie Is Too Strong"), so duplicate lookups compare
/// this key rather than `normalize_title`'s output.
///
/// The last token is always kept. A trailing "a" is a series marker ("Heavy Object A",
/// "Ultraman A"), not an article, and it guarantees a non-empty key for non-empty input.
///
/// Mirrored in SQL by the `work_alias.match_key` generated column (migration 0111).
pub fn match_key(normalized: &str) -> String {
    let tokens: Vec<&str> = normalized.split(' ').collect();
    let last = tokens.len().saturating_sub(1);
    tokens
        .iter()
        .enumerate()
        .filter(|(i, t)| *i == last || !matches!(**t, "the" | "a" | "an"))
        .map(|(_, t)| *t)
        .collect::<Vec<_>>()
        .join(" ")
}

/// True when two titles disagree on being a novel. A novel and its comic adaptation
/// share a name, alt titles and an author, so every other merge signal says
/// "duplicate" — but they are different works and must never be folded together.
///
/// Compares HOW OFTEN "novel" occurs, not merely whether it does: a comic called
/// "The Novel's Extra" and its source "The Novel's Extra (Novel)" both contain the
/// word, and only the count tells them apart. A missing title counts as zero.
pub fn novel_mismatch(a: Option<&str>, b: Option<&str>) -> bool {
    let novels = |t: Option<&str>| t.map_or(0, |t| t.to_lowercase().matches("novel").count());
    novels(a) != novels(b)
}

/// Noise words that name an instalment rather than a format. "Overlord Season 2" and
/// "Overlord" share a `normalize_title` key so a site's per-season listing finds its
/// series, but the stripped marker still says they may be different works.
const SEQUEL_TAIL: &[&str] = &[
    "season",
    "part",
    "cour",
    "arc",
    "remake",
    "remaster",
    "remastered",
];

/// The instalment marker `normalize_title` strips from the end of `raw`: its sequel
/// words and their numbers ("Rosario to Vampire Season II" -> `["season", "ii"]`),
/// format words ("Official Colored") ignored. Empty for a title without one.
fn sequel_marker(raw: &str) -> Vec<String> {
    let folded = raw.replace(['&', '＆'], " and ").to_lowercase();
    let tokens: Vec<String> = folded
        .chars()
        .map(fold_diacritic)
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let kept = normalize_title(raw).split_whitespace().count();
    tokens
        .into_iter()
        .skip(kept)
        .filter(|t| {
            SEQUEL_TAIL.contains(&t.as_str())
                || t.chars().all(|c| c.is_ascii_digit())
                || is_roman_numeral(t)
        })
        .collect()
}

/// True when two titles carry different instalment markers ("X" vs "X Season 2").
/// MangaDex lists a sequel as its own entry, so between two MangaDex-anchored works a
/// marker one side lacks means two works, not one. A missing title has no marker.
pub fn sequel_mismatch(a: Option<&str>, b: Option<&str>) -> bool {
    let marker = |t: Option<&str>| t.map(sequel_marker).unwrap_or_default();
    marker(a) != marker(b)
}

/// Words that name a different edition of a series rather than the series itself: a
/// promo, a pre-serialization, a doujinshi, an anthology, a one-shot, a side story.
const EDITION_WORDS: &[&str] = &[
    "promo",
    "serialization",
    "serialisation",
    "preserialization",
    "doujinshi",
    "anthology",
    "oneshot",
    "shot",
    "gaiden",
    "spinoff",
    "side",
    "extras",
    "fan",
];

/// Lowercased alphanumeric tokens of a raw title, diacritics folded.
fn raw_tokens(raw: &str) -> Vec<String> {
    raw.to_lowercase()
        .chars()
        .map(fold_diacritic)
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// True when one title names an edition the other does not ("X" vs "X (Promo)",
/// "X - Side Stories", "X (Fan Colored)"). Such a pair is related, but it is not one
/// work, so it never merges automatically. A missing title names no edition.
pub fn edition_mismatch(a: Option<&str>, b: Option<&str>) -> bool {
    let editions = |t: Option<&str>| {
        let mut found: Vec<String> = t
            .map(raw_tokens)
            .unwrap_or_default()
            .into_iter()
            .filter(|w| EDITION_WORDS.contains(&w.as_str()))
            .collect();
        found.sort();
        found.dedup();
        found
    };
    editions(a) != editions(b)
}

/// True when two titles carry different numbers ("X" vs "X 2", "Anz" vs "Anz 2025").
/// A number one title lacks usually marks a sequel or another year's edition.
pub fn number_mismatch(a: Option<&str>, b: Option<&str>) -> bool {
    let numbers = |t: Option<&str>| {
        let mut found: Vec<String> = t
            .map(raw_tokens)
            .unwrap_or_default()
            .into_iter()
            .filter(|w| w.chars().all(|c| c.is_ascii_digit()))
            .collect();
        found.sort();
        found
    };
    numbers(a) != numbers(b)
}

/// True when `s` is a genuinely, canonically formed Roman numeral (e.g. "ii", "iv",
/// "xiii"). The old implementation accepted any string over {i,v,x,l,c,d,m}, so
/// ordinary words like "did" or "civic" were mistaken for numerals and stripped.
/// Requiring the canonical additive/subtractive form rejects those. A few real words
/// (e.g. "mix" = 1009) are still valid numerals *by form*; the empty-result guard in
/// `normalize_title` keeps such a word from erasing a title like "The Mix".
fn is_roman_numeral(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    // Match the canonical grammar greedily, lowercased, over the whole string:
    //   M{0,3} (CM|CD|D?C{0,3}) (XC|XL|L?X{0,3}) (IX|IV|V?I{0,3})
    let b = s.as_bytes();
    let mut i = 0usize;
    // thousands: M{0,3}
    let mut m = 0;
    while i < b.len() && b[i] == b'm' && m < 3 {
        i += 1;
        m += 1;
    }
    // hundreds: CM | CD | D?C{0,3}
    if i + 1 < b.len() && b[i] == b'c' && (b[i + 1] == b'm' || b[i + 1] == b'd') {
        i += 2;
    } else {
        if i < b.len() && b[i] == b'd' {
            i += 1;
        }
        let mut c = 0;
        while i < b.len() && b[i] == b'c' && c < 3 {
            i += 1;
            c += 1;
        }
    }
    // tens: XC | XL | L?X{0,3}
    if i + 1 < b.len() && b[i] == b'x' && (b[i + 1] == b'c' || b[i + 1] == b'l') {
        i += 2;
    } else {
        if i < b.len() && b[i] == b'l' {
            i += 1;
        }
        let mut x = 0;
        while i < b.len() && b[i] == b'x' && x < 3 {
            i += 1;
            x += 1;
        }
    }
    // units: IX | IV | V?I{0,3}
    if i + 1 < b.len() && b[i] == b'i' && (b[i + 1] == b'x' || b[i + 1] == b'v') {
        i += 2;
    } else {
        if i < b.len() && b[i] == b'v' {
            i += 1;
        }
        let mut ones = 0;
        while i < b.len() && b[i] == b'i' && ones < 3 {
            i += 1;
            ones += 1;
        }
    }
    i == b.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercases_and_folds_punctuation() {
        assert_eq!(
            normalize_title("Re:ZERO -Starting Life-"),
            "re zero starting life"
        );
    }

    #[test]
    fn edition_and_number_markers_keep_related_works_apart() {
        assert!(edition_mismatch(
            Some("Solo Leveling: Ragnarok"),
            Some("Solo Leveling: Ragnarok (Pre-serialization)")
        ));
        assert!(edition_mismatch(Some("Estio"), Some("Estio (Promo)")));
        assert!(edition_mismatch(
            Some("Leviathan"),
            Some("Leviathan - Side Stories")
        ));
        assert!(!edition_mismatch(
            Some("Yuri Ecchi Anthology"),
            Some("Yuri Erotic Anthology")
        ));
        assert!(!edition_mismatch(Some("Naruto"), None));
        assert!(number_mismatch(
            Some("My Stepmom Made Me Her Toy"),
            Some("My Stepmom Made Me Her Toy 2")
        ));
        assert!(!number_mismatch(
            Some("Starting With 13 Hidden Traits"),
            Some("Hidden Teukseong 13-gae Deulgo Sijakhanda")
        ));
        // A remake is its own MangaDex entry, like a sequel.
        assert!(sequel_mismatch(
            Some("Lord of the Mysteries"),
            Some("Lord of the Mysteries (Remake)")
        ));
    }

    #[test]
    fn collapses_whitespace() {
        assert_eq!(normalize_title("  One    Piece  "), "one piece");
    }

    #[test]
    fn sequel_mismatch_sees_the_marker_normalization_strips() {
        assert!(sequel_mismatch(
            Some("Rosario to Vampire"),
            Some("Rosario to Vampire Season II")
        ));
        assert!(sequel_mismatch(Some("Kid Gang"), Some("Kid Gang Season 2")));
        assert!(sequel_mismatch(Some("X Part 1"), Some("X Part 2")));
        // Format words and punctuation are not instalments.
        assert!(!sequel_mismatch(
            Some("Jujutsu Kaisen"),
            Some("Jujutsu Kaisen (Official Colored)")
        ));
        assert!(!sequel_mismatch(
            Some("Pretty Rhythm - Aurora Dream"),
            Some("Pretty Rhythm: Aurora Dream")
        ));
        assert!(!sequel_mismatch(Some("X Season 2"), Some("x: season 2")));
        // A number that is part of the name is not stripped, so it is no marker.
        assert!(!sequel_mismatch(Some("Area 51"), Some("Area 51")));
        assert!(!sequel_mismatch(None, Some("Overlord")));
    }

    #[test]
    fn strips_season_and_part_markers() {
        assert_eq!(normalize_title("Overlord Season 2"), "overlord");
        assert_eq!(normalize_title("Berserk Part III"), "berserk");
        assert_eq!(normalize_title("Kingdom (The Animation)"), "kingdom");
    }

    #[test]
    fn keeps_meaningful_leading_numbers() {
        assert_eq!(normalize_title("7 Seeds"), "7 seeds");
        assert_eq!(normalize_title("86 EIGHTY-SIX"), "86 eighty six");
    }

    #[test]
    fn empty_for_symbol_only() {
        assert_eq!(normalize_title("!!!"), "");
    }

    #[test]
    fn ordinary_words_are_not_roman_numerals() {
        // These are words, not numerals — the canonical validator must reject them.
        assert!(!is_roman_numeral("did"));
        assert!(!is_roman_numeral("civic"));
        assert!(!is_roman_numeral("dic"));
        // Genuine numerals still validate.
        assert!(is_roman_numeral("iii"));
        assert!(is_roman_numeral("iv"));
        assert!(is_roman_numeral("xiii"));
    }

    #[test]
    fn never_normalizes_a_real_title_to_empty() {
        // "the" is noise and "mix" is a valid numeral *by form* (1009), so naive
        // stripping empties this; the guard must fall back to the pre-strip form.
        assert_eq!(normalize_title("The Mix"), "the mix");
        // "civic" is no longer misread as a numeral, so nothing is stripped here
        // ("the" is only noise when trailing).
        assert_eq!(normalize_title("The Civic"), "the civic");
    }

    #[test]
    fn folds_macrons_and_accents() {
        // Romaji macron vs bare vowel — the high-value manga case.
        assert_eq!(
            normalize_title("Tōkyō Ghoul"),
            normalize_title("Tokyo Ghoul")
        );
        assert_eq!(normalize_title("Jūjutsu"), "jujutsu");
        assert_eq!(normalize_title("Pokémon"), "pokemon");
        assert_eq!(normalize_title("Réincarnation"), "reincarnation");
        assert_eq!(
            normalize_title("Kaguya-sama wa Kokurasetai"),
            "kaguya sama wa kokurasetai"
        );
    }

    #[test]
    fn expands_ampersand() {
        assert_eq!(
            normalize_title("Kaiju No.8 & Friends"),
            normalize_title("Kaiju No.8 and Friends")
        );
        assert_eq!(normalize_title("A & B"), "a and b");
    }

    #[test]
    fn match_key_drops_articles_but_never_the_last_token() {
        let key = |raw: &str| match_key(&normalize_title(raw));
        assert_eq!(
            key("The Beginning After the End"),
            key("Beginning After End")
        );
        assert_eq!(key("A Returner's Magic"), key("Returner's Magic"));
        assert_eq!(key("An Unordinary Life"), "unordinary life");
        // A trailing "a" names a different series, not an article.
        assert_eq!(key("Heavy Object A"), "heavy object a");
        assert_ne!(key("Heavy Object A"), key("Heavy Object"));
        // Only whole tokens: "theatre" and "anthem" are untouched.
        assert_eq!(key("Theatre Anthem"), "theatre anthem");
        // Never empty for a real title.
        assert_eq!(key("The"), "the");
        assert_eq!(key("A"), "a");
        assert_eq!(match_key(""), "");
    }

    #[test]
    fn novel_mismatch_only_when_one_side_is_a_novel() {
        assert!(novel_mismatch(
            Some("Solo Leveling (Novel)"),
            Some("Solo Leveling")
        ));
        assert!(novel_mismatch(
            Some("Overlord"),
            Some("Overlord Light Novel")
        ));
        assert!(!novel_mismatch(
            Some("The Novel's Extra"),
            Some("Novel's Extra")
        ));
        assert!(!novel_mismatch(Some("Overlord"), Some("Overlord")));
        // "novel" is part of the name itself: the format marker still tells them apart.
        assert!(novel_mismatch(
            Some("The Novel's Extra"),
            Some("The Novel's Extra (Novel)")
        ));
        assert!(novel_mismatch(Some("Web Novel"), None));
        assert!(!novel_mismatch(None, None));
    }

    #[test]
    fn cjk_passes_through_lowercased() {
        // CJK chars are alphanumeric, so they survive folding and can match a
        // MangaDex `ja` alias normalized the same way.
        assert_eq!(normalize_title("鬼滅の刃"), "鬼滅の刃");
    }
}
