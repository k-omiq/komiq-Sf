//! One canonical way to turn whatever a source calls a chapter into what we show
//! (Phase A2). Every surface — updates feed, home, browse, series detail, source detail,
//! the reader — goes through this, so "Ch. N" means the same thing everywhere.
//!
//! WHAT THIS REPLACES. Four different ad-hoc rules, three of which printed something that
//! is not a chapter number:
//!
//!   * `feed_series_updates` inserts `latest_chapter = NULL` for the scanner half and the
//!     reader falls back to `chapterCount` — a series with 412 chapters renders "Ch. 412"
//!     whether its newest chapter is 412 or 10.5 (F4).
//!   * `map_canonical_chapter` parses the number with `.unwrap_or(0.0)`, so every oneshot
//!     that reaches the reader is "Chapter 0".
//!   * `work_source_chapters` excludes non-numeric chapters outright, so 21,422 works
//!     (18.5% of the catalogue) have no chapter list at all (F2).
//!   * The continue-reading card renders `read + 1` — a progress index, not a number.
//!
//! THE NUMBER COMES FROM THE CHAPTER, NEVER FROM A COUNT. In precedence order: the
//! source's structured number; else the first number embedded in the chapter's name; else
//! it is not a numbered chapter and says so.

/// What to display for one chapter, and what to group it by.
#[derive(Debug, Clone, PartialEq)]
pub enum ChapterLabel {
    /// A real chapter number. `10.5` is a legitimate half-chapter, and `0` a legitimate
    /// first chapter (`Chapter 0`, `Prologue`, common in webtoons) — neither is a
    /// sentinel.
    Numbered(f64),
    /// A chapter with no usable number: a oneshot, an `Extra`, an `Omake`, a `?`. Carries
    /// the raw label so the surface can print what the source actually called it rather
    /// than inventing a number for it.
    Unnumbered(String),
}

impl ChapterLabel {
    /// The number, when there is one. `None` is not "chapter 0" — callers must not
    /// `unwrap_or(0.0)`, which is exactly the bug this type exists to prevent.
    pub fn number(&self) -> Option<f64> {
        match self {
            Self::Numbered(n) => Some(*n),
            Self::Unnumbered(_) => None,
        }
    }

    /// What to print. Numbers are formatted without a trailing `.0`, so 10 renders "10"
    /// and 10.5 renders "10.5".
    pub fn text(&self) -> String {
        match self {
            Self::Numbered(n) => format_number(*n),
            Self::Unnumbered(raw) => raw.clone(),
        }
    }

    /// The grouping key that folds the same chapter across sources into one row.
    ///
    /// Numbered chapters reuse the EXISTING `round(number * 100)` key —
    /// `chapter_override.chapter_key` (migration 0032) and `graphql::chapter_key` are
    /// already defined that way and admin chapter-hiding matches on it, so inventing a
    /// different scale would silently stop hiding hidden chapters. Validated on real data:
    /// only 72 rows out of 1.44M lose any precision at 2 decimal places.
    ///
    /// Unnumbered chapters get their own namespace keyed by the caller-supplied identity
    /// (the source's chapter id), so a work's three oneshots stay three rows instead of
    /// colliding on `0` — and can never collide with a real chapter 0 either.
    pub fn key(&self, identity: &str) -> String {
        match self {
            Self::Numbered(n) => ((n * 100.0).round() as i64).to_string(),
            Self::Unnumbered(_) => format!("x:{identity}"),
        }
    }
}

/// Above this, a "chapter number" is data corruption, not a chapter.
///
/// Measured on production: two Suwayomi series and ten MangaDex works carry numbers like
/// `99999999` (a TEST upload) and `20240120` (a DATE used as a chapter number). Ranking or
/// labelling by a raw maximum prints "Ch. 100000000" on a real series page. The longest
/// genuine series in the catalogue is under 2,000 chapters, so 5,000 is far above anything
/// real and far below anything absurd.
pub const SANE_MAX_CHAPTER: f64 = 5_000.0;

