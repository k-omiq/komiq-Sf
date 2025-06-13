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
    let expanded = raw.replace('&', " and ").replace('＆', " and ");
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
    fn collapses_whitespace() {
        assert_eq!(normalize_title("  One    Piece  "), "one piece");
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
    fn cjk_passes_through_lowercased() {
        // CJK chars are alphanumeric, so they survive folding and can match a
        // MangaDex `ja` alias normalized the same way.
        assert_eq!(normalize_title("鬼滅の刃"), "鬼滅の刃");
    }
}