/// Is this a number a chapter could plausibly have? Negative numbers are excluded —
/// Suwayomi uses `-1` as its oneshot sentinel — while `0` is kept, because `Chapter 0` is
/// real and common.
pub fn is_sane_number(n: f64) -> bool {
    n.is_finite() && (0.0..=SANE_MAX_CHAPTER).contains(&n)
}

/// Resolve a chapter's label.
///
/// * `number` — the source's structured number, if it has one (`suwayomi_chapter.chapter_number`,
///   MangaDex `attributes.chapter` parsed). Used when sane.
/// * `raw` — the source's raw number string, when it is text rather than a float
///   (MangaDex sends `"10.5"`, but also `"Oneshot"` and `"Extra"`).
/// * `name` — the chapter's title/name. Only consulted when neither of the above yields a
///   number: on 4,000 sampled Suwayomi rows the structured number already agreed with the
///   name 3,994 times, so this is a ~0.15% fallback, not the primary path.
pub fn chapter_display(number: Option<f64>, raw: Option<&str>, name: Option<&str>) -> ChapterLabel {
    if let Some(n) = number.filter(|n| is_sane_number(*n)) {
        return ChapterLabel::Numbered(n);
    }
    // A raw string that IS a number ("10.5"). Parsed before the name is consulted because
    // it is the source's own structured answer, just typed as text.
    if let Some(n) = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|n| is_sane_number(*n))
    {
        return ChapterLabel::Numbered(n);
    }
    if let Some(n) = name.and_then(first_number).filter(|n| is_sane_number(*n)) {
        return ChapterLabel::Numbered(n);
    }
    // Nothing usable anywhere. Prefer the source's own words ("Oneshot", "Extra") over a
    // manufactured label; fall back to the name, then to a generic.
    //
    // A raw that PARSES AS A NUMBER is skipped here even though it reached this point:
    // reaching this point means the number was rejected as insane, and printing it is the
    // exact corruption `is_sane_number` exists to stop. Without this, Suwayomi's `-1`
    // oneshot sentinel renders as the literal label "Ch. -1" once the number arrives
    // through `raw` rather than through `number` — which is what happens the moment a
    // Suwayomi chapter is read back out of the canonical `chapter` spine (Phase B), where
    // its number is stored as text like every other source's. The same latent bug already
    // existed on the MangaDex half for any chapter whose `attributes.chapter` is out of
    // range.
    let usable_raw = raw
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.parse::<f64>().is_err());
    let raw_label = usable_raw
        .or_else(|| name.map(str::trim).filter(|s| !s.is_empty()))
        .unwrap_or("Oneshot");
    ChapterLabel::Unnumbered(raw_label.to_string())
}

/// The FIRST number in a chapter name, which is the chapter's own number.
///
/// "Chapter 45: The 100 Kings" is chapter 45, not chapter 100 — so this takes the first
/// run of digits, never the largest. Handles `Chapter 45`, `Ch. 45`, `Ch.45`, a bare `45`,
/// `Vol.4 Ch.16` (see below), and decimals like `Chapter 10.5`.
///
/// VOLUME PREFIXES ARE SKIPPED. `Vol.4 Ch.16` is chapter 16 of volume 4; taking the
/// literal first number would label it "Ch. 4". So a leading `Vol`/`Volume`/`V` token and
/// its number are dropped before the search.
fn first_number(name: &str) -> Option<f64> {
    let cleaned = strip_volume_prefix(name);
    let bytes = cleaned.as_bytes();
    let start = bytes.iter().position(|b| b.is_ascii_digit())?;
    let mut end = start;
    let mut seen_dot = false;
    while end < bytes.len() {
        let b = bytes[end];
        if b.is_ascii_digit() {
            end += 1;
        } else if b == b'.' && !seen_dot && end + 1 < bytes.len() && bytes[end + 1].is_ascii_digit()
        {
            // A dot only continues the number when a digit follows, so "Chapter 45." and
            // "Ch 45. The End" both stop at 45.
            seen_dot = true;
            end += 1;
        } else {
            break;
        }
    }
    cleaned[start..end].parse::<f64>().ok()
}

/// Drop a leading volume token so `Vol.4 Ch.16` reads as 16, not 4.
fn strip_volume_prefix(name: &str) -> &str {
    let trimmed = name.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    for prefix in ["volume", "vol.", "vol", "v."] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            // Only a real volume token: the prefix must be followed by its number
            // (possibly after separators), otherwise "Voleth's Gambit" would be eaten.
            let skipped = rest.trim_start_matches([' ', '.', '#', ':', '-']);
            if skipped.starts_with(|c: char| c.is_ascii_digit()) {
                let consumed = trimmed.len() - skipped.len();
                let after_number = trimmed[consumed..]
                    .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.');
                return after_number;
            }
        }
    }
    trimmed
}

/// Format a chapter number the way a reader writes it: no trailing `.0`, at most two
/// decimals (which is all the `round(number * 100)` key preserves anyway).
pub fn format_number(n: f64) -> String {
    if (n - n.round()).abs() < f64::EPSILON {
        return format!("{}", n.round() as i64);
    }
    let s = format!("{n:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_structured_number_wins_when_it_is_sane() {
        assert_eq!(
            chapter_display(Some(45.0), Some("45"), Some("Chapter 45")),
            ChapterLabel::Numbered(45.0)
        );
        // 10.5 is a real half-chapter, and 0 a real first chapter — neither is a sentinel.
        assert_eq!(
            chapter_display(Some(10.5), None, None),
            ChapterLabel::Numbered(10.5)
        );
        assert_eq!(
            chapter_display(Some(0.0), None, Some("Prologue")),
            ChapterLabel::Numbered(0.0)
        );
    }

    #[test]
    fn absurd_numbers_are_rejected_and_fall_through() {
        // Both measured in production: a TEST upload and a date used as a chapter number.
        // Falling through to the name is what stops a series page reading "Ch. 100000000".
        assert_eq!(
            chapter_display(Some(99_999_999.0), None, Some("Chapter 12 - TEST")),
            ChapterLabel::Numbered(12.0)
        );
        assert_eq!(
            chapter_display(Some(20_240_120.0), None, None),
            ChapterLabel::Unnumbered("Oneshot".into())
        );
        // Suwayomi's oneshot sentinel is -1, not a chapter.
        assert_eq!(
            chapter_display(Some(-1.0), None, Some("Oneshot")),
            ChapterLabel::Unnumbered("Oneshot".into())
        );
    }

    #[test]
    fn the_name_gives_up_its_first_number_not_its_largest() {
        // The owner's example, and the trap in it: "the 100 Kings" must not win.
        assert_eq!(first_number("Chapter 45: The 100 Kings"), Some(45.0));
        assert_eq!(first_number("Chapter 45: Beginning of the End"), Some(45.0));
        assert_eq!(first_number("Ch. 45"), Some(45.0));
        assert_eq!(first_number("Ch.45"), Some(45.0));
        assert_eq!(first_number("45"), Some(45.0));
        assert_eq!(first_number("Chapter 10.5"), Some(10.5));
        assert_eq!(first_number("Chapter 45."), Some(45.0));
        // A volume prefix is not the chapter number.
        assert_eq!(first_number("Vol.4 Ch.16"), Some(16.0));
        assert_eq!(first_number("Volume 2 Chapter 7"), Some(7.0));
        // …but a title that merely starts with "Vol" is not a volume.
        assert_eq!(first_number("Voleth's Gambit 3"), Some(3.0));
        assert_eq!(first_number("Oneshot"), None);
    }

    #[test]
    fn a_oneshot_keeps_the_words_the_source_used() {
        assert_eq!(
            chapter_display(None, Some("Oneshot"), None),
            ChapterLabel::Unnumbered("Oneshot".into())
        );
        assert_eq!(
            chapter_display(None, Some("Extra"), Some("Extra")),
            ChapterLabel::Unnumbered("Extra".into())
        );
        // Nothing at all still labels something rather than rendering blank — the 21,422
        // works with `latest_chapter IS NULL` are exactly this cohort.
        assert_eq!(
            chapter_display(None, None, None),
            ChapterLabel::Unnumbered("Oneshot".into())
        );
    }

    #[test]
    fn an_insane_number_is_never_used_as_the_label_whichever_slot_it_arrives_in() {
        // The SAME chapter, described the two ways the two halves describe it. Suwayomi
        // hands us a structured -1; the canonical `chapter` spine stores that number as
        // text and hands it back through `raw`. Both must label "Oneshot" — before this
        // was fixed the second rendered the literal "Ch. -1".
        assert_eq!(
            chapter_display(Some(-1.0), None, Some("Oneshot")),
            chapter_display(None, Some("-1"), Some("Oneshot")),
        );
        assert_eq!(
            chapter_display(None, Some("-1"), Some("Oneshot")),
            ChapterLabel::Unnumbered("Oneshot".into())
        );
        // With no name to fall back to, a generic still beats printing the corruption.
        assert_eq!(
            chapter_display(None, Some("99999999"), None),
            ChapterLabel::Unnumbered("Oneshot".into())
        );
        // A raw that is genuinely a word is still the best label we have.
        assert_eq!(
            chapter_display(None, Some("Extra"), Some("Bonus")),
            ChapterLabel::Unnumbered("Extra".into())
        );
    }

    #[test]
    fn keys_group_across_sources_and_never_collide_with_chapter_zero() {
        // Same chapter from two sources → one key. This is `round(number * 100)`, the key
        // `chapter_override` already uses; changing the scale would break admin hiding.
        assert_eq!(
            ChapterLabel::Numbered(10.5).key("a"),
            ChapterLabel::Numbered(10.5).key("b")
        );
        assert_eq!(ChapterLabel::Numbered(10.5).key("a"), "1050");
        assert_eq!(ChapterLabel::Numbered(0.0).key("a"), "0");
        // Two oneshots of one work stay two rows…
        let one = ChapterLabel::Unnumbered("Oneshot".into());
        assert_ne!(one.key("uuid-a"), one.key("uuid-b"));
        // …and neither is ever mistaken for chapter 0.
        assert_ne!(one.key("uuid-a"), ChapterLabel::Numbered(0.0).key("uuid-a"));
    }

    /// DUPLICATE-IMPLEMENTATION PIN — `round(number * 100)` is written TWICE.
    ///
    /// The chapter identity contract says the chapter key is "implemented
    /// once in `chapter_label::ChapterLabel::key()`". It is not. There is a second, textually
    /// identical implementation:
    ///
    /// ```text
    /// apps/server/src/graphql/mod.rs   fn chapter_key(number: f64) -> String {
    ///                                      ((number * 100.0).round() as i64).to_string()
    ///                                  }
    /// ```
    ///
    /// and that copy — not this one — is what admin chapter-hiding matches against
    /// (`chapter_override.chapter_key`, migration 0032). They agree today only because
    /// somebody wrote the same expression twice; nothing enforces it.
    ///
    /// The divergence is silent in both directions. Change this one and the ledger re-keys
    /// while admin hiding keeps matching the old scale; change that one and admin hiding
    /// stops matching rows it hid yesterday. No test fails, no log fires — a hidden chapter
    /// simply reappears.
    ///
    /// So this test replicates the `graphql` expression LITERALLY and drives both over an
    /// input set chosen to break them apart: integers, halves, round-half-away-from-zero
    /// boundaries, negatives and Suwayomi's `-1` oneshot sentinel, 0 and -0, values far past
    /// `SANE_MAX_CHAPTER`, the classic binary-fraction hazards (`1.005`, `0.1+0.2`, `2.675`),
    /// and the non-finite inputs where the `as i64` cast saturates. Unifying the two sites is
    /// the real fix — see the report — but until then, a divergence fails HERE, loudly.
    #[test]
    fn the_key_agrees_exactly_with_the_duplicate_implementation_in_graphql() {
        /// Copied VERBATIM from `graphql::chapter_key`. Do not "clean this up" — being a
        /// literal transcription of the other site is the entire point.
        fn graphql_chapter_key(number: f64) -> String {
            ((number * 100.0).round() as i64).to_string()
        }

        let hostile = [
            // Ordinary chapters.
            0.0,
            1.0,
            45.0,
            1999.0,
            // Half-chapters and finer steps — the reason the scale is ×100 at all.
            0.5,
            10.5,
            10.25,
            10.125,
            // Round-half-away-from-zero boundaries: `f64::round` is not banker's rounding,
            // and a reimplementation reaching for `.round_ties_even()` would diverge here.
            0.005,
            0.015,
            0.025,
            -0.005,
            -0.015,
            // Negatives and Suwayomi's oneshot sentinel. `Numbered(-1.0)` should never be
            // constructed (`is_sane_number` rejects it), but the key function itself has no
            // guard, so both copies must agree on what they'd do with it anyway.
            -1.0,
            -1.5,
            -0.0,
            // Binary-fraction hazards. None of these is exactly representable, and they
            // land on three different sides of the boundary — `1.005 * 100` is
            // 100.49999999999999 (down), `(0.1+0.2) * 100` is 30.000000000000004 (down to
            // the same 30), while `2.675 * 100` and `8.615 * 100` land on an exact .5 and
            // round AWAY from zero. Three behaviours, one expression, both copies.
            1.005,
            0.1 + 0.2,
            2.675,
            8.615,
            // Past SANE_MAX_CHAPTER, and the real production corruption (a TEST upload and
            // a date used as a chapter number) — well inside i64 after ×100.
            5_000.0,
            99_999_999.0,
            20_240_120.0,
            // Where the cast saturates rather than wraps. Both copies saturate identically
            // because both are the same cast; pinning it stops a future rewrite from
            // reaching for a checked conversion on one side only.
            f64::MAX,
            f64::MIN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
        ];

        for n in hostile {
            assert_eq!(
                ChapterLabel::Numbered(n).key("identity-is-ignored-for-numbered"),
                graphql_chapter_key(n),
                "chapter-key DIVERGENCE at {n}: `chapter_label::ChapterLabel::key` and \
                 `graphql::chapter_key` must produce byte-identical keys or admin \
                 chapter-hiding silently stops matching and the ledger silently re-keys"
            );
        }

        // Spot-check the absolute values too, so a change that breaks BOTH sites in the
        // same way (the one thing the agreement check above cannot see) still fails.
        assert_eq!(ChapterLabel::Numbered(10.5).key("x"), "1050");
        assert_eq!(
            ChapterLabel::Numbered(0.005).key("x"),
            "1",
            "half rounds away from 0"
        );
        assert_eq!(
            ChapterLabel::Numbered(1.005).key("x"),
            "100",
            "100.49999… rounds down"
        );

        // The unnumbered arm has NO counterpart in `graphql::chapter_key`, which only takes
        // an `f64`. That asymmetry is itself worth pinning: the `x:` namespace must never
        // produce something the numeric site could also emit, or a oneshot could collide
        // with a real chapter in `chapter_override`.
        let one = ChapterLabel::Unnumbered("Oneshot".into());
        assert!(one.key("uuid").starts_with("x:"));
        assert!(one.key("uuid").parse::<i64>().is_err());
    }

    #[test]
    fn numbers_render_the_way_a_reader_writes_them() {
        assert_eq!(format_number(45.0), "45");
        assert_eq!(format_number(10.5), "10.5");
        assert_eq!(format_number(0.0), "0");
        assert_eq!(format_number(10.25), "10.25");
        assert_eq!(ChapterLabel::Numbered(10.5).text(), "10.5");
        assert_eq!(ChapterLabel::Unnumbered("Extra".into()).text(), "Extra");
    }
}
